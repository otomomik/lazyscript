// 中央の App 状態と、左ツリーの表示行（フラット配列）まわりのロジック。

use std::collections::{HashMap, HashSet};

use ratatui::crossterm::event::KeyCode;

use crate::config::Config;
use crate::copy_mode::CopyState;
use crate::runner::{ProcId, ProcStatus, Process};
use crate::source::{AD_HOC, Registry, Script, SourceId};
use crate::text_input::TextInput;

/// 左ペインに並ぶ表示行（折りたたみ反映済み）。
pub enum Row {
    DirHeader {
        dir_index: usize,
        label: String,
        collapsed: bool,
    },
    SourceHeader {
        dir_index: usize,
        source: SourceId,
        label: String,
        collapsed: bool,
    },
    Script {
        dir_index: usize,
        source: SourceId,
        name: String,
    },
}

impl Row {
    fn is_script(&self) -> bool {
        matches!(self, Row::Script { .. })
    }

    fn is_header(&self) -> bool {
        matches!(self, Row::DirHeader { .. } | Row::SourceHeader { .. })
    }
}

/// 折りたたみ対象を一意に表す。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollapseKey {
    Dir(usize),
    Source(usize, SourceId),
}

/// h キーで上る親の種別。
enum ParentKind {
    DirHeader(usize),
    SourceHeader(usize, SourceId),
}

pub enum Mode {
    Normal,
    Copy(CopyState),
    Edit(EditState),
    /// その場限りのコマンド入力中。
    Run(TextInput),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Name,
    Command,
}

/// スクリプトの新規追加/編集の入力状態。
pub struct EditState {
    pub dir_index: usize,
    pub source: SourceId,
    /// 既存編集なら元の名前。新規追加なら None。
    pub original: Option<String>,
    pub name: TextInput,
    pub command: TextInput,
    pub field: EditField,
}

pub enum EditOutcome {
    Stay,
    Cancel,
    Commit,
}

impl EditState {
    pub fn handle_key(&mut self, code: KeyCode) -> EditOutcome {
        match code {
            KeyCode::Esc => EditOutcome::Cancel,
            KeyCode::Tab => {
                self.field = match self.field {
                    EditField::Name => EditField::Command,
                    EditField::Command => EditField::Name,
                };
                EditOutcome::Stay
            }
            KeyCode::Enter => match self.field {
                // Name で Enter は Command へ。Command で Enter は確定（名前が空なら確定しない）。
                EditField::Name => {
                    self.field = EditField::Command;
                    EditOutcome::Stay
                }
                EditField::Command if !self.name.as_str().trim().is_empty() => EditOutcome::Commit,
                EditField::Command => EditOutcome::Stay,
            },
            other => {
                self.active_mut().handle_key(other);
                EditOutcome::Stay
            }
        }
    }

    fn active_mut(&mut self) -> &mut TextInput {
        match self.field {
            EditField::Name => &mut self.name,
            EditField::Command => &mut self.command,
        }
    }
}

pub struct App {
    pub registry: Registry,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub collapsed: HashSet<CollapseKey>,
    pub procs: HashMap<ProcId, Process>,
    pub script_to_proc: HashMap<(usize, SourceId, String), ProcId>,
    pub mode: Mode,
    pub next_id: ProcId,
    /// 端末全体のサイズ。レイアウト（PTY サイズ算出）に使う。
    pub term_cols: u16,
    pub term_rows: u16,
    /// 出力ペイン内側のサイズ（PTY と一致させる）。term/scripts 幅から導出。
    pub out_cols: u16,
    pub out_rows: u16,
    /// 直近のエラー（起動失敗など）を出力ペインに表示する。
    pub error: Option<String>,
    pub config: Config,
    /// その場実行したコマンド群（メモリ保持、ファイルなし）。
    ad_hoc: Vec<String>,
    /// 絞り込みクエリ（名前の部分一致、大小無視）。空なら全件。
    filter: TextInput,
    /// フィルタ入力中かどうか。
    filtering: bool,
}

