// 段階3: ソース discover → 左ツリー一覧 → Enter で起動 → 右出力ペイン接続。
// copy-mode（段階2）は出力ペインに対して引き続き動作する。

mod app;
mod clipboard;
mod config;
mod copy_mode;
mod runner;
mod source;
mod text_input;
mod vt;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph};
use tokio::sync::mpsc::{self, UnboundedSender};

use app::{App, EditField, EditOutcome, EditState, Mode, Row};
use copy_mode::{CopyOutcome, CopyState};
use runner::{ProcEvent, ProcStatus, Process};
use source::{AD_HOC, Registry};
use text_input::TextInput;

/// メインループに集約されるイベント。
enum Event {
    Input(CtEvent),
    Proc(ProcEvent),
    /// 監視対象ファイルが変わったので一覧を再構築する。
    Rediscover,
}

/// ad-hoc コマンド入力の結果。
enum RunAction {
    Stay,
    Cancel,
    Submit(String),
}

/// package.json / scripts.json の変更を監視し、変わったら Rediscover を送る。
/// 返した watcher は drop すると監視が止まるので呼び出し側で保持する。
fn setup_watcher(roots: &[PathBuf], tx: UnboundedSender<Event>) -> Option<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && event.paths.iter().any(|p| {
                matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some("package.json") | Some("scripts.json")
                )
            })
        {
            let _ = tx.send(Event::Rediscover);
        }
    })
    .ok()?;
    for root in roots {
        let _ = watcher.watch(root, RecursiveMode::NonRecursive);
    }
    Some(watcher)
}

/// lazygit ライクに npm scripts / scripts.json を一覧・起動する TUI。
#[derive(Parser)]
#[command(name = "lazyscript", version, about)]
struct Cli {
    /// 作業ディレクトリ（複数指定可、未指定なら current_dir）。
    #[arg(value_name = "DIR")]
    dirs: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let roots: Vec<PathBuf> = if cli.dirs.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        cli.dirs
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, roots).await;
    ratatui::restore();
    result
}

/// 左右分割（左: Scripts は中身に合わせた固定幅 / 右: コマンド+出力）。
fn layout_main(area: Rect, scripts_width: u16) -> [Rect; 2] {
    Layout::horizontal([Constraint::Length(scripts_width), Constraint::Min(0)]).areas(area)
}

