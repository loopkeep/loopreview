//! The ratatui review UI.
//!
//! A line cursor always points at a diff line's `(file, side, line)` anchor.
//! The diff renders either unified or side-by-side; `auto` picks by width and a
//! key toggles at runtime. Changed words within a modified line are emphasized
//! using the core's intra-line diff. All diff data comes from
//! [`loopreview_core`]; this module lays out rows, routes events, and paints.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use notify::{RecursiveMode, Watcher};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use loopreview_core::{
    Anchor, Comment, Diff, DiffSource, LineKind, Review, Segment, Side, Thread, ThreadState,
    word_diff,
};

use crate::highlight::{Highlighter, Span as HlSpan};
use crate::store::Store;
use crate::textarea::TextArea;

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
/// Quiet period after a filesystem event before a watched source is reloaded,
/// so a burst of writes coalesces into one refresh.
const WATCH_DEBOUNCE_MS: u64 = 250;
/// How long the "updated" flash stays in the header after a watch reload.
const RELOAD_FLASH_MS: u64 = 900;
/// At or above this body width, `auto` layout chooses side-by-side.
/// Fallback wrap width for Conversation rendering before the body size is known.
const CONV_DEFAULT_WIDTH: usize = 80;

/// The diff layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pick unified or side-by-side by terminal width.
    Auto,
    Unified,
    SideBySide,
}

/// Which top-level view is showing (tabs appear once a review has threads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// The diff, with comments inline.
    Files,
    /// The comment threads, chronological with replies.
    Conversation,
}

/// Everything the review UI needs for one session.
pub struct Session {
    /// Human-readable description of the diff source (shown in the header).
    pub label: String,
    /// The diff to review.
    pub diff: Diff,
    /// The source, for background reloads when watching.
    pub source: Arc<dyn DiffSource + Send + Sync>,
    /// The directory to watch, or `None` to not auto-refresh.
    pub watch_root: Option<PathBuf>,
    /// The initial layout.
    pub mode: Mode,
    /// The review loaded from the store (may be empty).
    pub review: Review,
    /// The store to persist comments to, or `None` when unavailable.
    pub store: Option<Store>,
    /// The comment author (from `git config user.name`).
    pub author: String,
    /// Minimum body width for `auto` layout to choose side-by-side.
    pub split_min_width: usize,
}

/// Enter the alternate screen, run the review UI, then restore the terminal.
pub fn run(session: Session) -> Result<()> {
    let Session {
        label,
        diff,
        source,
        watch_root,
        mode,
        review,
        store,
        author,
        split_min_width,
    } = session;

    let mut app = App::new(label, diff, review, store, author, Highlighter::new());
    app.mode = mode;
    app.split_min_width = split_min_width;
    let updates = watch_root.map(|root| spawn_watcher(root, source));
    app.watching = updates.is_some();

    let mut terminal = ratatui::init();
    // Mouse capture and bracketed paste are best-effort: the UI is usable
    // without them, but they make navigation and pasting comments pleasant.
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = app.event_loop(&mut terminal, updates);
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();
    result
}

/// A message from the watch thread.
enum WatchMsg {
    /// A freshly-loaded diff after a change settled.
    Reloaded(Diff),
    /// Watching could not start or stopped; carries the reason to show.
    Error(String),
}

