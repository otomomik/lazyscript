// カーソル位置つきの1行テキスト入力（フィルタ / ad-hoc / 編集フォームで共用）。

use ratatui::crossterm::event::KeyCode;

#[derive(Default)]
pub struct TextInput {
    value: String,
    /// カーソル位置（文字インデックス、0..=文字数）。
    cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// 初期値つき（カーソルは末尾）。
    pub fn seeded(value: String) -> Self {
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// 編集系キーを処理し、処理したら true（Enter/Esc 等の制御キーは false）。
    pub fn handle_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }

    fn insert(&mut self, c: char) {
        let idx = self.byte_index(self.cursor);
        self.value.insert(idx, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let idx = self.byte_index(self.cursor);
            self.value.remove(idx);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let idx = self.byte_index(self.cursor);
            self.value.remove(idx);
        }
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_move_and_delete() {
        let mut t = TextInput::new();
        for c in "abc".chars() {
            t.handle_key(KeyCode::Char(c));
        }
        assert_eq!(t.as_str(), "abc");
        assert_eq!(t.cursor(), 3);

        t.handle_key(KeyCode::Left);
        t.handle_key(KeyCode::Left);
        t.handle_key(KeyCode::Char('X')); // a X b c
        assert_eq!(t.as_str(), "aXbc");
        assert_eq!(t.cursor(), 2);

        t.handle_key(KeyCode::Backspace); // a b c
        assert_eq!(t.as_str(), "abc");

        t.handle_key(KeyCode::Home);
        t.handle_key(KeyCode::Delete); // bc
        assert_eq!(t.as_str(), "bc");
        assert_eq!(t.cursor(), 0);

        t.handle_key(KeyCode::End);
        assert_eq!(t.cursor(), 2);
    }

    #[test]
    fn non_edit_key_returns_false() {
        let mut t = TextInput::new();
        assert!(!t.handle_key(KeyCode::Enter));
        assert!(!t.handle_key(KeyCode::Esc));
        assert!(!t.handle_key(KeyCode::Tab));
    }
}