async fn run(terminal: &mut DefaultTerminal, roots: Vec<PathBuf>) -> Result<()> {
    let (term_cols, term_rows) = terminal::size()?;

    let registry = Registry::discover(&roots);
    let mut app = App::new(registry, term_cols, term_rows, config::load());

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    // ファイル監視（drop されると監視が止まるのでループ中保持する）。
    let _watcher = setup_watcher(&roots, tx.clone());

    // 入力スレッド（crossterm の read は blocking）。
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = event::read() {
                if tx.send(Event::Input(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let mut tick = tokio::time::interval(Duration::from_millis(16));
    let mut awaiting_z = false;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                terminal.draw(|f| draw(f, &app))?;
            }
            maybe = rx.recv() => {
                let Some(ev) = maybe else { break };
                match ev {
                    Event::Input(CtEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                        // Ctrl+C はモードに関わらずアプリを終了する。
                        if k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            break;
                        }
                        if matches!(app.mode, Mode::Copy(_)) {
                            let outcome = if let Mode::Copy(state) = &mut app.mode {
                                state.handle_key(k.code, k.modifiers, app.out_rows as usize)
                            } else {
                                unreachable!()
                            };
                            match outcome {
                                CopyOutcome::Stay => {}
                                CopyOutcome::Cancel => app.mode = Mode::Normal,
                                CopyOutcome::Yank(text) => {
                                    let _ = clipboard::copy(&text, app.config.clipboard);
                                    app.mode = Mode::Normal;
                                }
                            }
                        } else if matches!(app.mode, Mode::Edit(_)) {
                            let outcome = if let Mode::Edit(state) = &mut app.mode {
                                state.handle_key(k.code)
                            } else {
                                unreachable!()
                            };
                            match outcome {
                                EditOutcome::Stay => {}
                                EditOutcome::Cancel => app.mode = Mode::Normal,
                                EditOutcome::Commit => commit_edit(&mut app, &roots),
                            }
                        } else if matches!(app.mode, Mode::Run(_)) {
                            let action = if let Mode::Run(input) = &mut app.mode {
                                match k.code {
                                    KeyCode::Esc => RunAction::Cancel,
                                    KeyCode::Enter => RunAction::Submit(input.as_str().to_string()),
                                    other => {
                                        input.handle_key(other);
                                        RunAction::Stay
                                    }
                                }
                            } else {
                                unreachable!()
                            };
                            match action {
                                RunAction::Stay => {}
                                RunAction::Cancel => app.mode = Mode::Normal,
                                RunAction::Submit(cmd) => {
                                    app.mode = Mode::Normal;
                                    let cmd = cmd.trim().to_string();
                                    if !cmd.is_empty() {
                                        app.add_adhoc(cmd);
                                        run_selected(&mut app, &tx);
                                    }
                                }
                            }
                        } else if app.is_filtering() {
                            // フィルタ入力中。
                            match k.code {
                                KeyCode::Esc => app.cancel_filter(),
                                KeyCode::Enter => app.confirm_filter(),
                                other => app.filter_key(other),
                            }
                        } else {
                            let z = awaiting_z;
                            awaiting_z = false;
                            match k.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                                KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                                KeyCode::Char('h') | KeyCode::Left => app.select_header(),
                                KeyCode::Char('l') | KeyCode::Right => app.select_first_child(),
                                KeyCode::Char('/') => app.start_filter(),
                                // Enter / Space: ヘッダ=開閉 / スクリプト=実行中なら停止・停止中なら起動。
                                KeyCode::Enter | KeyCode::Char(' ') => space_action(&mut app, &tx),
                                KeyCode::Char('z') => awaiting_z = true,
                                KeyCode::Char('a') if z => app.toggle_collapse_current(),
                                KeyCode::Char('o') if z => app.set_collapse_current(false),
                                KeyCode::Char('c') if z => app.set_collapse_current(true),
                                KeyCode::Char('a') => app.begin_add(),
                                KeyCode::Char('e') => app.begin_edit(),
                                KeyCode::Char('!') => app.begin_run(),
                                KeyCode::Char('v') => enter_copy_mode(&mut app),
                                _ => {}
                            }
                        }
                    }
                    Event::Input(CtEvent::Resize(c, r)) => {
                        app.term_cols = c;
                        app.term_rows = r;
                        app.recompute_output_size();
                        for proc in app.procs.values() {
                            proc.resize(app.out_rows, app.out_cols);
                        }
                    }
                    Event::Input(_) => {}
                    Event::Rediscover => {
                        app.reload(Registry::discover(&roots));
                        app.recompute_output_size();
                        for proc in app.procs.values() {
                            proc.resize(app.out_rows, app.out_cols);
                        }
                    }
                    Event::Proc(ProcEvent::Exited { id, status }) => {
                        if let Some(proc) = app.procs.get_mut(&id) {
                            proc.status = status;
                        }
                    }
                }
            }
        }
    }

    for proc in app.procs.values() {
        proc.kill();
    }
    Ok(())
}

/// 編集/追加を確定してファイルへ書き戻し、一覧を再構築する。
fn commit_edit(app: &mut App, roots: &[PathBuf]) {
    let (dir_index, source, original, name, command) = match &app.mode {
        Mode::Edit(s) => (
            s.dir_index,
            s.source,
            s.original.clone(),
            s.name.as_str().trim().to_string(),
            s.command.as_str().to_string(),
        ),
        _ => return,
    };
    app.mode = Mode::Normal;

    let result = match &original {
        Some(old) => app
            .registry
            .edit_script(dir_index, source, old, &name, &command),
        None => app.registry.add_script(dir_index, source, &name, &command),
    };
    match result {
        Ok(()) => {
            app.error = None;
            app.reload(Registry::discover(roots));
            app.recompute_output_size();
            for proc in app.procs.values() {
                proc.resize(app.out_rows, app.out_cols);
            }
        }
        Err(e) => app.error = Some(format!("save failed: {e}")),
    }
}