/// Watch `root` for changes and reload `source` after each burst settles,
/// streaming fresh diffs. Event-driven (not polling), so a save reflects
/// immediately and an idle session costs nothing. The thread and its watcher
/// stop when the receiver is dropped (the UI exits); a load error is dropped so
/// the last good view stays put, while a fatal watch-setup error is reported.
fn spawn_watcher(root: PathBuf, source: Arc<dyn DiffSource + Send + Sync>) -> Receiver<WatchMsg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (event_tx, event_rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = event_tx.send(res);
        }) {
            Ok(watcher) => watcher,
            Err(e) => {
                let _ = tx.send(WatchMsg::Error(format!("auto-refresh unavailable: {e}")));
                return;
            }
        };
        if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
            let _ = tx.send(WatchMsg::Error(format!(
                "cannot watch {}: {e}",
                root.display()
            )));
            return;
        }

        loop {
            // Block for the first change, then coalesce the burst that follows.
            if event_rx.recv().is_err() {
                return;
            }
            loop {
                match event_rx.recv_timeout(Duration::from_millis(WATCH_DEBOUNCE_MS)) {
                    Ok(_) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            // A load error is transient (mid-write, etc.); keep the last good
            // view and wait for the next change.
            if let Ok(diff) = source.load()
                && tx.send(WatchMsg::Reloaded(diff)).is_err()
            {
                return; // UI gone
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
    Line {
        file: usize,
        flat: usize,
    },
    /// One rendered line of an inline comment thread (thread index, line index).
    Comment(usize, usize),
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
    /// One rendered line of an inline comment thread (thread index, line index).
    Comment(usize, usize),
    Spacer,
}

/// What a composed comment will become on submit.
enum ComposeKind {
    /// A new thread at this anchor.
    New(Anchor),
    /// A reply to an existing thread (by id).
    Reply(String),
}

/// An in-progress comment being composed.
struct Compose {
    /// The text being edited.
    area: TextArea,
    /// Whether this starts a new thread or replies to one.
    kind: ComposeKind,
    /// A short description shown in the input header.
    target: String,
    /// True after Esc on non-empty text, awaiting a discard confirmation.
    confirming_discard: bool,
}

/// A relocatable cursor position: a file path and a line number on one side.
/// Used to keep the cursor on the same line across a watch reload.
struct CursorAnchor {
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
    /// True while a watcher is active (shown as a header indicator).
    watching: bool,
    /// A watch-setup error to surface, when auto-refresh could not start.
    watch_error: Option<String>,
    /// When the last watch reload happened, for the brief "updated" flash.
    reloaded_at: Option<Instant>,
    /// The review (comment threads) loaded from the store.
    review: Review,
    /// The store to persist to, or `None` when unavailable.
    store: Option<Store>,
    /// The comment author.
    author: String,
    /// The active comment composer, when writing.
    input: Option<Compose>,
    /// Rendered inline block per thread, index-aligned to `review.threads`.
    comment_blocks: Vec<Vec<TextLine<'static>>>,
    /// Rendered Conversation block per thread (root, replies), same order.
    conv_blocks: Vec<Vec<TextLine<'static>>>,
    /// The current top-level view.
    view: View,
    /// Selected thread index in the Conversation view.
    conv_cursor: usize,
    /// Scroll offset (in lines) of the Conversation view.
    conv_scroll: usize,
    /// Minimum body width for `auto` layout to choose side-by-side.
    split_min_width: usize,
    /// True while awaiting confirmation to close (delete) the review.
    confirming_close: bool,
    /// A transient status message (feedback or error).
    status: Option<String>,
    quit: bool,
}

impl App {
    fn new(
        label: String,
        diff: Diff,
        review: Review,
        store: Option<Store>,
        author: String,
        highlighter: Highlighter,
    ) -> App {
        let comment_blocks = build_comment_blocks(&review, &highlighter);
        let block_lens: Vec<usize> = comment_blocks.iter().map(Vec::len).collect();
        let layout = Layouts::build(&diff, &review, &block_lens);
        let outdated = outdated_flags(&review, &layout.placed);
        let conv_blocks = build_conversation(&review, CONV_DEFAULT_WIDTH, &highlighter, &outdated);
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
            watching: false,
            watch_error: None,
            review,
            store,
            author,
            input: None,
            comment_blocks,
            conv_blocks,
            view: View::Files,
            conv_cursor: 0,
            conv_scroll: 0,
            split_min_width: 160,
            confirming_close: false,
            status: None,
            reloaded_at: None,
            quit: false,
        }
    }

    // -- event loop -------------------------------------------------------

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        updates: Option<Receiver<WatchMsg>>,
    ) -> Result<()> {
        while !self.quit {
            // Drain watch messages: apply the newest diff, or record an error.
            // Held back while composing so an incoming change can't reshuffle the
            // diff under the open comment.
            if self.input.is_none()
                && let Some(rx) = &updates
            {
                let mut latest = None;
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        WatchMsg::Reloaded(diff) => latest = Some(diff),
                        WatchMsg::Error(reason) => {
                            self.watching = false;
                            self.watch_error = Some(reason);
                        }
                    }
                }
                if let Some(diff) = latest
                    && diff != self.diff
                {
                    self.reload(diff);
                    self.reloaded_at = Some(Instant::now());
                }
            }

            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(POLL_MS))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.on_key(key.code, key.modifiers);
                    }
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Recompute the layout and inline comment blocks from `diff` and the
    /// current review, replacing derived state. The caller restores the cursor.
    fn apply_layout(&mut self, diff: Diff) {
        self.comment_blocks = build_comment_blocks(&self.review, &self.highlighter);
        let block_lens: Vec<usize> = self.comment_blocks.iter().map(Vec::len).collect();
        let layout = Layouts::build(&diff, &self.review, &block_lens);
        let outdated = outdated_flags(&self.review, &layout.placed);
        let conv_width = self.body_width.get().clamp(40, 120);
        self.conv_blocks =
            build_conversation(&self.review, conv_width, &self.highlighter, &outdated);
        self.conv_cursor = self
            .conv_cursor
            .min(self.review.threads.len().saturating_sub(1));
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
    }

    /// Replace the diff with a freshly-loaded one, keeping the cursor on the
    /// same line when it still exists.
    fn reload(&mut self, diff: Diff) {
        let anchor = self.current_anchor();
        self.apply_layout(diff);
        self.cursor = anchor
            .and_then(|a| self.find_anchor(&a))
            .unwrap_or(0)
            .min(self.clines.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.rows_len().saturating_sub(1));
        self.follow_cursor();
    }

    /// Rebuild after the review changed (diff unchanged), keeping the cursor.
    fn relayout(&mut self) {
        let diff = std::mem::take(&mut self.diff);
        self.apply_layout(diff);
        self.cursor = self.cursor.min(self.clines.len().saturating_sub(1));
        self.follow_cursor();
    }

    /// The cursor's current line as a relocatable anchor.
    fn current_anchor(&self) -> Option<CursorAnchor> {
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
        Some(CursorAnchor {
            path: self.diff.files[file].display_path().to_string(),
            new_side,
            line: number,
        })
    }

    /// Find the cursor index of `anchor` in the current diff, if present.
    fn find_anchor(&self, anchor: &CursorAnchor) -> Option<usize> {
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

    /// Whether the review has any threads (and so the tab bar is shown).
    fn has_review(&self) -> bool {
        !self.review.threads.is_empty()
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // While composing, keys edit the comment (or submit/cancel).
        if self.input.is_some() {
            self.on_key_compose(code, mods);
            return;
        }
        // While confirming a close: y/Enter closes, anything else cancels.
        if self.confirming_close {
            self.confirming_close = false;
            if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
                self.close_review();
            } else {
                self.status = Some("close cancelled".to_string());
            }
            return;
        }
        self.status = None;

        // Tab switches views once a review exists; Esc/q/^c always quit.
        match (code, mods.contains(KeyModifiers::CONTROL)) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => {
                self.quit = true;
                return;
            }
            (KeyCode::Tab, _) if self.has_review() => {
                self.view = match self.view {
                    View::Files => View::Conversation,
                    View::Conversation => View::Files,
                };
                return;
            }
            _ => {}
        }

        if self.view == View::Conversation {
            self.on_key_conversation(code, mods);
        } else {
            self.on_key_files(code, mods);
        }
    }

    fn on_key_files(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let page = self.body_height.get().max(1) as isize;
        match (code, ctrl) {
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
            (KeyCode::Char('v'), false) => self.toggle_mode(),
            (KeyCode::Char('c'), false) => self.start_compose(),
            (KeyCode::Char('r'), false) => self.start_reply(),
            (KeyCode::Char('x'), false) => self.toggle_resolve(),
            _ => {}
        }
    }

    /// Route a key while the comment composer is open.
    fn on_key_compose(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let Some(compose) = self.input.as_mut() else {
            return;
        };

        // Awaiting discard confirmation: Esc discards, anything else resumes.
        if compose.confirming_discard {
            if code == KeyCode::Esc {
                self.input = None;
                self.status = Some("comment discarded".to_string());
            } else {
                compose.confirming_discard = false;
            }
            return;
        }

        match code {
            KeyCode::Esc => {
                if compose.area.is_blank() {
                    self.input = None;
                    self.status = Some("comment cancelled".to_string());
                } else {
                    compose.confirming_discard = true;
                }
            }
            KeyCode::Char('s') if ctrl => self.submit_compose(),
            _ if ctrl => {} // ignore other control combos
            _ => compose.area.on_key(code),
        }
    }

    /// Insert pasted text into the open composer (bracketed paste).
    fn on_paste(&mut self, text: &str) {
        if let Some(compose) = self.input.as_mut() {
            compose.confirming_discard = false;
            compose.area.paste(text);
        }
    }

    /// Begin composing a comment on the cursor's line.
    fn start_compose(&mut self) {
        if self.store.is_none() {
            self.status = Some("comments need a git repository".to_string());
            return;
        }
        if self.clines.is_empty() {
            return;
        }
        let (file, flat) = self.clines[self.cursor];
        let (hi, li) = self.flats[file][flat];
        let f = &self.diff.files[file];
        let line = &f.hunks[hi].lines[li];
        let new_side = line.kind != LineKind::Deletion;
        let side = if new_side { Side::New } else { Side::Old };
        let number = if new_side {
            line.new_lineno
        } else {
            line.old_lineno
        }
        .unwrap_or(0);
        let commit = if new_side {
            self.diff.provenance.head.clone()
        } else {
            self.diff.provenance.base.clone()
        };
        let context = context_snippet(&f.hunks[hi], li);
        let path = f.display_path().to_string();
        let target = format!("{path}:{number}");
        let anchor = Anchor::Line {
            file: path,
            side,
            start: number,
            end: number,
            commit,
            context,
        };
        self.input = Some(Compose {
            area: TextArea::default(),
            kind: ComposeKind::New(anchor),
            target,
            confirming_discard: false,
        });
    }

    /// Begin replying to the thread anchored at the cursor's line (Files view).
    fn start_reply(&mut self) {
        match self.thread_at_cursor() {
            Some(idx) => self.open_reply(idx),
            None => self.status = Some("no comment on this line to reply to".to_string()),
        }
    }

    /// Open a reply composer targeting thread `idx`.
    fn open_reply(&mut self, idx: usize) {
        let thread = &self.review.threads[idx];
        let who = thread.root().map(|c| c.author.as_str()).unwrap_or("thread");
        self.input = Some(Compose {
            area: TextArea::default(),
            kind: ComposeKind::Reply(thread.id.clone()),
            target: format!("reply to {who}"),
            confirming_discard: false,
        });
    }

    /// Toggle the resolved state of the thread anchored at the cursor's line.
    fn toggle_resolve(&mut self) {
        match self.thread_at_cursor() {
            Some(idx) => self.resolve_thread(idx),
            None => self.status = Some("no comment on this line to resolve".to_string()),
        }
    }

    /// Toggle the resolved state of thread `idx` and persist.
    fn resolve_thread(&mut self, idx: usize) {
        let thread = &mut self.review.threads[idx];
        thread.state = if thread.is_resolved() {
            ThreadState::Open
        } else {
            ThreadState::Resolved
        };
        let resolved = thread.is_resolved();
        self.status = self.persist(if resolved { "resolved" } else { "reopened" });
        self.relayout();
    }

    /// Route a key in the Conversation view.
    fn on_key_conversation(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let page = self.body_height.get().max(1);
        match (code, ctrl) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => self.move_conv(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => self.move_conv(-1),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => self.set_conv(0),
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => {
                self.set_conv(self.review.threads.len().saturating_sub(1))
            }
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
                self.conv_scroll = (self.conv_scroll + page / 2).min(self.conv_max_scroll())
            }
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
                self.conv_scroll = self.conv_scroll.saturating_sub(page / 2)
            }
            (KeyCode::Char('r'), false) if self.has_review() => self.open_reply(self.conv_cursor),
            (KeyCode::Char('x'), false) if self.has_review() => {
                self.resolve_thread(self.conv_cursor)
            }
            (KeyCode::Char('X'), false) if self.has_review() => self.confirming_close = true,
            _ => {}
        }
    }

    /// Close the review: delete the store and clear all threads.
    fn close_review(&mut self) {
        self.status = match self.store.as_ref().map(Store::delete) {
            Some(Ok(())) | None => Some("review closed".to_string()),
            Some(Err(e)) => Some(format!("could not remove the store: {e:#}")),
        };
        self.review.threads.clear();
        self.conv_cursor = 0;
        self.conv_scroll = 0;
        self.view = View::Files;
        self.relayout();
    }

    fn move_conv(&mut self, delta: isize) {
        if self.review.threads.is_empty() {
            return;
        }
        let last = (self.review.threads.len() - 1) as isize;
        let next = (self.conv_cursor as isize + delta).clamp(0, last);
        self.set_conv(next as usize);
    }

    fn set_conv(&mut self, index: usize) {
        if self.review.threads.is_empty() {
            return;
        }
        self.conv_cursor = index.min(self.review.threads.len() - 1);
        self.follow_conv();
    }

    /// The first line index of each Conversation thread block (blocks are
    /// separated by one spacer line).
    fn conv_offsets(&self) -> Vec<usize> {
        let mut offsets = Vec::with_capacity(self.conv_blocks.len());
        let mut line = 0;
        for block in &self.conv_blocks {
            offsets.push(line);
            line += block.len() + 1;
        }
        offsets
    }

    fn conv_total_lines(&self) -> usize {
        self.conv_blocks.iter().map(|b| b.len() + 1).sum()
    }

    fn conv_max_scroll(&self) -> usize {
        let height = self.body_height.get().max(1);
        self.conv_total_lines().saturating_sub(height)
    }

    /// Keep the selected thread visible in the Conversation view.
    fn follow_conv(&mut self) {
        let offsets = self.conv_offsets();
        let Some(&target) = offsets.get(self.conv_cursor) else {
            return;
        };
        let height = self.body_height.get().max(1);
        if target < self.conv_scroll {
            self.conv_scroll = target;
        } else if target >= self.conv_scroll + height {
            self.conv_scroll = target.saturating_sub(height / 2);
        }
        self.conv_scroll = self.conv_scroll.min(self.conv_max_scroll());
    }

    /// The index of the thread anchored at the cursor's line, if any.
    fn thread_at_cursor(&self) -> Option<usize> {
        if self.clines.is_empty() {
            return None;
        }
        let (file, flat) = self.clines[self.cursor];
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let path = self.diff.files[file].display_path();
        for (side, number) in [(Side::New, line.new_lineno), (Side::Old, line.old_lineno)] {
            if let Some(n) = number
                && let Some(idx) = self.review.threads.iter().position(|t| {
                    matches!(&t.anchor, Anchor::Line { file, side: s, end, .. }
                        if file == path && *s == side && *end == n)
                })
            {
                return Some(idx);
            }
        }
        None
    }

    /// Finish composing: create the thread or append the reply, then save.
    fn submit_compose(&mut self) {
        let Some(compose) = self.input.take() else {
            return;
        };
        if compose.area.is_blank() {
            self.status = Some("comment is empty".to_string());
            self.input = Some(compose);
            return;
        }
        let comment = Comment {
            id: generate_id(),
            author: self.author.clone(),
            body: compose.area.text().trim_end().to_string(),
            created_at: now(),
            remote_id: None,
        };
        self.status = match compose.kind {
            ComposeKind::New(anchor) => {
                self.review.threads.push(Thread {
                    id: generate_id(),
                    anchor,
                    state: ThreadState::Open,
                    comments: vec![comment],
                });
                self.persist("comment added")
            }
            ComposeKind::Reply(thread_id) => match self.review.thread_mut(&thread_id) {
                Some(thread) => {
                    thread.comments.push(comment);
                    self.persist("reply added")
                }
                None => Some("the thread is gone".to_string()),
            },
        };
        self.relayout();
    }

    /// Save the review, returning a status message describing the outcome.
    fn persist(&self, done: &str) -> Option<String> {
        match &self.store {
            Some(store) => match store.save(&self.review) {
                Ok(()) => Some(done.to_string()),
                Err(e) => Some(format!("{done}, but save failed: {e:#}")),
            },
            None => Some(format!("{done} (not saved)")),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.input.is_some() {
            return; // the composer owns input
        }
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
            Mode::Auto => self.body_width.get() >= self.split_min_width,
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
        // A tab bar appears once the review has threads.
        let tabs = self.has_review();
        let constraints = if tabs {
            vec![
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ]
        } else {
            vec![
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(f.area());
        let (header, body, footer) = if tabs {
            (chunks[0], chunks[2], chunks[3])
        } else {
            (chunks[0], chunks[1], chunks[2])
        };
        self.body_height.set(body.height as usize);
        self.body_width.set(body.width as usize);

        self.draw_header(f, header);
        if tabs {
            self.draw_tabs(f, chunks[1]);
        }
        if self.view == View::Conversation {
            self.draw_conversation(f, body);
        } else if self.clines.is_empty() && self.diff.files.is_empty() {
            self.draw_empty(f, body);
        } else if self.sbs() {
            self.draw_body_sbs(f, body);
        } else {
            self.draw_body_unified(f, body);
        }
        self.draw_footer(f, footer);

        if let Some(compose) = &self.input {
            self.draw_compose(f, compose);
        }
        if self.confirming_close {
            self.draw_close_confirm(f);
        }
    }

    /// The "close review?" confirmation modal.
    fn draw_close_confirm(&self, f: &mut Frame) {
        let area = centered_rect(60, 22, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Close review? ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let count = self.review.threads.len();
        let lines = vec![
            TextLine::from(TextSpan::styled(
                format!("Delete all {count} thread(s) and close this review?"),
                Style::default().fg(Color::White),
            )),
            TextLine::from(""),
            TextLine::from(TextSpan::styled(
                "y / Enter confirm · any other key cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let active = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let idle = Style::default().fg(Color::Gray);
        let files = format!(" Files ({}) ", self.diff.files.len());
        let conv = format!(" Conversation ({}) ", self.review.threads.len());
        let line = TextLine::from(vec![
            TextSpan::styled(
                files,
                if self.view == View::Files {
                    active
                } else {
                    idle
                },
            ),
            TextSpan::raw(" "),
            TextSpan::styled(
                conv,
                if self.view == View::Conversation {
                    active
                } else {
                    idle
                },
            ),
            TextSpan::styled("   tab to switch", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_conversation(&self, f: &mut Frame, area: Rect) {
        let select_bg = Color::Rgb(40, 46, 60);
        let mut lines: Vec<TextLine> = Vec::new();
        for (ti, block) in self.conv_blocks.iter().enumerate() {
            let selected = ti == self.conv_cursor;
            for line in block {
                if selected {
                    let spans: Vec<TextSpan> = line
                        .spans
                        .iter()
                        .map(|s| TextSpan::styled(s.content.clone(), s.style.bg(select_bg)))
                        .collect();
                    lines.push(TextLine::from(spans));
                } else {
                    lines.push(line.clone());
                }
            }
            lines.push(TextLine::from(""));
        }
        let height = area.height as usize;
        let start = self.conv_scroll.min(lines.len().saturating_sub(1));
        let end = (start + height).min(lines.len());
        f.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
    }

    /// The comment-composer modal, overlaid on the body.
    fn draw_compose(&self, f: &mut Frame, compose: &Compose) {
        let area = centered_rect(80, 50, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Comment on {} ", compose.target))
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        f.render_widget(
            Paragraph::new(
                compose
                    .area
                    .render(rows[0].width as usize, Style::default()),
            ),
            rows[0],
        );
        let hint = if compose.confirming_discard {
            TextSpan::styled(
                "Discard this comment? Esc again to discard · any key to keep editing",
                Style::default().fg(Color::Yellow),
            )
        } else {
            TextSpan::styled(
                "Enter newline · Ctrl-S submit · Esc cancel",
                Style::default().fg(Color::DarkGray),
            )
        };
        f.render_widget(Paragraph::new(TextLine::from(hint)), rows[1]);
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
        let mut spans = vec![
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
        ];
        if !self.review.is_empty() {
            spans.push(TextSpan::styled(
                format!("  💬 {} open", self.review.open_count()),
                bar.fg(Color::Rgb(120, 160, 220)),
            ));
        }
        spans.push(self.watch_indicator(bar));
        f.render_widget(Paragraph::new(TextLine::from(spans)).style(bar), area);
    }

    /// The header's auto-refresh indicator: a live dot, a brief "updated" flash
    /// on reload, or the reason auto-refresh is off.
    fn watch_indicator(&self, bar: Style) -> TextSpan<'static> {
        if let Some(reason) = &self.watch_error {
            return TextSpan::styled(format!("  ⚠ {reason}"), bar.fg(Color::Yellow));
        }
        if !self.watching {
            return TextSpan::styled("", bar);
        }
        let fresh = self
            .reloaded_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(RELOAD_FLASH_MS));
        if fresh {
            TextSpan::styled(
                "  ⟳ updated",
                bar.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )
        } else {
            TextSpan::styled("  ● watching", bar.fg(Color::Green))
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let bar = Style::default().bg(BAR_BG);
        let position = if self.view == View::Conversation {
            format!(
                " [{}/{}] ",
                self.conv_cursor + 1,
                self.review.threads.len().max(1)
            )
        } else {
            format!(
                " [{}/{}]{} ",
                self.current_file() + 1,
                self.diff.files.len().max(1),
                self.cursor_anchor()
            )
        };
        let mut spans = vec![TextSpan::styled(position, bar.fg(Color::Cyan))];
        if let Some(status) = &self.status {
            spans.push(TextSpan::styled(status.clone(), bar.fg(Color::Yellow)));
        } else {
            let help = if self.view == View::Conversation {
                "j/k thread · r reply · x resolve · X close review · tab files · q quit"
            } else {
                "j/k move · n/p file · c comment · r reply · x resolve · v split · q quit"
            };
            spans.push(TextSpan::styled(help, bar.fg(Color::DarkGray)));
        }
        f.render_widget(Paragraph::new(TextLine::from(spans)).style(bar), area);
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
                URow::Comment(t, k) => self.comment_blocks[*t][*k].clone(),
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
                SRow::Comment(t, k) => self.comment_blocks[*t][*k].clone(),
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

/// The left gutter bar drawn beside an inline comment thread.
const COMMENT_BAR: Color = Color::Rgb(90, 130, 200);
/// Width the inline comment body wraps to (before the gutter bar).
const INLINE_COMMENT_WRAP: usize = 76;

/// Render each thread's inline block (index-aligned to `review.threads`): a
/// header naming the author and state, then the root comment's body as markdown.
fn build_comment_blocks(review: &Review, highlighter: &Highlighter) -> Vec<Vec<TextLine<'static>>> {
    let bar = Style::default().fg(COMMENT_BAR);
    review
        .threads
        .iter()
        .map(|thread| {
            let mut lines = Vec::new();
            let author = thread.root().map(|c| c.author.clone()).unwrap_or_default();
            let mut header = vec![
                TextSpan::styled("  ▏ ", bar),
                TextSpan::styled("💬 ", Style::default().fg(Color::Cyan)),
                TextSpan::styled(
                    author,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if thread.is_resolved() {
                header.push(TextSpan::styled(
                    "  [resolved]",
                    Style::default().fg(Color::Green),
                ));
            }
            let replies = thread.replies().len();
            if replies > 0 {
                header.push(TextSpan::styled(
                    format!(
                        "  ({replies} repl{})",
                        if replies == 1 { "y" } else { "ies" }
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(TextLine::from(header));
            if let Some(root) = thread.root() {
                for body in
                    crate::markdown::render(&root.body, Some(INLINE_COMMENT_WRAP), highlighter)
                {
                    let mut spans = vec![TextSpan::styled("  ▏ ", bar)];
                    spans.extend(body.spans);
                    lines.push(TextLine::from(spans));
                }
            }
            lines
        })
        .collect()
}

/// Render each thread as a Conversation block (index-aligned to
/// `review.threads`): where it is anchored and its state, then the root comment
/// and nested replies, each with an author and a relative timestamp.
fn build_conversation(
    review: &Review,
    width: usize,
    highlighter: &Highlighter,
    outdated: &[bool],
) -> Vec<Vec<TextLine<'static>>> {
    let now = now();
    review
        .threads
        .iter()
        .enumerate()
        .map(|(ti, thread)| {
            let is_outdated = outdated.get(ti).copied().unwrap_or(false);
            let mut lines = Vec::new();
            let mut header = vec![TextSpan::styled(
                anchor_label(&thread.anchor),
                Style::default().fg(Color::DarkGray),
            )];
            if thread.is_resolved() {
                header.push(TextSpan::styled(
                    "  [resolved]",
                    Style::default().fg(Color::Green),
                ));
            }
            if is_outdated {
                header.push(TextSpan::styled(
                    "  [outdated]",
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(TextLine::from(header));

            // For an outdated thread, show the context saved at creation so the
            // reviewer can still see the line it was left on.
            if is_outdated && let Anchor::Line { context, .. } = &thread.anchor {
                for snippet in context {
                    lines.push(TextLine::from(TextSpan::styled(
                        format!("  │ {snippet}"),
                        Style::default().fg(Color::Rgb(90, 90, 100)),
                    )));
                }
            }

            if let Some(root) = thread.root() {
                lines.push(comment_meta_line(&root.author, root.created_at, now, false));
                lines.extend(crate::markdown::render(
                    &root.body,
                    Some(width),
                    highlighter,
                ));
            }
            for reply in thread.replies() {
                lines.push(comment_meta_line(
                    &reply.author,
                    reply.created_at,
                    now,
                    true,
                ));
                for line in
                    crate::markdown::render(&reply.body, Some(width.saturating_sub(2)), highlighter)
                {
                    let mut spans = vec![TextSpan::raw("  ")];
                    spans.extend(line.spans);
                    lines.push(TextLine::from(spans));
                }
            }
            lines
        })
        .collect()
}

/// The author/timestamp line for a comment; replies are marked and indented.
fn comment_meta_line(author: &str, created_at: u64, now: u64, reply: bool) -> TextLine<'static> {
    let prefix = if reply { "  ↳ " } else { "" };
    TextLine::from(vec![
        TextSpan::styled(prefix, Style::default().fg(Color::DarkGray)),
        TextSpan::styled(
            author.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        TextSpan::styled(
            format!("  · {}", relative_time(created_at, now)),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// Which threads are outdated: line-anchored, but their line is not in the
/// current diff (so they were not shown inline).
fn outdated_flags(review: &Review, placed: &[bool]) -> Vec<bool> {
    review
        .threads
        .iter()
        .enumerate()
        .map(|(i, thread)| {
            matches!(thread.anchor, Anchor::Line { .. }) && !placed.get(i).copied().unwrap_or(false)
        })
        .collect()
}

/// A human label for where a thread is anchored.
fn anchor_label(anchor: &Anchor) -> String {
    match anchor {
        Anchor::Line {
            file,
            side,
            start,
            end,
            ..
        } => {
            let range = if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            };
            let side = match side {
                Side::Old => "old",
                Side::New => "new",
            };
            format!("{file}:{range} ({side})")
        }
        Anchor::File { file } => file.clone(),
        Anchor::Review => "changeset".to_string(),
    }
}

/// A coarse "N ago" relative time from `then` to `now` (both epoch seconds).
fn relative_time(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    if d < 45 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", (d / 60).max(1))
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else if d < 30 * 86_400 {
        format!("{}d ago", d / 86_400)
    } else {
        format!("{}mo ago", d / (30 * 86_400))
    }
}

/// The lines around index `li` in `hunk`, saved as an anchor's context snippet.
fn context_snippet(hunk: &loopreview_core::Hunk, li: usize) -> Vec<String> {
    let start = li.saturating_sub(2);
    let end = (li + 3).min(hunk.lines.len());
    hunk.lines[start..end]
        .iter()
        .map(|l| l.content.clone())
        .collect()
}

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A process-unique id combining the current time with a counter.
fn generate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", now(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// A rectangle centered within `area`, sized as a percentage of it.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
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
    /// Whether each thread's anchor was found in the current diff (shown inline);
    /// a line-anchored thread that was not is outdated.
    placed: Vec<bool>,
    max_lineno: u32,
}

impl Layouts {
    fn build(diff: &Diff, review: &Review, block_lens: &[usize]) -> Layouts {
        let mut placed = vec![false; review.threads.len()];
        // Map each line-anchored thread to its file and (side, line) so it can be
        // shown inline beneath that line in the unified view.
        let mut thread_at: HashMap<&str, HashMap<(Side, u32), Vec<usize>>> = HashMap::new();
        for (idx, thread) in review.threads.iter().enumerate() {
            if let Anchor::Line {
                file, side, end, ..
            } = &thread.anchor
            {
                thread_at
                    .entry(file.as_str())
                    .or_default()
                    .entry((*side, *end))
                    .or_default()
                    .push(idx);
            }
        }

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
            let file_threads = thread_at.get(file.display_path());
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

                        // Insert any comment threads anchored to this line.
                        if let Some(file_threads) = file_threads {
                            let line = &hunk.lines[li];
                            for (side, number) in
                                [(Side::New, line.new_lineno), (Side::Old, line.old_lineno)]
                            {
                                if let Some(n) = number
                                    && let Some(indices) = file_threads.get(&(side, n))
                                {
                                    for &t in indices {
                                        placed[t] = true;
                                        for k in 0..block_lens[t] {
                                            urows.push(URow::Comment(t, k));
                                        }
                                    }
                                }
                            }
                        }
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
                let file_threads = thread_at.get(file.display_path());
                let mut flat_counter = 0usize;
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    srows.push(SRow::HunkHeader(fi, hi));
                    // Collect the old|new pairs for this hunk, then emit them with
                    // any inline comments beneath each.
                    let mut dels = Vec::new();
                    let mut adds = Vec::new();
                    let mut pairs: Vec<(Option<usize>, Option<usize>)> = Vec::new();
                    for line in &hunk.lines {
                        let flat = flat_counter;
                        flat_counter += 1;
                        match line.kind {
                            LineKind::Context => {
                                flush_block(&mut dels, &mut adds, &mut pairs);
                                pairs.push((Some(flat), Some(flat)));
                            }
                            LineKind::Deletion => dels.push(flat),
                            LineKind::Addition => adds.push(flat),
                        }
                    }
                    flush_block(&mut dels, &mut adds, &mut pairs);

                    for (old, new) in pairs {
                        let row = srows.len();
                        srows.push(SRow::Pair { file: fi, old, new });
                        if let Some(of) = old {
                            line_srow[cursor_of[fi][of]] = row;
                        }
                        if let Some(nf) = new {
                            line_srow[cursor_of[fi][nf]] = row;
                        }
                        if let Some(threads) = file_threads {
                            for (side, flat_opt) in [(Side::New, new), (Side::Old, old)] {
                                let Some(f) = flat_opt else { continue };
                                let (chi, cli) = flats[fi][f];
                                let cline = &file.hunks[chi].lines[cli];
                                let number = match side {
                                    Side::New => cline.new_lineno,
                                    Side::Old => cline.old_lineno,
                                };
                                if let Some(n) = number
                                    && let Some(indices) = threads.get(&(side, n))
                                {
                                    for &t in indices {
                                        for k in 0..block_lens[t] {
                                            srows.push(SRow::Comment(t, k));
                                        }
                                    }
                                }
                            }
                        }
                    }
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
            placed,
            max_lineno,
        }
    }
}

/// Pair a change block's deletions with additions positionally into old|new
/// pairs (a surplus on either side pairs with `None`).
fn flush_block(
    dels: &mut Vec<usize>,
    adds: &mut Vec<usize>,
    pairs: &mut Vec<(Option<usize>, Option<usize>)>,
) {
    let n = dels.len().max(adds.len());
    for k in 0..n {
        pairs.push((dels.get(k).copied(), adds.get(k).copied()));
    }
    dels.clear();
    adds.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(relative_time(100, 130), "just now");
        assert_eq!(relative_time(0, 120), "2m ago");
        assert_eq!(relative_time(0, 7200), "2h ago");
        assert_eq!(relative_time(0, 3 * 86_400), "3d ago");
    }

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