/// 左ツリー（外枠込み）の最小幅。
const MIN_SCRIPTS_WIDTH: u16 = 12;
/// 左ツリーの内側左右パディング（セル数）。
pub const SCRIPTS_PADDING: u16 = 1;
/// 出力ペイン上部に出すコマンド表示ボックスの高さ（外枠込み）。
pub const COMMAND_BOX_HEIGHT: u16 = 3;

/// 多ディレクトリ表示時、SourceHeader / Script に乗せる左インデント。
const MULTI_SOURCE_INDENT: u16 = 2;
const MULTI_SCRIPT_INDENT: u16 = 4;
/// 単一ディレクトリ表示時の Script インデント（従来通り）。
const SINGLE_SCRIPT_INDENT: u16 = 2;

impl App {
    pub fn new(registry: Registry, term_cols: u16, term_rows: u16, config: Config) -> Self {
        let mut app = Self {
            registry,
            rows: Vec::new(),
            selected: 0,
            collapsed: HashSet::new(),
            procs: HashMap::new(),
            script_to_proc: HashMap::new(),
            mode: Mode::Normal,
            next_id: 1,
            term_cols,
            term_rows,
            out_cols: 1,
            out_rows: 1,
            error: None,
            config,
            ad_hoc: Vec::new(),
            filter: TextInput::new(),
            filtering: false,
        };
        app.rebuild_rows();
        app.selected = app.first_script_index().unwrap_or(0);
        app.recompute_output_size();
        app
    }

    pub fn is_multi(&self) -> bool {
        self.registry.is_multi()
    }

    /// 左ツリーの幅（外枠込み）。中身（ラベル/スクリプト名）に合わせ、端末幅の4割で頭打ち。
    pub fn scripts_width(&self) -> u16 {
        let multi = self.is_multi();
        let content = if self.registry.is_empty() {
            28 // "No scripts found" メッセージ用の控えめな幅
        } else {
            let mut max: u16 = 0;
            let source_indent = if multi { MULTI_SOURCE_INDENT } else { 0 };
            let script_indent = if multi {
                MULTI_SCRIPT_INDENT
            } else {
                SINGLE_SCRIPT_INDENT
            };
            for dir in &self.registry.directories {
                if multi {
                    // "▾ " + dir label
                    max = max.max(2 + display_width(&dir.label));
                }
                for g in &dir.source_groups {
                    max = max.max(source_indent + 2 + display_width(&g.label));
                    for s in &g.scripts {
                        // "name" + " ●"(アイコン分2)
                        max = max.max(script_indent + display_width(&s.name) + 2);
                    }
                }
            }
            // ad-hoc は常に top-level（インデントなし）。
            if !self.ad_hoc.is_empty() {
                max = max.max(2 + display_width("commands"));
                for c in &self.ad_hoc {
                    max = max.max(SINGLE_SCRIPT_INDENT + display_width(c) + 2);
                }
            }
            max
        };
        let chrome = 2 + SCRIPTS_PADDING * 2;
        let max = (self.term_cols * 2 / 5).max(MIN_SCRIPTS_WIDTH);
        (content + chrome).clamp(MIN_SCRIPTS_WIDTH, max)
    }

    /// 端末サイズと左ツリー幅から出力ペイン内側サイズ（=PTY）を求める。
    /// レイアウト: 下端ヒント1行、左ツリー、右上にコマンドボックス、その下が出力。
    pub fn recompute_output_size(&mut self) {
        let cols = (self.term_cols.saturating_sub(self.scripts_width())).saturating_sub(2);
        let rows = self
            .term_rows
            .saturating_sub(1) // hint
            .saturating_sub(COMMAND_BOX_HEIGHT)
            .saturating_sub(2); // output 外枠
        self.out_cols = cols.max(1);
        self.out_rows = rows.max(1);
    }

