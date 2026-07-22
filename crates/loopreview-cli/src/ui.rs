//! The ratatui review UI.
//!
//! A line cursor always points at a diff line's `(file, side, line)` anchor.
//! The diff renders either unified or side-by-side; `auto` picks by width and a
//! key toggles at runtime. Changed words within a modified line are emphasized
//! using the core's intra-line diff. All diff data comes from
//! [`loopreview_core`]; this module lays out rows, routes events, and paints.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use loopreview_core::{Diff, DiffSource, LineKind, Segment, word_diff};

use crate::highlight::{Highlighter, Span as HlSpan};

/// Subtle row tints for changed lines, and the stronger tint for the exact
/// words that changed within them (readable on a dark terminal).
const ADD_BG: Color = Color::Rgb(18, 44, 26);
const DEL_BG: Color = Color::Rgb(52, 24, 27);
const ADD_EMPH_BG: Color = Color::Rgb(30, 84, 44);
const DEL_EMPH_BG: Color = Color::Rgb(96, 40, 46);
/// Background of the line the cursor is on (when it has no diff tint).
const CURSOR_BG: Color = Color::Rgb(38, 43, 56);
/// Background of a side-by-side cell with no line (the other side changed).
const ABSENT_BG: Color = Color::Rgb(22, 24, 30);
/// The bar background used for the header and footer.
const BAR_BG: Color = Color::Rgb(30, 33, 40);

/// Rows of context kept above/below the cursor when scrolling.
const SCROLLOFF: usize = 3;
/// How often the event loop wakes to repaint when idle.
const POLL_MS: u64 = 200;
/// How often a watched source is reloaded to pick up changes.
const WATCH_POLL_MS: u64 = 500;
/// At or above this body width, `auto` layout chooses side-by-side.
const AUTO_SBS_MIN_WIDTH: usize = 160;

/// The diff layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Pick unified or side-by-side by terminal width.
    Auto,
    Unified,
    SideBySide,
}

/// Enter the alternate screen, run the review UI over `diff`, then restore the
/// terminal. `label` describes the diff's source (shown in the header). When
/// `watch`, `source` is reloaded in the background so the view tracks changes.
pub fn run(
    label: String,
    diff: Diff,
    source: Arc<dyn DiffSource + Send + Sync>,
    watch: bool,
) -> Result<()> {
    let mut app = App::new(label, diff, Highlighter::new());
    let updates = watch.then(|| spawn_watcher(source));

    let mut terminal = ratatui::init();
    // Mouse capture is best-effort: the UI is fully usable without it.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = app.event_loop(&mut terminal, updates);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Spawn a thread that reloads `source` on an interval and streams fresh diffs.
/// It stops when the receiver is dropped (the UI exits). Load errors are
/// ignored so a transient failure keeps the last good view.
fn spawn_watcher(source: Arc<dyn DiffSource + Send + Sync>) -> Receiver<Diff> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(WATCH_POLL_MS));
            if let Ok(diff) = source.load()
                && tx.send(diff).is_err()
            {
                break;
            }
        }
    });
    rx
}

/// A row in the unified layout.
enum URow {
    FileHeader(usize),
    Note(String),
    HunkHeader(usize, usize),
    Line { file: usize, flat: usize },
    Spacer,
}

/// A row in the side-by-side layout. `Pair` holds the old and new flat line
/// indices for one file, either of which may be absent.
enum SRow {
    FileHeader(usize),
    Note(String),
    HunkHeader(usize, usize),
    Pair {
        file: usize,
        old: Option<usize>,
        new: Option<usize>,
    },
    Spacer,
}

/// A relocatable cursor position: a file path and a line number on one side.
/// Used to keep the cursor on the same line across a watch reload.
struct Anchor {
    path: String,
    new_side: bool,
    line: u32,
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

    /// Unified rows and, per diff line (cursor index), its row within them.
    urows: Vec<URow>,
    line_urow: Vec<usize>,
    /// Side-by-side rows and, per diff line, its row within them.
    srows: Vec<SRow>,
    line_srow: Vec<usize>,
    /// The `(file, flat)` of each diff line, in cursor order.
    clines: Vec<(usize, usize)>,
    /// Reverse of `clines`: `(file, flat)` to its cursor index (for clicks).
    cline_index: HashMap<(usize, usize), usize>,