/// Space: ヘッダなら開閉、スクリプトなら実行中=停止 / 停止中=起動 のトグル。
fn space_action(app: &mut App, tx: &UnboundedSender<Event>) {
    if app.selected_is_header() {
        app.toggle_collapse_current();
        return;
    }
    if is_selected_running(app) {
        stop_selected(app);
    } else {
        run_selected(app, tx);
    }
}

/// 選択中スクリプトが実行中なら SIGINT で停止する（停止中・ヘッダなら何もしない）。
fn stop_selected(app: &App) {
    if is_selected_running(app)
        && let Some(proc) = app.focused_proc()
    {
        proc.interrupt();
    }
}

fn is_selected_running(app: &App) -> bool {
    app.focused_proc()
        .is_some_and(|p| p.status == ProcStatus::Running)
}

/// 選択中スクリプトを実行する。既存プロセスがあれば停止してから起動し直す。
fn run_selected(app: &mut App, tx: &UnboundedSender<Event>) {
    let Some((dir_index, script)) = app.selected_script() else {
        return;
    };
    let key = (dir_index, script.source, script.name.clone());

    // 実行中・終了済みを問わず、既存プロセスは止めて作り直す（= 再実行）。
    if let Some(old_id) = app.script_to_proc.remove(&key)
        && let Some(old) = app.procs.remove(&old_id)
    {
        old.kill();
    }

    let Some(spec) = app.registry.build_command(dir_index, &script) else {
        return;
    };
    let id = app.next_id;
    app.next_id += 1;
    match Process::spawn(
        id,
        script.name.clone(),
        &spec,
        app.out_rows,
        app.out_cols,
        app.config.scrollback,
        tx.clone(),
    ) {
        Ok(proc) => {
            app.procs.insert(id, proc);
            app.script_to_proc.insert(key, id);
            app.error = None;
        }
        Err(e) => app.error = Some(format!("failed to start `{}`: {e}", script.name)),
    }
}

/// フォーカス中プロセスの出力を凍結して copy-mode に入る。
fn enter_copy_mode(app: &mut App) {
    let parser = app.focused_proc().map(|p| Arc::clone(&p.parser));
    if let Some(parser) = parser {
        let grid = {
            let mut guard = parser.lock().unwrap();
            copy_mode::snapshot(&mut guard)
        };
        app.mode = Mode::Copy(CopyState::new(grid, app.out_rows as usize));
    }
}

