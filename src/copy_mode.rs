// tmux 風 Vim copy-mode。
// 入口でスクリーン+スクロールバックを凍結した Grid スナップショットを対象に、
// カーソル移動・単語移動・文字/行/矩形選択・検索・テキスト抽出を行う。

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::vt;

/// copy-mode のキー処理結果。
pub enum CopyOutcome {
    /// copy-mode 継続。
    Stay,
    /// 選択テキストをコピーして copy-mode を抜ける。
    Yank(String),
    /// 何もコピーせず copy-mode を抜ける。
    Cancel,
}

/// 凍結したグリッドの1セル。
#[derive(Clone)]
pub struct GridCell {
    pub symbol: String,
    pub style: Style,
    pub wide_continuation: bool,
}

impl GridCell {
    fn blank() -> Self {
        Self {
            symbol: " ".to_string(),
            style: Style::default(),
            wide_continuation: false,
        }
    }
}

/// copy-mode 入口で凍結したセル格子（スクロールバック込み、行は古い順）。
pub struct Grid {
    pub rows: Vec<Vec<GridCell>>,
    pub width: u16,
}

impl Grid {
    pub fn height(&self) -> usize {
        self.rows.len()
    }

    fn cell(&self, row: usize, col: usize) -> Option<&GridCell> {
        self.rows.get(row).and_then(|r| r.get(col))
    }

    /// 内容のある最後の行。全行空なら 0。
    fn last_content_row(&self) -> usize {
        for row in (0..self.rows.len()).rev() {
            let has_content = self.rows[row]
                .iter()
                .any(|c| !c.wide_continuation && !c.symbol.trim().is_empty());
            if has_content {
                return row;
            }
        }
        0
    }

    /// その行で内容のある最後の列（末尾空白を除く）。空行なら 0。
    fn last_content_col(&self, row: usize) -> usize {
        let Some(cells) = self.rows.get(row) else {
            return 0;
        };
        for col in (0..cells.len()).rev() {
            let c = &cells[col];
            if !c.symbol.trim().is_empty() && !c.wide_continuation {
                return col;
            }
        }
        0
    }
}