    /// 再 discover した registry で一覧を作り直す。実行中プロセスは維持し、
    /// 選択は同じスクリプトがあればそこへ復帰する。
    pub fn reload(&mut self, registry: Registry) {
        let prev = self
            .selected_script()
            .map(|(di, s)| (di, s.source, s.name));
        self.registry = registry;
        self.rebuild_rows();
        if let Some((dir_index, source, name)) = prev
            && let Some(i) = self.rows.iter().position(|row| {
                matches!(
                    row,
                    Row::Script { dir_index: di, source: s, name: n }
                        if *di == dir_index && *s == source && *n == name
                )
            })
        {
            self.selected = i;
        }
    }

    /// registry のディレクトリ順 + collapsed + filter から表示行を作り直す。
    pub fn rebuild_rows(&mut self) {
        let multi = self.is_multi();
        let needle = self.filter.as_str().to_lowercase();
        let mut rows = Vec::new();

        for (dir_index, dir) in self.registry.directories.iter().enumerate() {
            // フィルタ中はディレクトリ単位の折りたたみも無視して中を見せる。
            let dir_collapsed = needle.is_empty()
                && multi
                && self.collapsed.contains(&CollapseKey::Dir(dir_index));

            // フィルタ中 & 一致なしのディレクトリは隠したい。一旦中身を組み立てて判定。
            let mut child_rows: Vec<Row> = Vec::new();
            for group in &dir.source_groups {
                let matching: Vec<&Script> = group
                    .scripts
                    .iter()
                    .filter(|s| needle.is_empty() || s.name.to_lowercase().contains(&needle))
                    .collect();
                if !needle.is_empty() && matching.is_empty() {
                    continue;
                }
                let collapsed = needle.is_empty()
                    && self
                        .collapsed
                        .contains(&CollapseKey::Source(dir_index, group.id));
                child_rows.push(Row::SourceHeader {
                    dir_index,
                    source: group.id,
                    label: group.label.clone(),
                    collapsed,
                });
                if !collapsed {
                    for script in matching {
                        child_rows.push(Row::Script {
                            dir_index,
                            source: group.id,
                            name: script.name.clone(),
                        });
                    }
                }
            }

            if !needle.is_empty() && child_rows.is_empty() {
                continue;
            }
            if multi {
                rows.push(Row::DirHeader {
                    dir_index,
                    label: dir.label.clone(),
                    collapsed: dir_collapsed,
                });
            }
            if !dir_collapsed {
                rows.extend(child_rows);
            }
        }

        // ad-hoc 実行コマンド群（全ディレクトリの末尾、ファイルなし）。
        let matching_adhoc: Vec<&String> = self
            .ad_hoc
            .iter()
            .filter(|c| needle.is_empty() || c.to_lowercase().contains(&needle))
            .collect();
        if !matching_adhoc.is_empty() {
            let collapsed = needle.is_empty()
                && self.collapsed.contains(&CollapseKey::Source(0, AD_HOC));
            rows.push(Row::SourceHeader {
                dir_index: 0,
                source: AD_HOC,
                label: "commands".to_string(),
                collapsed,
            });
            if !collapsed {
                for command in matching_adhoc {
                    rows.push(Row::Script {
                        dir_index: 0,
                        source: AD_HOC,
                        name: command.clone(),
                    });
                }
            }
        }

        self.rows = rows;
        self.normalize_selection();
    }

    fn first_script_index(&self) -> Option<usize> {
        self.rows.iter().position(Row::is_script)
    }

    /// selected を範囲内に収める（ヘッダ・スクリプトどちらも選択可）。
    fn normalize_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// h: 1段ずつ親へ。Script → SourceHeader → DirHeader。DirHeader 上では何もしない。
    pub fn select_header(&mut self) {
        let parent = match self.rows.get(self.selected) {
            Some(Row::Script {
                dir_index, source, ..
            }) => Some(ParentKind::SourceHeader(*dir_index, *source)),
            Some(Row::SourceHeader { dir_index, .. }) => {
                // ad-hoc には DirHeader が無いので h を no-op に。
                if self.is_multi() {
                    Some(ParentKind::DirHeader(*dir_index))
                } else {
                    None
                }
            }
            Some(Row::DirHeader { .. }) | None => None,
        };
        let Some(parent) = parent else { return };
        for i in (0..self.selected).rev() {
            if matches_parent(&self.rows[i], &parent) {
                self.selected = i;
                return;
            }
        }
    }

