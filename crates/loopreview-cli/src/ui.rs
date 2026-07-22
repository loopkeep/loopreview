//! The ratatui review UI: a scrolling unified-diff pane with a line cursor that
//! always points at a `(file, side, line)` anchor, plus file and hunk
//! navigation. All diff data comes from [`loopreview_core`]; this module lays
//! out rows, routes key events, and paints. Changed words within a modified
//! line are emphasized using the core's intra-line diff.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use loopreview_core::{Diff, LineKind, Segment, word_diff};

use crate::highlight::{Highlighter, Span as HlSpan};

/// Subtle row tints for changed lines, and the stronger tint for the exact
/// words that changed within them (readable on a dark terminal).
const ADD_BG: Color = Color::Rgb(18, 44, 26);
const DEL_BG: Color = Color::Rgb(52, 24, 27);
const ADD_EMPH_BG: Color = Color::Rgb(30, 84, 44);
const DEL_EMPH_BG: Color = Color::Rgb(96, 40, 46);
/// Background of the line the cursor is on (when it has no diff tint).
const CURSOR_BG: Color = Color::Rgb(38, 43, 56);
/// The bar background used for the header and footer.
const BAR_BG: Color = Color::Rgb(30, 33, 40);

/// Rows of context kept above/below the cursor when scrolling.
const SCROLLOFF: usize = 3;
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

/// Cached render data for one file, aligned to that file's flat line list.
struct FileRender {
    /// Syntax-highlighted runs per line.
    highlight: Vec<Vec<HlSpan>>,
    /// Byte ranges of the changed words per line (`None` when the whole line is
    /// the change, or it is context).
    intraline: Vec<Option<Vec<(usize, usize)>>>,
}

struct App {
    label: String,
    diff: Diff,
    highlighter: Highlighter,

    /// The flattened display rows.
    rows: Vec<Row>,
    /// Row index of each diff line, in order; the cursor indexes into this.
    line_rows: Vec<usize>,
    /// Cursor index (into `line_rows`) of each file's first line, if any.
    file_first: Vec<Option<usize>>,
    /// Cursor index (into `line_rows`) of every hunk's first line, in order.
    hunk_first: Vec<usize>,
    /// Per file: the `(hunk, line)` pairs in display order.
    flats: Vec<Vec<(usize, usize)>>,
    /// Lazily-computed render data per file.
    render: RefCell<Vec<Option<FileRender>>>,

    /// Width reserved for each line-number column.
    num_width: usize,
    /// Cursor position as an index into `line_rows`.
    cursor: usize,
    /// Top visible row.
    scroll: usize,
    /// Height of the body pane on the last frame (for paging and follow).
    body_height: Cell<usize>,
    quit: bool,
}

