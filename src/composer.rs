//! The interactive prompt composer.
//!
//! Owns the multi-line text being typed, the byte-offset cursor, an in-memory
//! history of prior submissions, and the saved draft that is restored if the
//! user scrolls into history and back out. UI code in [`crate::terminal_ui`]
//! drives it via the `handle_*` and `move_*` methods and renders the lines
//! returned by [`Composer::lines`].

use forge_cli::SessionTurn;

#[derive(Debug, Default)]
pub(crate) struct Composer {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_cursor: Option<usize>,
    saved_draft: Option<String>,
}

impl Composer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn line_count(&self) -> usize {
        // `split('\n')` always yields at least one item, even on empty input.
        self.text.split('\n').count()
    }

    /// `(row, col_chars)` of the cursor within the rendered grid. `col_chars`
    /// is a count of characters on the current line up to the cursor, which is
    /// what terminal cursors actually want (assuming monospace ASCII-ish glyphs).
    pub(crate) fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let col = match before.rfind('\n') {
            Some(idx) => before[idx + 1..].chars().count(),
            None => before.chars().count(),
        };
        (row, col)
    }

    pub(crate) fn replace(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_cursor = None;
        self.saved_draft = None;
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.exit_history_browse_if_needed();
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf);
        self.text.insert_str(self.cursor, encoded);
        self.cursor += encoded.len();
    }

    pub(crate) fn insert_str(&mut self, s: &str) {
        self.exit_history_browse_if_needed();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub(crate) fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.exit_history_browse_if_needed();
        let start = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub(crate) fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.exit_history_browse_if_needed();
        let end = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    pub(crate) fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_home(&mut self) {
        // Beginning of the current line (after the most recent '\n').
        self.cursor = match self.text[..self.cursor].rfind('\n') {
            Some(idx) => idx + 1,
            None => 0,
        };
    }

    pub(crate) fn move_end(&mut self) {
        // End of the current line (before the next '\n', or EOF).
        let rest = &self.text[self.cursor..];
        self.cursor += match rest.find('\n') {
            Some(idx) => idx,
            None => rest.len(),
        };
    }

    pub(crate) fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.text.as_bytes();
        let mut idx = self.cursor;
        // Skip trailing separators.
        while idx > 0 && is_word_separator_byte(bytes[idx - 1]) {
            idx -= 1;
        }
        // Walk back through word characters.
        while idx > 0 && !is_word_separator_byte(bytes[idx - 1]) {
            idx -= 1;
        }
        self.cursor = idx;
    }

    pub(crate) fn move_word_right(&mut self) {
        let len = self.text.len();
        if self.cursor >= len {
            return;
        }
        let bytes = self.text.as_bytes();
        let mut idx = self.cursor;
        // Walk forward through word characters.
        while idx < len && !is_word_separator_byte(bytes[idx]) {
            idx += 1;
        }
        // Skip trailing separators.
        while idx < len && is_word_separator_byte(bytes[idx]) {
            idx += 1;
        }
        self.cursor = idx;
    }

    /// Vertical movement uses the column of the current line and tries to
    /// preserve it on the previous line. When already on the first line, this
    /// is a no-op (history navigation is handled separately when the input is
    /// empty).
    pub(crate) fn move_up(&mut self) -> bool {
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            return false;
        }
        let target_row = row - 1;
        self.cursor = position_of(&self.text, target_row, col);
        true
    }

    pub(crate) fn move_down(&mut self) -> bool {
        let (row, col) = self.cursor_row_col();
        if row + 1 >= self.line_count() {
            return false;
        }
        let target_row = row + 1;
        self.cursor = position_of(&self.text, target_row, col);
        true
    }

    pub(crate) fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.exit_history_browse_if_needed();
        let bytes = self.text.as_bytes();
        let mut idx = self.cursor;
        while idx > 0 && is_word_separator_byte(bytes[idx - 1]) {
            idx -= 1;
        }
        while idx > 0 && !is_word_separator_byte(bytes[idx - 1]) {
            idx -= 1;
        }
        self.text.replace_range(idx..self.cursor, "");
        self.cursor = idx;
    }

    pub(crate) fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.exit_history_browse_if_needed();
        let line_start = match self.text[..self.cursor].rfind('\n') {
            Some(idx) => idx + 1,
            None => 0,
        };
        self.text.replace_range(line_start..self.cursor, "");
        self.cursor = line_start;
    }

    pub(crate) fn delete_to_line_end(&mut self) {
        let rest = &self.text[self.cursor..];
        let end_offset = match rest.find('\n') {
            Some(idx) => idx,
            None => rest.len(),
        };
        if end_offset == 0 {
            return;
        }
        self.exit_history_browse_if_needed();
        let end = self.cursor + end_offset;
        self.text.replace_range(self.cursor..end, "");
    }

    /// Submit the current buffer. Returns the text and resets to empty;
    /// also pushes onto history (deduped against the previous entry).
    pub(crate) fn submit(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.text);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.cursor = 0;
            return None;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.cursor = 0;
        self.history_cursor = None;
        self.saved_draft = None;
        Some(text)
    }

    pub(crate) fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_index = match self.history_cursor {
            Some(0) => return false,
            Some(idx) => idx - 1,
            None => {
                self.saved_draft = Some(std::mem::take(&mut self.text));
                self.history.len() - 1
            }
        };
        self.history_cursor = Some(next_index);
        self.text = self.history[next_index].clone();
        self.cursor = self.text.len();
        true
    }

    pub(crate) fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_cursor else {
            return false;
        };
        if idx + 1 >= self.history.len() {
            // Step out of history back to the saved draft (if any).
            self.history_cursor = None;
            self.text = self.saved_draft.take().unwrap_or_default();
            self.cursor = self.text.len();
            return true;
        }
        let next = idx + 1;
        self.history_cursor = Some(next);
        self.text = self.history[next].clone();
        self.cursor = self.text.len();
        true
    }

    pub(crate) fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_cursor = None;
        self.saved_draft = None;
    }

    /// Rebuild history from session user turns. Call after `/resume`, `/new`,
    /// or `/fork` so the up-arrow shows the same prompt list the user already
    /// typed in that conversation.
    pub(crate) fn load_history_from_turns(&mut self, turns: &[SessionTurn]) {
        let history = turns
            .iter()
            .filter_map(|turn| match turn {
                SessionTurn::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        self.set_history(history);
    }

    fn exit_history_browse_if_needed(&mut self) {
        if self.history_cursor.is_some() {
            // The user has started editing, so the history entry is now the
            // draft. Forget the previous unsaved draft.
            self.history_cursor = None;
            self.saved_draft = None;
        }
    }
}

fn is_word_separator_byte(b: u8) -> bool {
    // Word characters are ASCII alphanumerics, `_`, and any byte outside ASCII
    // (so multi-byte UTF-8 letters stay inside a word).
    b.is_ascii() && !b.is_ascii_alphanumeric() && b != b'_'
}

fn prev_char_boundary(text: &str, mut idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    idx -= 1;
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(text: &str, mut idx: usize) -> usize {
    let len = text.len();
    if idx >= len {
        return len;
    }
    idx += 1;
    while idx < len && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn position_of(text: &str, target_row: usize, target_col_chars: usize) -> usize {
    let mut row_start = 0usize;
    let mut row = 0usize;
    for (idx, ch) in text.char_indices() {
        if row == target_row {
            break;
        }
        if ch == '\n' {
            row += 1;
            row_start = idx + 1;
        }
    }
    // Walk forward up to target_col_chars characters or until end-of-line.
    let line_start = row_start;
    let mut pos = line_start;
    for (col, (idx, ch)) in text[line_start..].char_indices().enumerate() {
        if col == target_col_chars || ch == '\n' {
            return line_start + idx;
        }
        pos = line_start + idx + ch.len_utf8();
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_chars_and_moves_cursor() {
        let mut c = Composer::new();
        c.insert_char('a');
        c.insert_char('b');
        c.insert_char('c');
        assert_eq!(c.text(), "abc");
        assert_eq!(c.cursor(), 3);
        c.move_left();
        assert_eq!(c.cursor(), 2);
        c.insert_char('X');
        assert_eq!(c.text(), "abXc");
        assert_eq!(c.cursor(), 3);
    }

    #[test]
    fn newline_then_navigate_rows() {
        let mut c = Composer::new();
        c.insert_str("hello\nworld");
        assert_eq!(c.line_count(), 2);
        assert_eq!(c.cursor_row_col(), (1, 5));
        assert!(c.move_up());
        assert_eq!(c.cursor_row_col(), (0, 5));
        assert!(c.move_down());
        assert_eq!(c.cursor_row_col(), (1, 5));
    }

    #[test]
    fn backspace_and_word_delete() {
        let mut c = Composer::new();
        c.insert_str("hello world");
        c.delete_word_back();
        assert_eq!(c.text(), "hello ");
        c.backspace();
        assert_eq!(c.text(), "hello");
    }

    #[test]
    fn home_and_end_jump_within_line() {
        let mut c = Composer::new();
        c.insert_str("first\nsecond\nthird");
        // Cursor is at end of third.
        c.move_home();
        assert_eq!(c.cursor_row_col(), (2, 0));
        c.move_end();
        assert_eq!(c.cursor_row_col(), (2, 5));
    }

    #[test]
    fn history_navigates_and_restores_draft() {
        let mut c = Composer::new();
        c.set_history(vec!["a".to_string(), "ab".to_string(), "abc".to_string()]);
        c.insert_str("draft");
        assert!(c.history_prev());
        assert_eq!(c.text(), "abc");
        assert!(c.history_prev());
        assert_eq!(c.text(), "ab");
        assert!(c.history_next());
        assert_eq!(c.text(), "abc");
        assert!(c.history_next());
        assert_eq!(c.text(), "draft");
    }

    #[test]
    fn submit_pushes_to_history_and_dedupes() {
        let mut c = Composer::new();
        c.insert_str("hello");
        assert_eq!(c.submit().as_deref(), Some("hello"));
        c.insert_str("hello");
        assert_eq!(c.submit().as_deref(), Some("hello"));
        // Same value submitted twice -> only one history entry.
        c.insert_str("");
        c.set_history(c.history.clone());
        assert!(c.history_prev());
        assert_eq!(c.text(), "hello");
        assert!(!c.history_prev(), "only one entry expected");
    }

    #[test]
    fn submit_ignores_whitespace_only_input() {
        let mut c = Composer::new();
        c.insert_str("   \n  ");
        assert_eq!(c.submit(), None);
        assert!(c.is_empty());
    }
}