    /// l: ヘッダ上なら、配下の最初の行へ移動する（畳まれていれば開く）。
    /// DirHeader → SourceHeader、SourceHeader → Script。
    pub fn select_first_child(&mut self) {
        enum NextKind {
            SourceHeader,
            Script,
        }
        let info = match self.rows.get(self.selected) {
            Some(Row::DirHeader {
                dir_index,
                collapsed,
                ..
            }) => Some((
                CollapseKey::Dir(*dir_index),
                *collapsed,
                NextKind::SourceHeader,
            )),
            Some(Row::SourceHeader {
                dir_index,
                source,
                collapsed,
                ..
            }) => Some((
                CollapseKey::Source(*dir_index, *source),
                *collapsed,
                NextKind::Script,
            )),
            _ => None,
        };
        let Some((key, collapsed, kind)) = info else {
            return;
        };
        if collapsed {
            self.collapsed.remove(&key);
            self.rebuild_rows();
        }
        let next = self.rows.get(self.selected + 1);
        let moves = match kind {
            NextKind::SourceHeader => matches!(next, Some(Row::SourceHeader { .. })),
            NextKind::Script => matches!(next, Some(Row::Script { .. })),
        };
        if moves {
            self.selected += 1;
        }
    }

    /// a: 現在行のソース配下に新規スクリプトを追加する入力を開始（ad-hoc 見出しでは不可）。
    pub fn begin_add(&mut self) {
        if let Some((dir_index, source)) = self.current_target()
            && source != AD_HOC
        {
            self.mode = Mode::Edit(EditState {
                dir_index,
                source,
                original: None,
                name: TextInput::new(),
                command: TextInput::new(),
                field: EditField::Name,
            });
        }
    }

    /// e: 選択中スクリプトの名前/中身を編集する入力を開始（ヘッダ・ad-hoc では何もしない）。
    pub fn begin_edit(&mut self) {
        if let Some((dir_index, script)) = self.selected_script()
            && script.source != AD_HOC
        {
            self.mode = Mode::Edit(EditState {
                dir_index,
                source: script.source,
                original: Some(script.name.clone()),
                name: TextInput::seeded(script.name),
                command: TextInput::seeded(script.command),
                field: EditField::Name,
            });
        }
    }

    /// !: その場限りのコマンド入力を開始する。
    pub fn begin_run(&mut self) {
        self.mode = Mode::Run(TextInput::new());
    }

    /// ad-hoc コマンドを登録して、その行を選択する（実際の起動は呼び出し側）。
    pub fn add_adhoc(&mut self, command: String) {
        if !self.ad_hoc.contains(&command) {
            self.ad_hoc.push(command.clone());
        }
        self.rebuild_rows();
        if let Some(i) = self.rows.iter().position(
            |r| matches!(r, Row::Script { name, source, .. } if *source == AD_HOC && *name == command),
        ) {
            self.selected = i;
        }
    }

    // --- フィルタ ----------------------------------------------------------

    pub fn is_filtering(&self) -> bool {
        self.filtering
    }

    pub fn filter_query(&self) -> &str {
        self.filter.as_str()
    }

    pub fn filter_cursor(&self) -> usize {
        self.filter.cursor()
    }

    pub fn start_filter(&mut self) {
        // 毎回まっさらから入力し直す。
        self.filter.clear();
        self.filtering = true;
        self.rebuild_rows();
    }