impl App {
    fn new(label: String, diff: Diff, highlighter: Highlighter) -> App {
        let mut rows = Vec::new();
        let mut line_rows = Vec::new();
        let mut file_first = vec![None; diff.files.len()];
        let mut hunk_first = Vec::new();
        let mut flats = Vec::with_capacity(diff.files.len());
        let mut max_lineno = 0u32;

        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 {
                rows.push(Row::Spacer);
            }
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
                    rows.push(Row::HunkHeader(fi, hi));
                    hunk_first.push(line_rows.len());
                    max_lineno = max_lineno
                        .max(hunk.old_start + hunk.old_lines)
                        .max(hunk.new_start + hunk.new_lines);
                    for li in 0..hunk.lines.len() {
                        if file_first[fi].is_none() {
                            file_first[fi] = Some(line_rows.len());
                        }
                        line_rows.push(rows.len());
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

        let render = RefCell::new(Vec::from_iter((0..diff.files.len()).map(|_| None)));
        App {
            label,
            diff,
            highlighter,
            rows,
            line_rows,
            file_first,
            hunk_first,
            flats,
            render,
            num_width: digits(max_lineno).max(3),
            cursor: 0,
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
        let page = self.body_height.get().max(1) as isize;
        match (code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => {
                self.quit = true;
            }
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => self.move_cursor(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => self.move_cursor(-1),
            (KeyCode::Char('d'), true) => self.move_cursor(page / 2),
            (KeyCode::Char('u'), true) => self.move_cursor(-page / 2),
            (KeyCode::PageDown, _) | (KeyCode::Char(' '), false) => self.move_cursor(page - 1),
            (KeyCode::PageUp, _) => self.move_cursor(-(page - 1)),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => self.set_cursor(0),
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => {
                self.set_cursor(self.line_rows.len().saturating_sub(1))
            }
            (KeyCode::Char('n'), false) => self.goto_file(1),
            (KeyCode::Char('p'), false) => self.goto_file(-1),
            (KeyCode::Char('}'), false) | (KeyCode::Char(']'), false) => self.goto_hunk(1),
            (KeyCode::Char('{'), false) | (KeyCode::Char('['), false) => self.goto_hunk(-1),
            _ => {}
        }
    }

    // -- navigation -------------------------------------------------------

    fn move_cursor(&mut self, delta: isize) {
        if self.line_rows.is_empty() {
            return;
        }
        let last = (self.line_rows.len() - 1) as isize;
        let next = (self.cursor as isize + delta).clamp(0, last);
        self.set_cursor(next as usize);
    }

    fn set_cursor(&mut self, index: usize) {
        if self.line_rows.is_empty() {
            return;
        }
        self.cursor = index.min(self.line_rows.len() - 1);
        self.follow_cursor();
    }

    /// The file the cursor is currently in.
    fn current_file(&self) -> usize {
        if self.line_rows.is_empty() {
            return 0;
        }
        match self.rows[self.line_rows[self.cursor]] {
            Row::Line { file, .. } => file,
            _ => 0,
        }
    }

    fn goto_file(&mut self, dir: isize) {
        if self.line_rows.is_empty() {
            return;
        }
        let current = self.current_file();
        if dir > 0 {
            if let Some(index) =
                (current + 1..self.diff.files.len()).find_map(|fi| self.file_first[fi])
            {
                self.set_cursor(index);
            }
        } else {
            // From mid-file, first jump to this file's top; then to the previous.
            if let Some(first) = self.file_first[current]
                && self.cursor > first
            {
                self.set_cursor(first);
                return;
            }
            if let Some(index) = (0..current).rev().find_map(|fi| self.file_first[fi]) {
                self.set_cursor(index);
            }
        }
    }

    fn goto_hunk(&mut self, dir: isize) {
        let target = if dir > 0 {
            self.hunk_first.iter().find(|&&h| h > self.cursor).copied()
        } else {
            self.hunk_first
                .iter()
                .rev()
                .find(|&&h| h < self.cursor)
                .copied()
        };
        if let Some(index) = target {
            self.set_cursor(index);
        }
    }

    /// Adjust the scroll so the cursor row stays visible with a little context.
    fn follow_cursor(&mut self) {
        if self.line_rows.is_empty() {
            return;
        }
        let target = self.line_rows[self.cursor];
        let height = self.body_height.get().max(1);
        let margin = SCROLLOFF.min(height / 2);
        if target < self.scroll + margin {
            self.scroll = target.saturating_sub(margin);
        } else if target + margin >= self.scroll + height {
            self.scroll = (target + margin + 1).saturating_sub(height);
        }
        let max_scroll = self.rows.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    // -- render data ------------------------------------------------------

    /// Ensure the render cache for `file` is populated, computing highlights and
    /// intra-line change ranges on the first frame the file becomes visible.
    fn ensure_render(&self, file: usize) {
        if self.render.borrow()[file].is_some() {
            return;
        }
        let f = &self.diff.files[file];
        let flat = &self.flats[file];
        let texts: Vec<&str> = flat
            .iter()
            .map(|&(h, l)| f.hunks[h].lines[l].content.as_str())
            .collect();
        let highlight = self.highlighter.highlight(f.display_path(), &texts);

        // Map (hunk, line) back to a flat index so intra-line ranges land on the
        // right rows.
        let flat_index: HashMap<(usize, usize), usize> = flat
            .iter()
            .enumerate()
            .map(|(idx, &pair)| (pair, idx))
            .collect();
        let mut intraline = vec![None; flat.len()];
        for (hi, hunk) in f.hunks.iter().enumerate() {
            for (old_i, new_i) in hunk.change_pairs() {
                let (old_segs, new_segs) =
                    word_diff(&hunk.lines[old_i].content, &hunk.lines[new_i].content);
                if let Some(&idx) = flat_index.get(&(hi, old_i)) {
                    intraline[idx] = Some(changed_ranges(&old_segs));
                }
                if let Some(&idx) = flat_index.get(&(hi, new_i)) {
                    intraline[idx] = Some(changed_ranges(&new_segs));
                }
            }
        }

        self.render.borrow_mut()[file] = Some(FileRender {
            highlight,
            intraline,
        });
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
        let position = format!(
            " [{}/{}]{} ",
            self.current_file() + 1,
            self.diff.files.len(),
            self.cursor_anchor()
        );
        let help = "j/k move · n/p file · [ ] hunk · ^d/^u page · g/G ends · q quit";
        let line = TextLine::from(vec![
            TextSpan::styled(position, bar.fg(Color::Cyan)),
            TextSpan::styled(help, bar.fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line).style(bar), area);
    }

    /// A short description of the cursor's line, e.g. ` new:42`, for the footer.
    fn cursor_anchor(&self) -> String {
        if self.line_rows.is_empty() {
            return String::new();
        }
        let Row::Line { file, flat } = self.rows[self.line_rows[self.cursor]] else {
            return String::new();
        };
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        match (line.new_lineno, line.old_lineno) {
            (Some(n), _) => format!(" new:{n}"),
            (None, Some(o)) => format!(" old:{o}"),
            _ => String::new(),
        }
    }

    fn draw_body(&self, f: &mut Frame, area: Rect) {
        let start = self.scroll;
        let end = (start + area.height as usize).min(self.rows.len());
        let current = self.current_file();
        let cursor_row = self.line_rows.get(self.cursor).copied();
        let lines: Vec<TextLine> = (start..end)
            .map(|i| self.render_row(&self.rows[i], current, Some(i) == cursor_row))
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_row(&self, row: &Row, current_file: usize, is_cursor: bool) -> TextLine<'static> {
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
            Row::Line { file, flat } => self.render_diff_line(*file, *flat, is_cursor),
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

    fn render_diff_line(&self, file: usize, flat: usize, is_cursor: bool) -> TextLine<'static> {
        self.ensure_render(file);
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];

        let (tint, emph_bg, sign, sign_color) = match line.kind {
            LineKind::Addition => (Some(ADD_BG), ADD_EMPH_BG, '+', Color::Green),
            LineKind::Deletion => (Some(DEL_BG), DEL_EMPH_BG, '-', Color::Red),
            LineKind::Context => (None, CURSOR_BG, ' ', Color::DarkGray),
        };
        // The cursor line is tinted even where a context line otherwise is not.
        let bg = if is_cursor {
            Some(tint.unwrap_or(CURSOR_BG))
        } else {
            tint
        };
        let base = bg.map_or_else(Style::default, |c| Style::default().bg(c));

        let marker = if is_cursor { "▎" } else { " " };
        let old = optional_number(line.old_lineno, self.num_width);
        let new = optional_number(line.new_lineno, self.num_width);
        let mut spans = vec![
            TextSpan::styled(
                marker.to_string(),
                base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(format!("{old} {new} "), base.fg(Color::DarkGray)),
            TextSpan::styled(format!("{sign} "), base.fg(sign_color)),
        ];

        let render = self.render.borrow();
        let data = render[file].as_ref().expect("render populated");
        let highlight = &data.highlight[flat];
        match data.intraline[flat].as_deref() {
            Some(ranges) if !ranges.is_empty() => {
                spans.extend(emphasize(highlight, ranges, base, emph_bg));
            }
            _ => {
                for span in highlight {
                    spans.push(TextSpan::styled(
                        span.text.clone(),
                        base.fg(rgb(span.color)),
                    ));
                }
            }
        }
        TextLine::from(spans)
    }
}

/// Build content spans from highlighted runs, giving the byte `ranges` (the
/// changed words) the emphasis background while keeping each run's syntax color.
fn emphasize(
    highlight: &[HlSpan],
    ranges: &[(usize, usize)],
    base: Style,
    emph_bg: Color,
) -> Vec<TextSpan<'static>> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for run in highlight {
        let fg = rgb(run.color);
        let start = offset;
        let end = offset + run.text.len();
        let mut pos = start;
        while pos < end {
            let inside = ranges.iter().any(|&(a, b)| pos >= a && pos < b);
            // Next boundary within this run: the end of the current in/out span.
            let mut next = end;
            for &(a, b) in ranges {
                if inside {
                    if pos >= a && pos < b {
                        next = next.min(b);
                    }
                } else if a > pos {
                    next = next.min(a);
                }
            }
            let slice = run.text[pos - start..next - start].to_string();
            let style = if inside {
                base.fg(fg).bg(emph_bg)
            } else {
                base.fg(fg)
            };
            spans.push(TextSpan::styled(slice, style));
            pos = next;
        }
        offset = end;
    }
    spans
}

/// Byte ranges of the changed segments within a side of a word diff.
fn changed_ranges(segments: &[Segment]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    for segment in segments {
        let len = segment.text.len();
        if segment.changed {
            ranges.push((offset, offset + len));
        }
        offset += len;
    }
    ranges
}

fn rgb(color: (u8, u8, u8)) -> Color {
    Color::Rgb(color.0, color.1, color.2)
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
    use super::*;

    #[test]
    fn digit_counts() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(1000), 4);
    }

    #[test]
    fn changed_ranges_are_byte_offsets_of_changed_segments() {
        let (_old, new) = word_diff("foo bar", "foo qux");
        // "foo " unchanged (4 bytes), "qux" changed (3 bytes).
        assert_eq!(changed_ranges(&new), vec![(4, 7)]);
    }

    #[test]
    fn emphasize_splits_runs_at_range_boundaries() {
        let run = HlSpan {
            text: "foo qux".to_string(),
            color: (200, 200, 200),
        };
        let spans = emphasize(&[run], &[(4, 7)], Style::default(), Color::Rgb(1, 2, 3));
        // "foo " then "qux": two spans, the second carrying the emphasis bg.
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "foo ");
        assert_eq!(spans[1].content, "qux");
        assert_eq!(spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }
}