/// スクロールバックを含む全グリッドを凍結スナップショットに変換する。
///
/// vt100 はスクロールバックへのランダムアクセスを持たないため、`set_scrollback`
/// で表示窓をずらしながら全論理行を読み出す。最後にオフセットを 0 に戻す。
pub fn snapshot(parser: &mut vt100::Parser) -> Grid {
    let (screen_rows, cols) = parser.screen().size();
    let screen_rows = screen_rows as usize;

    // クランプ挙動を利用して最大スクロールバックオフセット（=画面より上の行数）を得る。
    parser.screen_mut().set_scrollback(usize::MAX);
    let max_offset = parser.screen().scrollback();

    let total = max_offset + screen_rows;
    let mut rows: Vec<Vec<GridCell>> = vec![vec![GridCell::blank(); cols as usize]; total];

    // オフセット k の窓は論理行 [max_offset - k .. +screen_rows) を映す。
    // k を screen_rows 刻みで動かし全行を埋める（端数は重複するが上書きで無害）。
    let mut k = max_offset;
    loop {
        parser.screen_mut().set_scrollback(k);
        let screen = parser.screen();
        let base = max_offset - k; // この窓の row 0 が指す論理行
        for r in 0..screen_rows {
            let logical = base + r;
            if logical >= total {
                break;
            }
            for c in 0..cols {
                if let Some(cell) = screen.cell(r as u16, c) {
                    rows[logical][c as usize] = GridCell {
                        symbol: {
                            let s = cell.contents();
                            if s.is_empty() {
                                " ".to_string()
                            } else {
                                s.to_string()
                            }
                        },
                        style: vt::vt_style(cell),
                        wide_continuation: cell.is_wide_continuation(),
                    };
                }
            }
        }
        if k == 0 {
            break;
        }
        k = k.saturating_sub(screen_rows);
    }

    parser.screen_mut().set_scrollback(0);
    Grid { rows, width: cols }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

impl Pos {
    /// 行優先で2点を昇順に並べる。
    fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
        if (a.row, a.col) <= (b.row, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// 選択の種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectionKind {
    Char,
    Line,
    Block,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Direction {
    Forward,
    Backward,
}

impl Direction {
    fn opposite(self) -> Self {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }
}

/// 検索クエリ入力中の状態。
struct SearchInput {
    query: String,
    direction: Direction,
}

/// 確定済みの検索（ハイライトと n/N ジャンプに使う）。
struct Search {
    query: String,
    direction: Direction,
    matches: Vec<Pos>,
}

pub struct CopyState {
    pub grid: Grid,
    pub cursor: Pos,
    pub anchor: Option<Pos>,
    selection: SelectionKind,
    /// ビューポート最上段に映る論理行。
    pub view_offset: usize,
    pending: String,
    awaiting_g: bool,
    search: Option<Search>,
    search_input: Option<SearchInput>,
}

impl CopyState {
    /// 出力の最後の行にカーソルを置き、末尾が見える状態で開始する。
    pub fn new(grid: Grid, viewport_height: usize) -> Self {
        let height = grid.height();
        let h = viewport_height.max(1);
        let last = grid.last_content_row();
        let cursor = Pos { row: last, col: 0 };
        // 末尾を映しつつ、カーソルが可視領域に収まるよう調整する。
        let mut view_offset = height.saturating_sub(h);
        if last < view_offset {
            view_offset = last;
        } else if last >= view_offset + h {
            view_offset = last + 1 - h;
        }
        Self {
            grid,
            cursor,
            anchor: None,
            selection: SelectionKind::Char,
            view_offset,
            pending: String::new(),
            awaiting_g: false,
            search: None,
            search_input: None,
        }
    }

    /// crossterm のキーを copy-mode の操作に解釈する。
    pub fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        viewport_height: usize,
    ) -> CopyOutcome {
        // 検索クエリ入力中はテキスト編集を優先する。
        if self.search_input.is_some() {
            return self.handle_search_input(code, viewport_height);
        }

        let was_awaiting_g = self.awaiting_g;
        self.awaiting_g = false;
        let ctrl = mods.contains(KeyModifiers::CONTROL);

        match code {
            KeyCode::Char('q') => return CopyOutcome::Cancel,
            // Esc は copy-mode を抜けず、選択と検索ハイライトを消す。
            KeyCode::Esc => {
                self.anchor = None;
                self.search = None;
            }
            KeyCode::Char('y') => {
                return match self.extract_selection() {
                    Some(text) => CopyOutcome::Yank(text),
                    None => CopyOutcome::Cancel,
                };
            }
            KeyCode::Char('h') | KeyCode::Left => self.move_left(),
            KeyCode::Char('l') | KeyCode::Right => self.move_right(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(viewport_height),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(viewport_height),
            KeyCode::Char('0') => {
                if self.has_pending() {
                    self.push_digit('0');
                } else {
                    self.move_line_start();
                }
            }
            KeyCode::Char('^') => self.move_line_start(),
            KeyCode::Char('$') => self.move_line_end(),
            KeyCode::Char('G') => self.move_bottom(viewport_height),
            KeyCode::Char('g') => {
                if was_awaiting_g {
                    self.move_top(viewport_height);
                } else {
                    self.awaiting_g = true;
                }
            }
            KeyCode::Char('d') if ctrl => self.half_page_down(viewport_height),
            KeyCode::Char('u') if ctrl => self.half_page_up(viewport_height),
            KeyCode::Char('f') if ctrl => self.full_page_down(viewport_height),
            KeyCode::Char('b') if ctrl => self.full_page_up(viewport_height),
            KeyCode::Char('w') => self.word_forward(viewport_height),
            KeyCode::Char('b') => self.word_back(viewport_height),
            KeyCode::Char('e') => self.word_end(viewport_height),
            // 選択（Ctrl-v は矩形なので先に判定）。
            KeyCode::Char('v') if ctrl => self.set_selection(SelectionKind::Block),
            KeyCode::Char('v') => self.toggle_selection(),
            KeyCode::Char('V') => self.set_selection(SelectionKind::Line),
            KeyCode::Char('o') => self.swap_ends(viewport_height),
            // 検索。
            KeyCode::Char('/') => self.begin_search(Direction::Forward),
            KeyCode::Char('?') => self.begin_search(Direction::Backward),
            KeyCode::Char('n') => self.search_repeat(true, viewport_height),
            KeyCode::Char('N') => self.search_repeat(false, viewport_height),
            KeyCode::Char(c @ '1'..='9') => self.push_digit(c),
            _ => {}
        }
        CopyOutcome::Stay
    }

    fn handle_search_input(&mut self, code: KeyCode, viewport_height: usize) -> CopyOutcome {
        match code {
            KeyCode::Esc => {
                self.search_input = None;
                self.search = None; // ライブハイライトも消す
            }
            KeyCode::Enter => {
                if let Some(input) = self.search_input.take() {
                    self.run_search(input.query, input.direction, viewport_height);
                }
            }
            KeyCode::Backspace => {
                if let Some(input) = self.search_input.as_mut() {
                    input.query.pop();
                }
                self.update_live_search();
            }
            KeyCode::Char(c) => {
                if let Some(input) = self.search_input.as_mut() {
                    input.query.push(c);
                }
                self.update_live_search();
            }
            _ => {}
        }
        CopyOutcome::Stay
    }

    /// 入力中のクエリで一致を計算し、カーソルは動かさずハイライトだけ更新する。
    fn update_live_search(&mut self) {
        let query = self.search_input.as_ref().map(|i| i.query.clone());
        let direction = self.search_input.as_ref().map(|i| i.direction);
        match (query, direction) {
            (Some(q), Some(dir)) if !q.is_empty() => {
                let matches = self.find_matches(&q);
                self.search = Some(Search {
                    query: q,
                    direction: dir,
                    matches,
                });
            }
            _ => self.search = None,
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 検索入力中なら現在のプロンプト（"/foo" / "?foo"）を返す。
    pub fn search_prompt(&self) -> Option<String> {
        self.search_input.as_ref().map(|i| {
            let prefix = if i.direction == Direction::Forward {
                '/'
            } else {
                '?'
            };
            format!("{prefix}{}", i.query)
        })
    }

    /// 検索中なら (現在の一致番号(1始まり, カーソル外なら0), 総一致数) を返す。
    pub fn search_count(&self) -> Option<(usize, usize)> {
        let search = self.search.as_ref()?;
        let current = search
            .matches
            .iter()
            .position(|m| *m == self.cursor)
            .map(|i| i + 1)
            .unwrap_or(0);
        Some((current, search.matches.len()))
    }

    /// 確定済み検索のクエリ。
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    fn last_row(&self) -> usize {
        self.grid.height().saturating_sub(1)
    }

    fn last_col(&self) -> usize {
        (self.grid.width as usize).saturating_sub(1)
    }

    /// カーソルが常に可視領域に入るよう view_offset を調整する。
    fn clamp_view(&mut self, viewport_height: usize) {
        let h = viewport_height.max(1);
        if self.cursor.row < self.view_offset {
            self.view_offset = self.cursor.row;
        } else if self.cursor.row >= self.view_offset + h {
            self.view_offset = self.cursor.row + 1 - h;
        }
        let max_offset = self.grid.height().saturating_sub(h);
        if self.view_offset > max_offset {
            self.view_offset = max_offset;
        }
    }

    /// 数値プレフィックス（"5j" の 5 等）。未指定なら 1。
    fn take_count(&mut self) -> usize {
        let n = self.pending.parse::<usize>().unwrap_or(1).max(1);
        self.pending.clear();
        n
    }

    pub fn push_digit(&mut self, d: char) {
        // 先頭の 0 は行頭移動なので数値として扱わない。
        if d == '0' && self.pending.is_empty() {
            return;
        }
        self.pending.push(d);
    }

    pub fn move_left(&mut self) {
        let n = self.take_count();
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    pub fn move_right(&mut self) {
        let n = self.take_count();
        self.cursor.col = (self.cursor.col + n).min(self.last_col());
    }

    pub fn move_up(&mut self, viewport_height: usize) {
        let n = self.take_count();
        self.cursor.row = self.cursor.row.saturating_sub(n);
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn move_down(&mut self, viewport_height: usize) {
        let n = self.take_count();
        self.cursor.row = (self.cursor.row + n).min(self.last_row());
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn move_line_start(&mut self) {
        self.pending.clear();
        self.cursor.col = 0;
    }

    pub fn move_line_end(&mut self) {
        self.pending.clear();
        self.cursor.col = self.grid.last_content_col(self.cursor.row);
    }

    pub fn move_top(&mut self, viewport_height: usize) {
        self.pending.clear();
        self.cursor.row = 0;
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn move_bottom(&mut self, viewport_height: usize) {
        self.pending.clear();
        self.cursor.row = self.last_row();
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn half_page_down(&mut self, viewport_height: usize) {
        self.pending.clear();
        let step = (viewport_height / 2).max(1);
        self.cursor.row = (self.cursor.row + step).min(self.last_row());
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn half_page_up(&mut self, viewport_height: usize) {
        self.pending.clear();
        let step = (viewport_height / 2).max(1);
        self.cursor.row = self.cursor.row.saturating_sub(step);
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn full_page_down(&mut self, viewport_height: usize) {
        self.pending.clear();
        let step = viewport_height.max(1);
        self.cursor.row = (self.cursor.row + step).min(self.last_row());
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    pub fn full_page_up(&mut self, viewport_height: usize) {
        self.pending.clear();
        let step = viewport_height.max(1);
        self.cursor.row = self.cursor.row.saturating_sub(step);
        self.clamp_cursor_col();
        self.clamp_view(viewport_height);
    }

    fn clamp_cursor_col(&mut self) {
        self.cursor.col = self.cursor.col.min(self.last_col());
    }

    // --- 単語移動 -----------------------------------------------------------

    fn is_word_cell(&self, row: usize, col: usize) -> bool {
        match self.grid.cell(row, col) {
            Some(c) => c.wide_continuation || !c.symbol.trim().is_empty(),
            None => false,
        }
    }

    /// 各行の非空白の連なりを単語 (開始, 終了) として全行ぶん集める。
    fn words(&self) -> Vec<(Pos, Pos)> {
        let mut words = Vec::new();
        let width = self.grid.width as usize;
        for row in 0..self.grid.height() {
            let mut start: Option<usize> = None;
            for col in 0..width {
                if self.is_word_cell(row, col) {
                    start.get_or_insert(col);
                } else if let Some(s) = start.take() {
                    words.push((Pos { row, col: s }, Pos { row, col: col - 1 }));
                }
            }
            if let Some(s) = start {
                words.push((
                    Pos { row, col: s },
                    Pos {
                        row,
                        col: width - 1,
                    },
                ));
            }
        }
        words
    }

    pub fn word_forward(&mut self, viewport_height: usize) {
        self.pending.clear();
        let cur = (self.cursor.row, self.cursor.col);
        if let Some((start, _)) = self.words().into_iter().find(|(s, _)| (s.row, s.col) > cur) {
            self.cursor = start;
            self.clamp_view(viewport_height);
        }
    }

    pub fn word_back(&mut self, viewport_height: usize) {
        self.pending.clear();
        let cur = (self.cursor.row, self.cursor.col);
        if let Some((start, _)) = self
            .words()
            .into_iter()
            .rev()
            .find(|(s, _)| (s.row, s.col) < cur)
        {
            self.cursor = start;
            self.clamp_view(viewport_height);
        }
    }

    pub fn word_end(&mut self, viewport_height: usize) {
        self.pending.clear();
        let cur = (self.cursor.row, self.cursor.col);
        if let Some((_, end)) = self.words().into_iter().find(|(_, e)| (e.row, e.col) > cur) {
            self.cursor = end;
            self.clamp_view(viewport_height);
        }
    }

    // --- 選択 ---------------------------------------------------------------

    fn set_selection(&mut self, kind: SelectionKind) {
        match self.anchor {
            None => {
                self.anchor = Some(self.cursor);
                self.selection = kind;
            }
            Some(_) => {
                if self.selection == kind {
                    self.anchor = None;
                } else {
                    self.selection = kind;
                }
            }
        }
    }

    /// 文字選択をトグル（開始/解除）する。
    pub fn toggle_selection(&mut self) {
        self.set_selection(SelectionKind::Char);
    }

    /// 選択の端（anchor とカーソル）を入れ替える。
    fn swap_ends(&mut self, viewport_height: usize) {
        if let Some(anchor) = self.anchor {
            self.anchor = Some(self.cursor);
            self.cursor = anchor;
            self.clamp_view(viewport_height);
        }
    }

    fn in_selection(&self, pos: Pos) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        let cursor = self.cursor;
        match self.selection {
            SelectionKind::Char => {
                let (s, e) = Pos::ordered(anchor, cursor);
                (pos.row, pos.col) >= (s.row, s.col) && (pos.row, pos.col) <= (e.row, e.col)
            }
            SelectionKind::Line => {
                let (r0, r1) = (anchor.row.min(cursor.row), anchor.row.max(cursor.row));
                pos.row >= r0 && pos.row <= r1
            }
            SelectionKind::Block => {
                let (r0, r1) = (anchor.row.min(cursor.row), anchor.row.max(cursor.row));
                let (c0, c1) = (anchor.col.min(cursor.col), anchor.col.max(cursor.col));
                pos.row >= r0 && pos.row <= r1 && pos.col >= c0 && pos.col <= c1
            }
        }
    }

    /// [from, to] の非 continuation セルを連結する。
    fn row_slice(&self, row: usize, from: usize, to: usize) -> String {
        let mut s = String::new();
        for col in from..=to {
            if let Some(cell) = self.grid.cell(row, col)
                && !cell.wide_continuation
            {
                s.push_str(&cell.symbol);
            }
        }
        s
    }

    /// 選択範囲のテキストを抽出する（種類ごと、各行末尾の空白は trim）。
    pub fn extract_selection(&self) -> Option<String> {
        let anchor = self.anchor?;
        let cursor = self.cursor;
        let width = self.grid.width as usize;
        if width == 0 {
            return Some(String::new());
        }
        let last_col = width - 1;

        let text = match self.selection {
            SelectionKind::Char => {
                let (start, end) = Pos::ordered(anchor, cursor);
                let mut out = String::new();
                for row in start.row..=end.row {
                    let from = if row == start.row { start.col } else { 0 };
                    let to = if row == end.row { end.col } else { last_col };
                    out.push_str(self.row_slice(row, from, to).trim_end());
                    if row != end.row {
                        out.push('\n');
                    }
                }
                out
            }
            SelectionKind::Line => {
                let (r0, r1) = (anchor.row.min(cursor.row), anchor.row.max(cursor.row));
                (r0..=r1)
                    .map(|row| self.row_slice(row, 0, last_col).trim_end().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            SelectionKind::Block => {
                let (r0, r1) = (anchor.row.min(cursor.row), anchor.row.max(cursor.row));
                let (c0, c1) = (anchor.col.min(cursor.col), anchor.col.max(cursor.col));
                (r0..=r1)
                    .map(|row| self.row_slice(row, c0, c1).trim_end().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        Some(text)
    }

    // --- 検索 ---------------------------------------------------------------

    fn begin_search(&mut self, direction: Direction) {
        self.search_input = Some(SearchInput {
            query: String::new(),
            direction,
        });
    }

    fn run_search(&mut self, query: String, direction: Direction, viewport_height: usize) {
        if query.is_empty() {
            self.search = None;
            return;
        }
        let matches = self.find_matches(&query);
        if let Some(pos) = next_match(&matches, self.cursor, direction == Direction::Forward) {
            self.cursor = pos;
            self.clamp_view(viewport_height);
        }
        self.search = Some(Search {
            query,
            direction,
            matches,
        });
    }

    fn search_repeat(&mut self, same_direction: bool, viewport_height: usize) {
        let Some(search) = &self.search else {
            return;
        };
        let direction = if same_direction {
            search.direction
        } else {
            search.direction.opposite()
        };
        let matches = search.matches.clone();
        if let Some(pos) = next_match(&matches, self.cursor, direction == Direction::Forward) {
            self.cursor = pos;
            self.clamp_view(viewport_height);
        }
    }

    /// クエリ（大文字小文字を無視）の出現位置を行優先で集める。
    fn find_matches(&self, query: &str) -> Vec<Pos> {
        let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for row in 0..self.grid.height() {
            // (小文字化した文字, 由来の列) を作る。
            let mut chars: Vec<(char, usize)> = Vec::new();
            for col in 0..self.grid.width as usize {
                if let Some(cell) = self.grid.cell(row, col)
                    && !cell.wide_continuation
                {
                    for ch in cell.symbol.chars().flat_map(char::to_lowercase) {
                        chars.push((ch, col));
                    }
                }
            }
            if chars.len() < needle.len() {
                continue;
            }
            for i in 0..=chars.len() - needle.len() {
                if (0..needle.len()).all(|k| chars[i + k].0 == needle[k]) {
                    matches.push(Pos {
                        row,
                        col: chars[i].1,
                    });
                }
            }
        }
        matches
    }

    fn is_search_hit(&self, pos: Pos) -> bool {
        let Some(search) = &self.search else {
            return false;
        };
        let len = search.query.chars().count();
        search
            .matches
            .iter()
            .any(|m| m.row == pos.row && pos.col >= m.col && pos.col < m.col + len)
    }
}

/// matches（行優先で昇順）から、カーソルの次/前の一致を返す（端は折り返し）。
fn next_match(matches: &[Pos], cursor: Pos, forward: bool) -> Option<Pos> {
    if matches.is_empty() {
        return None;
    }
    let cur = (cursor.row, cursor.col);
    if forward {
        matches
            .iter()
            .find(|m| (m.row, m.col) > cur)
            .or_else(|| matches.first())
            .copied()
    } else {
        matches
            .iter()
            .rev()
            .find(|m| (m.row, m.col) < cur)
            .or_else(|| matches.last())
            .copied()
    }
}

/// 凍結グリッドを選択/検索/カーソルのハイライト付きで描画する。
/// `selection_color` が None なら選択は反転表示。
pub fn render(
    state: &CopyState,
    area: Rect,
    buf: &mut Buffer,
    cursor_color: Color,
    search_color: Color,
    selection_color: Option<Color>,
) {
    let cursor_style = Style::default()
        .bg(cursor_color)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    for r in 0..area.height {
        let logical = state.view_offset + r as usize;
        if logical >= state.grid.height() {
            break;
        }
        for c in 0..area.width.min(state.grid.width) {
            let pos = Pos {
                row: logical,
                col: c as usize,
            };
            let Some(cell) = state.grid.cell(logical, c as usize) else {
                continue;
            };
            if cell.wide_continuation {
                continue;
            }
            let Some(buf_cell) = buf.cell_mut((area.x + c, area.y + r)) else {
                continue;
            };
            buf_cell.set_symbol(&cell.symbol);

            let mut style = cell.style;
            if state.is_search_hit(pos) {
                style = style.bg(search_color).fg(Color::Black);
            }
            if state.in_selection(pos) {
                style = match selection_color {
                    Some(color) => style.bg(color),
                    None => style.add_modifier(Modifier::REVERSED),
                };
            }
            if pos == state.cursor {
                style = cursor_style;
            }
            buf_cell.set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_from(lines: &[&str], width: u16) -> Grid {
        let rows = lines
            .iter()
            .map(|line| {
                let mut cells: Vec<GridCell> = line
                    .chars()
                    .map(|ch| GridCell {
                        symbol: ch.to_string(),
                        style: Style::default(),
                        wide_continuation: false,
                    })
                    .collect();
                cells.resize(width as usize, GridCell::blank());
                cells
            })
            .collect();
        Grid { rows, width }
    }

    fn state_with(lines: &[&str], width: u16) -> CopyState {
        CopyState::new(grid_from(lines, width), lines.len())
    }

    #[test]
    fn snapshot_reads_scrollback_in_order() {
        // 画面 2 行で 5 行出力 → 上 3 行はスクロールバックに入る。
        let mut parser = vt100::Parser::new(2, 8, 100);
        parser.process(b"l0\r\nl1\r\nl2\r\nl3\r\nl4");
        let grid = snapshot(&mut parser);
        assert_eq!(grid.height(), 5);
        let line = |r: usize| {
            grid.rows[r]
                .iter()
                .map(|c| c.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(line(0), "l0");
        assert_eq!(line(4), "l4");
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn starts_at_last_content_row() {
        let s = CopyState::new(grid_from(&["a", "b", "c", "", ""], 8), 2);
        assert_eq!(s.cursor, Pos { row: 2, col: 0 });
        assert!(s.view_offset <= 2 && 2 < s.view_offset + 2);
    }

    #[test]
    fn char_selection_single_line() {
        let mut s = state_with(&["hello world"], 16);
        s.cursor = Pos { row: 0, col: 0 };
        s.toggle_selection();
        s.cursor = Pos { row: 0, col: 4 };
        assert_eq!(s.extract_selection().as_deref(), Some("hello"));
    }

    #[test]
    fn char_selection_spans_rows_and_trims() {
        let mut s = state_with(&["abc", "defgh"], 16);
        s.cursor = Pos { row: 0, col: 1 };
        s.toggle_selection();
        s.cursor = Pos { row: 1, col: 2 };
        assert_eq!(s.extract_selection().as_deref(), Some("bc\ndef"));
    }

    #[test]
    fn line_selection_takes_whole_lines() {
        let mut s = state_with(&["abc", "defgh", "ij"], 16);
        s.cursor = Pos { row: 0, col: 2 };
        s.set_selection(SelectionKind::Line);
        s.cursor = Pos { row: 1, col: 0 };
        // 列は無視して 0 行目と 1 行目の全体。
        assert_eq!(s.extract_selection().as_deref(), Some("abc\ndefgh"));
    }

    #[test]
    fn block_selection_takes_column_range() {
        let mut s = state_with(&["abcd", "efgh", "ijkl"], 16);
        s.cursor = Pos { row: 0, col: 1 };
        s.set_selection(SelectionKind::Block);
        s.cursor = Pos { row: 2, col: 2 };
        // 各行 col1..=2 を切り出す。
        assert_eq!(s.extract_selection().as_deref(), Some("bc\nfg\njk"));
    }

    #[test]
    fn word_motions_move_between_words() {
        let mut s = state_with(&["foo bar baz"], 16);
        s.cursor = Pos { row: 0, col: 0 };
        s.word_forward(1);
        assert_eq!(s.cursor.col, 4); // "bar"
        s.word_end(1);
        assert_eq!(s.cursor.col, 6); // "bar" の末尾
        s.word_forward(1);
        assert_eq!(s.cursor.col, 8); // "baz"
        s.word_back(1);
        assert_eq!(s.cursor.col, 4); // 戻って "bar"
    }

    #[test]
    fn swap_ends_swaps_anchor_and_cursor() {
        let mut s = state_with(&["abcdef"], 16);
        s.cursor = Pos { row: 0, col: 1 };
        s.toggle_selection();
        s.cursor = Pos { row: 0, col: 4 };
        s.swap_ends(1);
        assert_eq!(s.cursor, Pos { row: 0, col: 1 });
        // 端を入れ替えても選択範囲（テキスト）は変わらない。
        assert_eq!(s.extract_selection().as_deref(), Some("bcde"));
    }

    #[test]
    fn search_jumps_to_next_match() {
        let mut s = state_with(&["abc", "xabc", "abc"], 8);
        s.cursor = Pos { row: 0, col: 0 };
        // /abc を確定 → カーソルより後の最初の一致 (1,1) へ。
        s.run_search("abc".to_string(), Direction::Forward, 3);
        assert_eq!(s.cursor, Pos { row: 1, col: 1 });
        // n で次 (2,0)。
        s.search_repeat(true, 3);
        assert_eq!(s.cursor, Pos { row: 2, col: 0 });
        // さらに n で先頭へ折り返し (0,0)。
        s.search_repeat(true, 3);
        assert_eq!(s.cursor, Pos { row: 0, col: 0 });
        // N で逆方向 → (2,0)。
        s.search_repeat(false, 3);
        assert_eq!(s.cursor, Pos { row: 2, col: 0 });
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut s = state_with(&["Hello"], 8);
        s.cursor = Pos { row: 0, col: 4 };
        s.run_search("hello".to_string(), Direction::Forward, 1);
        assert_eq!(s.cursor, Pos { row: 0, col: 0 }); // 折り返して先頭一致
    }

    #[test]
    fn search_prompt_reflects_typing() {
        let mut s = state_with(&["abc"], 8);
        s.handle_key(KeyCode::Char('/'), KeyModifiers::NONE, 1);
        s.handle_key(KeyCode::Char('a'), KeyModifiers::NONE, 1);
        s.handle_key(KeyCode::Char('b'), KeyModifiers::NONE, 1);
        assert_eq!(s.search_prompt().as_deref(), Some("/ab"));
        s.handle_key(KeyCode::Enter, KeyModifiers::NONE, 1);
        assert_eq!(s.search_prompt(), None);
    }

    #[test]
    fn esc_clears_selection_but_stays() {
        let mut s = state_with(&["abc"], 8);
        s.toggle_selection();
        assert!(s.anchor.is_some());
        let outcome = s.handle_key(KeyCode::Esc, KeyModifiers::NONE, 1);
        assert!(matches!(outcome, CopyOutcome::Stay));
        assert!(s.anchor.is_none());
    }

    #[test]
    fn q_quits_copy_mode() {
        let mut s = state_with(&["abc"], 8);
        let outcome = s.handle_key(KeyCode::Char('q'), KeyModifiers::NONE, 1);
        assert!(matches!(outcome, CopyOutcome::Cancel));
    }

    #[test]
    fn numeric_prefix_repeats_motion() {
        let mut s = state_with(&["0123456789"], 16);
        s.push_digit('3');
        s.move_right();
        assert_eq!(s.cursor.col, 3);
    }
}
