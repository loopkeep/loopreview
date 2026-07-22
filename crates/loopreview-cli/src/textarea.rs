//! A small multi-line text input for composing comments.
//!
//! Hand-rolled rather than pulled from a crate: the maintained `tui-textarea`
//! targets an older ratatui and cannot share our render buffer. This covers what
//! writing a comment needs — insert, newline, delete, and cursor movement — and
//! renders itself with a visible caret.

use crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use unicode_width::UnicodeWidthChar;

/// A multi-line editable text buffer with a caret.
pub struct TextArea {
    lines: Vec<String>,
    /// Caret row (line index).
    row: usize,
    /// Caret column, counted in characters within the row.
    col: usize,
}

impl Default for TextArea {
    fn default() -> TextArea {
        TextArea {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }
}

impl TextArea {
    /// A buffer pre-seeded with `text`, caret at the end — for editing the body
    /// of an existing comment.
    pub fn from_text(text: &str) -> TextArea {
        let mut area = TextArea::default();
        area.paste(text);
        area
    }

    /// The full text, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer holds no non-whitespace text.
    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|line| line.trim().is_empty())
    }

    /// Insert pasted text at the caret, splitting on newlines. Carriage returns
    /// are dropped so bracketed-paste input never leaks a stray `\r`.
    pub fn paste(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' => self.newline(),
                '\r' => {}
                other => self.insert_char(other),
            }
        }
    }

    /// Apply an editing key. Submit and cancel are handled by the caller.
    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Enter => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.line_len(self.row),
            _ => {}
        }
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    /// Byte offset of character column `col` within `row`.
    fn byte_at(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(self.lines[row].len())
    }

    fn insert_char(&mut self, c: char) {
        let byte = self.byte_at(self.row, self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
    }

    fn newline(&mut self) {
        let byte = self.byte_at(self.row, self.col);
        let tail = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let start = self.byte_at(self.row, self.col - 1);
            let end = self.byte_at(self.row, self.col);
            self.lines[self.row].replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len(self.row);
            self.lines[self.row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len(self.row) {
            let start = self.byte_at(self.row, self.col);
            let end = self.byte_at(self.row, self.col + 1);
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        }
    }

    fn move_right(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_len(self.row));
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_len(self.row));
        }
    }

    /// Render the buffer wrapped to `width` display columns, drawing the caret as
    /// a highlighted cell. Wrapping and the caret both use character *display*
    /// width, so full-width (CJK) text stays aligned.
    pub fn render(&self, width: usize, base: Style) -> Vec<TextLine<'static>> {
        let caret = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let width = width.max(1);
        let mut out = Vec::new();
        for (r, line) in self.lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let starts = wrap_starts(&chars, width);
            for (b, &start) in starts.iter().enumerate() {
                let end = starts.get(b + 1).copied().unwrap_or(chars.len());
                let segment = &chars[start..end];
                let last = b + 1 == starts.len();
                // The caret belongs to this visual row when its column falls
                // inside it, or sits at the very end of the final row.
                let has_caret = r == self.row
                    && self.col >= start
                    && (self.col < end || (last && self.col == end));
                if has_caret {
                    out.push(caret_row(segment, self.col - start, base, caret));
                } else {
                    out.push(TextLine::from(TextSpan::styled(
                        segment.iter().collect::<String>(),
                        base,
                    )));
                }
            }
        }
        out
    }
}

/// The char indices at which each wrapped visual row starts, breaking when the
/// accumulated display width would exceed `width`.
fn wrap_starts(chars: &[char], width: usize) -> Vec<usize> {
    let mut starts = vec![0];
    let mut used = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > width && i > *starts.last().unwrap() {
            starts.push(i);
            used = 0;
        }
        used += cw;
    }
    starts
}

/// One visual row with the caret drawn at character `idx` within `segment`.
fn caret_row(segment: &[char], idx: usize, base: Style, caret: Style) -> TextLine<'static> {
    let before: String = segment[..idx.min(segment.len())].iter().collect();
    let at: String = segment
        .get(idx)
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".to_string());
    let after: String = if idx < segment.len() {
        segment[idx + 1..].iter().collect()
    } else {
        String::new()
    };
    TextLine::from(vec![
        TextSpan::styled(before, base),
        TextSpan::styled(at, caret),
        TextSpan::styled(after, base),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> TextArea {
        let mut area = TextArea::default();
        for c in text.chars() {
            match c {
                '\n' => area.on_key(KeyCode::Enter),
                other => area.on_key(KeyCode::Char(other)),
            }
        }
        area
    }

    #[test]
    fn inserts_and_joins_lines() {
        let area = typed("ab\ncd");
        assert_eq!(area.text(), "ab\ncd");
        assert!(!area.is_blank());
    }

    #[test]
    fn backspace_joins_lines() {
        let mut area = typed("ab\n");
        // Caret is at the start of the empty second line; backspace rejoins.
        area.on_key(KeyCode::Backspace);
        assert_eq!(area.text(), "ab");
    }

    #[test]
    fn blank_detects_only_whitespace() {
        assert!(TextArea::default().is_blank());
        assert!(typed("  \n\t").is_blank());
        assert!(!typed("x").is_blank());
    }

    #[test]
    fn caret_movement_within_a_line() {
        let mut area = typed("hello");
        area.on_key(KeyCode::Home);
        area.on_key(KeyCode::Right);
        area.on_key(KeyCode::Char('X'));
        assert_eq!(area.text(), "hXello");
    }

    #[test]
    fn paste_inserts_multiline_text_without_carriage_returns() {
        let mut area = TextArea::default();
        area.paste("foo\r\nbar");
        assert_eq!(area.text(), "foo\nbar");
    }

    #[test]
    fn wrap_uses_display_width() {
        // Two full-width chars are 4 cells; width 3 wraps after the first.
        let full: Vec<char> = "あい".chars().collect();
        assert_eq!(wrap_starts(&full, 3), vec![0, 1]);
        // ASCII that fits stays one visual row.
        let ascii: Vec<char> = "abc".chars().collect();
        assert_eq!(wrap_starts(&ascii, 3), vec![0]);
    }
}
