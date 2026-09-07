//! A multi-line text input with the editing keys people expect.
//!
//! Hand-rolled rather than pulled from a crate, because the input box is the
//! single most-touched surface in the program and its behaviour has to compose
//! exactly with the rest of the UI: Enter submits but Shift+Enter inserts a
//! newline, history recall only fires when the cursor is on the first line, and
//! the box grows to fit its content up to a cap.
//!
//! The cursor is a `(row, column)` pair over a `Vec<String>` of lines, with
//! column counted in **characters, not bytes**. Byte indices would split
//! multi-byte characters the moment someone types an accent or an emoji.

use unicode_width::UnicodeWidthStr;

/// Most lines the box will grow to before it scrolls internally.
pub const MAX_VISIBLE_LINES: usize = 10;
/// How many submitted prompts to remember.
const HISTORY_LIMIT: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct TextArea {
    lines: Vec<String>,
    /// Cursor row into `lines`.
    row: usize,
    /// Cursor column, in characters.
    col: usize,
    /// Previously submitted inputs, oldest first.
    history: Vec<String>,
    /// Position while browsing history; `None` means "editing a fresh line".
    history_index: Option<usize>,
    /// What was being typed before history browsing started, so Down restores it.
    draft: Option<String>,
}

impl TextArea {
    pub fn new() -> Self {
        Self { lines: vec![String::new()], ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(String::is_empty)
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Display width of the text before the cursor, for placing the caret.
    /// Wide characters (CJK, emoji) occupy two cells.
    pub fn cursor_display_col(&self) -> usize {
        let line = &self.lines[self.row];
        let prefix: String = line.chars().take(self.col).collect();
        prefix.width()
    }

    /// Height the box should occupy, clamped so a long paste cannot swallow
    /// the transcript.
    pub fn visible_height(&self) -> usize {
        self.lines.len().clamp(1, MAX_VISIBLE_LINES)
    }

    /// First visible line when the content is taller than the box.
    pub fn scroll_offset(&self) -> usize {
        self.row.saturating_sub(MAX_VISIBLE_LINES.saturating_sub(1))
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.history_index = None;
        self.draft = None;
    }

    /// Take the contents, record them in history, and reset.
    pub fn take(&mut self) -> String {
        let text = self.text();
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            // Do not record an immediate repeat: pressing Enter twice on the
            // same command should not fill the history with duplicates.
            if self.history.last() != Some(&trimmed) {
                self.history.push(trimmed);
                if self.history.len() > HISTORY_LIMIT {
                    self.history.remove(0);
                }
            }
        }
        self.clear();
        text
    }

    // -- editing ------------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.insert(byte, ch);
        self.col += 1;
        self.history_index = None;
    }

    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.insert_newline();
            } else if ch != '\r' {
                self.insert_char(ch);
            }
        }
    }

    pub fn insert_newline(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        let tail = line.split_off(byte);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
        self.history_index = None;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let start = char_to_byte(line, self.col - 1);
            let end = char_to_byte(line, self.col);
            line.replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            // Joining onto the previous line puts the cursor at the seam.
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&current);
        }
        self.history_index = None;
    }

    pub fn delete(&mut self) {
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            let line = &mut self.lines[self.row];
            let start = char_to_byte(line, self.col);
            let end = char_to_byte(line, self.col + 1);
            line.replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.history_index = None;
    }

    /// Delete the word before the cursor (Ctrl+W / Alt+Backspace).
    pub fn delete_word_before(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut start = self.col;
        // Skip the whitespace immediately behind the cursor, then the word.
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        if start == self.col {
            // Nothing behind the cursor on this line; join to the previous one.
            self.backspace();
            return;
        }
        let line = &mut self.lines[self.row];
        let from = char_to_byte(line, start);
        let to = char_to_byte(line, self.col);
        line.replace_range(from..to, "");
        self.col = start;
        self.history_index = None;
    }

    /// Delete from the cursor to end of line (Ctrl+K).
    pub fn kill_to_end(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.truncate(byte);
        self.history_index = None;
    }

    /// Delete from start of line to the cursor (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.replace_range(..byte, "");
        self.col = 0;
        self.history_index = None;
    }

    // -- motion -------------------------------------------------------------

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_word_left(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        while self.col > 0 && chars[self.col - 1].is_whitespace() {
            self.col -= 1;
        }
        while self.col > 0 && !chars[self.col - 1].is_whitespace() {
            self.col -= 1;
        }
    }

    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let len = chars.len();
        while self.col < len && !chars[self.col].is_whitespace() {
            self.col += 1;
        }
        while self.col < len && chars[self.col].is_whitespace() {
            self.col += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    /// Move up a line. Returns false when already on the first line, which is
    /// the caller's cue to recall history instead.
    pub fn move_up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
        true
    }

    /// Move down a line. Returns false when already on the last line.
    pub fn move_down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
        true
    }

    // -- history ------------------------------------------------------------

    /// Step back through submitted prompts.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                // Remember the in-progress line so Down can restore it.
                self.draft = Some(self.text());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_index = Some(next);
        let entry = self.history[next].clone();
        self.set_text(&entry);
        self.history_index = Some(next);
    }

    /// Step forward, ending on the draft that was interrupted.
    pub fn history_next(&mut self) {
        let Some(current) = self.history_index else { return };
        if current + 1 < self.history.len() {
            let next = current + 1;
            let entry = self.history[next].clone();
            self.set_text(&entry);
            self.history_index = Some(next);
        } else {
            let draft = self.draft.take().unwrap_or_default();
            self.set_text(&draft);
            self.history_index = None;
        }
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Seed history from a resumed session's earlier prompts.
    pub fn load_history(&mut self, entries: Vec<String>) {
        self.history = entries.into_iter().filter(|e| !e.trim().is_empty()).collect();
        if self.history.len() > HISTORY_LIMIT {
            let excess = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..excess);
        }
    }
}

