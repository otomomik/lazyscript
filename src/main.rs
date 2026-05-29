// 段階3: ソース discover → 左ツリー一覧 → Enter で起動 → 右出力ペイン接続。
// copy-mode（段階2）は出力ペインに対して引き続き動作する。

mod app;
mod clipboard;
mod config;
mod copy_mode;
mod runner;
mod source;
mod vt;

use std::path::{Path, PathBuf};
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
use ratatui::widgets::{Block, Padding, Paragraph};
use tokio::sync::mpsc::{self, UnboundedSender};

use app::{App, EditField, EditOutcome, EditState, Mode, Row};
use copy_mode::{CopyOutcome, CopyState};
use runner::{ProcEvent, ProcStatus, Process};
use source::Registry;

/// メインループに集約されるイベント。
enum Event {
    Input(CtEvent),
    Proc(ProcEvent),
    /// 監視対象ファイルが変わったので一覧を再構築する。
    Rediscover,
}

/// package.json / scripts.json の変更を監視し、変わったら Rediscover を送る。
/// 返した watcher は drop すると監視が止まるので呼び出し側で保持する。
fn setup_watcher(root: &Path, tx: UnboundedSender<Event>) -> Option<RecommendedWatcher> {
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
    watcher.watch(root, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

/// lazygit ライクに npm scripts / scripts.json を一覧・起動する TUI。
#[derive(Parser)]
#[command(name = "lazyscript", version, about)]
struct Cli {
    /// スクリプトを探す作業ディレクトリ（位置引数でも指定可）。
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// 作業ディレクトリ（位置引数）。--cwd が優先。
    #[arg(value_name = "DIR")]
    dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = match cli.cwd.or(cli.dir) {
        Some(path) => path,
        None => std::env::current_dir()?,
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, root).await;
    ratatui::restore();
    result
}

/// 左右分割（左: Scripts は中身に合わせた固定幅 / 右: コマンド+出力）。
fn layout_main(area: Rect, scripts_width: u16) -> [Rect; 2] {
    Layout::horizontal([Constraint::Length(scripts_width), Constraint::Min(0)]).areas(area)
}

async fn run(terminal: &mut DefaultTerminal, root: PathBuf) -> Result<()> {
    let (term_cols, term_rows) = terminal::size()?;

    let registry = Registry::discover(&root);
    let mut app = App::new(registry, term_cols, term_rows, config::load());

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    // ファイル監視（drop されると監視が止まるのでループ中保持する）。
    let _watcher = setup_watcher(&root, tx.clone());

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
                                EditOutcome::Commit => commit_edit(&mut app, &root),
                            }
                        } else if app.is_filtering() {
                            // フィルタ入力中。
                            match k.code {
                                KeyCode::Esc => app.cancel_filter(),
                                KeyCode::Enter => app.confirm_filter(),
                                KeyCode::Backspace => app.filter_backspace(),
                                KeyCode::Char(c) => app.filter_push(c),
                                _ => {}
                            }
                        } else {
                            let z = awaiting_z;
                            awaiting_z = false;
                            match k.code {
                                KeyCode::Char('q') | KeyCode::Esc => break,
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
                        app.reload(Registry::discover(&root));
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
fn commit_edit(app: &mut App, root: &Path) {
    let (source, original, name, command) = match &app.mode {
        Mode::Edit(s) => (
            s.source,
            s.original.clone(),
            s.name.trim().to_string(),
            s.command.clone(),
        ),
        _ => return,
    };
    app.mode = Mode::Normal;

    let result = match &original {
        Some(old) => app.registry.edit_script(source, old, &name, &command),
        None => app.registry.add_script(source, &name, &command),
    };
    match result {
        Ok(()) => {
            app.error = None;
            app.reload(Registry::discover(root));
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
    let Some(script) = app.selected_script() else {
        return;
    };
    let key = (script.source, script.name.clone());

    // 実行中・終了済みを問わず、既存プロセスは止めて作り直す（= 再実行）。
    if let Some(old_id) = app.script_to_proc.remove(&key)
        && let Some(old) = app.procs.remove(&old_id)
    {
        old.kill();
    }

    let Some(spec) = app.registry.build_command(&script) else {
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

    // フォーカスは Normal=Scripts / Copy=Output。
    let scripts_focused = matches!(app.mode, Mode::Normal);
    render_scripts(f, app, scripts_area, scripts_focused);
    render_command(f, app, command_area);
    render_output(f, app, output_area, !scripts_focused);

    let hint: String = match &app.mode {
        Mode::Normal => {
            if app.is_filtering() {
                format!(" filter: {}_   (Enter: keep   Esc: clear) ", app.filter_query())
            } else {
                " j/k: move   h/l: nav   space: run/stop   a: add   e: edit   /: filter   v: copy   q/^C: quit "
                    .to_string()
            }
        }
        Mode::Edit(state) => {
            let what = if state.original.is_some() { "edit" } else { "add" };
            format!(" {what} script — Tab: switch field   Enter: next/save   Esc: cancel ")
        }
        Mode::Copy(state) => {
            if let Some(prompt) = state.search_prompt() {
                let total = state.search_count().map(|(_, t)| t).unwrap_or(0);
                format!(" {prompt}_   {total} matches   (Enter: jump   Esc: cancel) ")
            } else if let Some((current, total)) = state.search_count() {
                format!(
                    " [{current}/{total}] \"{}\"   n/N: next   y/Enter: yank   Esc: clear   q: exit ",
                    state.search_query().unwrap_or("")
                )
            } else {
                " hjkl: move   w/b/e: word   v/V/C-v: select   o: swap   /?: search   y/Enter: yank   Esc: clear   q: exit "
                    .to_string()
            }
        }
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().add_modifier(Modifier::REVERSED)),
        hint_area,
    );
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
        Row::Header {
            label, collapsed, ..
        } => {
            spans.push(Span::raw(if *collapsed { "▸ " } else { "▾ " }));
            spans.push(Span::styled(
                label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        Row::Script { name, source } => {
            spans.push(Span::raw("  "));
            spans.push(Span::raw(name.clone()));
            if let Some((icon, color)) = status_icon(app.status_of(*source, name), &app.config.theme)
            {
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

/// 新規追加/編集の入力フォームを出力ペイン位置に表示する。
fn render_edit_form(f: &mut Frame, app: &App, area: Rect, pane_focused: bool, state: &EditState) {
    let title = if state.original.is_some() {
        " Edit script "
    } else {
        " Add script "
    };
    let label_style = Style::default().fg(Color::DarkGray);
    let active_style = Style::default().fg(app.config.theme.focus);

    let field_line = |label: &str, value: &str, active: bool| -> Line<'static> {
        let cursor = if active { "_" } else { "" };
        Line::from(vec![
            Span::styled(format!("{label} "), label_style),
            Span::styled(
                format!("{value}{cursor}"),
                if active { active_style } else { Style::default() },
            ),
        ])
    };

    let name_active = matches!(state.field, EditField::Name);
    let lines = vec![
        Line::from(Span::styled(
            format!("Source:  {}", state.source.0),
            label_style,
        )),
        Line::from(""),
        field_line("Name:   ", &state.name, name_active),
        field_line("Command:", &state.command, !name_active),
    ];

    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(title)
                .border_style(border_style(pane_focused, app.config.theme.focus)),
        ),
        area,
    );
}

/// 出力ペインの上に、選択中スクリプトの中身（実行されるコマンド）を表示する。
fn render_command(f: &mut Frame, app: &App, area: Rect) {
    let (title, body) = match (app.selected_script(), app.selected_command()) {
        (Some(script), Some(command)) => {
            (format!(" Command: {} ", script.name), command.to_string())
        }
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
    if let Mode::Edit(state) = &app.mode {
        render_edit_form(f, app, area, pane_focused, state);
        return;
    }
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
        Mode::Normal => match focused {
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
        // Edit は冒頭で別途描画済み。
        Mode::Edit(_) => {}
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