    /// 編集系キーをフィルタ入力に渡す（変化があれば一覧を作り直す）。
    pub fn filter_key(&mut self, code: KeyCode) {
        if self.filter.handle_key(code) {
            self.rebuild_rows();
        }
    }

    /// 入力を終えてフィルタは保持。
    pub fn confirm_filter(&mut self) {
        self.filtering = false;
    }

    /// フィルタを破棄して全件に戻す。
    pub fn cancel_filter(&mut self) {
        self.filtering = false;
        self.filter.clear();
        self.rebuild_rows();
    }

    /// 現在行が属する (dir_index, source)。DirHeader の上では None。
    fn current_target(&self) -> Option<(usize, SourceId)> {
        match self.rows.get(self.selected) {
            Some(Row::SourceHeader {
                dir_index, source, ..
            })
            | Some(Row::Script {
                dir_index, source, ..
            }) => Some((*dir_index, *source)),
            _ => None,
        }
    }

    /// 現在行に対応する折りたたみキー（Script 行では None）。
    fn current_collapse_key(&self) -> Option<CollapseKey> {
        match self.rows.get(self.selected) {
            Some(Row::DirHeader { dir_index, .. }) => Some(CollapseKey::Dir(*dir_index)),
            Some(Row::SourceHeader {
                dir_index, source, ..
            }) => Some(CollapseKey::Source(*dir_index, *source)),
            _ => None,
        }
    }

    /// 選択中のスクリプト（ヘッダ上なら None）。ad-hoc は名前=コマンド。
    pub fn selected_script(&self) -> Option<(usize, Script)> {
        match self.rows.get(self.selected) {
            Some(Row::Script {
                dir_index,
                source,
                name,
            }) if *source == AD_HOC => Some((
                *dir_index,
                Script {
                    name: name.clone(),
                    source: AD_HOC,
                    command: name.clone(),
                },
            )),
            Some(Row::Script {
                dir_index,
                source,
                name,
            }) => self
                .registry
                .script(*dir_index, *source, name)
                .cloned()
                .map(|s| (*dir_index, s)),
            _ => None,
        }
    }

    /// 選択中スクリプトの中身（実行されるコマンド文字列）。表示用。
    pub fn selected_command(&self) -> Option<&str> {
        match self.rows.get(self.selected) {
            Some(Row::Script { source, name, .. }) if *source == AD_HOC => Some(name.as_str()),
            Some(Row::Script {
                dir_index,
                source,
                name,
            }) => self
                .registry
                .script(*dir_index, *source, name)
                .map(|s| s.command.as_str()),
            _ => None,
        }
    }

    pub fn selected_is_header(&self) -> bool {
        self.rows.get(self.selected).is_some_and(Row::is_header)
    }

    pub fn toggle_collapse_current(&mut self) {
        if let Some(key) = self.current_collapse_key() {
            if self.collapsed.contains(&key) {
                self.collapsed.remove(&key);
            } else {
                self.collapsed.insert(key);
            }
            self.rebuild_rows();
        }
    }

    pub fn set_collapse_current(&mut self, collapsed: bool) {
        if let Some(key) = self.current_collapse_key() {
            if collapsed {
                self.collapsed.insert(key);
            } else {
                self.collapsed.remove(&key);
            }
            self.rebuild_rows();
        }
    }

    /// 出力ペインに映すプロセス（= 選択中スクリプトのプロセス）。
    /// フォーカスは選択カーソルに追従する。
    pub fn focused_proc(&self) -> Option<&Process> {
        let (dir_index, script) = self.selected_script()?;
        let id = self
            .script_to_proc
            .get(&(dir_index, script.source, script.name))?;
        self.procs.get(id)
    }

    pub fn status_of(
        &self,
        dir_index: usize,
        source: SourceId,
        name: &str,
    ) -> Option<ProcStatus> {
        self.script_to_proc
            .get(&(dir_index, source, name.to_string()))
            .and_then(|id| self.procs.get(id))
            .map(|p| p.status)
    }
}