/// Byte offset of character index `col` within `line`.
fn char_to_byte(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area_with(text: &str) -> TextArea {
        let mut a = TextArea::new();
        a.insert_str(text);
        a
    }

    #[test]
    fn typing_and_reading_back_round_trips() {
        let a = area_with("hello");
        assert_eq!(a.text(), "hello");
        assert_eq!(a.cursor(), (0, 5));
        assert!(!a.is_empty());
    }

    #[test]
    fn a_new_area_is_empty_but_has_one_line() {
        let a = TextArea::new();
        assert!(a.is_empty());
        assert_eq!(a.lines().len(), 1, "there is always a line to type on");
        assert_eq!(a.visible_height(), 1);
    }

    #[test]
    fn newlines_split_lines_and_the_box_grows() {
        let a = area_with("one\ntwo\nthree");
        assert_eq!(a.lines().len(), 3);
        assert_eq!(a.visible_height(), 3);
        assert_eq!(a.text(), "one\ntwo\nthree");
    }

    #[test]
    fn the_box_stops_growing_at_the_cap() {
        let a = area_with(&"x\n".repeat(50));
        assert_eq!(a.visible_height(), MAX_VISIBLE_LINES, "a long paste must not eat the transcript");
        assert!(a.scroll_offset() > 0, "it scrolls internally instead");
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_previous_one() {
        let mut a = area_with("ab\ncd");
        a.move_home();
        assert_eq!(a.cursor(), (1, 0));
        a.backspace();
        assert_eq!(a.text(), "abcd");
        assert_eq!(a.cursor(), (0, 2), "the cursor sits at the seam");
    }

    #[test]
    fn editing_is_character_wise_not_byte_wise() {
        // A byte-indexed implementation panics or corrupts here.
        let mut a = area_with("héllo→");
        a.backspace();
        assert_eq!(a.text(), "héllo");
        a.move_left();
        a.insert_char('X');
        assert_eq!(a.text(), "héllXo");
    }

    #[test]
    fn wide_characters_advance_the_caret_by_two_cells() {
        let a = area_with("日本");
        assert_eq!(a.cursor().1, 2, "two characters");
        assert_eq!(a.cursor_display_col(), 4, "but four terminal cells");
    }

    #[test]
    fn word_motion_skips_whitespace_then_the_word() {
        let mut a = area_with("alpha beta gamma");
        a.move_word_left();
        assert_eq!(a.cursor().1, 11, "start of `gamma`");
        a.move_word_left();
        assert_eq!(a.cursor().1, 6, "start of `beta`");
        a.move_word_right();
        assert_eq!(a.cursor().1, 11);
    }

    #[test]
    fn deleting_a_word_removes_it_and_its_trailing_space() {
        let mut a = area_with("alpha beta");
        a.delete_word_before();
        assert_eq!(a.text(), "alpha ");
        a.delete_word_before();
        assert_eq!(a.text(), "");
    }

    #[test]
    fn kill_to_end_and_start_split_at_the_cursor() {
        let mut a = area_with("keep this drop this");
        a.move_home();
        for _ in 0..10 {
            a.move_right();
        }
        a.kill_to_end();
        assert_eq!(a.text(), "keep this ");

        a.kill_to_start();
        assert_eq!(a.text(), "");
    }

    #[test]
    fn up_reports_whether_it_moved_so_the_caller_can_recall_history() {
        let mut a = area_with("one\ntwo");
        assert!(a.move_up(), "there is a line above");
        assert!(!a.move_up(), "on the first line, Up means history");
        assert!(a.move_down());
        assert!(!a.move_down(), "on the last line, Down means history");
    }

    #[test]
    fn submitting_records_history_and_clears_the_box() {
        let mut a = area_with("first command");
        assert_eq!(a.take(), "first command");
        assert!(a.is_empty());
        assert_eq!(a.history_len(), 1);
    }

    #[test]
    fn an_immediate_repeat_is_not_recorded_twice() {
        let mut a = TextArea::new();
        a.insert_str("same");
        a.take();
        a.insert_str("same");
        a.take();
        assert_eq!(a.history_len(), 1, "duplicate consecutive entries are noise");
    }

    #[test]
    fn blank_submissions_are_not_recorded() {
        let mut a = area_with("   ");
        a.take();
        assert_eq!(a.history_len(), 0);
    }

    #[test]
    fn history_walks_backwards_and_stops_at_the_oldest() {
        let mut a = TextArea::new();
        for cmd in ["first", "second", "third"] {
            a.insert_str(cmd);
            a.take();
        }

        a.history_prev();
        assert_eq!(a.text(), "third");
        a.history_prev();
        assert_eq!(a.text(), "second");
        a.history_prev();
        assert_eq!(a.text(), "first");
        a.history_prev();
        assert_eq!(a.text(), "first", "stepping past the oldest stays put");
    }

    #[test]
    fn history_forward_restores_the_interrupted_draft() {
        let mut a = TextArea::new();
        a.insert_str("committed");
        a.take();

        a.insert_str("half-typed thought");
        a.history_prev();
        assert_eq!(a.text(), "committed");

        a.history_next();
        assert_eq!(a.text(), "half-typed thought", "the draft must not be lost");
    }

    #[test]
    fn typing_after_recalling_history_detaches_from_it() {
        let mut a = TextArea::new();
        a.insert_str("original");
        a.take();

        a.history_prev();
        a.insert_char('!');
        assert_eq!(a.text(), "original!");

        // Down should not now jump to a draft; the edit is the current line.
        a.history_next();
        assert_eq!(a.text(), "original!");
    }

    #[test]
    fn history_can_be_seeded_from_a_resumed_session() {
        let mut a = TextArea::new();
        a.load_history(vec!["older".into(), "  ".into(), "newer".into()]);
        assert_eq!(a.history_len(), 2, "blank entries are dropped");

        a.history_prev();
        assert_eq!(a.text(), "newer");
    }

    #[test]
    fn setting_text_puts_the_cursor_at_the_end() {
        let mut a = TextArea::new();
        a.set_text("line one\nline two");
        assert_eq!(a.cursor(), (1, 8));
    }

    #[test]
    fn carriage_returns_from_a_paste_are_dropped() {
        let a = area_with("one\r\ntwo");
        assert_eq!(a.text(), "one\ntwo", "CRLF pastes must not leave stray \\r");
    }

    #[test]
    fn delete_removes_forward_and_joins_lines() {
        let mut a = area_with("ab\ncd");
        a.set_text("ab\ncd");
        a.row = 0;
        a.col = 2;
        a.delete();
        assert_eq!(a.text(), "abcd");
    }
}