    /// Cursor index of each file's first line, if any.
    file_first: Vec<Option<usize>>,
    /// Cursor index of every hunk's first line, in order.
    hunk_first: Vec<usize>,
    /// Per file: the `(hunk, line)` pairs in display order.
    flats: Vec<Vec<(usize, usize)>>,
    /// Lazily-computed render data per file.
    render: RefCell<Vec<Option<FileRender>>>,

    num_width: usize,
    mode: Mode,
    cursor: usize,
    scroll: usize,
    body_height: Cell<usize>,
    body_width: Cell<usize>,
    quit: bool,
}

impl App {
    fn new(label: String, diff: Diff, highlighter: Highlighter) -> App {
        let layout = Layouts::build(&diff);
        let num_width = digits(layout.max_lineno).max(3);
        let file_count = diff.files.len();
        let cline_index = layout
            .clines
            .iter()
            .enumerate()
            .map(|(i, &pair)| (pair, i))
            .collect();
        App {
            label,
            diff,
            highlighter,
            urows: layout.urows,
            line_urow: layout.line_urow,
            srows: layout.srows,
            line_srow: layout.line_srow,
            clines: layout.clines,
            cline_index,
            file_first: layout.file_first,
            hunk_first: layout.hunk_first,
            flats: layout.flats,
            render: RefCell::new(Vec::from_iter((0..file_count).map(|_| None))),
            num_width,
            mode: Mode::Auto,
            cursor: 0,
            scroll: 0,
            body_height: Cell::new(20),
            body_width: Cell::new(80),
            quit: false,
        }
    }