fn draw(f: &mut Frame, app: &App) {
    let [main_area, hint_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let [scripts_area, right_area] = layout_main(main_area, app.scripts_width());
    // 右側は上にコマンド表示、下に出力。
    let [command_area, output_area] = Layout::vertical([
        Constraint::Length(app::COMMAND_BOX_HEIGHT),
        Constraint::Min(0),
    ])
    .areas(right_area);

    // フォーカスは Scripts 側（Normal/Run/Edit はモーダル入力）か Output 側（Copy）か。
    let scripts_focused = matches!(app.mode, Mode::Normal | Mode::Run(_) | Mode::Edit(_));
    render_scripts(f, app, scripts_area, scripts_focused);
    render_command(f, app, command_area);
    render_output(f, app, output_area, !scripts_focused);

    let bar = Style::default().add_modifier(Modifier::REVERSED);
    if app.is_filtering() {
        // フィルタはブロックカーソルつきで描画（位置を見やすく）。
        let cursor_style = Style::default()
            .bg(app.config.theme.cursor)
            .fg(Color::Black);
        let mut spans = vec![Span::styled(" filter: ", bar)];
        spans.extend(cursor_spans(
            app.filter_query(),
            app.filter_cursor(),
            bar,
            cursor_style,
        ));
        spans.push(Span::styled("   (Enter: keep   Esc: clear) ", bar));
        f.render_widget(Paragraph::new(Line::from(spans)), hint_area);
    } else {
        let hint: String = match &app.mode {
            Mode::Normal => {
                " j/k: move   h/l: nav   space: run/stop   a: add   e: edit   !: run   /: filter   v: copy   q/^C: quit "
                    .to_string()
            }
            Mode::Run(_) => {
                " run command — type a command   Enter: run   Esc: cancel ".to_string()
            }
            Mode::Edit(state) => {
                let what = if state.original.is_some() { "edit" } else { "add" };
                format!(" {what} script — Tab: switch field   Enter: next/save   Esc: cancel ")
            }
            Mode::Copy(state) => {
                if let Some(prompt) = state.search_prompt() {
                    let total = state.search_count().map(|(_, t)| t).unwrap_or(0);
                    format!(" {prompt}   {total} matches   (Enter: jump   Esc: cancel) ")
                } else if let Some((current, total)) = state.search_count() {
                    format!(
                        " [{current}/{total}] \"{}\"   n/N: next   y: yank   Esc: clear   q: exit ",
                        state.search_query().unwrap_or("")
                    )
                } else {
                    " hjkl: move   w/b/e: word   C-u/C-d/C-f/C-b: page   v/V/C-v: select   /?: search   y: yank   Esc: clear   q: exit "
                        .to_string()
                }
            }
        };
        f.render_widget(Paragraph::new(hint).style(bar), hint_area);
    }

    // 入力系モード（ad-hoc 実行 / 追加・編集）はモーダルで重ねる。
    if let Mode::Run(input) = &app.mode {
        render_run_modal(f, app, input);
    }
    if let Mode::Edit(state) = &app.mode {
        render_edit_modal(f, app, state);
    }
}

/// 値をブロックカーソルつきの spans にする（カーソル位置のセルを反転色で表示）。
fn cursor_spans(
    value: &str,
    cursor: usize,
    base: Style,
    cursor_style: Style,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let c = cursor.min(chars.len());
    let before: String = chars[..c].iter().collect();
    let at: String = chars
        .get(c)
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| " ".to_string());
    let mut spans = vec![Span::styled(before, base), Span::styled(at, cursor_style)];
    if c + 1 < chars.len() {
        let after: String = chars[c + 1..].iter().collect();
        spans.push(Span::styled(after, base));
    }
    spans
}

/// 画面中央に width×height の矩形を作る。
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// ad-hoc コマンド入力のモーダル。
/// 最初はコンテンツに合わせて fit、端末幅の 50% に達したら折り返す。
fn render_run_modal(f: &mut Frame, app: &App, input: &TextInput) {
    let theme = app.config.theme;
    let term_w = f.area().width;
    let term_h = f.area().height;
    let content_chars = input.as_str().chars().count() as u16 + 1; // +1 for cursor
    let width = modal_width(content_chars, term_w, 30);
    let inner_w = width.saturating_sub(2).max(1); // 外枠分
    let inner_rows = content_chars.div_ceil(inner_w).max(1);
    let height = (inner_rows + 2).min(term_h).max(3);
    let area = centered_rect(width, height, f.area());
    f.render_widget(Clear, area);

    let block = Block::bordered()
        .title(" Run command ")
        .border_style(border_style(true, theme.focus));
    let inner = block.inner(area);
    f.render_widget(&block, area);

    let cursor_style = Style::default().bg(theme.cursor).fg(Color::Black);
    let spans = cursor_spans(
        input.as_str(),
        input.cursor(),
        Style::default(),
        cursor_style,
    );
    let lines = wrap_line_chars(Line::from(spans), inner_w as usize);
    f.render_widget(Paragraph::new(lines), inner);
}

