/// Minimal single-line text input. Owns its own cursor.
#[derive(Debug, Default, Clone)]
pub struct TextInput {
    buf: String,
    cursor: usize, // byte offset
}

/// Line-editing operations shared by every text field in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditAction {
    Insert(char),
    Backspace,
    Left,
    Right,
    Home,
    End,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(s: impl Into<String>) -> Self {
        let buf = s.into();
        let cursor = buf.len();
        Self { buf, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// Cursor position as a byte offset into [`TextInput::as_str`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    /// Replace the whole text and put the cursor at the end.
    pub fn set_text(&mut self, s: &str) {
        self.buf.clear();
        self.buf.push_str(s);
        self.cursor = self.buf.len();
    }

    pub fn apply(&mut self, edit: EditAction) {
        match edit {
            EditAction::Insert(c) => self.insert(c),
            EditAction::Backspace => self.backspace(),
            EditAction::Left => self.move_left(),
            EditAction::Right => self.move_right(),
            EditAction::Home => self.move_home(),
            EditAction::End => self.move_end(),
        }
    }

    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let new_cursor = prev_char_boundary(&self.buf, self.cursor);
        self.buf.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.buf, self.cursor);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor = next_char_boundary(&self.buf, self.cursor);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Readline `Ctrl-W`: delete from the cursor back to the start of the
    /// previous word. Skips trailing whitespace, then non-whitespace.
    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.buf.as_bytes();
        let mut i = self.cursor;
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        self.buf.replace_range(i..self.cursor, "");
        self.cursor = i;
    }

    /// Column position for rendering. For multibyte content this is
    /// approximate but acceptable for our prompts.
    pub fn display_cursor(&self) -> u16 {
        self.buf[..self.cursor].chars().count() as u16
    }
}

fn prev_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i.saturating_sub(1);
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j.min(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace() {
        let mut t = TextInput::new();
        t.insert('a');
        t.insert('b');
        t.insert('c');
        assert_eq!(t.as_str(), "abc");
        t.backspace();
        assert_eq!(t.as_str(), "ab");
    }

    #[test]
    fn cursor_movement() {
        let mut t = TextInput::with_text("hello");
        t.move_home();
        assert_eq!(t.cursor(), 0);
        t.insert('X');
        assert_eq!(t.as_str(), "Xhello");
    }

    #[test]
    fn delete_word_back_removes_one_word() {
        let mut t = TextInput::with_text("foo bar baz");
        t.delete_word_back();
        assert_eq!(t.as_str(), "foo bar ");
        t.delete_word_back();
        assert_eq!(t.as_str(), "foo ");
        t.delete_word_back();
        assert_eq!(t.as_str(), "");
        t.delete_word_back();
        assert_eq!(t.as_str(), "");
    }

    #[test]
    fn set_text_replaces_and_moves_cursor_to_end() {
        let mut t = TextInput::with_text("old");
        t.move_home();
        t.set_text("new text");
        assert_eq!(t.as_str(), "new text");
        assert_eq!(t.cursor(), "new text".len());
        t.insert('!');
        assert_eq!(t.as_str(), "new text!");
    }

    #[test]
    fn delete_word_back_mid_word() {
        let mut t = TextInput::with_text("foobar");
        t.move_home();
        for _ in 0..3 {
            t.move_right();
        }
        t.delete_word_back();
        assert_eq!(t.as_str(), "bar");
    }
}
