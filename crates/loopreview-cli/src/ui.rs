//! The ratatui review UI: a single scrolling unified-diff pane with file and
//! hunk navigation. All diff data comes from [`loopreview_core`]; this module
//! only lays out rows, routes key events, and paints.

use std::cell::{Cell, RefCell};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use loopreview_core::{Diff, LineKind};

use crate::highlight::{Highlighter, Span as HlSpan};

/// Subtle row tints for changed lines (readable on a dark terminal).
const ADD_BG: Color = Color::Rgb(18, 44, 26);
const DEL_BG: Color = Color::Rgb(52, 24, 27);
/// The bar background used for the header and footer.
const BAR_BG: Color = Color::Rgb(30, 33, 40);

/// How often the event loop wakes to repaint when idle.
const POLL_MS: u64 = 200;

/// Enter the alternate screen, run the review UI over `diff`, then restore the
/// terminal. `label` describes the diff's source (shown in the header).
pub fn run(label: String, diff: Diff) -> Result<()> {
    let mut app = App::new(label, diff, Highlighter::new());
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

/// One line of the flattened, scrollable layout.
enum Row {
    /// A file's header line (index into [`Diff::files`]).
    FileHeader(usize),
    /// An informational note for a file (binary, or no content change).
    Note(String),
    /// A hunk's `@@ … @@` header (file index, hunk index).
    HunkHeader(usize, usize),
    /// A diff line: the file it belongs to and its index into that file's flat
    /// line list (see [`App::flats`]).
    Line { file: usize, flat: usize },
    /// A blank separator between files.
    Spacer,
}

struct App {
    label: String,
    diff: Diff,
    highlighter: Highlighter,

    /// The flattened display rows.
    rows: Vec<Row>,
    /// Row index of each file's header, for file-to-file navigation.
    file_rows: Vec<usize>,
    /// Row indices of every hunk header, for hunk-to-hunk navigation.
    hunk_rows: Vec<usize>,
    /// Per file: the `(hunk, line)` pairs in display order. The highlight cache
    /// for a file is aligned to this list.
    flats: Vec<Vec<(usize, usize)>>,
    /// Lazily-computed highlight spans per file, aligned to [`Self::flats`].
    highlights: RefCell<Vec<Option<Vec<Vec<HlSpan>>>>>,

    /// Width reserved for each line-number column.
    num_width: usize,
    /// Top visible row.
    scroll: usize,
    /// Height of the body pane on the last frame (for paging).
    body_height: Cell<usize>,
    quit: bool,
}

impl App {
    fn new(label: String, diff: Diff, highlighter: Highlighter) -> App {
        let mut rows = Vec::new();
        let mut file_rows = Vec::with_capacity(diff.files.len());
        let mut hunk_rows = Vec::new();
        let mut flats = Vec::with_capacity(diff.files.len());
        let mut max_lineno = 0u32;

        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 {
                rows.push(Row::Spacer);
            }
            file_rows.push(rows.len());
            rows.push(Row::FileHeader(fi));

            let mut flat = Vec::new();
            if file.binary {
                rows.push(Row::Note("binary file — contents not shown".to_string()));
            } else if file.hunks.is_empty() {
                rows.push(Row::Note(format!(
                    "{}, no content changes",
                    file.status.label()
                )));
            } else {
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    hunk_rows.push(rows.len());
                    rows.push(Row::HunkHeader(fi, hi));
                    max_lineno = max_lineno
                        .max(hunk.old_start + hunk.old_lines)
                        .max(hunk.new_start + hunk.new_lines);
                    for li in 0..hunk.lines.len() {
                        rows.push(Row::Line {
                            file: fi,
                            flat: flat.len(),
                        });
                        flat.push((hi, li));
                    }
                }
            }
            flats.push(flat);
        }

        let highlights = RefCell::new(vec![None; diff.files.len()]);
        App {
            label,
            diff,
            highlighter,
            rows,
            file_rows,
            hunk_rows,
            flats,
            highlights,
            num_width: digits(max_lineno).max(3),
            scroll: 0,
            body_height: Cell::new(20),
            quit: false,
        }
    }

    // -- event loop -------------------------------------------------------

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(std::time::Duration::from_millis(POLL_MS))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key.code, key.modifiers);
            }
        }
        Ok(())
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let page = self.body_height.get().max(1);
        match (code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => {
                self.quit = true;
            }
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => self.scroll_by(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => self.scroll_by(-1),
            (KeyCode::Char('d'), true) => self.scroll_by((page / 2) as isize),
            (KeyCode::Char('u'), true) => self.scroll_by(-((page / 2) as isize)),
            (KeyCode::PageDown, _) | (KeyCode::Char(' '), false) => {
                self.scroll_by(page.saturating_sub(1) as isize)
            }
            (KeyCode::PageUp, _) => self.scroll_by(-(page.saturating_sub(1) as isize)),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => self.scroll = 0,
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => self.scroll = self.max_scroll(),
            (KeyCode::Char('n'), false) => self.goto_file(1),
            (KeyCode::Char('p'), false) => self.goto_file(-1),
            (KeyCode::Char('}'), false) | (KeyCode::Char(']'), false) => self.goto_hunk(1),
            (KeyCode::Char('{'), false) | (KeyCode::Char('['), false) => self.goto_hunk(-1),
            _ => {}
        }
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = (self.scroll as isize + delta).clamp(0, self.max_scroll() as isize);
        self.scroll = next as usize;
    }

    /// The file whose header is at or above the current scroll position.
    fn current_file(&self) -> usize {
        let mut current = 0;
        for (fi, &row) in self.file_rows.iter().enumerate() {
            if row <= self.scroll {
                current = fi;
            } else {
                break;
            }
        }
        current
    }

    fn goto_file(&mut self, dir: isize) {
        let current = self.current_file();
        let target = if dir < 0 {
            // Up from mid-file first returns to the current file's top.
            if self.scroll > self.file_rows[current] {
                current
            } else {
                current.saturating_sub(1)
            }
        } else {
            (current + 1).min(self.file_rows.len().saturating_sub(1))
        };
        self.scroll = self.file_rows[target];
    }

    fn goto_hunk(&mut self, dir: isize) {
        let target = if dir > 0 {
            self.hunk_rows.iter().find(|&&r| r > self.scroll).copied()
        } else {
            self.hunk_rows
                .iter()
                .rev()
                .find(|&&r| r < self.scroll)
                .copied()
        };
        if let Some(row) = target {
            self.scroll = row;
        }
    }

    // -- highlighting -----------------------------------------------------

    /// Ensure the highlight cache for `file` is populated, computing it on the
    /// first frame the file becomes visible.
    fn ensure_highlight(&self, file: usize) {
        if self.highlights.borrow()[file].is_some() {
            return;
        }
        let f = &self.diff.files[file];
        let lines: Vec<&str> = self.flats[file]
            .iter()
            .map(|&(h, l)| f.hunks[h].lines[l].content.as_str())
            .collect();
        let highlighted = self.highlighter.highlight(f.display_path(), &lines);
        self.highlights.borrow_mut()[file] = Some(highlighted);
    }

    // -- rendering --------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(f.area());
        self.body_height.set(chunks[1].height as usize);

        self.draw_header(f, chunks[0]);
        self.draw_body(f, chunks[1]);
        self.draw_footer(f, chunks[2]);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let stats = self.diff.stats();
        let bar = Style::default().bg(BAR_BG);
        let line = TextLine::from(vec![
            TextSpan::styled(
                " loopreview ",
                bar.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(format!("· {} ", self.label), bar.fg(Color::Gray)),
            TextSpan::styled(
                format!("· {} file{} ", stats.files, plural(stats.files)),
                bar.fg(Color::Gray),
            ),
            TextSpan::styled(format!("+{} ", stats.insertions), bar.fg(Color::Green)),
            TextSpan::styled(format!("-{}", stats.deletions), bar.fg(Color::Red)),
        ]);
        f.render_widget(Paragraph::new(line).style(bar), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let bar = Style::default().bg(BAR_BG);
        let position = format!(" [{}/{}] ", self.current_file() + 1, self.diff.files.len());
        let help = "j/k scroll · n/p file · [ ] hunk · g/G top/bottom · q quit";
        let line = TextLine::from(vec![
            TextSpan::styled(position, bar.fg(Color::Cyan)),
            TextSpan::styled(help, bar.fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line).style(bar), area);
    }

    fn draw_body(&self, f: &mut Frame, area: Rect) {
        let start = self.scroll;
        let end = (start + area.height as usize).min(self.rows.len());
        let current = self.current_file();
        let lines: Vec<TextLine> = self.rows[start..end]
            .iter()
            .map(|row| self.render_row(row, current))
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_row(&self, row: &Row, current_file: usize) -> TextLine<'static> {
        match row {
            Row::Spacer => TextLine::from(""),
            Row::FileHeader(fi) => self.render_file_header(*fi, *fi == current_file),
            Row::Note(msg) => TextLine::from(TextSpan::styled(
                format!("  {msg}"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
            Row::HunkHeader(fi, hi) => {
                let hunk = &self.diff.files[*fi].hunks[*hi];
                let mut spans = vec![TextSpan::styled(
                    hunk.header(),
                    Style::default().fg(Color::Cyan),
                )];
                if let Some(section) = &hunk.section {
                    spans.push(TextSpan::styled(
                        format!(" {section}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                TextLine::from(spans)
            }
            Row::Line { file, flat } => self.render_diff_line(*file, *flat),
        }
    }

    fn render_file_header(&self, fi: usize, is_current: bool) -> TextLine<'static> {
        let file = &self.diff.files[fi];
        let (added, removed) = file.line_stats();
        let path = match (&file.old_path, &file.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            _ => file.display_path().to_string(),
        };
        let marker = if is_current { "▸ " } else { "  " };
        let path_style = if is_current {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        };
        TextLine::from(vec![
            TextSpan::styled(marker, Style::default().fg(Color::Cyan)),
            TextSpan::styled(path, path_style),
            TextSpan::styled(
                format!("  [{}]", file.status.label()),
                Style::default().fg(status_color(file.status)),
            ),
            TextSpan::styled(format!("  +{added}"), Style::default().fg(Color::Green)),
            TextSpan::styled(format!(" -{removed}"), Style::default().fg(Color::Red)),
        ])
    }

    fn render_diff_line(&self, file: usize, flat: usize) -> TextLine<'static> {
        self.ensure_highlight(file);
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];

        let (bg, sign, sign_color) = match line.kind {
            LineKind::Addition => (Some(ADD_BG), '+', Color::Green),
            LineKind::Deletion => (Some(DEL_BG), '-', Color::Red),
            LineKind::Context => (None, ' ', Color::DarkGray),
        };
        let base = bg.map_or_else(Style::default, |c| Style::default().bg(c));

        let gutter = base.fg(Color::DarkGray);
        let old = optional_number(line.old_lineno, self.num_width);
        let new = optional_number(line.new_lineno, self.num_width);
        let mut spans = vec![
            TextSpan::styled(format!("{old} {new} "), gutter),
            TextSpan::styled(format!("{sign} "), base.fg(sign_color)),
        ];

        let highlights = self.highlights.borrow();
        let file_spans = highlights[file].as_ref().expect("highlight populated");
        for hs in &file_spans[flat] {
            spans.push(TextSpan::styled(
                hs.text.clone(),
                base.fg(Color::Rgb(hs.color.0, hs.color.1, hs.color.2)),
            ));
        }
        TextLine::from(spans)
    }
}

/// Right-align a line number to `width`, or blank when the line is absent on
/// that side.
fn optional_number(number: Option<u32>, width: usize) -> String {
    match number {
        Some(n) => format!("{n:>width$}"),
        None => " ".repeat(width),
    }
}

/// Number of decimal digits in `n` (at least 1).
fn digits(n: u32) -> usize {
    if n == 0 { 1 } else { (n.ilog10() + 1) as usize }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn status_color(status: loopreview_core::ChangeStatus) -> Color {
    use loopreview_core::ChangeStatus::*;
    match status {
        Added => Color::Green,
        Deleted => Color::Red,
        Modified => Color::Yellow,
        Renamed | Copied => Color::Magenta,
    }
}

#[cfg(test)]
mod tests {
    use super::digits;

    #[test]
    fn digit_counts() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(999), 3);
        assert_eq!(digits(1000), 4);
    }
}
