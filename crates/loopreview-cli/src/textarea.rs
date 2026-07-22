//! A small multi-line text input for composing comments.
//!
//! Hand-rolled rather than pulled from a crate: the maintained `tui-textarea`
//! targets an older ratatui and cannot share our render buffer. This covers what
//! writing a comment needs — insert, newline, delete, and cursor movement — and
//! renders itself with a visible caret.

use crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};

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
    /// The full text, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer holds no non-whitespace text.
    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|line| line.trim().is_empty())
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

    /// Render the buffer as styled lines, drawing the caret as a reversed cell.
    pub fn render(&self, base: Style) -> Vec<TextLine<'static>> {
        let caret = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        self.lines
            .iter()
            .enumerate()
            .map(|(r, line)| {
                if r != self.row {
                    return TextLine::from(TextSpan::styled(line.clone(), base));
                }
                let chars: Vec<char> = line.chars().collect();
                let before: String = chars[..self.col.min(chars.len())].iter().collect();
                let at: String = chars
                    .get(self.col)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());
                let after: String = if self.col < chars.len() {
                    chars[self.col + 1..].iter().collect()
                } else {
                    String::new()
                };
                TextLine::from(vec![
                    TextSpan::styled(before, base),
                    TextSpan::styled(at, caret),
                    TextSpan::styled(after, base),
                ])
            })
            .collect()
    }
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
}