fn matches_parent(row: &Row, parent: &ParentKind) -> bool {
    match (row, parent) {
        (
            Row::SourceHeader {
                dir_index, source, ..
            },
            ParentKind::SourceHeader(d, s),
        ) => dir_index == d && source == s,
        (Row::DirHeader { dir_index, .. }, ParentKind::DirHeader(d)) => dir_index == d,
        _ => false,
    }
}

/// 表示幅（概算）。スクリプト名/ラベルは概ね ASCII なので文字数で近似する。
fn display_width(s: &str) -> u16 {
    s.chars().count().min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{DirGroup, SourceGroup};

    fn registry(dirs: Vec<(&'static str, Vec<(&'static str, Vec<&str>)>)>) -> Registry {
        let directories = dirs
            .into_iter()
            .map(|(label, sources)| DirGroup {
                label: label.to_string(),
                source_groups: sources
                    .into_iter()
                    .map(|(id, scripts)| SourceGroup {
                        id: SourceId(id),
                        label: id.to_string(),
                        scripts: scripts
                            .into_iter()
                            .map(|name| Script {
                                name: name.to_string(),
                                source: SourceId(id),
                                command: format!("echo {name}"),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        Registry::from_directories(directories)
    }

    fn app() -> App {
        App::new(
            registry(vec![(
                "dir",
                vec![
                    ("package.json", vec!["dev", "build", "test"]),
                    ("scripts.json", vec!["deploy"]),
                ],
            )]),
            80,
            24,
            Config::default(),
        )
    }

    fn multi_app() -> App {
        App::new(
            registry(vec![
                ("a", vec![("package.json", vec!["dev", "build"])]),
                ("b", vec![("scripts.json", vec!["deploy"])]),
            ]),
            80,
            24,
            Config::default(),
        )
    }

    fn names(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|r| match r {
                Row::DirHeader {
                    label, collapsed, ..
                } => format!("[D{}{}]", if *collapsed { "+" } else { "-" }, label),
                Row::SourceHeader {
                    label, collapsed, ..
                } => format!("[{}{}]", if *collapsed { "+" } else { "-" }, label),
                Row::Script { name, .. } => name.clone(),
            })
            .collect()
    }

    #[test]
    fn rows_have_headers_and_scripts() {
        let app = app();
        assert_eq!(
            names(&app),
            [
                "[-package.json]",
                "dev",
                "build",
                "test",
                "[-scripts.json]",
                "deploy"
            ]
        );
    }

    #[test]
    fn selection_starts_on_first_script() {
        let app = app();
        assert_eq!(
            app.selected_script().map(|(_, s)| s.name),
            Some("dev".into())
        );
    }

    #[test]
    fn selected_command_reflects_script_body() {
        let app = app();
        assert_eq!(app.selected_command(), Some("echo dev"));
    }

    #[test]
    fn scripts_width_fits_widest_label() {
        let app = app();
        // 単一ディレクトリ。"▾ "(2) + "package.json"(12) + 外枠(2) + 左右パディング(2) = 18。
        assert_eq!(app.scripts_width(), 18);
    }

    #[test]
    fn navigation_visits_headers_and_scripts() {
        let mut app = app();
        // 初期は最初のスクリプト dev(index 1)。
        assert_eq!(app.selected, 1);
        app.select_prev(); // package.json ヘッダ(index 0)
        assert_eq!(app.selected, 0);
        assert!(app.selected_is_header());
        // 0:H 1:dev 2:build 3:test 4:H(scripts.json) 5:deploy
        for _ in 0..5 {
            app.select_next();
        }
        assert_eq!(app.selected, 5);
        assert_eq!(
            app.selected_script().map(|(_, s)| s.name),
            Some("deploy".into())
        );
        app.select_next(); // 末尾より先へは進まない
        assert_eq!(app.selected, 5);
    }

    #[test]
    fn reload_updates_list_and_keeps_selection() {
        let mut app = app();
        app.select_next(); // dev -> build
        assert_eq!(
            app.selected_script().map(|(_, s)| s.name),
            Some("build".into())
        );
        // 再 discover で lint を追加（build は残す）。
        app.reload(registry(vec![(
            "dir",
            vec![
                ("package.json", vec!["dev", "build", "lint", "test"]),
                ("scripts.json", vec!["deploy"]),
            ],
        )]));
        // 選択は同じ build に復帰。
        assert_eq!(
            app.selected_script().map(|(_, s)| s.name),
            Some("build".into())
        );
        // 新スクリプトが一覧に反映される。
        assert!(names(&app).iter().any(|n| n == "lint"));
    }

    #[test]
    fn filter_limits_to_matching_scripts() {
        let mut app = app();
        app.start_filter();
        for c in "de".chars() {
            app.filter_key(KeyCode::Char(c));
        }
        // "de" を含むのは dev と deploy。各ソース見出しは残る。
        assert_eq!(
            names(&app),
            ["[-package.json]", "dev", "[-scripts.json]", "deploy"]
        );
        app.cancel_filter();
        assert!(names(&app).iter().any(|n| n == "build"));
    }

    #[test]
    fn select_header_and_first_child() {
        let mut app = app(); // selected = dev (index 1)
        app.select_header();
        assert!(app.selected_is_header());
        assert_eq!(app.selected, 0); // package.json
        app.select_first_child();
        assert_eq!(
            app.selected_script().map(|(_, s)| s.name),
            Some("dev".into())
        );
    }

    #[test]
    fn collapse_header_hides_its_scripts() {
        let mut app = app();
        app.select_prev(); // package.json ヘッダへ
        assert!(app.selected_is_header());
        app.toggle_collapse_current(); // 畳む
        assert_eq!(
            names(&app),
            ["[+package.json]", "[-scripts.json]", "deploy"]
        );
        // 折りたたんでもヘッダ上に留まる。
        assert_eq!(app.selected, 0);
        app.toggle_collapse_current(); // 展開
        assert_eq!(
            names(&app),
            [
                "[-package.json]",
                "dev",
                "build",
                "test",
                "[-scripts.json]",
                "deploy"
            ]
        );
    }

    #[test]
    fn multi_dir_emits_dir_headers() {
        let app = multi_app();
        assert_eq!(
            names(&app),
            [
                "[D-a]",
                "[-package.json]",
                "dev",
                "build",
                "[D-b]",
                "[-scripts.json]",
                "deploy"
            ]
        );
    }

    #[test]
    fn multi_dir_h_steps_up_to_dir_header() {
        let mut app = multi_app();
        // 初期選択は最初のスクリプト "dev" (index 2)。
        assert_eq!(app.selected, 2);
        app.select_header(); // -> package.json ヘッダ (index 1)
        assert_eq!(app.selected, 1);
        app.select_header(); // -> a の DirHeader (index 0)
        assert_eq!(app.selected, 0);
        app.select_header(); // DirHeader 上では止まる
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn multi_dir_l_drills_down_dir_then_source() {
        let mut app = multi_app();
        app.select_prev(); // dev -> package.json ヘッダ
        app.select_prev(); // -> a の DirHeader
        assert_eq!(app.selected, 0);
        app.select_first_child(); // DirHeader → SourceHeader
        assert_eq!(app.selected, 1);
        app.select_first_child(); // SourceHeader → 最初の Script
        assert_eq!(app.selected, 2);
        assert_eq!(
            app.selected_script().map(|(_, s)| s.name),
            Some("dev".into())
        );
    }

    #[test]
    fn multi_dir_collapse_dir_hides_all_children() {
        let mut app = multi_app();
        app.select_prev(); // dev -> package.json ヘッダ
        app.select_prev(); // -> a の DirHeader
        app.toggle_collapse_current(); // a を畳む
        assert_eq!(
            names(&app),
            ["[D+a]", "[D-b]", "[-scripts.json]", "deploy"]
        );
    }
}