/// モーダル幅: content_chars (= 中身の最大幅) + 外枠 2 でフィットさせ、
/// 端末幅の 50% を上限とする（最小 min_w、最大 = 端末幅 - 2）。
fn modal_width(content_chars: u16, term_w: u16, min_w: u16) -> u16 {
    let needed = content_chars.saturating_add(2);
    let cap = (term_w / 2).max(min_w).min(term_w.saturating_sub(2).max(min_w));
    needed.max(min_w).min(cap)
}

/// フォーカス中パネルの外枠色。
fn border_style(focused: bool, color: Color) -> Style {
    if focused {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

fn render_scripts(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let block = Block::bordered()
        .title(" Scripts ")
        .border_style(border_style(focused, app.config.theme.focus))
        .padding(Padding::horizontal(app::SCRIPTS_PADDING));
    let inner = block.inner(area);

    if app.registry.is_empty() {
        f.render_widget(
            Paragraph::new("No scripts found.\n(no package.json with scripts here)")
                .block(block)
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let height = inner.height as usize;
    let offset = app.selected.saturating_sub(height.saturating_sub(1));

    let lines: Vec<Line> = app
        .rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, row)| script_line(app, i, row))
        .collect();

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn script_line<'a>(app: &App, index: usize, row: &'a Row) -> Line<'a> {
    let selected = index == app.selected;
    let mut spans: Vec<Span> = Vec::new();

    match row {
        Row::DirHeader {
            label, collapsed, ..
        } => {
            spans.push(Span::raw(if *collapsed { "▸ " } else { "▾ " }));
            spans.push(Span::styled(
                label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        Row::SourceHeader {
            label,
            collapsed,
            source,
            ..
        } => {
            // ad-hoc は DirHeader を持たないため top-level（インデントなし）。
            let indent = if *source == AD_HOC { "" } else { "  " };
            spans.push(Span::raw(indent));
            spans.push(Span::raw(if *collapsed { "▸ " } else { "▾ " }));
            spans.push(Span::styled(
                label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        Row::Script {
            name,
            source,
            dir_index,
        } => {
            let indent = if *source == AD_HOC { "  " } else { "    " };
            spans.push(Span::raw(indent));
            spans.push(Span::raw(name.clone()));
            if let Some((icon, color)) = status_icon(
                app.status_of(*dir_index, *source, name),
                &app.config.theme,
            ) {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(icon, Style::default().fg(color)));
            }
        }
    }

    let line = Line::from(spans);
    if selected {
        let style = match app.config.theme.selection {
            Some(color) => Style::default().bg(color),
            None => Style::default().add_modifier(Modifier::REVERSED),
        };
        line.style(style)
    } else {
        line
    }
}

fn status_icon(status: Option<ProcStatus>, theme: &config::Theme) -> Option<(&'static str, Color)> {
    match status? {
        ProcStatus::Running => Some(("●", theme.running)),
        ProcStatus::Exited(0) => Some(("✓", theme.running)),
        ProcStatus::Exited(_) | ProcStatus::Failed => Some(("✗", theme.failed)),
    }
}

/// 新規追加/編集の入力モーダル。
/// 最初はコンテンツに合わせて fit、端末幅の 50% に達したら各行を文字単位で折り返す。
/// （word-wrap は使わない: ラベル直後で長い1単語が来ると勝手に改行されて Command が消えるため）。
fn render_edit_modal(f: &mut Frame, app: &App, state: &EditState) {
    let theme = app.config.theme;

    let dir = app
        .registry
        .directories
        .get(state.dir_index)
        .map(|d| d.path.as_str())
        .unwrap_or("?");
    let source_label = format!("Source:  {}/{}", dir, state.source.0);
    // "Name:    " / "Command: " ラベル (9 文字) + 区切り空白。
    const FIELD_PREFIX_LEN: usize = 10;

    let source_chars = source_label.chars().count() as u16;
    let name_chars = (FIELD_PREFIX_LEN + state.name.as_str().chars().count() + 1) as u16;
    let cmd_chars = (FIELD_PREFIX_LEN + state.command.as_str().chars().count() + 1) as u16;
    let max_chars = source_chars.max(name_chars).max(cmd_chars);

    let term_w = f.area().width;
    let term_h = f.area().height;
    let width = modal_width(max_chars, term_w, 40);
    let inner_w = width.saturating_sub(2).max(1);

    let label_style = Style::default().fg(Color::DarkGray);
    let cursor_style = Style::default().bg(theme.cursor).fg(Color::Black);

    let field_line = |label: &str, input: &TextInput, active: bool| -> Line<'static> {
        let mut spans = vec![Span::styled(format!("{label} "), label_style)];
        if active {
            spans.extend(cursor_spans(
                input.as_str(),
                input.cursor(),
                Style::default(),
                cursor_style,
            ));
        } else {
            spans.push(Span::raw(input.as_str().to_string()));
        }
        Line::from(spans)
    };

    let name_active = matches!(state.field, EditField::Name);
    let logical_lines = vec![
        Line::from(Span::styled(source_label, label_style)),
        Line::from(""),
        field_line("Name:   ", &state.name, name_active),
        field_line("Command:", &state.command, !name_active),
    ];

    // 文字単位で事前折り返し。これで Wrap を使わずに済み、行数も完全に予測できる。
    let lines: Vec<Line> = logical_lines
        .into_iter()
        .flat_map(|line| wrap_line_chars(line, inner_w as usize))
        .collect();

    let inner_rows = lines.len() as u16;
    let height = (inner_rows + 2).min(term_h).max(6);
    let area = centered_rect(width, height, f.area());
    f.render_widget(Clear, area);

    let title = if state.original.is_some() {
        " Edit script "
    } else {
        " Add script "
    };
    let block = Block::bordered()
        .title(title)
        .border_style(border_style(true, theme.focus));
    let inner = block.inner(area);
    f.render_widget(&block, area);

    f.render_widget(Paragraph::new(lines), inner);
}

/// Line を文字単位で width ごとに折り返す。空行は 1 行のままにする。
/// Span ごとのスタイルは保持する（連続する同スタイル文字を 1 つの Span にまとめる）。
fn wrap_line_chars(line: Line<'_>, width: usize) -> Vec<Line<'static>> {
    let chars: Vec<(String, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = span.style;
            span.content
                .chars()
                .map(move |c| (c.to_string(), style))
        })
        .collect();

    if chars.is_empty() {
        return vec![Line::from(String::new())];
    }
    if width == 0 {
        // 守りの分岐。各文字を独立した行に。
        return chars
            .into_iter()
            .map(|(s, style)| Line::from(Span::styled(s, style)))
            .collect();
    }

    let mut result = Vec::new();
    for chunk in chars.chunks(width) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut buf = String::new();
        let mut buf_style = chunk[0].1;
        for (s, style) in chunk {
            if *style == buf_style {
                buf.push_str(s);
            } else {
                spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
                buf.push_str(s);
                buf_style = *style;
            }
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, buf_style));
        }
        result.push(Line::from(spans));
    }
    result
}

/// 出力ペインの上の情報ボックス。title は親コンテキスト（space 区切り）、body は当該行の値だけ。
/// - Script:       title=" Dir: <p> File: <s> ", body="Command: <cmd>"
/// - SourceHeader: title=" Dir: <p> ",           body="File: <s>"
/// - DirHeader:    title=なし,                    body="Dir: <p>"
fn render_command(f: &mut Frame, app: &App, area: Rect) {
    let dir = app
        .current_dir_index()
        .and_then(|i| app.registry.directories.get(i));
    let source = app.current_source_id();
    let script = app.selected_script();

    let (title, body) = match (script, source, dir) {
        (Some((_, _)), Some(src), Some(d)) => {
            let title = format!(" Dir: {} File: {} ", d.path, src.0);
            let body = format!("Command: {}", app.selected_command().unwrap_or(""));
            (title, body)
        }
        (None, Some(src), Some(d)) => {
            let title = format!(" Dir: {} ", d.path);
            let body = format!("File: {}", src.0);
            (title, body)
        }
        (None, None, Some(d)) => (String::new(), format!("Dir: {}", d.path)),
        _ => (" Command ".to_string(), String::new()),
    };

    f.render_widget(
        Paragraph::new(body)
            .block(Block::bordered().title(title))
            .style(Style::default().fg(Color::White)),
        area,
    );
}

fn render_output(f: &mut Frame, app: &App, area: Rect, pane_focused: bool) {
    let focused = app.focused_proc();
    let title = match focused {
        Some(proc) => format!(" Output: {} [{}] ", proc.script, status_label(proc.status)),
        None => " Output ".to_string(),
    };
    let copy_suffix = if matches!(app.mode, Mode::Copy(_)) {
        " — copy-mode"
    } else {
        ""
    };
    let theme = app.config.theme;
    let mut block = Block::bordered()
        .title(format!("{}{}", title.trim_end(), copy_suffix))
        .border_style(border_style(pane_focused, theme.focus));
    // copy-mode 中は tmux 風に右上へ「現在行/総行数」を色付きで表示。
    if let Mode::Copy(state) = &app.mode {
        let pos = format!(" {}/{} ", state.cursor.row + 1, state.grid.height());
        block = block.title_top(
            Line::from(Span::styled(
                pos,
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.cursor)
                    .add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    f.render_widget(&block, area);

    match &app.mode {
        Mode::Copy(state) => copy_mode::render(
            state,
            inner,
            f.buffer_mut(),
            theme.cursor,
            theme.search,
            theme.selection,
        ),
        Mode::Normal | Mode::Run(_) | Mode::Edit(_) => match focused {
            Some(proc) => {
                let parser = proc.parser.lock().unwrap();
                render_screen(parser.screen(), inner, f.buffer_mut());
            }
            None => {
                let text = app
                    .error
                    .clone()
                    .unwrap_or_else(|| "Press enter/space to run the selected script.".to_string());
                f.render_widget(
                    Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
                    inner,
                );
            }
        },
    }
}

fn status_label(status: ProcStatus) -> String {
    match status {
        ProcStatus::Running => "running".to_string(),
        ProcStatus::Exited(0) => "exited".to_string(),
        ProcStatus::Exited(n) => format!("failed ({n})"),
        ProcStatus::Failed => "failed".to_string(),
    }
}

/// ライブの vt100 スクリーンを ratatui の Buffer に書き込む。
fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let (rows, cols) = screen.size();
    let max_row = area.height.min(rows);
    let max_col = area.width.min(cols);

    for row in 0..max_row {
        for col in 0..max_col {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let Some(buf_cell) = buf.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };
            let contents = cell.contents();
            buf_cell.set_symbol(if contents.is_empty() { " " } else { contents });
            buf_cell.set_style(vt::vt_style(cell));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn render(bytes: &[u8], rows: u16, cols: u16) -> Buffer {
        let mut parser = vt100::Parser::new(rows, cols, 100);
        parser.process(bytes);
        let area = Rect::new(0, 0, cols, rows);
        let mut buf = Buffer::empty(area);
        render_screen(parser.screen(), area, &mut buf);
        buf
    }

    #[test]
    fn renders_plain_text() {
        let buf = render(b"hi", 1, 5);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "i");
    }

    #[test]
    fn applies_fg_color_and_bold() {
        let buf = render(b"\x1b[1;31mX\x1b[0m", 1, 5);
        let cell = &buf[(0, 0)];
        assert_eq!(cell.symbol(), "X");
        assert_eq!(cell.fg, Color::Indexed(1));
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        let buf = render(b"abc\rX", 1, 5);
        assert_eq!(buf[(0, 0)].symbol(), "X");
        assert_eq!(buf[(1, 0)].symbol(), "b");
        assert_eq!(buf[(2, 0)].symbol(), "c");
    }

    #[test]
    fn skips_wide_continuation_cell() {
        let buf = render("あ".as_bytes(), 1, 5);
        assert_eq!(buf[(0, 0)].symbol(), "あ");
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }
}