    // -- event loop -------------------------------------------------------

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        updates: Option<Receiver<Diff>>,
    ) -> Result<()> {
        while !self.quit {
            // Apply the newest watched diff, if it differs from what we show.
            if let Some(rx) = &updates {
                let mut latest = None;
                while let Ok(diff) = rx.try_recv() {
                    latest = Some(diff);
                }
                if let Some(diff) = latest
                    && diff != self.diff
                {
                    self.reload(diff);
                }
            }

            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(POLL_MS))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.on_key(key.code, key.modifiers);
                    }
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Replace the diff with a freshly-loaded one, rebuilding the layout and
    /// keeping the cursor on the same line when it still exists.
    fn reload(&mut self, diff: Diff) {
        let anchor = self.current_anchor();
        let layout = Layouts::build(&diff);
        self.num_width = digits(layout.max_lineno).max(3);
        self.cline_index = layout
            .clines
            .iter()
            .enumerate()
            .map(|(i, &pair)| (pair, i))
            .collect();
        self.render = RefCell::new(Vec::from_iter((0..diff.files.len()).map(|_| None)));
        self.urows = layout.urows;
        self.line_urow = layout.line_urow;
        self.srows = layout.srows;
        self.line_srow = layout.line_srow;
        self.clines = layout.clines;
        self.file_first = layout.file_first;
        self.hunk_first = layout.hunk_first;
        self.flats = layout.flats;
        self.diff = diff;

        self.cursor = anchor
            .and_then(|a| self.find_anchor(&a))
            .unwrap_or(0)
            .min(self.clines.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.rows_len().saturating_sub(1));
        self.follow_cursor();
    }

    /// The cursor's current line as a relocatable anchor.
    fn current_anchor(&self) -> Option<Anchor> {
        if self.clines.is_empty() {
            return None;
        }
        let (file, flat) = self.clines[self.cursor];
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let new_side = line.kind != LineKind::Deletion;
        let number = if new_side {
            line.new_lineno
        } else {
            line.old_lineno
        }?;
        Some(Anchor {
            path: self.diff.files[file].display_path().to_string(),
            new_side,
            line: number,
        })
    }

    /// Find the cursor index of `anchor` in the current diff, if present.
    fn find_anchor(&self, anchor: &Anchor) -> Option<usize> {
        self.clines.iter().position(|&(file, flat)| {
            let (hi, li) = self.flats[file][flat];
            let line = &self.diff.files[file].hunks[hi].lines[li];
            let new_side = line.kind != LineKind::Deletion;
            let number = if new_side {
                line.new_lineno
            } else {
                line.old_lineno
            };
            new_side == anchor.new_side
                && number == Some(anchor.line)
                && self.diff.files[file].display_path() == anchor.path
        })
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
                self.set_cursor(self.clines.len().saturating_sub(1))
            }
            (KeyCode::Char('n'), false) => self.goto_file(1),
            (KeyCode::Char('p'), false) => self.goto_file(-1),
            (KeyCode::Char('}'), false) | (KeyCode::Char(']'), false) => self.goto_hunk(1),
            (KeyCode::Char('{'), false) | (KeyCode::Char('['), false) => self.goto_hunk(-1),
            (KeyCode::Char('v'), false) | (KeyCode::Tab, _) => self.toggle_mode(),
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_view(3),
            MouseEventKind::ScrollUp => self.scroll_view(-3),
            MouseEventKind::Down(MouseButton::Left) => self.click(mouse.column, mouse.row),
            _ => {}
        }
    }

    /// Scroll the viewport without moving the cursor (wheel scrolling).
    fn scroll_view(&mut self, delta: isize) {
        let height = self.body_height.get().max(1);
        let max_scroll = self.rows_len().saturating_sub(height) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max_scroll) as usize;
    }

    /// Move the cursor to the diff line under a click, if any.
    fn click(&mut self, column: u16, row: u16) {
        // Row 0 is the header; the body starts at row 1.
        if row < 1 {
            return;
        }
        let body_row = (row - 1) as usize;
        if body_row >= self.body_height.get() {
            return; // footer or below
        }
        let row_index = self.scroll + body_row;
        if row_index >= self.rows_len() {
            return;
        }
        let target = if self.sbs() {
            self.sbs_click(row_index, column as usize)
        } else {
            self.unified_click(row_index)
        };
        if let Some(cursor) = target {
            self.set_cursor(cursor);
        }
    }

    fn unified_click(&self, row_index: usize) -> Option<usize> {
        match self.urows[row_index] {
            URow::Line { file, flat } => self.cline_index.get(&(file, flat)).copied(),
            _ => None,
        }
    }

    fn sbs_click(&self, row_index: usize, column: usize) -> Option<usize> {
        let SRow::Pair { file, old, new } = self.srows[row_index] else {
            return None;
        };
        let left_w = (self.body_width.get().saturating_sub(1)) / 2;
        let flat = if column < left_w {
            old
        } else if column > left_w {
            new
        } else {
            None
        };
        flat.and_then(|f| self.cline_index.get(&(file, f)).copied())
    }

    // -- navigation -------------------------------------------------------

    fn sbs(&self) -> bool {
        match self.mode {
            Mode::Auto => self.body_width.get() >= AUTO_SBS_MIN_WIDTH,
            Mode::Unified => false,
            Mode::SideBySide => true,
        }
    }

    /// Toggle between unified and side-by-side, pinning the choice (leaving auto).
    fn toggle_mode(&mut self) {
        self.mode = if self.sbs() {
            Mode::Unified
        } else {
            Mode::SideBySide
        };
        self.follow_cursor();
    }

    fn cursor_row(&self) -> usize {
        if self.clines.is_empty() {
            return 0;
        }
        if self.sbs() {
            self.line_srow[self.cursor]
        } else {
            self.line_urow[self.cursor]
        }
    }

    fn rows_len(&self) -> usize {
        if self.sbs() {
            self.srows.len()
        } else {
            self.urows.len()
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.clines.is_empty() {
            return;
        }
        let last = (self.clines.len() - 1) as isize;
        let next = (self.cursor as isize + delta).clamp(0, last);
        self.set_cursor(next as usize);
    }

    fn set_cursor(&mut self, index: usize) {
        if self.clines.is_empty() {
            return;
        }
        self.cursor = index.min(self.clines.len() - 1);
        self.follow_cursor();
    }

    fn current_file(&self) -> usize {
        if self.clines.is_empty() {
            return 0;
        }
        self.clines[self.cursor].0
    }

    fn goto_file(&mut self, dir: isize) {
        if self.clines.is_empty() {
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

    fn follow_cursor(&mut self) {
        if self.clines.is_empty() {
            return;
        }
        let target = self.cursor_row();
        let height = self.body_height.get().max(1);
        let margin = SCROLLOFF.min(height / 2);
        if target < self.scroll + margin {
            self.scroll = target.saturating_sub(margin);
        } else if target + margin >= self.scroll + height {
            self.scroll = (target + margin + 1).saturating_sub(height);
        }
        self.scroll = self.scroll.min(self.rows_len().saturating_sub(height));
    }

    // -- render data ------------------------------------------------------

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
        self.body_width.set(chunks[1].width as usize);

        self.draw_header(f, chunks[0]);
        if self.clines.is_empty() && self.diff.files.is_empty() {
            self.draw_empty(f, chunks[1]);
        } else if self.sbs() {
            self.draw_body_sbs(f, chunks[1]);
        } else {
            self.draw_body_unified(f, chunks[1]);
        }
        self.draw_footer(f, chunks[2]);
    }

    fn draw_empty(&self, f: &mut Frame, area: Rect) {
        let hint = TextLine::from(TextSpan::styled(
            "  no changes",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(hint), area);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let stats = self.diff.stats();
        let bar = Style::default().bg(BAR_BG);
        let layout_label = if self.sbs() { "split" } else { "unified" };
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
            TextSpan::styled(format!("-{} ", stats.deletions), bar.fg(Color::Red)),
            TextSpan::styled(format!("· {layout_label}"), bar.fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line).style(bar), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let bar = Style::default().bg(BAR_BG);
        let position = format!(
            " [{}/{}]{} ",
            self.current_file() + 1,
            self.diff.files.len().max(1),
            self.cursor_anchor()
        );
        let help = "j/k move · n/p file · [ ] hunk · v split · ^d/^u page · q quit";
        let line = TextLine::from(vec![
            TextSpan::styled(position, bar.fg(Color::Cyan)),
            TextSpan::styled(help, bar.fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line).style(bar), area);
    }

    fn cursor_anchor(&self) -> String {
        if self.clines.is_empty() {
            return String::new();
        }
        let (file, flat) = self.clines[self.cursor];
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        match (line.new_lineno, line.old_lineno) {
            (Some(n), _) => format!(" new:{n}"),
            (None, Some(o)) => format!(" old:{o}"),
            _ => String::new(),
        }
    }

    fn draw_body_unified(&self, f: &mut Frame, area: Rect) {
        let start = self.scroll;
        let end = (start + area.height as usize).min(self.urows.len());
        let current = self.current_file();
        let cursor_row = self.line_urow.get(self.cursor).copied();
        let lines: Vec<TextLine> = (start..end)
            .map(|i| match &self.urows[i] {
                URow::Spacer => TextLine::from(""),
                URow::FileHeader(fi) => self.file_header_line(*fi, *fi == current),
                URow::Note(msg) => note_line(msg),
                URow::HunkHeader(fi, hi) => self.hunk_header_line(*fi, *hi),
                URow::Line { file, flat } => self.diff_line(*file, *flat, Some(i) == cursor_row),
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_body_sbs(&self, f: &mut Frame, area: Rect) {
        let divider = 1u16;
        let left_w = area.width.saturating_sub(divider) / 2;
        let right_w = area.width.saturating_sub(divider + left_w);
        let (cursor_file, cursor_flat) = self.clines.get(self.cursor).copied().unwrap_or((0, 0));
        let current = self.current_file();

        let start = self.scroll;
        let end = (start + area.height as usize).min(self.srows.len());
        let mut lines = Vec::with_capacity(end - start);
        for i in start..end {
            let line = match &self.srows[i] {
                SRow::Spacer => TextLine::from(""),
                SRow::FileHeader(fi) => self.file_header_line(*fi, *fi == current),
                SRow::Note(msg) => note_line(msg),
                SRow::HunkHeader(fi, hi) => self.hunk_header_line(*fi, *hi),
                SRow::Pair { file, old, new } => {
                    let left_cursor = *file == cursor_file && *old == Some(cursor_flat);
                    let right_cursor = *file == cursor_file && *new == Some(cursor_flat);
                    let mut spans = self.sbs_cell(*file, *old, false, left_cursor, left_w as usize);
                    spans.push(TextSpan::styled("│", Style::default().fg(Color::DarkGray)));
                    spans.extend(self.sbs_cell(*file, *new, true, right_cursor, right_w as usize));
                    TextLine::from(spans)
                }
            };
            lines.push(line);
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// One side-by-side cell, padded/truncated to `width`.
    fn sbs_cell(
        &self,
        file: usize,
        flat: Option<usize>,
        new_side: bool,
        is_cursor: bool,
        width: usize,
    ) -> Vec<TextSpan<'static>> {
        let Some(flat) = flat else {
            // Absent side: fill the column with the "not present" background.
            return vec![TextSpan::styled(
                " ".repeat(width),
                Style::default().bg(ABSENT_BG),
            )];
        };
        self.ensure_render(file);
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let (tint, emph_bg, sign, sign_color) = kind_style(line.kind);
        let bg = if is_cursor {
            Some(tint.unwrap_or(CURSOR_BG))
        } else {
            tint
        };
        let base = bg.map_or_else(Style::default, |c| Style::default().bg(c));
        let marker = if is_cursor { "▎" } else { " " };
        let number = if new_side {
            line.new_lineno
        } else {
            line.old_lineno
        };
        let mut spans = vec![
            TextSpan::styled(
                marker.to_string(),
                base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(
                format!("{} ", optional_number(number, self.num_width)),
                base.fg(Color::DarkGray),
            ),
            TextSpan::styled(format!("{sign} "), base.fg(sign_color)),
        ];
        spans.extend(self.content_spans(file, flat, base, emph_bg));
        fit(spans, width, base)
    }

    fn file_header_line(&self, fi: usize, is_current: bool) -> TextLine<'static> {
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

    fn hunk_header_line(&self, fi: usize, hi: usize) -> TextLine<'static> {
        let hunk = &self.diff.files[fi].hunks[hi];
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

    /// A full-width unified diff line.
    fn diff_line(&self, file: usize, flat: usize, is_cursor: bool) -> TextLine<'static> {
        self.ensure_render(file);
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let (tint, emph_bg, sign, sign_color) = kind_style(line.kind);
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
        spans.extend(self.content_spans(file, flat, base, emph_bg));
        TextLine::from(spans)
    }

    /// The highlighted (and, where changed, emphasized) content of one line.
    fn content_spans(
        &self,
        file: usize,
        flat: usize,
        base: Style,
        emph_bg: Color,
    ) -> Vec<TextSpan<'static>> {
        let render = self.render.borrow();
        let data = render[file].as_ref().expect("render populated");
        let highlight = &data.highlight[flat];
        match data.intraline[flat].as_deref() {
            Some(ranges) if !ranges.is_empty() => emphasize(highlight, ranges, base, emph_bg),
            _ => highlight
                .iter()
                .map(|span| TextSpan::styled(span.text.clone(), base.fg(rgb(span.color))))
                .collect(),
        }
    }
}

/// The tint, emphasis background, sign, and sign color for a line kind.
fn kind_style(kind: LineKind) -> (Option<Color>, Color, char, Color) {
    match kind {
        LineKind::Addition => (Some(ADD_BG), ADD_EMPH_BG, '+', Color::Green),
        LineKind::Deletion => (Some(DEL_BG), DEL_EMPH_BG, '-', Color::Red),
        LineKind::Context => (None, CURSOR_BG, ' ', Color::DarkGray),
    }
}

fn note_line(msg: &str) -> TextLine<'static> {
    TextLine::from(TextSpan::styled(
        format!("  {msg}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// Truncate/pad styled `spans` to exactly `width` display columns (approximated
/// by character count), filling any remainder with `fill`.
fn fit(spans: Vec<TextSpan<'static>>, width: usize, fill: Style) -> Vec<TextSpan<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let chars: Vec<char> = span.content.chars().collect();
        if chars.len() <= remaining {
            used += chars.len();
            out.push(span);
        } else {
            let clipped: String = chars[..remaining].iter().collect();
            used += remaining;
            out.push(TextSpan::styled(clipped, span.style));
            break;
        }
    }
    if used < width {
        out.push(TextSpan::styled(" ".repeat(width - used), fill));
    }
    out
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

fn optional_number(number: Option<u32>, width: usize) -> String {
    match number {
        Some(n) => format!("{n:>width$}"),
        None => " ".repeat(width),
    }
}

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

/// The precomputed layouts and navigation indices for a diff.
struct Layouts {
    urows: Vec<URow>,
    line_urow: Vec<usize>,
    srows: Vec<SRow>,
    line_srow: Vec<usize>,
    clines: Vec<(usize, usize)>,
    file_first: Vec<Option<usize>>,
    hunk_first: Vec<usize>,
    flats: Vec<Vec<(usize, usize)>>,
    max_lineno: u32,
}

impl Layouts {
    fn build(diff: &Diff) -> Layouts {
        let mut urows = Vec::new();
        let mut clines = Vec::new();
        let mut line_urow = Vec::new();
        let mut file_first = vec![None; diff.files.len()];
        let mut hunk_first = Vec::new();
        let mut flats = Vec::with_capacity(diff.files.len());
        // Per file, flat index -> cursor index (for the side-by-side pass).
        let mut cursor_of: Vec<Vec<usize>> = Vec::with_capacity(diff.files.len());
        let mut max_lineno = 0u32;

        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 {
                urows.push(URow::Spacer);
            }
            urows.push(URow::FileHeader(fi));
            let mut flat = Vec::new();
            let mut cof = Vec::new();
            if file.binary {
                urows.push(URow::Note("binary file — contents not shown".to_string()));
            } else if file.hunks.is_empty() {
                urows.push(URow::Note(format!(
                    "{}, no content changes",
                    file.status.label()
                )));
            } else {
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    urows.push(URow::HunkHeader(fi, hi));
                    hunk_first.push(clines.len());
                    max_lineno = max_lineno
                        .max(hunk.old_start + hunk.old_lines)
                        .max(hunk.new_start + hunk.new_lines);
                    for li in 0..hunk.lines.len() {
                        let cursor = clines.len();
                        if file_first[fi].is_none() {
                            file_first[fi] = Some(cursor);
                        }
                        line_urow.push(urows.len());
                        urows.push(URow::Line {
                            file: fi,
                            flat: flat.len(),
                        });
                        cof.push(cursor);
                        clines.push((fi, flat.len()));
                        flat.push((hi, li));
                    }
                }
            }
            flats.push(flat);
            cursor_of.push(cof);
        }

        // Side-by-side pass, using cursor_of to point each line at its row.
        let mut srows = Vec::new();
        let mut line_srow = vec![0usize; clines.len()];
        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 {
                srows.push(SRow::Spacer);
            }
            srows.push(SRow::FileHeader(fi));
            if file.binary {
                srows.push(SRow::Note("binary file — contents not shown".to_string()));
            } else if file.hunks.is_empty() {
                srows.push(SRow::Note(format!(
                    "{}, no content changes",
                    file.status.label()
                )));
            } else {
                let mut flat_counter = 0usize;
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    srows.push(SRow::HunkHeader(fi, hi));
                    let mut dels = Vec::new();
                    let mut adds = Vec::new();
                    for line in &hunk.lines {
                        let flat = flat_counter;
                        flat_counter += 1;
                        match line.kind {
                            LineKind::Context => {
                                flush_block(
                                    fi,
                                    &mut dels,
                                    &mut adds,
                                    &mut srows,
                                    &mut line_srow,
                                    &cursor_of,
                                );
                                let row = srows.len();
                                srows.push(SRow::Pair {
                                    file: fi,
                                    old: Some(flat),
                                    new: Some(flat),
                                });
                                line_srow[cursor_of[fi][flat]] = row;
                            }
                            LineKind::Deletion => dels.push(flat),
                            LineKind::Addition => adds.push(flat),
                        }
                    }
                    flush_block(
                        fi,
                        &mut dels,
                        &mut adds,
                        &mut srows,
                        &mut line_srow,
                        &cursor_of,
                    );
                }
            }
        }

        Layouts {
            urows,
            line_urow,
            srows,
            line_srow,
            clines,
            file_first,
            hunk_first,
            flats,
            max_lineno,
        }
    }
}

/// Emit side-by-side rows for a change block, pairing deletions with additions
/// positionally and recording each line's row.
fn flush_block(
    file: usize,
    dels: &mut Vec<usize>,
    adds: &mut Vec<usize>,
    srows: &mut Vec<SRow>,
    line_srow: &mut [usize],
    cursor_of: &[Vec<usize>],
) {
    let n = dels.len().max(adds.len());
    for k in 0..n {
        let old = dels.get(k).copied();
        let new = adds.get(k).copied();
        let row = srows.len();
        srows.push(SRow::Pair { file, old, new });
        if let Some(of) = old {
            line_srow[cursor_of[file][of]] = row;
        }
        if let Some(nf) = new {
            line_srow[cursor_of[file][nf]] = row;
        }
    }
    dels.clear();
    adds.clear();
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
        assert_eq!(changed_ranges(&new), vec![(4, 7)]);
    }

    #[test]
    fn emphasize_splits_runs_at_range_boundaries() {
        let run = HlSpan {
            text: "foo qux".to_string(),
            color: (200, 200, 200),
        };
        let spans = emphasize(&[run], &[(4, 7)], Style::default(), Color::Rgb(1, 2, 3));
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "foo ");
        assert_eq!(spans[1].content, "qux");
        assert_eq!(spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn fit_pads_and_truncates_to_width() {
        let short = fit(vec![TextSpan::raw("ab")], 5, Style::default());
        let text: String = short.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "ab   ");

        let long = fit(vec![TextSpan::raw("abcdef")], 3, Style::default());
        let text: String = long.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abc");
    }
}
