//! The ratatui review UI.
//!
//! A line cursor always points at a diff line's `(file, side, line)` anchor.
//! The diff renders either unified or side-by-side; `auto` picks by width and a
//! key toggles at runtime. Changed words within a modified line are emphasized
//! using the core's intra-line diff. All diff data comes from
//! [`loopreview_core`]; this module lays out rows, routes events, and paints.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use notify::{RecursiveMode, Watcher};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::UnicodeWidthChar;

use loopreview_control::events::EventLog;
use loopreview_control::protocol::{
    self, ContextInfo, EventKind, NavigateResult, ReloadResult, Reply, Request, Response,
    ReviewInfo, SessionInfo, SubjectInfo,
};

use loopreview_core::{
    Anchor, Comment, CommentKind, Diff, DiffSource, Line, LineKind, Review, Segment, Side, Thread,
    ThreadState, word_diff,
};

use loopreview_github::{CommentEndpoint, PrStatus};

use crate::control::{self, UiRequest};
use crate::highlight::{Highlighter, LineHighlighter, Span as HlSpan};
use crate::keys::Action;
use crate::palette::*;
use crate::prsync::{IssueHandle, PrHandle, SubjectOverview, SubjectStatus};
use crate::store::Store;
use crate::textarea::TextArea;

/// Rows of context kept above/below the cursor when scrolling.
const SCROLLOFF: usize = 3;
/// Terminal height at or above which the tab bar gets padding rows above and
/// below it; below this the padding collapses so the body is not squeezed.
const TAB_SPACING_MIN_HEIGHT: u16 = 16;
/// Columns moved per horizontal-scroll step.
const HSCROLL_STEP: isize = 8;
/// Trailing whitespace allowed past the longest line at the far-right scroll
/// stop — a small reading margin so the tail is not glued to the edge.
const HSCROLL_MARGIN: usize = 8;
/// Sidebar width bounds (the minimum diff width kept beside it is configurable).
const SIDEBAR_MIN: usize = 22;
const SIDEBAR_MAX: usize = 44;
/// A cursor stop's `flat` value meaning "the file header" (not a content line).
/// Headers are cursor stops so `j`/`k` walks them and folded files stay
/// navigable; content lines use their real flat index.
const HEADER: usize = usize::MAX;
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

/// Which pane holds the keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The diff (or Conversation) body.
    Body,
    /// The file-explorer sidebar.
    Sidebar,
}

/// The fuzzy file-finder overlay (Ctrl-P).
struct Finder {
    /// The current search text.
    query: String,
    /// Ranked matches: `(file index, matched char positions)`, best first.
    matches: Vec<(usize, Vec<u32>)>,
    /// Index into `matches` of the highlighted row.
    selected: usize,
}

/// The `?` command palette: a searchable list of every action with its key.
struct Palette {
    /// The current search text.
    query: String,
    /// Ranked action indices (into [`Action::ALL`]), available-in-context first.
    matches: Vec<usize>,
    /// Index into `matches` of the highlighted row.
    selected: usize,
}

/// Which top-level view is showing (tabs appear for any comment-capable source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// The pull request's facts and description — PR only, leftmost.
    Overview,
    /// The diff, with comments inline.
    Files,
    /// The comment threads, chronological with replies.
    Conversation,
}

impl View {
    /// The full tab order, left to right. `Overview` is present only on a pull
    /// request; the app filters this by availability when cycling and drawing.
    const ORDER: &'static [View] = &[View::Overview, View::Files, View::Conversation];
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
    /// A diff with more files than this opens with every file collapsed.
    pub auto_collapse_files: usize,
    /// A diff with more changed lines than this opens with every file collapsed.
    pub auto_collapse_lines: usize,
    /// When the file-explorer sidebar is shown by default.
    pub sidebar_mode: crate::config::SidebarMode,
    /// Minimum diff width kept beside the sidebar.
    pub sidebar_min_content: usize,
    /// A fixed sidebar width, if the user pinned one (else it auto-fits).
    pub sidebar_width: Option<usize>,
    /// What Enter does in the comment composer (newline by default; save opt-in).
    pub composer_enter: crate::config::ComposerEnter,
    /// The resolved key bindings.
    pub keymap: crate::keys::Keymap,
    /// The repository directory, for reconstructing outdated comment lines from
    /// history (`git show <commit>:<path>`). `None` for patch sources.
    pub repo_dir: Option<PathBuf>,
    /// A background loader (used by `lr pr`): when present, the UI opens on a
    /// spinner and this runs off-thread to produce the diff and threads.
    pub loader: Option<Loader>,
    /// An initial status line to show (e.g. a store-recovery warning).
    pub notice: Option<String>,
}

/// The result of a background load: the diff and threads to show.
pub struct Loaded {
    /// Source description for the header.
    pub label: String,
    /// The loaded diff.
    pub diff: Diff,
    /// The review threads to seed (e.g. a PR's pulled comments).
    pub review: Review,
    /// The PR handle, when this load is a pull request (enables sync/submit).
    pub pr: Option<PrHandle>,
    /// The issue handle, when this load is an issue (no diff; conversation only).
    pub issue: Option<IssueHandle>,
    /// The store key for the subject's drafts (`owner/repo#number`), PR or issue.
    pub pr_key: Option<String>,
    /// Stale draft ghosts dropped while merging saved drafts (pre-F2 store
    /// contamination); surfaced in the status when non-zero.
    pub stale_cleaned: usize,
}

/// A background load job: reports progress via the callback, then yields the
/// diff and threads (or an error message).
pub type Loader = Box<dyn FnOnce(&dyn Fn(&str)) -> Result<Loaded, String> + Send>;

/// A message streamed from a running [`Loader`].
enum LoadMsg {
    Stage(String),
    Ready(Box<Loaded>),
    Failed(String),
}

/// The state of an in-progress background load.
struct Loading {
    stage: String,
    rx: Receiver<LoadMsg>,
}

/// A short-lived background action against GitHub (refresh, resolve, submit),
/// shown as a spinner overlay while it runs.
struct Job {
    title: String,
    stage: String,
    rx: Receiver<JobMsg>,
}

/// A message streamed from a running [`Job`].
enum JobMsg {
    Stage(String),
    Done(Result<JobOutcome, String>),
}

/// The result a finished [`Job`] applies to the review.
enum JobOutcome {
    /// A re-pulled thread list to merge with local drafts, plus the PR's fresh
    /// overview (`None` when the metadata re-fetch failed — keep the old).
    Refreshed {
        threads: Vec<Thread>,
        overview: Option<Box<SubjectOverview>>,
    },
    /// The thread (by id) had its resolution synced. Id, not index: the review
    /// can shift while the network job runs.
    Resolved { thread_id: String, resolved: bool },
    /// A submitted review's id stamps.
    Submitted(crate::prsync::Submitted),
    /// A published comment's body was edited on GitHub; apply it locally.
    Edited {
        thread_id: String,
        comment_id: String,
        body: String,
    },
    /// A published comment was deleted on GitHub; remove it locally.
    Deleted {
        thread_id: String,
        comment_id: String,
    },
    /// Issue conversation drafts were posted; stamp each root's remote id so it
    /// reads as published. `(thread id, remote id)` per posted draft.
    IssueSent(Vec<(String, Option<String>)>),
}

/// A background action worker: reports progress, then yields an outcome.
type JobWorker = Box<dyn FnOnce(&dyn Fn(&str)) -> Result<JobOutcome, String> + Send>;

/// How a URL is launched (a link/image click, `Ctrl-O`); injectable for tests.
type UrlOpener = Box<dyn Fn(&str) -> std::io::Result<()>>;

/// Spinner frames shown while a background load runs.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
        auto_collapse_files,
        auto_collapse_lines,
        sidebar_mode,
        sidebar_min_content,
        sidebar_width,
        composer_enter,
        keymap,
        repo_dir,
        loader,
        notice,
    } = session;

    let mut app = App::new(
        label,
        diff,
        review,
        store,
        author,
        Highlighter::new(),
        repo_dir.clone(),
    );
    app.mode = mode;
    app.split_min_width = split_min_width;
    app.auto_collapse_files = auto_collapse_files;
    app.auto_collapse_lines = auto_collapse_lines;
    app.sidebar_mode = sidebar_mode;
    app.sidebar_min_content = sidebar_min_content;
    app.sidebar_width_cfg = sidebar_width;
    app.composer_enter = composer_enter;
    app.keymap = keymap;
    app.status = notice;
    // A directly-loaded local review may carry old contradictory state.
    app.normalize_resolved_drafts();
    app.normalize_conversation_reply_drafts();
    // For a directly-loaded diff (not a background PR load), apply auto-collapse
    // now; the PR path does it in install_loaded once the diff arrives.
    if loader.is_none() {
        app.maybe_auto_collapse();
    }

    // Host the control plane so agents can read, steer, comment, and wait. A
    // failure here degrades to a plain UI (no `lr session`), never a crash.
    let control = control::start(
        crate::config::sessions_dir().as_deref(),
        repo_dir.as_deref(),
        &app.label,
    );
    if let Some(control) = &control {
        app.session_id = control.session_id.clone();
        app.events = control.events.clone();
    }

    // The source is kept for on-demand reloads (a control-plane `reload`); the
    // watcher, when active, gets its own clone.
    app.source = Some(source.clone());
    if let Some(loader) = loader {
        app.start_loading(loader);
    }
    let updates = watch_root.map(|root| spawn_watcher(root, source));
    app.watching = updates.is_some();

    let mut terminal = ratatui::init();
    // Mouse capture and bracketed paste are best-effort: the UI is usable
    // without them, but they make navigation and pasting comments pleasant.
    // Bracketed paste in particular delivers a multi-line paste as one event, so
    // its newlines are inserted verbatim rather than handled key-by-key (which
    // matters under the opt-in `composer_enter = "save"`, where Enter saves).
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // The Kitty keyboard protocol lets the terminal disambiguate Shift+Enter from
    // a bare Enter — needed for `composer_enter = "save"`, where Shift/Alt+Enter
    // is the newline. Harmless under the default (Enter is already the newline).
    // Only push it where supported; remember that, so teardown pops what we set.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = app.event_loop(
        &mut terminal,
        updates,
        control.as_ref().map(|c| &c.requests),
    );
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();
    if let Some(control) = &control {
        control.deregister();
    }
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
    /// Editing an existing comment's body (thread id, comment id).
    Edit { thread: String, comment: String },
}

/// A pending removal awaiting `d` confirmation.
#[derive(Debug)]
struct DeleteTarget {
    /// The thread's id — resolved to a fresh index at confirm time, since the
    /// review can shift between arming `d` and pressing `y`.
    thread_id: String,
    /// The comment id to remove, or `None` to remove the whole draft thread
    /// (a draft root takes its thread with it).
    comment_id: Option<String>,
    /// For a published comment, its GitHub endpoint — the removal goes to GitHub
    /// first. `None` for a purely-local removal.
    published: Option<CommentEndpoint>,
    /// How many *other* comments in the thread this removal takes with it: when
    /// the deletion leaves the thread with no published comment (a conversation
    /// root with local replies is the case), the whole thread — and the local
    /// notes hanging under it — goes. Surfaced in the confirmation so the reader
    /// consents before their own notes are removed. `0` when the thread survives.
    also_removed: usize,
}

/// The review events offered in the submit modal.
const SUBMIT_EVENTS: &[(&str, crate::prsync::SubmitEvent)] = &[
    ("Comment", crate::prsync::SubmitEvent::Comment),
    ("Approve", crate::prsync::SubmitEvent::Approve),
    (
        "Request changes",
        crate::prsync::SubmitEvent::RequestChanges,
    ),
    (
        "Pending (don't submit)",
        crate::prsync::SubmitEvent::Pending,
    ),
];

/// What a pending submit would post — the counts, authors, and foreign flag the
/// submit modal is built from.
struct DraftSummary {
    /// New inline review comments (line-anchored draft roots).
    new_inline: usize,
    /// Inline replies to already-published threads.
    replies: usize,
    /// New conversation-comment roots (Review-anchored draft roots).
    conversation: usize,
    /// Draft authors and their counts, most first.
    authors: Vec<(String, usize)>,
    /// True when a draft is authored by someone other than the submitting human.
    foreign: bool,
}

impl DraftSummary {
    /// Total drafts that will actually post.
    fn total(&self) -> usize {
        self.new_inline + self.replies + self.conversation
    }
}

/// The review-submission modal for a pull request.
struct SubmitModal {
    /// Selected index into [`SUBMIT_EVENTS`].
    selected: usize,
    /// The optional summary body.
    body: TextArea,
    /// New inline drafts that will be posted.
    new_count: usize,
    /// Draft replies that will be posted.
    reply_count: usize,
    /// New conversation comments that will be posted.
    conversation_count: usize,
    /// Draft authors and their counts, most first — a check before sending under
    /// the human's GitHub identity.
    authors: Vec<(String, usize)>,
    /// True when a draft is authored by someone other than the submitting human
    /// (e.g. an agent) — flagged so it is not sent unnoticed.
    foreign: bool,
}

impl SubmitModal {
    /// A reply-only / conversation-only batch — no new inline comments, so no
    /// review POST and no event to choose (just a send confirmation).
    fn is_send_only(&self) -> bool {
        self.new_count == 0
    }
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
    /// True when this composes a GitHub *suggested change* — the body is
    /// pre-seeded with a ```suggestion block. Only the header wording differs;
    /// it saves through the same kind/submit flow as any other comment.
    suggestion: bool,
}

/// A disambiguation picker: when several threads cover the cursor's line
/// (overlapping range comments), a thread action opens this instead of silently
/// choosing one, so the reviewer picks the intended target.
struct ThreadPicker {
    /// Candidate thread indices, in display order (end line ascending, then start).
    candidates: Vec<usize>,
    /// The highlighted candidate.
    selected: usize,
    /// The action to run against the chosen thread once confirmed.
    action: Action,
}

/// A relocatable cursor position: a file path and a line number on one side.
/// Used to keep the cursor on the same line across a watch reload.
struct CursorAnchor {
    path: String,
    new_side: bool,
    line: u32,
}

/// A hit-tested screen region (a pure mapping of a click to a pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    /// The tab bar.
    Tabs,
    /// A sidebar file row (index from the top of the sidebar).
    Sidebar(usize),
    /// A diff cell: `col` is relative to the diff (past any sidebar), `row` is
    /// relative to the body top.
    Content { col: u16, row: usize },
    /// The layout indicator in the footer (click to toggle unified/split).
    LayoutToggle,
    /// The `#N` link in the header (click to open the PR page).
    PrLink,
    /// Anything else (header, footer, divider column, outside the body).
    Outside,
}

/// Map a screen cell `(x, y)` to a [`Region`], using the geometry captured at the
/// last draw. Pure so it can be unit-tested across layout combinations. The
/// content and sidebar inner rects share a vertical extent (`body_top`,
/// `body_height`); pane borders fall outside both, mapping to `Outside`.
fn hit_region(x: u16, y: u16, hit: HitLayout) -> Region {
    if hit.tabs_row == Some(y) {
        return Region::Tabs;
    }
    if y == hit.footer_row && x < hit.layout_end {
        return Region::LayoutToggle;
    }
    if hit.pr_link_row == Some(y) && x >= hit.pr_link_x0 && x < hit.pr_link_x1 {
        return Region::PrLink;
    }
    if y < hit.body_top || y >= hit.body_top + hit.body_height {
        return Region::Outside;
    }
    let row = (y - hit.body_top) as usize;
    if hit.sidebar_w > 0 && x >= hit.sidebar_x0 && x < hit.sidebar_x0 + hit.sidebar_w {
        return Region::Sidebar(row);
    }
    if x >= hit.content_x0 && x < hit.content_x0 + hit.content_w {
        return Region::Content {
            col: x - hit.content_x0,
            row,
        };
    }
    Region::Outside
}

/// Where things landed on screen at the last draw, for mouse hit-testing.
#[derive(Clone, Copy, Default)]
struct HitLayout {
    /// First content/sidebar row (below the header, tab bar, and any top border).
    body_top: u16,
    /// Content/sidebar inner height in rows.
    body_height: u16,
    /// Leftmost column of the diff content (past any sidebar and borders).
    content_x0: u16,
    /// Diff content inner width in columns.
    content_w: u16,
    /// Leftmost column of the sidebar file list (past its left border).
    sidebar_x0: u16,
    /// Sidebar inner width in columns (0 = hidden).
    sidebar_w: u16,
    /// The tab bar row, when the tabs are shown. The tab a click lands on is
    /// recomputed from the labels (`tab_at_column`), so no per-tab columns here.
    tabs_row: Option<u16>,
    /// The footer row.
    footer_row: u16,
    /// The layout indicator occupies `[0, layout_end)` on the footer row.
    layout_end: u16,
    /// The header row carrying the clickable `#N` PR link, when in PR context.
    pr_link_row: Option<u16>,
    /// The `#N` link occupies `[pr_link_x0, pr_link_x1)` on `pr_link_row`.
    pr_link_x0: u16,
    pr_link_x1: u16,
}

/// Cached render data for one file, aligned to that file's flat line list.
///
/// Highlighting is incremental: `highlight` holds only the lines shown so far
/// and `line_highlighter` carries the syntect state to extend it on demand, so
/// opening a large file highlights a screenful rather than the whole file. The
/// intra-line word ranges are cheap and computed up front.
struct FileRender {
    /// Incremental syntect state, advanced as more lines are highlighted.
    line_highlighter: LineHighlighter,
    /// Syntax-highlighted runs for the flat lines highlighted so far (a prefix
    /// of the file's flat line list).
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
    /// The active review-submission modal, when submitting a PR review.
    submit: Option<SubmitModal>,
    /// Rendered inline block per thread, index-aligned to `review.threads`.
    comment_blocks: Vec<Vec<TextLine<'static>>>,
    /// Rendered Conversation block per thread (root, replies), same order.
    conv_blocks: Vec<Vec<TextLine<'static>>>,
    /// Per thread, the block-line index where each comment begins (root first,
    /// then replies) — lets the Conversation cursor land on a single comment.
    conv_comment_starts: Vec<Vec<usize>>,
    /// Per thread, the block-relative click regions (links/images/`<details>`
    /// toggles) over its comment bodies, aligned to `conv_blocks`.
    conv_block_regions: Vec<Vec<crate::markdown::MdRegion>>,
    /// The composed Conversation click regions from the last draw (absolute line
    /// indices in the scrolled line list), for mouse hit-testing.
    conv_regions: RefCell<Vec<crate::markdown::MdRegion>>,
    /// Session fold overrides for comment-body `<details>`, keyed by (thread id,
    /// the block's 0-based details index); survives a rebuild while the thread id
    /// does. Not persisted.
    conv_folds: HashMap<(String, usize), bool>,
    /// The last draw's map from a composed `ToggleDetails` index to its
    /// (thread id, block-local details index), so a click routes to the right fold.
    conv_details: RefCell<Vec<(String, usize)>>,
    /// The comment-body `<details>` effective open state from the last build, so a
    /// toggle flips the current value (respecting a `<details open>` default).
    conv_effective: RefCell<HashMap<(String, usize), bool>>,
    /// Whether each thread is outdated (line-anchored but no longer in the diff),
    /// index-aligned to `review.threads`. Drives the thread index's status icon.
    thread_outdated: Vec<bool>,
    /// Display order of threads (each entry a `review.threads` index), sorted by
    /// root-comment time. The Conversation view and thread index render in this
    /// order; `conv_cursor` is a position within it, not a thread index.
    conv_order: Vec<usize>,
    /// Thread ids whose inline/Conversation body is collapsed to its header.
    collapsed: HashSet<String>,
    /// Thread ids the reviewer has folded/unfolded by hand this session; their
    /// state is kept as-is when defaults (resolved = collapsed) are re-derived.
    manual_fold: HashSet<String>,
    /// File display paths that are collapsed to their header (contents hidden;
    /// highlighting is skipped for them).
    collapsed_files: HashSet<String>,
    /// A diff with more files than this opens with every file collapsed.
    auto_collapse_files: usize,
    /// A diff with more changed lines than this opens with every file collapsed.
    auto_collapse_lines: usize,
    /// The current top-level view.
    view: View,
    /// When the sidebar is shown by default.
    sidebar_mode: crate::config::SidebarMode,
    /// A temporary `b` override of the sidebar's visibility; cleared on resize.
    sidebar_override: Option<bool>,
    /// Minimum diff width kept beside the sidebar (below which it auto-hides).
    sidebar_min_content: usize,
    /// A fixed sidebar width (columns), if the user pinned one; else it auto-fits.
    sidebar_width_cfg: Option<usize>,
    /// What Enter does in the comment composer: insert a newline (the default,
    /// Ctrl-S saves) or save (Shift/Alt+Enter for a newline).
    composer_enter: crate::config::ComposerEnter,
    /// Which pane has the keyboard focus.
    focus: Focus,
    /// Selected file index in the sidebar.
    sidebar_cursor: usize,
    /// Scroll offset (in rows) of the sidebar.
    sidebar_scroll: usize,
    /// The fuzzy file-finder overlay, when open.
    finder: Option<Finder>,
    /// The `?` command-palette overlay, when open.
    palette: Option<Palette>,
    /// The disambiguation picker shown when several threads cover the cursor line
    /// and a thread action is invoked, holding the action to run on the choice.
    thread_picker: Option<ThreadPicker>,
    /// A thread index that overrides the cursor→thread mapping while a picked
    /// action runs (set by the picker, cleared right after). `None` normally.
    forced_thread: Option<usize>,
    /// The cursor line where a line-range selection began (`V` or a drag).
    selection: Option<usize>,
    /// The cursor line a mouse-drag started on (to distinguish a click).
    drag_anchor: Option<usize>,
    /// Screen geometry from the last draw, for mouse hit-testing.
    hit: Cell<HitLayout>,
    /// The `#N` PR-link column range `[x0, x1)` in the header, set by
    /// `draw_header` and read into [`HitLayout`]; `None` off a pull request.
    header_pr_link: Cell<Option<(u16, u16)>>,
    /// Horizontal scroll offset, in display columns, of the diff content (the
    /// line-number gutter stays fixed). Reset on a layout switch or file jump.
    hscroll: usize,
    /// The resolved key bindings (defaults plus config overrides).
    keymap: crate::keys::Keymap,
    /// Selected position within `conv_order` (the Conversation view / thread
    /// index), not a `review.threads` index — map through `conv_order`.
    conv_cursor: usize,
    /// Which comment within the selected thread the body cursor rests on (0 =
    /// root, 1.. = replies). Lets `e`/`d` target a specific reply. Reset to 0
    /// whenever the selected thread changes.
    conv_comment: usize,
    /// Scroll offset (in lines) of the Conversation view.
    conv_scroll: usize,
    /// Minimum body width for `auto` layout to choose side-by-side.
    split_min_width: usize,
    /// True while awaiting confirmation to close (delete) the review.
    confirming_close: bool,
    /// The delete target awaiting confirmation when `d` is armed.
    confirming_delete: Option<DeleteTarget>,
    /// An in-progress background load (spinner), for `lr pr`.
    loading: Option<Loading>,
    /// A fatal load error to show instead of the diff.
    load_error: Option<String>,
    /// A short-lived background action against GitHub, when one is running.
    job: Option<Job>,
    /// The pull-request handle, when reviewing a PR (enables sync/submit).
    pr: Option<Arc<PrHandle>>,
    /// The issue handle, when reviewing an issue (no diff; a flat conversation
    /// with a send-only comment path).
    issue: Option<Arc<IssueHandle>>,
    /// The subject overview (PR or issue: status + facts + description) for the
    /// header badge and the Overview tab — resolved at load, re-fetched on refresh
    /// so a transition (open → merged / closed) or a description edit follows.
    pr_overview: Option<SubjectOverview>,
    /// The Overview tab's read-only scroll offset (in rendered lines).
    overview_scroll: usize,
    /// Session fold overrides for the Overview body's `<details>` (index → open),
    /// over each block's `open` attribute; not persisted.
    overview_folds: HashMap<usize, bool>,
    /// The Overview body's click regions from the last render (links, images,
    /// `<details>` toggles), for mouse hit-testing.
    overview_regions: RefCell<Vec<crate::markdown::MdRegion>>,
    /// The Overview `<details>` effective open state from the last render, so a
    /// toggle flips the current value.
    overview_effective: RefCell<HashMap<usize, bool>>,
    /// How a URL is opened (a link/image click, `Ctrl-O`). Injectable so tests
    /// capture the URL instead of launching the system browser.
    url_opener: UrlOpener,
    /// The store key for the current PR's drafts (`owner/repo#number`).
    pr_key: Option<String>,
    /// The repo directory, for reconstructing outdated lines from history.
    repo_dir: Option<PathBuf>,
    /// The diff source, for an on-demand reload from the control plane.
    source: Option<Arc<dyn DiffSource + Send + Sync>>,
    /// This session's control-plane id (empty when the control plane is off).
    session_id: String,
    /// The event log the control plane publishes to and `wait` blocks on.
    events: Arc<EventLog>,
    /// Repaint tick, for the spinner animation.
    tick: usize,
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
        repo_dir: Option<PathBuf>,
    ) -> App {
        // Resolved threads start collapsed (open ones expanded); manual folds win.
        let collapsed: HashSet<String> = review
            .threads
            .iter()
            .filter(|t| t.is_resolved())
            .map(|t| t.id.clone())
            .collect();
        let comment_blocks = build_comment_blocks(&review, &highlighter, &collapsed);
        let block_lens: Vec<usize> = comment_blocks.iter().map(Vec::len).collect();
        let layout = Layouts::build(&diff, &review, &block_lens, &HashSet::new());
        let outdated = outdated_flags(&review, &layout.placed);
        let conv_order = conv_display_order(&review);
        let (conv_blocks, conv_comment_starts, conv_block_regions) = build_conversation(
            &review,
            &diff,
            CONV_DEFAULT_WIDTH,
            &highlighter,
            &outdated,
            &collapsed,
            repo_dir.as_deref(),
            &HashMap::new(),
            &RefCell::new(HashMap::new()),
        );
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
            submit: None,
            comment_blocks,
            conv_blocks,
            conv_comment_starts,
            conv_block_regions,
            conv_regions: RefCell::new(Vec::new()),
            conv_folds: HashMap::new(),
            conv_details: RefCell::new(Vec::new()),
            conv_effective: RefCell::new(HashMap::new()),
            thread_outdated: outdated,
            conv_order,
            conv_comment: 0,
            collapsed,
            manual_fold: HashSet::new(),
            collapsed_files: HashSet::new(),
            auto_collapse_files: 50,
            auto_collapse_lines: 20_000,
            view: View::Files,
            sidebar_mode: crate::config::SidebarMode::Auto,
            sidebar_override: None,
            sidebar_min_content: 44,
            sidebar_width_cfg: None,
            composer_enter: crate::config::ComposerEnter::Newline,
            focus: Focus::Body,
            sidebar_cursor: 0,
            sidebar_scroll: 0,
            finder: None,
            palette: None,
            thread_picker: None,
            forced_thread: None,
            selection: None,
            drag_anchor: None,
            hit: Cell::new(HitLayout::default()),
            header_pr_link: Cell::new(None),
            hscroll: 0,
            keymap: crate::keys::Keymap::defaults(),
            conv_cursor: 0,
            conv_scroll: 0,
            split_min_width: 160,
            confirming_close: false,
            confirming_delete: None,
            loading: None,
            load_error: None,
            job: None,
            pr: None,
            issue: None,
            pr_overview: None,
            overview_scroll: 0,
            overview_folds: HashMap::new(),
            overview_regions: RefCell::new(Vec::new()),
            overview_effective: RefCell::new(HashMap::new()),
            url_opener: Box::new(crate::opener::open_url),
            pr_key: None,
            repo_dir,
            source: None,
            session_id: String::new(),
            events: Arc::new(EventLog::new()),
            tick: 0,
            status: None,
            reloaded_at: None,
            quit: false,
        }
    }

    /// Spawn `loader` on a background thread and enter the loading state.
    fn start_loading(&mut self, loader: Loader) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let stage_tx = tx.clone();
            let progress = move |stage: &str| {
                let _ = stage_tx.send(LoadMsg::Stage(stage.to_string()));
            };
            let message = match loader(&progress) {
                Ok(loaded) => LoadMsg::Ready(Box::new(loaded)),
                Err(reason) => LoadMsg::Failed(reason),
            };
            let _ = tx.send(message);
        });
        self.loading = Some(Loading {
            stage: "loading…".to_string(),
            rx,
        });
    }

    /// Install a completed load: swap in the diff, threads, and PR handle.
    fn install_loaded(&mut self, loaded: Loaded) {
        self.label = loaded.label;
        self.review = loaded.review;
        self.normalize_resolved_drafts();
        self.normalize_conversation_reply_drafts();
        self.pr = loaded.pr.map(Arc::new);
        self.issue = loaded.issue.map(Arc::new);
        // The overview comes from whichever handle this load carried.
        self.pr_overview = self
            .pr
            .as_deref()
            .map(PrHandle::overview)
            .or_else(|| self.issue.as_deref().map(IssueHandle::overview));
        self.overview_scroll = 0;
        self.pr_key = loaded.pr_key;
        self.apply_layout(loaded.diff);
        self.cursor = 0;
        self.scroll = 0;
        self.conv_cursor = 0;
        self.conv_scroll = 0;
        self.loading = None;
        // An issue has no diff — open on its Overview (the description is the
        // content), not the (empty) Files view.
        if self.issue.is_some() {
            self.view = View::Overview;
        }
        if loaded.stale_cleaned > 0 {
            self.status = Some(format!(
                "cleaned {} stale draft(s) from an old build",
                loaded.stale_cleaned
            ));
        }
        // A large pull request opens collapsed (fast); this relays out again.
        self.maybe_auto_collapse();
    }

    /// Spawn a background action and show its spinner.
    fn start_job(&mut self, title: &str, worker: JobWorker) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let stage_tx = tx.clone();
            let progress = move |stage: &str| {
                let _ = stage_tx.send(JobMsg::Stage(stage.to_string()));
            };
            let outcome = worker(&progress);
            let _ = tx.send(JobMsg::Done(outcome));
        });
        self.job = Some(Job {
            title: title.to_string(),
            stage: "working…".to_string(),
            rx,
        });
    }

    /// Drain the job channel, applying the result when it finishes.
    fn poll_job(&mut self) {
        let Some(mut job) = self.job.take() else {
            return;
        };
        let mut outcome: Option<Result<JobOutcome, String>> = None;
        loop {
            match job.rx.try_recv() {
                Ok(JobMsg::Stage(stage)) => job.stage = stage,
                Ok(JobMsg::Done(result)) => {
                    outcome = Some(result);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    outcome = Some(Err("the action ended unexpectedly".to_string()));
                    break;
                }
            }
        }
        match outcome {
            Some(result) => self.apply_job(result),
            None => self.job = Some(job), // still running
        }
    }

    /// Apply a finished background action.
    fn apply_job(&mut self, result: Result<JobOutcome, String>) {
        match result {
            Ok(JobOutcome::Refreshed { threads, overview }) => {
                let (merged, cleaned, orphans) = crate::prsync::merge_drafts(&self.review, threads);
                self.review.threads = merged;
                // Follow a lifecycle transition (open → merged) or a description
                // edit; a failed metadata re-fetch (`None`) leaves the old values.
                if let Some(overview) = overview {
                    self.pr_overview = Some(*overview);
                }
                let mut notes = Vec::new();
                if cleaned > 0 {
                    notes.push(format!("cleaned {cleaned} stale draft(s)"));
                }
                if orphans > 0 {
                    notes.push(format!(
                        "{orphans} local note(s) removed — their thread was deleted on GitHub"
                    ));
                }
                self.status = Some(if notes.is_empty() {
                    "refreshed from GitHub".to_string()
                } else {
                    format!("refreshed from GitHub — {}", notes.join("; "))
                });
                self.relayout();
            }
            Ok(JobOutcome::Resolved {
                thread_id,
                resolved,
            }) => {
                let mut changed = None;
                if let Some(thread) = self.review.thread_mut(&thread_id) {
                    thread.state = if resolved {
                        ThreadState::Resolved
                    } else {
                        ThreadState::Open
                    };
                    changed = Some(thread_id);
                }
                // Fold on resolve / expand on reopen, via the default-fold pass.
                if let Some(id) = &changed {
                    self.manual_fold.remove(id);
                }
                self.emit(EventKind::Resolve, changed);
                self.status = Some(
                    if resolved {
                        "resolved on GitHub"
                    } else {
                        "reopened on GitHub"
                    }
                    .to_string(),
                );
                self.relayout();
            }
            Ok(JobOutcome::Submitted(submitted)) => self.apply_submitted(submitted),
            Ok(JobOutcome::Edited {
                thread_id,
                comment_id,
                body,
            }) => {
                if let Some(comment) = self
                    .review
                    .thread_mut(&thread_id)
                    .and_then(|t| t.comments.iter_mut().find(|c| c.id == comment_id))
                {
                    comment.body = body;
                }
                self.status = Some("edited on GitHub".to_string());
                self.relayout();
            }
            Ok(JobOutcome::Deleted {
                thread_id,
                comment_id,
            }) => {
                self.remove_comment_by_id(&thread_id, &comment_id);
                self.status = Some("deleted from GitHub".to_string());
                self.relayout();
            }
            Ok(JobOutcome::IssueSent(sent)) => {
                let n = sent.len();
                for (thread_id, remote_id) in sent {
                    if let Some(root) = self
                        .review
                        .thread_mut(&thread_id)
                        .and_then(|t| t.comments.first_mut())
                    {
                        root.remote_id = remote_id;
                        root.kind = CommentKind::Published;
                    }
                }
                // The posted roots are no longer drafts — drop them from the store.
                let _ = self.save_pr_drafts();
                self.status = Some(format!("sent {n} comment(s) to GitHub"));
                self.relayout();
            }
            Err(reason) => self.status = Some(format!("failed: {reason}")),
        }
    }

    /// Remove one comment (by ids) from the in-memory review, dropping its thread
    /// if it becomes empty. For a published comment already deleted on GitHub —
    /// the local store holds only drafts, so no store write is needed.
    fn remove_comment_by_id(&mut self, thread_id: &str, comment_id: &str) {
        let Some(ti) = self.review.threads.iter().position(|t| t.id == thread_id) else {
            return;
        };
        if let Some(ci) = self.review.threads[ti]
            .comments
            .iter()
            .position(|c| c.id == comment_id)
        {
            self.review.threads[ti].comments.remove(ci);
        }
        // The thread lives on only while a published comment anchors it. Once the
        // last published comment is gone, remove the whole thread — cascading the
        // local/draft notes that hung under it (and their store entries), so no
        // orphan reply is stranded under a root that no longer exists.
        let anchored = self.review.threads[ti]
            .comments
            .iter()
            .any(|c| c.is_published());
        if !anchored {
            self.review.threads.remove(ti);
            self.store_remove(thread_id, None);
        }
        self.conv_cursor = self
            .conv_cursor
            .min(self.review.threads.len().saturating_sub(1));
    }

    /// Re-pull the subject's threads (keeping local drafts) — a PR's or an
    /// issue's conversation, with a best-effort facts re-fetch for the badge.
    fn refresh(&mut self) {
        if let Some(pr) = self.pr.clone() {
            self.start_job(
                "Refreshing",
                Box::new(move |progress| {
                    progress("fetching comments…");
                    let threads = pr.pull()?;
                    // Best-effort: a metadata re-fetch failure keeps the facts.
                    let overview = pr.fetch_overview().ok().map(Box::new);
                    Ok(JobOutcome::Refreshed { threads, overview })
                }),
            );
        } else if let Some(issue) = self.issue.clone() {
            self.start_job(
                "Refreshing",
                Box::new(move |progress| {
                    progress("fetching comments…");
                    let threads = issue.pull()?;
                    let overview = issue.fetch_overview().ok().map(Box::new);
                    Ok(JobOutcome::Refreshed { threads, overview })
                }),
            );
        }
    }

    /// Post the issue's draft conversation comments (Ctrl-S on an issue). An issue
    /// has no review to batch into, so each unpublished draft root posts directly
    /// as an issue comment; replies stay local (the conversation is flat).
    fn send_issue_drafts(&mut self) {
        let Some(issue) = self.issue.clone() else {
            return;
        };
        let drafts: Vec<(String, String)> = self
            .review
            .threads
            .iter()
            .filter_map(|t| {
                let root = t.root()?;
                (root.disposition() == CommentKind::Draft && root.remote_id.is_none())
                    .then(|| (t.id.clone(), root.body.clone()))
            })
            .collect();
        if drafts.is_empty() {
            self.status = Some("no drafts to send".to_string());
            return;
        }
        self.start_job(
            "Sending",
            Box::new(move |progress| {
                let mut sent = Vec::new();
                for (thread_id, body) in &drafts {
                    progress("posting comment…");
                    let comment = issue.create_comment(body)?;
                    sent.push((thread_id.clone(), comment.remote_id));
                }
                Ok(JobOutcome::IssueSent(sent))
            }),
        );
    }

    /// Open the current position on github.com in the browser (`open_github`,
    /// default Ctrl-O). A pull request or an issue has a page; a plain local review
    /// has nowhere to go. On a PR the target is a deep link to the published
    /// comment under the Conversation cursor, else the PR page; an issue opens its
    /// page. A launcher that won't run falls back to printing the URL for the user
    /// to open by hand.
    fn open_github(&mut self) {
        let url = if let Some(pr) = self.pr.clone() {
            self.github_link(&pr)
        } else if let Some(issue) = &self.issue {
            // An issue's page (a comment deep-link isn't needed per the design).
            issue.url().to_string()
        } else {
            self.status = Some("no GitHub context here".to_string());
            return;
        };
        self.status = Some(match (self.url_opener)(&url) {
            Ok(()) => format!("opened {url}"),
            Err(_) => format!("open it yourself: {url}"),
        });
    }

    /// Open the subject's page (its `#N` header link, clicked). Reuses the same
    /// launcher as `open_github`; a plain review has no page.
    fn open_pr_page(&mut self) {
        let url = if let Some(pr) = self.pr.clone() {
            pr.url().to_string()
        } else if let Some(issue) = &self.issue {
            issue.url().to_string()
        } else {
            return;
        };
        self.status = Some(match (self.url_opener)(&url) {
            Ok(()) => format!("opened {url}"),
            Err(_) => format!("open it yourself: {url}"),
        });
    }

    /// The github.com URL for the current position: a deep link to the published
    /// comment the Conversation cursor rests on (built from its kept remote id via
    /// [`CommentEndpoint::anchor`]), or the PR page for anything else — a diff
    /// line, a draft/local note, or the Files view.
    fn github_link(&self, pr: &PrHandle) -> String {
        if self.view == View::Conversation
            && let Some((ti, ci)) = self.selected_comment()
            && let Some(comment) = self.review.threads[ti].comments.get(ci)
            && comment.is_published()
            && let Some(endpoint) =
                self.published_endpoint(&self.review.threads[ti].id, &comment.id)
        {
            return format!("{}{}", pr.url(), endpoint.anchor());
        }
        pr.url().to_string()
    }

    /// What a submit would actually post, matching the push plan: new inline
    /// threads, inline replies, and new conversation-comment roots — each a draft
    /// (local notes are never sent). Returns those three counts, the draft authors
    /// (most first), and whether any draft is by someone other than the submitting
    /// human. A conversation *reply* never posts (it stays local), so it is not
    /// counted.
    fn draft_summary(&self) -> DraftSummary {
        let mut new_inline = 0;
        let mut replies = 0;
        let mut conversation = 0;
        let mut authors: Vec<(String, usize)> = Vec::new();
        for thread in &self.review.threads {
            let review_anchored = thread.anchor == Anchor::Review;
            for (i, comment) in thread.comments.iter().enumerate() {
                if !comment.is_draft() {
                    continue;
                }
                // What actually posts: an inline root (a new review comment) or a
                // conversation root (a new PR comment); an inline reply; never a
                // conversation reply (it stays local).
                let sends = if i == 0 {
                    matches!(thread.anchor, Anchor::Line { .. }) || review_anchored
                } else {
                    !review_anchored
                };
                if !sends {
                    continue;
                }
                if i == 0 && review_anchored {
                    conversation += 1;
                } else if i == 0 {
                    new_inline += 1;
                } else {
                    replies += 1;
                }
                match authors.iter_mut().find(|(name, _)| *name == comment.author) {
                    Some(entry) => entry.1 += 1,
                    None => authors.push((comment.author.clone(), 1)),
                }
            }
        }
        authors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let foreign = authors.iter().any(|(name, _)| *name != self.author);
        DraftSummary {
            new_inline,
            replies,
            conversation,
            authors,
            foreign,
        }
    }

    /// Open the review-submission modal (pull requests only).
    fn open_submit(&mut self) {
        if self.pr.is_none() {
            return;
        }
        let summary = self.draft_summary();
        // Nothing to send — don't open an empty modal.
        if summary.total() == 0 {
            self.status = Some("no drafts to submit".to_string());
            return;
        }
        self.submit = Some(SubmitModal {
            selected: 0,
            body: TextArea::default(),
            new_count: summary.new_inline,
            reply_count: summary.replies,
            conversation_count: summary.conversation,
            authors: summary.authors,
            foreign: summary.foreign,
        });
    }

    /// Route a key while the submit modal is open.
    fn on_key_submit(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let Some(modal) = self.submit.as_mut() else {
            return;
        };
        // A send-only batch has no event to pick and no summary to type — it is a
        // plain confirmation, so only Esc and Ctrl-S act.
        let send_only = modal.is_send_only();
        match code {
            KeyCode::Esc => {
                self.submit = None;
                self.status = Some("submit cancelled".to_string());
            }
            KeyCode::Char('s') if ctrl => self.confirm_submit(),
            KeyCode::Up if !send_only => modal.selected = modal.selected.saturating_sub(1),
            KeyCode::Down if !send_only => {
                modal.selected = (modal.selected + 1).min(SUBMIT_EVENTS.len() - 1)
            }
            _ if ctrl || send_only => {}
            _ => modal.body.on_key(code),
        }
    }

    /// Submit the review in the background.
    fn confirm_submit(&mut self) {
        let Some(modal) = self.submit.take() else {
            return;
        };
        let Some(pr) = self.pr.clone() else {
            return;
        };
        // A send-only batch posts no review, so the event is moot — use Pending so
        // no empty review is attempted; otherwise honor the picked event.
        let event = if modal.is_send_only() {
            crate::prsync::SubmitEvent::Pending
        } else {
            SUBMIT_EVENTS[modal.selected].1
        };
        let body = modal.body.text().trim().to_string();
        let threads = self.review.threads.clone();
        self.start_job(
            "Submitting review",
            Box::new(move |progress| {
                progress("submitting review…");
                let submitted = pr
                    .submit(event, &body, &threads)
                    .map_err(friendly_github_write_error)?;
                Ok(JobOutcome::Submitted(submitted))
            }),
        );
    }

    /// Stamp remote ids from a submitted review onto the local threads.
    fn apply_submitted(&mut self, submitted: crate::prsync::Submitted) {
        // Some root ids weren't read back this round (a failed reconcile, or a POST
        // whose response didn't parse) — they publish under a pending sentinel and
        // recover their real id on the next pull.
        let pending_ids = submitted
            .published
            .iter()
            .any(|(_, id)| id == crate::prsync::PENDING_REMOTE_ID);
        for (thread_id, remote_id) in submitted.published {
            if let Some(thread) = self.review.thread_mut(&thread_id)
                && let Some(root) = thread.comments.first_mut()
            {
                root.remote_id = Some(remote_id);
            }
        }
        for stamp in submitted.replies {
            if let Some(thread) = self.review.thread_mut(&stamp.thread_id)
                && let Some(comment) = thread
                    .comments
                    .iter_mut()
                    .find(|c| c.id == stamp.comment_id)
            {
                comment.remote_id = Some(stamp.remote_id);
            }
        }
        // The just-published drafts are now remote: replace the store's draft set
        // with only what remains unpublished, so a repeat Ctrl-S finds nothing and
        // a re-pull won't duplicate them. Any reply that failed stays a draft here.
        if let (Some(store), Some(key)) = (&self.store, &self.pr_key) {
            let _ = store.replace_pr_drafts(key, &self.pr_drafts());
        }
        self.emit(EventKind::Submit, None);
        let plural = |n: usize| if n == 1 { "y" } else { "ies" };
        self.status = Some(if submitted.deferred_replies > 0 {
            // A reply couldn't attach because its root's id wasn't read back —
            // never silent: point at the two-step recovery.
            format!(
                "review submitted — {} repl{} kept as draft (root id not synced yet); refresh and submit again",
                submitted.deferred_replies,
                plural(submitted.deferred_replies)
            )
        } else if submitted.failed_replies > 0 {
            format!(
                "review submitted — {} repl{} failed, still draft",
                submitted.failed_replies,
                plural(submitted.failed_replies)
            )
        } else if pending_ids {
            // Posted, but some ids didn't come back — they reconcile on the next
            // pull, so a refresh (not a resubmit) completes it.
            "review posted — reconciling ids on the next refresh (Ctrl-R)".to_string()
        } else {
            "review submitted".to_string()
        });
        self.relayout();
    }

    /// The pull request's current draft threads — those still holding an
    /// unpublished comment.
    fn pr_drafts(&self) -> Review {
        Review {
            threads: self
                .review
                .threads
                .iter()
                .filter(|t| t.comments.iter().any(|c| c.remote_id.is_none()))
                .cloned()
                .collect(),
        }
    }

    /// Drain the load channel, updating the stage or finishing the load.
    fn poll_loading(&mut self) {
        // Take the state out so the stage can be mutated without aliasing.
        let Some(mut loading) = self.loading.take() else {
            return;
        };
        let mut outcome: Option<Result<Box<Loaded>, String>> = None;
        loop {
            match loading.rx.try_recv() {
                Ok(LoadMsg::Stage(stage)) => loading.stage = stage,
                Ok(LoadMsg::Ready(loaded)) => {
                    outcome = Some(Ok(loaded));
                    break;
                }
                Ok(LoadMsg::Failed(reason)) => {
                    outcome = Some(Err(reason));
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    outcome = Some(Err("the load ended unexpectedly".to_string()));
                    break;
                }
            }
        }
        match outcome {
            Some(Ok(loaded)) => self.install_loaded(*loaded),
            Some(Err(reason)) => self.load_error = Some(reason),
            None => self.loading = Some(loading), // still running
        }
    }

    /// Drain and apply control-plane requests against the running review. Reads
    /// and mutations both run here, on the UI thread, so an agent's comment or
    /// navigation lands in the same `App` the human is looking at and shows on
    /// the next repaint.
    fn poll_control(&mut self, control: Option<&Receiver<UiRequest>>) {
        let Some(rx) = control else {
            return;
        };
        while let Ok(request) = rx.try_recv() {
            let response = self.handle_control(request.request);
            let _ = request.reply.send(response);
        }
    }

    // -- event loop -------------------------------------------------------

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        updates: Option<Receiver<WatchMsg>>,
        control: Option<&Receiver<UiRequest>>,
    ) -> Result<()> {
        while !self.quit {
            self.tick = self.tick.wrapping_add(1);
            self.poll_loading();
            self.poll_job();
            self.poll_control(control);

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
                // Coalesce a burst: apply every event already queued (a held key
                // or a wheel spin can deliver many at once), then loop to draw
                // once. Without this, each event forces its own draw and input
                // backs up behind slow frames — the reported scroll lag.
                loop {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            self.on_key(key.code, key.modifiers);
                        }
                        Event::Mouse(mouse) => self.on_mouse(mouse),
                        Event::Paste(text) => self.on_paste(&text),
                        Event::Resize(cols, _) => self.on_resize(cols),
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Recompute the layout and inline comment blocks from `diff` and the
    /// current review, replacing derived state. The caller restores the cursor.
    /// Re-derive the default fold state (resolved = collapsed) for every thread
    /// the reviewer has not folded by hand — so a refresh/pull that brings new or
    /// newly-resolved threads folds them correctly.
    fn apply_default_folds(&mut self) {
        let updates: Vec<(String, bool)> = self
            .review
            .threads
            .iter()
            .filter(|t| !self.manual_fold.contains(&t.id))
            .map(|t| (t.id.clone(), t.is_resolved()))
            .collect();
        for (id, resolved) in updates {
            if resolved {
                self.collapsed.insert(id);
            } else {
                self.collapsed.remove(&id);
            }
        }
    }

    fn apply_layout(&mut self, diff: Diff) {
        self.apply_default_folds();
        self.comment_blocks =
            build_comment_blocks(&self.review, &self.highlighter, &self.collapsed);
        let block_lens: Vec<usize> = self.comment_blocks.iter().map(Vec::len).collect();
        let layout = Layouts::build(&diff, &self.review, &block_lens, &self.collapsed_files);
        self.thread_outdated = outdated_flags(&self.review, &layout.placed);
        let conv_width = self.body_width.get().clamp(40, 120);
        (
            self.conv_blocks,
            self.conv_comment_starts,
            self.conv_block_regions,
        ) = build_conversation(
            &self.review,
            &diff,
            conv_width,
            &self.highlighter,
            &self.thread_outdated,
            &self.collapsed,
            self.repo_dir.as_deref(),
            &self.conv_folds,
            &self.conv_effective,
        );
        self.conv_order = conv_display_order(&self.review);
        self.conv_cursor = self
            .conv_cursor
            .min(self.conv_order.len().saturating_sub(1));
        self.conv_comment = self.clamped_conv_comment();
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
        self.emit(EventKind::Reload, None);
    }

    /// Rebuild after the review changed (diff unchanged), keeping the cursor.
    fn relayout(&mut self) {
        let diff = std::mem::take(&mut self.diff);
        self.apply_layout(diff);
        self.cursor = self.cursor.min(self.clines.len().saturating_sub(1));
        self.follow_cursor();
    }

    /// Whether the cursor is resting on a file header (not a content line).
    fn cursor_is_header(&self) -> bool {
        self.clines
            .get(self.cursor)
            .is_some_and(|&(_, flat)| flat == HEADER)
    }

    /// The cursor's content line `(file, flat)`, or `None` when it is on a header.
    fn cursor_content(&self) -> Option<(usize, usize)> {
        match self.clines.get(self.cursor).copied() {
            Some((file, flat)) if flat != HEADER => Some((file, flat)),
            _ => None,
        }
    }

    /// The cursor's current line as a relocatable anchor (`None` on a header).
    fn current_anchor(&self) -> Option<CursorAnchor> {
        let (file, flat) = self.cursor_content()?;
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
            if flat == HEADER {
                return false;
            }
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

    /// Whether the review has any threads.
    fn has_review(&self) -> bool {
        !self.review.threads.is_empty()
    }

    /// Whether this session can take comments at all: a git-backed store (every
    /// repo-derived source — worktree, `lr diff <target>`, `lr show`) or a live
    /// pull request. A bare stdin/file patch carries neither, so it stays a
    /// lightweight pager with no comment surface.
    fn comments_enabled(&self) -> bool {
        self.store.is_some() || self.has_subject()
    }

    /// Whether this session reviews a GitHub subject — a pull request or an issue.
    fn has_subject(&self) -> bool {
        self.pr.is_some() || self.issue.is_some()
    }

    /// Whether this session reviews an issue (no diff; a flat conversation).
    fn is_issue(&self) -> bool {
        self.issue.is_some()
    }

    /// Whether the Conversation | Files tab structure is shown. A comment-capable
    /// session shows it always, comments or not — the Conversation tab's `c` is
    /// the only entry for a review-level (non-line) comment, so it must exist
    /// before the first one (otherwise there is no way to start it). A pure patch
    /// shows tabs only if it somehow already carries threads.
    fn shows_tabs(&self) -> bool {
        self.comments_enabled() || self.has_review()
    }

    /// Whether `view` is reachable right now: the Overview on a PR or an issue;
    /// Files on anything with a diff (an issue has none, so it drops the tab).
    fn view_available(&self, view: View) -> bool {
        match view {
            View::Overview => self.has_subject(),
            View::Files => !self.is_issue(),
            View::Conversation => true,
        }
    }

    /// The tabs currently shown, left to right (Overview on a pull request or an
    /// issue; Files on anything with a diff, so an issue drops it).
    fn visible_views(&self) -> Vec<View> {
        View::ORDER
            .iter()
            .copied()
            .filter(|&v| self.view_available(v))
            .collect()
    }

    /// The next (`forward`) or previous visible tab, wrapping around — what
    /// `Tab` / `Shift+Tab` step through.
    fn cycle_view(&self, forward: bool) -> View {
        let order = self.visible_views();
        if order.is_empty() {
            return self.view;
        }
        let n = order.len();
        let i = order.iter().position(|&v| v == self.view).unwrap_or(0);
        let delta = if forward { 1 } else { n - 1 };
        order[(i + delta) % n]
    }

    /// Switch the top-level view, re-syncing the sidebar so its index scroll
    /// tracks the new view's selection (the current file, or the selected
    /// thread). The Overview has no sidebar.
    fn set_view(&mut self, view: View) {
        self.view = view;
        match view {
            View::Conversation => self.reveal_in_sidebar(self.conv_cursor),
            View::Files => self.reveal_file_in_sidebar(self.current_file()),
            // The Overview has no sidebar; keep focus in the body pane.
            View::Overview => self.focus = Focus::Body,
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // While loading, a background job runs, or a load error shows, only quit
        // is accepted (the job keeps running until it reports back).
        if self.loading.is_some() || self.load_error.is_some() || self.job.is_some() {
            if matches!(code, KeyCode::Esc | KeyCode::Char('q'))
                || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
            {
                self.quit = true;
            }
            return;
        }
        // Modals take keys next: the composer, the submit modal, the finder, the
        // command palette (each prioritizes its own text input).
        if self.input.is_some() {
            self.on_key_compose(code, mods);
            return;
        }
        if self.submit.is_some() {
            self.on_key_submit(code, mods);
            return;
        }
        if self.finder.is_some() {
            self.on_key_finder(code, mods);
            return;
        }
        if self.thread_picker.is_some() {
            self.on_key_thread_picker(code, mods);
            return;
        }
        if self.palette.is_some() {
            self.on_key_palette(code, mods);
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
        // While confirming a delete: y/Enter removes, anything else cancels.
        if let Some(target) = self.confirming_delete.take() {
            if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
                self.confirm_delete(target);
            } else {
                self.status = Some("delete cancelled".to_string());
            }
            return;
        }
        let in_sidebar =
            self.focus == Focus::Sidebar && self.sidebar_width(self.body_width.get()).is_some();
        // Esc cancels a selection, or leaves the sidebar, before it can quit.
        if code == KeyCode::Esc {
            if self.selection.is_some() {
                self.clear_selection();
                self.status = Some("selection cleared".to_string());
                return;
            }
            if in_sidebar {
                self.focus = Focus::Body;
                return;
            }
        }
        // Structural keys (not remappable): quit and view switch.
        match (code, mods.contains(KeyModifiers::CONTROL)) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => {
                self.quit = true;
                return;
            }
            // Tab cycles the tabs forward; Shift+Tab cycles back — reaching it as
            // BackTab or as Tab carrying SHIFT, since terminals differ. Same gate
            // as forward: inert when there are no tabs.
            (KeyCode::Tab, _) if self.shows_tabs() => {
                let forward = !mods.contains(KeyModifiers::SHIFT);
                self.set_view(self.cycle_view(forward));
                return;
            }
            (KeyCode::BackTab, _) if self.shows_tabs() => {
                self.set_view(self.cycle_view(false));
                return;
            }
            _ => {}
        }
        self.status = None;

        // Enter is handled directly, not through the keymap. It toggles a fold on
        // the tree's header — a file header in the Files view, a thread's root in
        // the Conversation view — the common tree-UI convention (`h`/`l` are pure
        // movement, `o` also folds). In the sidebar it keeps its `NavIn` meaning
        // (activate the row); on a diff line or a conversation reply it does
        // nothing, the same no-op `NavIn` has there.
        if code == KeyCode::Enter {
            if in_sidebar {
                self.run_action(Action::NavIn);
            } else if self.view == View::Files && self.cursor_is_header() {
                self.set_file_collapsed(self.current_file(), None);
            } else if self.view == View::Conversation && self.conv_on_thread_header() {
                self.toggle_collapse_conv();
            }
            return;
        }

        // Resolve the remappable action and run it in the active context.
        let Some(action) = self.keymap.action(code, mods) else {
            return;
        };
        self.run_action(action);
    }

    /// Dispatch a remappable action through the current context — the shared path
    /// for a key press and for the command palette. Globals first, then the pane
    /// that has focus.
    fn run_action(&mut self, action: Action) {
        match action {
            Action::ToggleSidebar => return self.toggle_sidebar(),
            Action::FileFinder => return self.open_finder(),
            Action::Palette => return self.open_palette(),
            Action::Refresh if self.has_subject() => return self.refresh(),
            // A PR opens the submit modal (pick an event); an issue has no review,
            // so its drafts post directly.
            Action::Submit if self.pr.is_some() => return self.open_submit(),
            Action::Submit if self.issue.is_some() => return self.send_issue_drafts(),
            Action::OpenGithub => return self.open_github(),
            _ => {}
        }
        let in_sidebar =
            self.focus == Focus::Sidebar && self.sidebar_width(self.body_width.get()).is_some();
        if in_sidebar {
            self.sidebar_action(action);
        } else {
            match self.view {
                View::Overview => self.overview_action(action),
                View::Conversation => self.conversation_action(action),
                View::Files => self.files_action(action),
            }
        }
    }

    /// Route an action in the Overview tab — a read-only scroll pane, so only the
    /// movement keys do anything (comment actions are absent by design).
    fn overview_action(&mut self, action: Action) {
        let page = self.body_height.get().max(1);
        let max = self.overview_max_scroll();
        match action {
            Action::MoveDown => self.overview_scroll = (self.overview_scroll + 1).min(max),
            Action::MoveUp => self.overview_scroll = self.overview_scroll.saturating_sub(1),
            Action::Top => self.overview_scroll = 0,
            Action::Bottom => self.overview_scroll = max,
            Action::HalfPageDown | Action::PageDown => {
                self.overview_scroll = (self.overview_scroll + page / 2).min(max)
            }
            Action::HalfPageUp | Action::PageUp => {
                self.overview_scroll = self.overview_scroll.saturating_sub(page / 2)
            }
            _ => {}
        }
    }

    /// The Overview's maximum scroll offset (its rendered height past the pane).
    fn overview_max_scroll(&self) -> usize {
        let height = self.body_height.get().max(1);
        self.overview_lines(self.body_width.get())
            .len()
            .saturating_sub(height)
    }

    /// Whether `action` applies to the *exact* spot the cursor rests on — the
    /// single source the palette greys by and the footer is built from. Unlike a
    /// view-level check this looks at the target under the cursor: `r` only over a
    /// thread, `t` only over an unpublished comment, `e`/`d` only over your own
    /// editable/deletable one, `c` only on a diff line. The key press itself stays
    /// permissive (its handler reports the fine cases); this drives what is
    /// *shown* as useful right now.
    fn action_available(&self, action: Action) -> bool {
        use Action::*;
        match action {
            ToggleSidebar | Palette => return true,
            // The file finder needs files; an issue (no diff) has none.
            FileFinder => return !self.diff.files.is_empty(),
            Refresh | OpenGithub => return self.has_subject(),
            Submit => return self.has_subject(),
            _ => {}
        }
        let in_sidebar =
            self.focus == Focus::Sidebar && self.sidebar_width(self.body_width.get()).is_some();
        if in_sidebar {
            // The Conversation index also offers `d` — delete the selected thread
            // via its root — when that root is actually removable (yours to delete,
            // its id synced, etc.). The file index carries no comment actions.
            if action == Delete {
                return self.view == View::Conversation
                    && self
                        .selected_thread()
                        .is_some_and(|ti| self.delete_target_for(ti, 0).is_ok());
            }
            return matches!(action, MoveDown | MoveUp | Top | Bottom | NavIn | Fold);
        }
        // Body movement always applies. (`NavIn` is handled per-view below — in
        // Files it does something only on a header.)
        if matches!(
            action,
            MoveDown
                | MoveUp
                | HalfPageDown
                | HalfPageUp
                | PageDown
                | PageUp
                | Top
                | Bottom
                | NavOut
        ) {
            return true;
        }
        // The thread/comment the cursor points at (Conversation: the selected
        // comment; Files: the thread anchored at the cursor line's root).
        let target = self.edit_target();
        let target_unpublished_kind = || {
            self.has_subject()
                && target.is_some_and(|(ti, ci)| {
                    self.review.threads[ti]
                        .comments
                        .get(ci)
                        .is_some_and(|c| c.disposition() != CommentKind::Published)
                })
        };
        let common = |action: Action| match action {
            Reply => target.is_some(),
            Resolve => target.is_some_and(|(ti, _)| self.is_resolvable(ti)),
            Edit => self.can_edit_target(),
            Delete => self.selected_delete_target().is_some(),
            ToggleKind => target_unpublished_kind(),
            _ => false,
        };
        match self.view {
            // The Overview is read-only: only the movement keys (handled above)
            // and the globals (Ctrl-O / Ctrl-R) apply — no comment actions.
            View::Overview => false,
            View::Conversation => match action {
                NavIn => true,
                // `c` starts a new conversation comment (needs somewhere to keep it).
                Comment => self.comments_enabled(),
                Fold => !self.review.threads.is_empty(),
                CloseReview => self.has_review(),
                _ => common(action),
            },
            View::Files => match action {
                NextFile | PrevFile | NextHunk | PrevHunk | ScrollLeft | ScrollRight
                | ToggleLayout | Fold => !self.diff.files.is_empty(),
                // `l` (go in) only acts on a header — expand or step into the file.
                NavIn => !self.diff.files.is_empty() && self.cursor_is_header(),
                Comment | Select => !self.clines.is_empty() && !self.cursor_is_header(),
                // A suggestion replaces new-side lines, so it needs a new-side target.
                Suggest => self.cursor_targets_new_side(),
                _ => common(action),
            },
        }
    }

    /// Whether the comment the cursor targets is one the reviewer can edit — their
    /// own comment, and (if already published) one with an addressable GitHub id.
    /// The gate `start_edit` applies, factored out for `action_available`.
    fn can_edit_target(&self) -> bool {
        let Some((ti, ci)) = self.edit_target() else {
            return false;
        };
        let Some(comment) = self.review.threads[ti].comments.get(ci) else {
            return false;
        };
        if !self.comment_is_mine(comment) {
            return false;
        }
        if comment.is_published() {
            return self
                .published_endpoint(&self.review.threads[ti].id, &comment.id)
                .is_some();
        }
        true
    }

    fn files_action(&mut self, action: Action) {
        // When several threads cover the cursor line (overlapping range comments)
        // and the action targets a thread, disambiguate with a picker rather than
        // silently choosing — picking the wrong reply/delete target is too costly.
        // The `forced_thread` guard lets the picked action run straight through.
        if self.forced_thread.is_none()
            && matches!(
                action,
                Action::Reply
                    | Action::Resolve
                    | Action::Edit
                    | Action::Delete
                    | Action::ToggleKind
                    | Action::Fold
            )
        {
            let hits = self.threads_at_cursor();
            if hits.len() > 1 {
                self.open_thread_picker(hits, action);
                return;
            }
        }
        let page = self.body_height.get().max(1) as isize;
        match action {
            Action::MoveDown => self.move_cursor(1),
            Action::MoveUp => self.move_cursor(-1),
            Action::HalfPageDown => self.move_cursor(page / 2),
            Action::HalfPageUp => self.move_cursor(-page / 2),
            Action::PageDown => self.move_cursor(page - 1),
            Action::PageUp => self.move_cursor(-(page - 1)),
            Action::Top => self.set_cursor(0),
            Action::Bottom => self.set_cursor(self.clines.len().saturating_sub(1)),
            Action::NextFile => self.goto_file(1),
            Action::PrevFile => self.goto_file(-1),
            Action::NextHunk => self.goto_hunk(1),
            Action::PrevHunk => self.goto_hunk(-1),
            Action::ToggleLayout => self.toggle_mode(),
            Action::Comment => self.start_compose(),
            Action::Suggest => self.start_suggest(),
            Action::Reply => self.start_reply(),
            Action::Resolve => self.toggle_resolve(),
            Action::Fold => self.toggle_fold(),
            Action::NavIn => self.nav_in(),
            Action::NavOut => self.nav_out(),
            Action::ScrollLeft => self.hscroll_by(-HSCROLL_STEP),
            Action::ScrollRight => self.hscroll_by(HSCROLL_STEP),
            Action::Select => self.start_selection(),
            Action::Delete => self.request_delete(),
            Action::Edit => self.start_edit(),
            Action::ToggleKind => self.toggle_selected_kind(),
            _ => {}
        }
    }

    /// Arm the delete confirmation for the comment the cursor points at, or, when
    /// it can't be removed, say exactly why (rather than a silent no-op).
    fn request_delete(&mut self) {
        match self.delete_target() {
            Ok(target) => self.confirming_delete = Some(target),
            Err(reason) => self.status = Some(reason),
        }
    }

    /// Arm the delete confirmation for thread `ti`'s root — `d` from the
    /// Conversation index. Routes through the same target logic as `d` on a root
    /// elsewhere: the ownership gate, the cascade count, and the refusal reasons.
    fn request_delete_thread(&mut self, ti: usize) {
        match self.delete_target_for(ti, 0) {
            Ok(target) => self.confirming_delete = Some(target),
            Err(reason) => self.status = Some(reason),
        }
    }

    /// Carry out a confirmed delete: a published comment goes to GitHub in the
    /// background (then removed locally on success); an unpublished draft/local
    /// is removed in place.
    fn confirm_delete(&mut self, target: DeleteTarget) {
        match target.published {
            // A published comment always names a single comment; delete it on
            // GitHub, then remove it locally by id when the job succeeds.
            Some(endpoint) => {
                let Some(comment_id) = target.comment_id else {
                    return;
                };
                let thread_id = target.thread_id;
                // Route to whichever handle owns the published comment.
                if let Some(pr) = self.pr.clone() {
                    self.start_job(
                        "Deleting on GitHub",
                        Box::new(move |progress| {
                            progress("deleting comment…");
                            pr.delete_published(endpoint)
                                .map_err(friendly_github_write_error)?;
                            Ok(JobOutcome::Deleted {
                                thread_id,
                                comment_id,
                            })
                        }),
                    );
                } else if let Some(issue) = self.issue.clone() {
                    self.start_job(
                        "Deleting on GitHub",
                        Box::new(move |progress| {
                            progress("deleting comment…");
                            issue
                                .delete_published(endpoint)
                                .map_err(friendly_github_write_error)?;
                            Ok(JobOutcome::Deleted {
                                thread_id,
                                comment_id,
                            })
                        }),
                    );
                }
            }
            // Local removal: re-resolve the ids to fresh indices — the review may
            // have changed between arming and confirming (an agent event).
            None => {
                let Some(ti) = self
                    .review
                    .threads
                    .iter()
                    .position(|t| t.id == target.thread_id)
                else {
                    self.status = Some("the thread is gone".to_string());
                    return;
                };
                let is_reply = target.comment_id.is_some();
                let ci = match &target.comment_id {
                    Some(cid) => match self.review.threads[ti]
                        .comments
                        .iter()
                        .position(|c| c.id == *cid)
                    {
                        Some(ci) => Some(ci),
                        None => {
                            self.status = Some("the comment is gone".to_string());
                            return;
                        }
                    },
                    None => None,
                };
                self.remove_draft(ti, ci);
                self.status = Some(
                    if is_reply {
                        "reply removed"
                    } else {
                        "draft removed"
                    }
                    .to_string(),
                );
            }
        }
    }

    /// Whether `d` has something to remove here — the availability gate. See
    /// [`Self::delete_target`] for the reasons behind a refusal.
    fn selected_delete_target(&self) -> Option<DeleteTarget> {
        self.delete_target().ok()
    }

    /// What `d` removes, or a reason it can't. In the Conversation view the
    /// selected comment (a reply removes just itself; a draft root takes its whole
    /// thread); in Files the thread at the cursor. An unpublished draft/local is
    /// removed locally; your own published comment is deleted from GitHub (a single
    /// comment). Refusals name their cause so the key press is never a silent
    /// no-op: someone else's published comment, a review summary (no GitHub delete
    /// API), or a comment whose real id has not synced yet (a just-submitted one,
    /// recoverable by refreshing).
    fn delete_target(&self) -> Result<DeleteTarget, String> {
        let (ti, ci) = if self.view == View::Conversation {
            self.selected_comment().ok_or("no comment selected")?
        } else {
            (
                self.thread_at_cursor()
                    .ok_or("no comment on this line to remove")?,
                0,
            )
        };
        self.delete_target_for(ti, ci)
    }

    /// What deleting comment `ci` of thread `ti` removes, or a reason it can't —
    /// the shared core of [`Self::delete_target`], also called with `ci = 0` to
    /// target a thread's root from the Conversation index (`d` on an entry).
    fn delete_target_for(&self, ti: usize, ci: usize) -> Result<DeleteTarget, String> {
        let thread_id = self.review.threads[ti].id.clone();
        let comment = self.review.threads[ti]
            .comments
            .get(ci)
            .ok_or("no comment here")?;
        let comment_id = comment.id.clone();
        let published = comment.is_published();
        let mine = self.comment_is_mine(comment);
        if published {
            if !mine {
                // Not mine by local author — ownership hinges on the GitHub login;
                // if that is unknown (gh unreachable at load) say so rather than
                // flatly "not yours".
                if self.has_subject() && self.viewer().is_none() {
                    return Err(
                        "can't confirm your GitHub identity — check `gh auth login`".to_string()
                    );
                }
                return Err("you can only delete your own published comment".to_string());
            }
            let Some(endpoint) = self.published_endpoint(&thread_id, &comment_id) else {
                return Err(
                    "this comment has no synced API id — refresh (Ctrl-R); if that doesn't help, manage it on GitHub".to_string(),
                );
            };
            // A submitted review's summary has no GitHub delete.
            if !endpoint.is_deletable() {
                return Err("review summaries can't be deleted on GitHub".to_string());
            }
            // If removing this leaves no published comment, the whole thread goes
            // (its local notes with it) — count them so the confirmation can warn.
            let leaves_published = self.review.threads[ti]
                .comments
                .iter()
                .any(|c| c.id != comment_id && c.is_published());
            let also_removed = if leaves_published {
                0
            } else {
                self.review.threads[ti].comments.len() - 1
            };
            Ok(DeleteTarget {
                thread_id,
                comment_id: Some(comment_id),
                published: Some(endpoint),
                also_removed,
            })
        } else {
            // A draft root takes its whole thread (and any replies) with it; a
            // reply removes just itself.
            let also_removed = if ci == 0 {
                self.review.threads[ti].comments.len() - 1
            } else {
                0
            };
            Ok(DeleteTarget {
                comment_id: (ci != 0).then_some(comment_id),
                thread_id,
                published: None,
                also_removed,
            })
        }
    }

    /// Toggle the targeted comment between a local note and a draft (a human
    /// action, e.g. adopting an agent's note to send it). The target mirrors
    /// `e`/`d`: in the Conversation view the comment the cursor rests on (root or
    /// reply), in Files the thread's root. Only on a pull request, and only for an
    /// unpublished comment.
    ///
    /// The kind rules keep a thread coherent:
    /// - A reply may become a draft only under a draft or published root. Under a
    ///   local root the promotion is refused — the root must be promoted first, so
    ///   a queued reply never dangles above a note that is never sent.
    /// - Demoting a root back to local drags its draft replies down with it, so a
    ///   local thread never strands a queued draft reply.
    /// - A published comment is on the remote for good; its kind never changes.
    fn toggle_selected_kind(&mut self) {
        if !self.has_subject() {
            self.status = Some("local/draft applies only to a pull request or issue".to_string());
            return;
        }
        // Same targeting as edit/delete: the cursor's comment in Conversation, the
        // thread root in Files.
        let Some((ti, ci)) = self.edit_target() else {
            self.status = Some("no comment selected".to_string());
            return;
        };
        let Some(target) = self.review.threads[ti].comments.get(ci) else {
            return;
        };
        if target.disposition() == CommentKind::Published {
            self.status = Some("a published comment can't change kind".to_string());
            return;
        }
        // A reply under a conversation thread stays local — it never posts, so it
        // cannot be promoted to a draft. Point at the way to say it on GitHub.
        if ci != 0 && self.review.threads[ti].anchor == Anchor::Review {
            self.status = Some(
                "conversation replies stay local — post a new conversation comment (c) instead"
                    .to_string(),
            );
            return;
        }
        // Promoting a reply to a draft under a local root would queue a reply whose
        // root is never sent. Refuse and point at the root.
        if ci != 0
            && target.is_local()
            && self.review.threads[ti]
                .root()
                .is_some_and(|c| c.disposition() == CommentKind::Local)
        {
            self.status = Some("promote the thread root first (t on the root)".to_string());
            return;
        }
        let now_draft = {
            let comment = &mut self.review.threads[ti].comments[ci];
            comment.kind = if comment.kind == CommentKind::Local {
                CommentKind::Draft
            } else {
                CommentKind::Local
            };
            comment.kind == CommentKind::Draft
        };
        // Demoting a root to local pulls its draft replies down too; promoting a
        // root leaves the replies alone (they may stay local under a draft root).
        if ci == 0 && !now_draft {
            for reply in self.review.threads[ti].comments.iter_mut().skip(1) {
                if reply.disposition() == CommentKind::Draft {
                    reply.kind = CommentKind::Local;
                }
            }
        }
        let _ = self.persist(if now_draft {
            "queued as draft"
        } else {
            "kept local"
        });
        self.relayout();
        self.status = Some(
            if now_draft {
                "→ draft (will submit on Ctrl-S)"
            } else {
                "→ local (kept off GitHub)"
            }
            .to_string(),
        );
    }

    /// The comment `e` edits: in the Conversation view the comment the cursor
    /// rests on (root or reply); in Files the thread's root (the diff shows only
    /// the root inline).
    fn edit_target(&self) -> Option<(usize, usize)> {
        if self.view == View::Conversation {
            self.selected_comment()
        } else {
            self.thread_at_cursor().map(|ti| (ti, 0))
        }
    }

    /// Whether the reviewer may edit or delete `comment`: their own unpublished
    /// comment (matched by the local author name), or — on a pull request — their
    /// own published comment (matched by the GitHub viewer login, since a
    /// published comment's author is a GitHub login). Editing another author's
    /// comment would misattribute it, so it is never offered.
    fn comment_is_mine(&self, comment: &Comment) -> bool {
        // A comment is mine if I authored it locally (its author is my git
        // `user.name`) or GitHub attributes it to my login. The local-author check
        // is what makes a *just-submitted* comment editable: it publishes with my
        // git name still as the author — only the next pull rewrites that to my
        // login — so comparing against the login alone would disown my own comment
        // whenever my git name and GitHub login differ.
        if comment.author == self.author {
            return true;
        }
        self.viewer() == Some(comment.author.as_str())
    }

    /// The authenticated GitHub login for this session's subject (a PR or an
    /// issue), when known — for gating edits/deletes to the viewer's own comments.
    fn viewer(&self) -> Option<&str> {
        self.pr
            .as_deref()
            .and_then(|p| p.viewer())
            .or_else(|| self.issue.as_deref().and_then(|i| i.viewer()))
    }

    /// Open the composer to edit the targeted comment, pre-filled with its body.
    /// Your own comment only. A published comment is saved back to GitHub (see
    /// [`Self::submit_compose`]); an unpublished one is edited locally.
    fn start_edit(&mut self) {
        let Some((ti, ci)) = self.edit_target() else {
            self.status = Some("no comment selected".to_string());
            return;
        };
        let Some(comment) = self.review.threads[ti].comments.get(ci) else {
            return;
        };
        let published = comment.is_published();
        let mine = self.comment_is_mine(comment);
        let body = comment.body.clone();
        let thread_id = self.review.threads[ti].id.clone();
        let comment_id = comment.id.clone();

        if !mine {
            // For a published comment, ownership hinges on the GitHub login; if
            // that is unknown (gh unreachable at load) say so rather than implying
            // the comment isn't yours.
            if published && self.has_subject() && self.viewer().is_none() {
                self.status =
                    Some("can't confirm your GitHub identity — check `gh auth login`".to_string());
                return;
            }
            self.status = Some(
                if published {
                    "you can only edit your own published comment"
                } else {
                    "only your own comments can be edited"
                }
                .to_string(),
            );
            return;
        }
        // A published comment needs an addressable id on GitHub to edit (a pulled
        // comment can report a null databaseId).
        if published && self.published_endpoint(&thread_id, &comment_id).is_none() {
            self.status = Some("this comment has no synced API id — refresh (Ctrl-R); if that doesn't help, manage it on GitHub".to_string());
            return;
        }
        self.input = Some(Compose {
            area: TextArea::from_text(&body),
            kind: ComposeKind::Edit {
                thread: thread_id,
                comment: comment_id,
            },
            target: "edit comment".to_string(),
            confirming_discard: false,
            suggestion: false,
        });
    }

    /// Route a key while the comment composer is open.
    fn on_key_compose(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        // Read the Enter policy before borrowing `input` (so the Enter arm can
        // both consult it and touch the buffer without a borrow conflict).
        let enter_saves = self.composer_enter_saves();
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
            // Ctrl-S saves from inside the composer. (Submit — sending the whole
            // review to GitHub — is a Ctrl-S only when no composer is open.)
            KeyCode::Char('s') if ctrl => self.submit_compose(),
            // Enter inserts a newline by default (so multi-line comments and
            // suggestions always work); with `composer_enter = "save"` it saves,
            // and a modifier (Shift/Alt+Enter) makes the newline instead.
            KeyCode::Enter
                if enter_saves && !mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.submit_compose()
            }
            KeyCode::Enter => compose.area.on_key(KeyCode::Enter),
            // Other Ctrl combos do nothing here.
            _ if ctrl => {}
            _ => compose.area.on_key(code),
        }
    }

    /// Whether Enter saves the composer (the opt-in `composer_enter = "save"`),
    /// as opposed to inserting a newline (the default, where Ctrl-S saves).
    fn composer_enter_saves(&self) -> bool {
        self.composer_enter == crate::config::ComposerEnter::Save
    }

    /// What saving the composer does, phrased for the current context (used in
    /// the hint bar next to whichever key saves): a plain review has no
    /// local/draft split ("save comment"); on a GitHub subject (pull request or
    /// issue) the word tracks the resulting kind — "save draft" (queued to
    /// submit/send) or "save note" (local).
    fn compose_save_label(&self) -> &'static str {
        if !self.has_subject() {
            return "save comment";
        }
        let Some(compose) = self.input.as_ref() else {
            return "save";
        };
        let kind = match &compose.kind {
            ComposeKind::New(_) => self.human_new_kind(),
            ComposeKind::Reply(id) => self.reply_kind(id),
            ComposeKind::Edit { .. } => return "save edit",
        };
        if kind == CommentKind::Draft {
            "save draft"
        } else {
            "save note"
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
        if !self.comments_enabled() {
            self.status = Some("comments need a git repository or a pull request".to_string());
            return;
        }
        let Some((anchor, target)) = self.compose_target() else {
            // A file header (or an empty diff) has no line to anchor to — say so
            // and point at the two ways to comment, rather than a silent no-op.
            self.status = Some(
                "move to a diff line to comment, or press Tab for a conversation comment"
                    .to_string(),
            );
            return;
        };
        self.input = Some(Compose {
            area: TextArea::default(),
            kind: ComposeKind::New(anchor),
            target,
            confirming_discard: false,
            suggestion: false,
        });
        // The range is captured in the anchor; drop the visual selection.
        self.clear_selection();
    }

    /// Begin composing a new PR-conversation comment — a thread tied to nothing
    /// in the diff ([`Anchor::Review`]), the Conversation view's `c`. It saves
    /// through the same kind/submit flow as any comment: a draft on a pull
    /// request (sent as a new conversation comment), a local note otherwise.
    fn start_conversation_comment(&mut self) {
        if !self.comments_enabled() {
            self.status = Some("comments need a git repository or a pull request".to_string());
            return;
        }
        self.input = Some(Compose {
            area: TextArea::default(),
            kind: ComposeKind::New(Anchor::Review),
            target: "conversation".to_string(),
            confirming_discard: false,
            suggestion: false,
        });
    }

    /// Begin composing a GitHub suggested change on the cursor's line or the
    /// active selection. The composer opens with a ```suggestion block holding
    /// the lines' current new-side text, ready to be rewritten; from there it is
    /// an ordinary comment (same kind and submit flow). Suggestions replace the
    /// *new* side, so a pure-deletion (old-side) range is refused.
    fn start_suggest(&mut self) {
        if !self.comments_enabled() {
            self.status = Some("comments need a git repository or a pull request".to_string());
            return;
        }
        let Some((anchor, target)) = self.compose_target() else {
            return;
        };
        if !matches!(
            &anchor,
            Anchor::Line {
                side: Side::New,
                ..
            }
        ) {
            self.status = Some("suggestions apply to the new side".to_string());
            return;
        }
        let body = self.suggestion_body();
        self.input = Some(Compose {
            area: TextArea::from_text(&body),
            kind: ComposeKind::New(anchor),
            target,
            confirming_discard: false,
            suggestion: true,
        });
        self.clear_selection();
    }

    /// The pre-seeded body for a suggested change: a ```suggestion block holding
    /// the current new-side text of the cursor line or the selected range, which
    /// the reviewer rewrites into the change they want applied.
    fn suggestion_body(&self) -> String {
        let mut lines = Vec::new();
        if !self.clines.is_empty() {
            let (lo, hi) = self.selection_range().unwrap_or((self.cursor, self.cursor));
            let afile = self.clines[lo].0;
            for idx in lo..=hi {
                let (file, flat) = self.clines[idx];
                if file != afile || flat == HEADER {
                    continue;
                }
                let (h, l) = self.flats[file][flat];
                let line = &self.diff.files[file].hunks[h].lines[l];
                if line.new_lineno.is_some() {
                    lines.push(line.content.clone());
                }
            }
        }
        format!("```suggestion\n{}\n```\n", lines.join("\n"))
    }

    /// Whether the cursor (or active selection) points at new-side lines — where
    /// a suggestion can apply. Mirrors [`Self::compose_target`] choosing
    /// [`Side::New`], so the `s` affordance and the compose guard agree.
    fn cursor_targets_new_side(&self) -> bool {
        matches!(
            self.compose_target(),
            Some((
                Anchor::Line {
                    side: Side::New,
                    ..
                },
                _
            ))
        )
    }

    /// The anchor and label for a new comment: a line range when a selection is
    /// active, otherwise the single cursor line. Ranges are addressed on the new
    /// side when any selected line has a new-side number (a reviewer points at
    /// the after-state), falling back to the old side for a pure deletion.
    fn compose_target(&self) -> Option<(Anchor, String)> {
        if self.clines.is_empty() {
            return None;
        }
        // The range is the selection (clamped to its own file), else the cursor
        // line; the anchor file comes from the range, not the cursor (which may
        // have moved past the selection's file while extending).
        let (lo, hi) = self.selection_range().unwrap_or((self.cursor, self.cursor));
        let afile = self.clines[lo].0;
        let path = self.diff.files[afile].display_path().to_string();

        let mut new_nums = Vec::new();
        let mut old_nums = Vec::new();
        // The last content flat in the range, for the context snippet.
        let mut last_flat: Option<usize> = None;
        for idx in lo..=hi {
            let (file, flat) = self.clines[idx];
            if file != afile || flat == HEADER {
                continue; // skip other files and header stops
            }
            let (h, l) = self.flats[file][flat];
            let line = &self.diff.files[file].hunks[h].lines[l];
            if let Some(n) = line.new_lineno {
                new_nums.push(n);
            }
            if let Some(n) = line.old_lineno {
                old_nums.push(n);
            }
            last_flat = Some(flat);
        }
        let (side, start, end) = if !new_nums.is_empty() {
            (
                Side::New,
                *new_nums.iter().min().unwrap(),
                *new_nums.iter().max().unwrap(),
            )
        } else if !old_nums.is_empty() {
            (
                Side::Old,
                *old_nums.iter().min().unwrap(),
                *old_nums.iter().max().unwrap(),
            )
        } else {
            return None; // nothing but headers selected
        };
        let commit = if side == Side::New {
            self.diff.provenance.head.clone()
        } else {
            self.diff.provenance.base.clone()
        };
        // Context snippet from the range's last content line (the display anchor).
        let context = last_flat
            .map(|flat| {
                let (h, l) = self.flats[afile][flat];
                context_snippet(&self.diff.files[afile].hunks[h], l)
            })
            .unwrap_or_default();
        let target = if start == end {
            format!("{path}:{start}")
        } else {
            format!("{path}:{start}-{end}")
        };
        let anchor = Anchor::Line {
            file: path,
            side,
            start,
            end,
            commit,
            context,
        };
        Some((anchor, target))
    }

    /// Begin (or, if already active, cancel) a line-range selection at the cursor.
    fn start_selection(&mut self) {
        if self.selection.is_some() {
            self.clear_selection();
            self.status = Some("selection cleared".to_string());
            return;
        }
        if self.cursor_is_header() {
            self.status = Some("move to a line to select a range".to_string());
            return;
        }
        self.selection = Some(self.cursor);
        self.status =
            Some("visual line — j/k extend · c comment · s suggest · Esc cancel".to_string());
    }

    /// Clear any line-range selection.
    fn clear_selection(&mut self) {
        self.selection = None;
        self.drag_anchor = None;
    }

    /// The selected cursor-line range `(lo, hi)`, clamped to the file the
    /// selection started in (selections stay within one file).
    fn selection_range(&self) -> Option<(usize, usize)> {
        let sel = self.selection?;
        if self.clines.is_empty() || sel >= self.clines.len() {
            return None;
        }
        let afile = self.clines[sel].0;
        let mut lo = sel.min(self.cursor);
        let mut hi = sel.max(self.cursor).min(self.clines.len() - 1);
        while lo < hi && self.clines[lo].0 != afile {
            lo += 1;
        }
        while hi > lo && self.clines[hi].0 != afile {
            hi -= 1;
        }
        (self.clines[lo].0 == afile).then_some((lo, hi))
    }

    /// Whether the line `(file, flat)` is inside the current range selection.
    fn in_selection(&self, file: usize, flat: usize) -> bool {
        let Some((lo, hi)) = self.selection_range() else {
            return false;
        };
        self.cline_index
            .get(&(file, flat))
            .is_some_and(|&idx| lo <= idx && idx <= hi)
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
            suggestion: false,
        });
    }

    /// Toggle the resolved state of the thread anchored at the cursor's line.
    fn toggle_resolve(&mut self) {
        match self.thread_at_cursor() {
            Some(idx) => self.resolve_thread(idx),
            None => self.status = Some("no comment on this line to resolve".to_string()),
        }
    }

    /// Whether thread `idx` can be resolved. Local reviews resolve everything (a
    /// local concept). On a pull request:
    /// - conversation comments (issue comments and review bodies, anchored at
    ///   [`Anchor::Review`]) have no resolve affordance on GitHub;
    /// - a **draft** thread (root queued to submit, not yet published) can't be
    ///   resolved either — resolving folds something still scheduled to send, so
    ///   the only coherent moves are to demote it to a local note or delete it.
    ///
    /// Local notes and published threads on a pull request stay resolvable.
    fn is_resolvable(&self, idx: usize) -> bool {
        let Some(thread) = self.review.threads.get(idx) else {
            return false;
        };
        // A plain local review resolves everything (a local concept).
        if !self.has_subject() {
            return true;
        }
        // An issue's threads are all conversation comments: a local one resolves
        // locally; a published one has no GitHub resolve (issues can't resolve).
        if self.is_issue() {
            return !thread.root().is_some_and(|c| c.remote_id.is_some());
        }
        if matches!(thread.anchor, Anchor::Review) {
            return false;
        }
        !thread
            .root()
            .is_some_and(|c| c.disposition() == CommentKind::Draft)
    }

    /// Toggle the resolved state of thread `idx`. A published thread in a PR
    /// syncs to GitHub in the background; a local thread just toggles and saves.
    fn resolve_thread(&mut self, idx: usize) {
        if !self.is_resolvable(idx) {
            let draft = self
                .review
                .threads
                .get(idx)
                .and_then(|t| t.root())
                .is_some_and(|c| c.disposition() == CommentKind::Draft);
            self.status = Some(
                if draft {
                    "a draft can't be resolved — demote it to local (t) or delete it (d)"
                } else {
                    "conversation comments can't be resolved on GitHub"
                }
                .to_string(),
            );
            return;
        }
        let thread = &self.review.threads[idx];
        let published = thread.root().is_some_and(|c| c.remote_id.is_some());
        let want_resolved = !thread.is_resolved();
        if published && let Some(pr) = self.pr.clone() {
            let node_id = thread.id.clone();
            self.start_job(
                "Syncing resolution",
                Box::new(move |progress| {
                    progress(if want_resolved {
                        "resolving on GitHub…"
                    } else {
                        "reopening on GitHub…"
                    });
                    pr.set_resolved(&node_id, want_resolved)?;
                    Ok(JobOutcome::Resolved {
                        thread_id: node_id,
                        resolved: want_resolved,
                    })
                }),
            );
            return;
        }
        let id = self.review.threads[idx].id.clone();
        let thread = &mut self.review.threads[idx];
        thread.state = if thread.is_resolved() {
            ThreadState::Open
        } else {
            ThreadState::Resolved
        };
        let resolved = thread.is_resolved();
        // Resolving folds the thread, reopening expands it: clear any manual
        // override so the relayout's default-fold pass follows the new state.
        self.manual_fold.remove(&id);
        self.emit(EventKind::Resolve, Some(id));
        self.status = self.persist(if resolved { "resolved" } else { "reopened" });
        self.relayout();
    }

    /// Reopen any thread that is both a draft and marked resolved — a
    /// contradiction (resolving folds work still queued to submit) that the guards
    /// now prevent, but an older store may carry. Run at load so no stale
    /// resolved-draft state is ever carried into a session.
    fn normalize_resolved_drafts(&mut self) {
        for thread in &mut self.review.threads {
            let contradiction = thread.is_resolved()
                && thread
                    .root()
                    .is_some_and(|c| c.disposition() == CommentKind::Draft);
            if contradiction {
                thread.state = ThreadState::Open;
            }
        }
    }

    /// Demote to local any draft reply under a conversation thread. Conversation
    /// replies never post (a conversation is flat on GitHub), so a draft one is a
    /// contradiction the current guards prevent but an older store may carry.
    fn normalize_conversation_reply_drafts(&mut self) {
        for thread in &mut self.review.threads {
            if thread.anchor != Anchor::Review {
                continue;
            }
            for reply in thread.comments.iter_mut().skip(1) {
                if reply.disposition() == CommentKind::Draft {
                    reply.kind = CommentKind::Local;
                }
            }
        }
    }

    /// `o` in the Files view, context-dependent: fold the comment thread at the
    /// cursor line if there is one; otherwise toggle the current file's collapse
    /// (leaving the cursor on its header).
    fn toggle_fold(&mut self) {
        if let Some(idx) = self.thread_at_cursor() {
            let id = self.review.threads[idx].id.clone();
            self.toggle_collapse(id);
            return;
        }
        self.set_file_collapsed(self.current_file(), None);
    }

    /// Collapse, expand, or (with `None`) toggle `file`'s fold, relaying out and
    /// leaving the cursor on the file's header. Returns the new collapsed state.
    fn set_file_collapsed(&mut self, file: usize, collapse: Option<bool>) -> bool {
        let Some(path) = self
            .diff
            .files
            .get(file)
            .map(|f| f.display_path().to_string())
        else {
            return false;
        };
        let now = collapse.unwrap_or(!self.collapsed_files.contains(&path));
        if now {
            self.collapsed_files.insert(path.clone());
            self.status = Some(format!("collapsed {}", file_name(&path)));
        } else {
            self.collapsed_files.remove(&path);
            self.status = Some(format!("expanded {}", file_name(&path)));
        }
        self.relayout_to_file_header(file);
        now
    }

    /// Rebuild the layout (folds changed) and put the cursor on `file`'s header.
    fn relayout_to_file_header(&mut self, file: usize) {
        let diff = std::mem::take(&mut self.diff);
        self.apply_layout(diff);
        let cursor = self
            .file_first
            .get(file)
            .copied()
            .flatten()
            .unwrap_or(self.cursor);
        self.cursor = cursor.min(self.clines.len().saturating_sub(1));
        self.follow_cursor();
    }

    /// The cursor index of `file`'s first content line, if it has one (a folded
    /// or empty file has none).
    fn file_first_line(&self, file: usize) -> Option<usize> {
        let start = self.file_first.get(file).copied().flatten()?;
        (start + 1..self.clines.len()).find(|&i| {
            let (f, flat) = self.clines[i];
            f == file && flat != HEADER
        })
    }

    /// `l` in the body: go one level in. A collapsed header expands; an expanded
    /// header moves the cursor to the file's first line; on a line it is a no-op.
    fn nav_in(&mut self) {
        if !self.cursor_is_header() {
            return;
        }
        let file = self.current_file();
        let collapsed = self
            .diff
            .files
            .get(file)
            .is_some_and(|f| self.collapsed_files.contains(f.display_path()));
        if collapsed {
            self.set_file_collapsed(file, Some(false));
        } else if let Some(line) = self.file_first_line(file) {
            self.hscroll = 0; // entering a file resets horizontal scroll
            self.set_cursor(line);
        }
    }

    /// `h` in the body: step one level out, in a hierarchy (nvim-tree style) —
    /// pure movement, no folding (folding is Enter's / `o`'s job now). On a line,
    /// jump to the file's own header; on a header (expanded or collapsed), move
    /// focus to the sidebar when it is showing. `b` is the direct jump to the
    /// sidebar for when the cascade is more than you want.
    fn nav_out(&mut self) {
        let file = self.current_file();
        if !self.cursor_is_header() {
            if let Some(header) = self.file_first.get(file).copied().flatten() {
                self.set_cursor(header);
            }
            return;
        }
        if self.sidebar_width(self.body_width.get()).is_some() {
            self.focus_sidebar();
        }
    }

    /// Collapse every file when the diff is large (above the configured limits),
    /// so a big review opens fast; the reviewer expands what they want to read.
    fn maybe_auto_collapse(&mut self) {
        let files = self.diff.files.len();
        let lines: usize = self
            .diff
            .files
            .iter()
            .map(|f| {
                let (a, r) = f.line_stats();
                (a + r) as usize
            })
            .sum();
        if files > self.auto_collapse_files || lines > self.auto_collapse_lines {
            let paths: Vec<String> = self
                .diff
                .files
                .iter()
                .map(|f| f.display_path().to_string())
                .collect();
            for path in paths {
                self.collapsed_files.insert(path);
            }
            if self.status.is_none() {
                self.status = Some(format!(
                    "large diff — {files} files collapsed (o to expand)"
                ));
            }
            self.relayout();
        }
    }

    // -- file explorer (sidebar + finder) -----------------------------------

    /// The changed files, in diff order, for the sidebar and the finder.
    fn file_entries(&self) -> Vec<FileEntry> {
        self.diff
            .files
            .iter()
            .enumerate()
            .map(|(index, f)| {
                let (added, removed) = f.line_stats();
                let path = f.display_path().to_string();
                FileEntry {
                    index,
                    comments: self.file_comment_count(&path),
                    collapsed: self.collapsed_files.contains(&path),
                    status: f.status,
                    path,
                    added,
                    removed,
                }
            })
            .collect()
    }

    /// Jump the diff view to `file`: expand it if collapsed, move the cursor to
    /// its first line, and return focus to the body.
    fn jump_to_file(&mut self, file: usize) {
        if let Some(path) = self
            .diff
            .files
            .get(file)
            .map(|f| f.display_path().to_string())
            && self.collapsed_files.remove(&path)
        {
            let diff = std::mem::take(&mut self.diff);
            self.apply_layout(diff);
        }
        self.view = View::Files;
        self.focus = Focus::Body;
        self.sidebar_cursor = file;
        self.hscroll = 0; // a file jump resets horizontal scroll
        // Land on the file's first content line (its header if it has none).
        let target = self
            .file_first_line(file)
            .or_else(|| self.file_first.get(file).copied().flatten());
        if let Some(cursor) = target {
            self.cursor = cursor.min(self.clines.len().saturating_sub(1));
            self.follow_cursor();
        }
    }

    /// Jump the diff view to `file`'s header: expand it if collapsed, land the
    /// cursor on the header row, and move focus to the body. The sidebar activate
    /// (click / `l` / Enter) uses this — it navigates but never folds, so landing
    /// on the header means a second Enter (the diff pane's header-fold) collapses
    /// the file, keeping the old toggle as a predictable two-key sequence.
    fn jump_to_file_header(&mut self, file: usize) {
        if file >= self.diff.files.len() {
            return;
        }
        if let Some(path) = self
            .diff
            .files
            .get(file)
            .map(|f| f.display_path().to_string())
            && self.collapsed_files.remove(&path)
        {
            let diff = std::mem::take(&mut self.diff);
            self.apply_layout(diff);
        }
        self.view = View::Files;
        self.focus = Focus::Body;
        self.sidebar_cursor = file;
        self.hscroll = 0; // a file jump resets horizontal scroll
        if let Some(header) = self.file_first.get(file).copied().flatten() {
            self.cursor = header.min(self.clines.len().saturating_sub(1));
            self.follow_cursor();
        }
    }

    /// `b`: hidden → show and focus; visible & body-focused → focus it; visible &
    /// focused → hide. The override is temporary — a resize re-evaluates the mode.
    fn toggle_sidebar(&mut self) {
        let visible = self.sidebar_width(self.body_width.get()).is_some();
        if visible && self.focus == Focus::Sidebar {
            self.sidebar_override = Some(false);
            self.focus = Focus::Body;
        } else if visible {
            self.focus_sidebar();
        } else {
            self.sidebar_override = Some(true);
            if self.sidebar_width(self.body_width.get()).is_some() {
                self.focus_sidebar();
            }
        }
    }

    /// Move focus into the sidebar, synced to the right pane's selection: the
    /// current file (Files view) or the selected thread (Conversation view).
    fn focus_sidebar(&mut self) {
        self.focus = Focus::Sidebar;
        if self.view == View::Conversation {
            self.reveal_in_sidebar(self.conv_cursor);
        } else {
            self.sidebar_cursor = self.current_file();
            self.follow_sidebar();
        }
    }

    /// Keep the sidebar selection visible.
    fn follow_sidebar(&mut self) {
        let files = self.diff.files.len();
        self.sidebar_cursor = self.sidebar_cursor.min(files.saturating_sub(1));
        self.reveal_file_in_sidebar(self.sidebar_cursor);
    }

    /// Scroll the file sidebar so `file`'s display row (headers shift it) is in
    /// view. The thread index has one row per thread and reveals directly.
    fn reveal_file_in_sidebar(&mut self, file: usize) {
        let rows = sidebar_rows(&self.file_entries());
        if let Some(row) = row_of_file(&rows, file) {
            self.reveal_in_sidebar(row);
        }
    }

    /// Scroll the sidebar's viewport so display `row` is within it.
    fn reveal_in_sidebar(&mut self, row: usize) {
        let height = self.body_height.get().max(1);
        if row < self.sidebar_scroll {
            self.sidebar_scroll = row;
        } else if row >= self.sidebar_scroll + height {
            self.sidebar_scroll = row + 1 - height;
        }
    }

    /// Scroll the diff pane to preview `file` while the sidebar keeps focus. The
    /// sidebar is a table of contents, so moving its selection (j/k, g/G) brings
    /// the file's header to the top of the diff viewport — mirroring how the
    /// Conversation index already tracks the selected thread in the right pane.
    /// This is a peek only: it never moves the body cursor or steals focus.
    /// `l`/Enter is the confirm-move that commits the cursor into the body.
    fn preview_file_in_diff(&mut self, file: usize) {
        if self.view != View::Files || self.clines.is_empty() {
            return;
        }
        let Some(header) = self.file_first.get(file).copied().flatten() else {
            return;
        };
        let row = if self.sbs() {
            self.line_srow[header]
        } else {
            self.line_urow[header]
        };
        let height = self.body_height.get().max(1);
        let max = self.rows_len().saturating_sub(height);
        self.scroll = row.min(max);
    }

    /// Scroll the sidebar list under the wheel (independent of the selection),
    /// clamped so the last row stays reachable. Rows count display rows — file
    /// rows plus directory headers, or the threads in the Conversation index.
    fn scroll_sidebar(&mut self, delta: isize) {
        let total = if self.view == View::Conversation {
            self.conv_order.len()
        } else {
            sidebar_rows(&self.file_entries()).len()
        };
        let height = self.body_height.get().max(1);
        let max = total.saturating_sub(height) as isize;
        self.sidebar_scroll = (self.sidebar_scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Route a key while the sidebar has focus. In the Conversation view the
    /// sidebar drives the thread index instead of the file index.
    fn sidebar_action(&mut self, action: Action) {
        if self.view == View::Conversation {
            self.thread_index_action(action);
            return;
        }
        match action {
            // j/k step through file rows in display order (skipping the directory
            // headers, which are not selectable).
            Action::MoveDown => self.move_sidebar_file(1),
            Action::MoveUp => self.move_sidebar_file(-1),
            Action::Top => self.jump_sidebar_file(SidebarEnd::First),
            Action::Bottom => self.jump_sidebar_file(SidebarEnd::Last),
            // `l` / Enter jump to the file (expanding it if collapsed) and hand
            // focus to the body — navigate only, never fold. `o` is a pure fold
            // toggle; `h` is a no-op (the outermost level).
            Action::NavIn => self.sidebar_activate(self.sidebar_cursor),
            Action::Fold => self.toggle_fold_at(self.sidebar_cursor),
            _ => {}
        }
    }

    /// Move the sidebar cursor by `delta` file rows in display order (directory
    /// headers are skipped).
    fn move_sidebar_file(&mut self, delta: isize) {
        let order = sidebar_file_order(&sidebar_rows(&self.file_entries()));
        if order.is_empty() {
            return;
        }
        let pos = order
            .iter()
            .position(|&f| f == self.sidebar_cursor)
            .unwrap_or(0);
        let next = (pos as isize + delta).clamp(0, order.len() as isize - 1) as usize;
        self.sidebar_cursor = order[next];
        self.follow_sidebar();
        self.preview_file_in_diff(self.sidebar_cursor);
    }

    /// Jump the sidebar cursor to the first/last file row in display order.
    fn jump_sidebar_file(&mut self, end: SidebarEnd) {
        let order = sidebar_file_order(&sidebar_rows(&self.file_entries()));
        let target = match end {
            SidebarEnd::First => order.first(),
            SidebarEnd::Last => order.last(),
        };
        if let Some(&file) = target {
            self.sidebar_cursor = file;
            self.follow_sidebar();
            self.preview_file_in_diff(file);
        }
    }

    /// Activate a file from the sidebar (`l` / Enter / click): navigate to it —
    /// jump/confirm only, never fold. A table-of-contents entry navigates (the
    /// GitHub file-tree / VS Code standard), it does not toggle: a collapsed file
    /// expands, an open file is simply jumped to; either way the cursor lands on
    /// the file's header and focus moves to the body. Folding an open file is the
    /// diff pane's Enter on that header — reachable as a second Enter after this.
    fn sidebar_activate(&mut self, file: usize) {
        self.jump_to_file_header(file);
    }

    /// Route a key while the thread index (Conversation sidebar) has focus. The
    /// grammar mirrors the file index: j/k select, l/Enter jump into the thread,
    /// o folds it; `d` removes the selected thread via its root (the only
    /// comment action the index carries).
    fn thread_index_action(&mut self, action: Action) {
        let n = self.review.threads.len();
        if n == 0 {
            return;
        }
        match action {
            Action::MoveDown => self.set_conv((self.conv_cursor + 1).min(n - 1)),
            Action::MoveUp => self.set_conv(self.conv_cursor.saturating_sub(1)),
            Action::Top => self.set_conv(0),
            Action::Bottom => self.set_conv(n - 1),
            Action::NavIn => self.jump_to_thread(self.conv_cursor),
            Action::Fold => self.toggle_collapse_conv(),
            Action::Delete => {
                if let Some(ti) = self.selected_thread() {
                    self.request_delete_thread(ti);
                }
            }
            _ => {}
        }
    }

    /// Jump the Conversation view to the thread at display position `pos` and
    /// move focus to the body pane so the reviewer can read and reply (the
    /// sidebar analogue of `jump_to_file`).
    fn jump_to_thread(&mut self, pos: usize) {
        if pos >= self.conv_order.len() {
            return;
        }
        self.view = View::Conversation;
        self.set_conv(pos);
        self.focus = Focus::Body;
    }

    /// Toggle a file's collapse from the sidebar (does not move the body cursor).
    fn toggle_fold_at(&mut self, file: usize) {
        let Some(path) = self
            .diff
            .files
            .get(file)
            .map(|f| f.display_path().to_string())
        else {
            return;
        };
        if !self.collapsed_files.remove(&path) {
            self.collapsed_files.insert(path);
        }
        let diff = std::mem::take(&mut self.diff);
        self.apply_layout(diff);
        self.cursor = self.cursor.min(self.clines.len().saturating_sub(1));
    }

    /// Open the fuzzy file finder (Ctrl-P).
    fn open_finder(&mut self) {
        if self.diff.files.is_empty() {
            return;
        }
        let entries = self.file_entries();
        let matches = fuzzy_files(&entries, "");
        self.finder = Some(Finder {
            query: String::new(),
            matches,
            selected: 0,
        });
    }

    /// Route a key while the finder overlay is open.
    fn on_key_finder(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.finder = None,
            KeyCode::Enter => {
                let target = self
                    .finder
                    .as_ref()
                    .and_then(|f| f.matches.get(f.selected).map(|&(i, _)| i));
                self.finder = None;
                if let Some(file) = target {
                    self.jump_to_file(file);
                }
            }
            KeyCode::Down => self.move_finder(1),
            KeyCode::Up => self.move_finder(-1),
            KeyCode::Char('n') if ctrl => self.move_finder(1),
            KeyCode::Char('p') if ctrl => self.move_finder(-1),
            KeyCode::Backspace => {
                if let Some(f) = self.finder.as_mut() {
                    f.query.pop();
                }
                self.refilter_finder();
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(f) = self.finder.as_mut() {
                    f.query.push(c);
                }
                self.refilter_finder();
            }
            _ => {}
        }
    }

    /// Move the finder selection, clamped.
    fn move_finder(&mut self, delta: isize) {
        if let Some(f) = self.finder.as_mut() {
            let last = f.matches.len().saturating_sub(1) as isize;
            f.selected = (f.selected as isize + delta).clamp(0, last) as usize;
        }
    }

    /// Recompute the finder's matches after the query changed.
    fn refilter_finder(&mut self) {
        let entries = self.file_entries();
        if let Some(f) = self.finder.as_mut() {
            f.matches = fuzzy_files(&entries, &f.query);
            f.selected = f.selected.min(f.matches.len().saturating_sub(1));
        }
    }

    /// Open the `?` command palette: every action, searchable, available-first.
    fn open_palette(&mut self) {
        let available = self.action_availability();
        self.palette = Some(Palette {
            query: String::new(),
            matches: fuzzy_actions("", &available),
            selected: 0,
        });
    }

    /// The availability of every action (index-aligned to [`Action::ALL`]) in the
    /// current context.
    fn action_availability(&self) -> Vec<bool> {
        Action::ALL
            .iter()
            .map(|&a| self.action_available(a))
            .collect()
    }

    /// Route a key while the command palette is open.
    fn on_key_palette(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Enter => {
                let action = self
                    .palette
                    .as_ref()
                    .and_then(|p| p.matches.get(p.selected).copied())
                    .map(|i| Action::ALL[i]);
                self.palette = None;
                if let Some(action) = action {
                    if self.action_available(action) {
                        self.run_action(action);
                    } else {
                        self.status =
                            Some(format!("{} isn't available here", action.config_name()));
                    }
                }
            }
            KeyCode::Down => self.move_palette(1),
            KeyCode::Up => self.move_palette(-1),
            KeyCode::Char('n') if ctrl => self.move_palette(1),
            KeyCode::Char('p') if ctrl => self.move_palette(-1),
            KeyCode::Backspace => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.pop();
                }
                self.refilter_palette();
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.push(c);
                }
                self.refilter_palette();
            }
            _ => {}
        }
    }

    /// Move the palette selection, clamped.
    fn move_palette(&mut self, delta: isize) {
        if let Some(p) = self.palette.as_mut() {
            let last = p.matches.len().saturating_sub(1) as isize;
            p.selected = (p.selected as isize + delta).clamp(0, last) as usize;
        }
    }

    /// Recompute the palette's matches after the query changed.
    fn refilter_palette(&mut self) {
        let available = self.action_availability();
        if let Some(p) = self.palette.as_mut() {
            p.matches = fuzzy_actions(&p.query, &available);
            p.selected = p.selected.min(p.matches.len().saturating_sub(1));
        }
    }

    /// Toggle collapse of the selected Conversation thread.
    fn toggle_collapse_conv(&mut self) {
        if let Some(t) = self.selected_thread() {
            let id = self.review.threads[t].id.clone();
            self.toggle_collapse(id);
        }
    }

    /// In the Conversation view, whether the cursor rests on the selected thread's
    /// root — its header line, the fold anchor (mirroring a file header). The
    /// cursor sits on the root whenever it is not down on a reply (and a collapsed
    /// thread shows only its header, so it always is). This is where Enter folds.
    fn conv_on_thread_header(&self) -> bool {
        !self.conv_order.is_empty() && self.conv_comment == 0
    }

    fn toggle_collapse(&mut self, id: String) {
        // A hand fold/unfold sticks for the session (defaults won't override it).
        self.manual_fold.insert(id.clone());
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        self.relayout();
    }

    /// Route an action in the Conversation view.
    fn conversation_action(&mut self, action: Action) {
        let page = self.body_height.get().max(1);
        match action {
            // j/k step through comments (root, then replies) and cross into the
            // next thread at a thread's ends; g/G jump the selection to the very
            // first/last comment (the pane follows); the wheel and page keys scroll
            // the pane freely without moving the cursor.
            Action::MoveDown => self.move_conv_comment(1),
            Action::MoveUp => self.move_conv_comment(-1),
            Action::Top => self.conv_first(),
            Action::Bottom => self.conv_last(),
            Action::HalfPageDown | Action::PageDown => {
                self.conv_scroll = (self.conv_scroll + page / 2).min(self.conv_max_scroll())
            }
            Action::HalfPageUp | Action::PageUp => {
                self.conv_scroll = self.conv_scroll.saturating_sub(page / 2)
            }
            Action::Reply if self.has_review() => {
                if let Some(t) = self.selected_thread() {
                    self.open_reply(t);
                }
            }
            Action::Resolve if self.has_review() => {
                if let Some(t) = self.selected_thread() {
                    self.resolve_thread(t);
                }
            }
            Action::CloseReview if self.has_review() => self.confirming_close = true,
            Action::Comment => self.start_conversation_comment(),
            Action::Delete => self.request_delete(),
            Action::Edit => self.start_edit(),
            Action::ToggleKind => self.toggle_selected_kind(),
            Action::Fold => self.toggle_collapse_conv(),
            // l: a collapsed thread expands; an open one scrolls to its top.
            Action::NavIn => {
                if self.selected_collapsed() {
                    self.fold_selected(false);
                }
                self.follow_conv();
            }
            // h: step straight out to the thread index — pure movement, no
            // folding (Enter and `o` fold now), mirroring the Files view's
            // line/header → sidebar cascade.
            Action::NavOut if self.sidebar_width(self.body_width.get()).is_some() => {
                self.focus_sidebar();
            }
            _ => {}
        }
    }

    /// Whether the selected Conversation thread is collapsed.
    fn selected_collapsed(&self) -> bool {
        self.selected_thread()
            .is_some_and(|t| self.collapsed.contains(&self.review.threads[t].id))
    }

    /// Fold or unfold the selected thread (a manual override for the session).
    fn fold_selected(&mut self, collapse: bool) {
        if let Some(t) = self.selected_thread() {
            let id = self.review.threads[t].id.clone();
            self.manual_fold.insert(id.clone());
            if collapse {
                self.collapsed.insert(id);
            } else {
                self.collapsed.remove(&id);
            }
            self.relayout();
        }
    }

    /// Close the review. For a GitHub subject (pull request or issue) this
    /// discards the local drafts (published comments stay) and rewrites just this
    /// subject's draft bucket; otherwise it deletes the local review store.
    fn close_review(&mut self) {
        if self.has_subject() {
            // Drop fully-local draft threads and draft replies; keep published.
            self.review
                .threads
                .retain(|t| t.root().is_some_and(|c| c.remote_id.is_some()));
            for thread in &mut self.review.threads {
                thread.comments.retain(|c| c.remote_id.is_some());
            }
            // Rewrites only `pr_drafts[key]` — never the shared store file, so a
            // worktree review and every other subject's drafts are untouched.
            let _ = self.save_pr_drafts();
            self.status = Some("drafts discarded".to_string());
        } else {
            self.status = match self.store.as_ref().map(Store::delete) {
                Some(Ok(())) | None => Some("review closed".to_string()),
                Some(Err(e)) => Some(format!("could not remove the store: {e:#}")),
            };
            self.review.threads.clear();
        }
        self.conv_cursor = 0;
        self.conv_scroll = 0;
        // Land on a view that exists here. An issue has no Files tab, so return it
        // to the Conversation it was just managing; a diff-bearing review (PR or
        // worktree) keeps its Files landing.
        self.view = if self.is_issue() {
            View::Conversation
        } else {
            View::Files
        };
        self.relayout();
    }

    /// The `review.threads` index of the selected thread (Conversation view).
    fn selected_thread(&self) -> Option<usize> {
        self.conv_order.get(self.conv_cursor).copied()
    }

    /// Number of cursor stops in the selected thread: each comment when it is
    /// expanded, else one (its header). Always ≥ 1 when a thread is selected.
    fn selected_comment_count(&self) -> usize {
        let Some(ti) = self.selected_thread() else {
            return 0;
        };
        if self.collapsed.contains(&self.review.threads[ti].id) {
            1
        } else {
            self.review.threads[ti].comments.len().max(1)
        }
    }

    /// `conv_comment` clamped to the selected thread's stop count.
    fn clamped_conv_comment(&self) -> usize {
        self.conv_comment
            .min(self.selected_comment_count().saturating_sub(1))
    }

    /// The (thread, comment) index the Conversation cursor rests on.
    fn selected_comment(&self) -> Option<(usize, usize)> {
        let ti = self.selected_thread()?;
        let ci = self
            .conv_comment
            .min(self.review.threads[ti].comments.len().saturating_sub(1));
        Some((ti, ci))
    }

    /// Move the Conversation body cursor by `delta` comments, stepping into the
    /// next or previous thread at a thread's ends.
    fn move_conv_comment(&mut self, delta: isize) {
        if self.conv_order.is_empty() {
            return;
        }
        if delta > 0 {
            if self.conv_comment + 1 < self.selected_comment_count() {
                self.conv_comment += 1;
            } else if self.conv_cursor + 1 < self.conv_order.len() {
                self.conv_cursor += 1;
                self.conv_comment = 0;
            }
        } else if delta < 0 {
            if self.conv_comment > 0 {
                self.conv_comment -= 1;
            } else if self.conv_cursor > 0 {
                self.conv_cursor -= 1;
                self.conv_comment = self.selected_comment_count().saturating_sub(1);
            }
        }
        self.follow_conv_comment();
        self.reveal_in_sidebar(self.conv_cursor);
    }

    /// `g` in the Conversation body: select the very first comment (the first
    /// thread's root) and scroll to it — the same cursor grammar as Files' `g`.
    fn conv_first(&mut self) {
        if self.conv_order.is_empty() {
            return;
        }
        self.conv_cursor = 0;
        self.conv_comment = 0;
        self.follow_conv_comment();
        self.reveal_in_sidebar(self.conv_cursor);
    }

    /// `G` in the Conversation body: select the very last comment — the last
    /// thread's last reply, or its header when collapsed (a collapsed thread has a
    /// single stop) — and scroll to it.
    fn conv_last(&mut self) {
        if self.conv_order.is_empty() {
            return;
        }
        self.conv_cursor = self.conv_order.len() - 1;
        // `selected_comment_count` reads the now-selected thread, so set the
        // cursor first; it is 1 for a collapsed thread, so the stop is its header.
        self.conv_comment = self.selected_comment_count().saturating_sub(1);
        self.follow_conv_comment();
        self.reveal_in_sidebar(self.conv_cursor);
    }

    /// Scroll so the selected comment is visible (its meta line within the block).
    fn follow_conv_comment(&mut self) {
        let offsets = self.conv_offsets();
        let Some(&block_start) = offsets.get(self.conv_cursor) else {
            return;
        };
        let within = self
            .selected_thread()
            .and_then(|ti| self.conv_comment_starts.get(ti))
            .and_then(|starts| starts.get(self.conv_comment.min(starts.len().saturating_sub(1))))
            .copied()
            .unwrap_or(0);
        let target = block_start + within;
        let height = self.body_height.get().max(1);
        if target < self.conv_scroll {
            self.conv_scroll = target;
        } else if target >= self.conv_scroll + height {
            self.conv_scroll = target.saturating_sub(height / 2);
        }
        self.conv_scroll = self.conv_scroll.min(self.conv_max_scroll());
    }

    /// The display position (within `conv_order`) of thread `storage`.
    fn thread_display_pos(&self, storage: usize) -> usize {
        self.conv_order
            .iter()
            .position(|&t| t == storage)
            .unwrap_or(0)
    }

    fn set_conv(&mut self, index: usize) {
        if self.conv_order.is_empty() {
            return;
        }
        self.conv_cursor = index.min(self.conv_order.len() - 1);
        // Landing on a thread (a sidebar jump) rests the cursor on its root.
        self.conv_comment = 0;
        self.follow_conv();
        // Keep the thread index (sidebar) tracking the selection too.
        self.reveal_in_sidebar(self.conv_cursor);
    }

    /// The first line index of each Conversation thread block, in display order
    /// (blocks are separated by one spacer line).
    fn conv_offsets(&self) -> Vec<usize> {
        let mut offsets = Vec::with_capacity(self.conv_order.len());
        let mut line = 0;
        for &t in &self.conv_order {
            offsets.push(line);
            line += self.conv_blocks[t].len() + 1;
        }
        offsets
    }

    fn conv_total_lines(&self) -> usize {
        self.conv_blocks.iter().map(|b| b.len() + 1).sum()
    }

    /// Map a Conversation body row to the thread there and whether the row is the
    /// thread's header line (its block's first line) — for click routing.
    fn conv_hit(&self, body_row: usize) -> Option<(usize, bool)> {
        if body_row >= self.body_height.get() {
            return None;
        }
        let line = self.conv_scroll + body_row;
        let mut start = 0;
        for (pos, &ti) in self.conv_order.iter().enumerate() {
            let len = self.conv_blocks[ti].len();
            if line >= start && line < start + len {
                return Some((pos, line == start));
            }
            start += len + 1; // the block plus its trailing spacer
        }
        None
    }

    /// The thread whose inline comment header sits at Files body `row` (the first
    /// line of its comment block), for a fold-toggle click.
    fn comment_header_at(&self, body_row: usize) -> Option<usize> {
        if body_row >= self.body_height.get() {
            return None;
        }
        let i = self.scroll + body_row;
        if self.sbs() {
            match self.srows.get(i) {
                Some(SRow::Comment(t, 0)) => Some(*t),
                _ => None,
            }
        } else {
            match self.urows.get(i) {
                Some(URow::Comment(t, 0)) => Some(*t),
                _ => None,
            }
        }
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
    /// Every thread whose line-range anchor *covers* the cursor's line — so a
    /// range comment (`start..=end`) is reachable from any line inside it, not
    /// only its last. Ordered end-line ascending then start ascending, which
    /// matches the on-screen stack (each inline block renders just below its end
    /// line); storage order breaks ties.
    fn threads_at_cursor(&self) -> Vec<usize> {
        let Some((file, flat)) = self.cursor_content() else {
            return Vec::new();
        };
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let path = self.diff.files[file].display_path();
        let mut hits: Vec<usize> = (0..self.review.threads.len())
            .filter(|&ti| self.thread_covers_line(ti, path, line))
            .collect();
        hits.sort_by_key(|&ti| match &self.review.threads[ti].anchor {
            Anchor::Line { start, end, .. } => (*end, *start),
            _ => (u32::MAX, u32::MAX),
        });
        hits
    }

    /// Whether thread `ti`'s line anchor covers `line` within `path` — its range
    /// contains the line's number on the anchored side.
    fn thread_covers_line(&self, ti: usize, path: &str, line: &Line) -> bool {
        matches!(&self.review.threads[ti].anchor,
        Anchor::Line { file, side, start, end, .. } if file == path && {
            let n = if *side == Side::New { line.new_lineno } else { line.old_lineno };
            n.is_some_and(|n| *start <= n && n <= *end)
        })
    }

    /// Whether any thread's range covers the diff line `(file, flat)` — used to
    /// mark every row of a multi-line comment in the gutter, not only its anchor
    /// line, so a range comment is visible on the lines it applies to.
    fn line_has_comment(&self, file: usize, flat: usize) -> bool {
        if flat == HEADER {
            return false;
        }
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let path = self.diff.files[file].display_path();
        (0..self.review.threads.len()).any(|ti| self.thread_covers_line(ti, path, line))
    }

    /// The single thread a per-line action targets: the forced pick while a
    /// picked action runs, else the top thread covering the cursor (the display
    /// representative). Callers that must disambiguate several use
    /// [`Self::threads_at_cursor`] and open a picker.
    fn thread_at_cursor(&self) -> Option<usize> {
        if let Some(ti) = self.forced_thread {
            return Some(ti);
        }
        self.threads_at_cursor().first().copied()
    }

    /// Open the disambiguation picker for `candidates`, to run `action` on the one
    /// the reviewer chooses.
    fn open_thread_picker(&mut self, candidates: Vec<usize>, action: Action) {
        let n = candidates.len();
        self.thread_picker = Some(ThreadPicker {
            candidates,
            selected: 0,
            action,
        });
        self.status = Some(format!("{n} comments here — pick one (Esc to cancel)"));
    }

    /// Run the picker's action against candidate `choice`, then clear the picker
    /// and the forced-target override.
    fn run_thread_pick(&mut self, choice: usize) {
        let Some(picker) = self.thread_picker.take() else {
            return;
        };
        let Some(&ti) = picker.candidates.get(choice) else {
            return;
        };
        self.forced_thread = Some(ti);
        self.run_action(picker.action);
        self.forced_thread = None;
    }

    /// Route a key while the thread-disambiguation picker is open: `j`/`k` (or
    /// arrows) move, a digit picks that candidate directly, Enter confirms the
    /// highlighted one, Esc cancels.
    fn on_key_thread_picker(&mut self, code: KeyCode, _mods: KeyModifiers) {
        let Some(picker) = self.thread_picker.as_mut() else {
            return;
        };
        let n = picker.candidates.len();
        match code {
            KeyCode::Esc => {
                self.thread_picker = None;
                self.status = Some("cancelled".to_string());
            }
            KeyCode::Char('j') | KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(n - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                if idx < n {
                    self.run_thread_pick(idx);
                }
            }
            KeyCode::Enter => {
                let choice = picker.selected;
                self.run_thread_pick(choice);
            }
            _ => {}
        }
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
        let author = self.author.clone();
        let body = compose.area.text();
        self.status = match compose.kind {
            ComposeKind::New(anchor) => {
                let kind = self.human_new_kind();
                self.add_thread(anchor, &author, &body, kind);
                self.persist("comment added")
            }
            ComposeKind::Reply(thread_id) => {
                let kind = self.reply_kind(&thread_id);
                match self.add_reply(&thread_id, &author, &body, kind) {
                    Some(_) => self.persist("reply added"),
                    None => Some("the thread is gone".to_string()),
                }
            }
            ComposeKind::Edit { thread, comment } => {
                match self.published_endpoint(&thread, &comment) {
                    // A published comment: save the edit back to GitHub, then
                    // apply it locally when the job reports success — routed to
                    // whichever handle (PR or issue) owns it.
                    Some(endpoint) => {
                        if let Some(pr) = self.pr.clone() {
                            self.start_job(
                                "Editing on GitHub",
                                Box::new(move |progress| {
                                    progress("updating comment…");
                                    pr.edit_published(endpoint, &body)
                                        .map_err(friendly_github_write_error)?;
                                    Ok(JobOutcome::Edited {
                                        thread_id: thread,
                                        comment_id: comment,
                                        body,
                                    })
                                }),
                            );
                            None
                        } else if let Some(issue) = self.issue.clone() {
                            self.start_job(
                                "Editing on GitHub",
                                Box::new(move |progress| {
                                    progress("updating comment…");
                                    issue
                                        .edit_published(endpoint, &body)
                                        .map_err(friendly_github_write_error)?;
                                    Ok(JobOutcome::Edited {
                                        thread_id: thread,
                                        comment_id: comment,
                                        body,
                                    })
                                }),
                            );
                            None
                        } else {
                            Some("no GitHub subject to save the edit to".to_string())
                        }
                    }
                    // An unpublished draft or local note: edit in place.
                    None => {
                        if self.edit_comment(&thread, &comment, &body) {
                            self.persist("comment edited")
                        } else {
                            Some("the comment can no longer be edited".to_string())
                        }
                    }
                }
            }
        };
        self.relayout();
    }

    /// If `(thread_id, comment_id)` names a published comment with a numeric
    /// remote id, the GitHub endpoint to edit/delete it through. A pulled thread
    /// id carries its kind as a prefix: `review:` is a submitted review's summary,
    /// `issuecomment:` a PR conversation comment. A **locally-created**
    /// conversation comment (from `c`) has a plain generated id but a
    /// [`Anchor::Review`] anchor, so it too routes to the issue-comment endpoint
    /// once published — without this it would wrongly hit `/pulls/comments` and
    /// 404. Everything else is an inline review comment (`/pulls/comments`).
    fn published_endpoint(&self, thread_id: &str, comment_id: &str) -> Option<CommentEndpoint> {
        let thread = self.review.thread(thread_id)?;
        let comment = thread.comments.iter().find(|c| c.id == comment_id)?;
        let remote_id = comment.remote_id.as_deref()?.parse::<u64>().ok()?;
        Some(if thread_id.starts_with("review:") {
            CommentEndpoint::ReviewSummary(remote_id)
        } else if thread_id.starts_with("issuecomment:") || thread.anchor == Anchor::Review {
            CommentEndpoint::IssueComment(remote_id)
        } else {
            CommentEndpoint::ReviewComment(remote_id)
        })
    }

    /// Replace the body of an existing comment, re-checking it is still the
    /// author's own unpublished comment (state may have changed while the
    /// composer was open). Returns false when it is gone or no longer editable.
    fn edit_comment(&mut self, thread_id: &str, comment_id: &str, body: &str) -> bool {
        let author = self.author.clone();
        let Some(thread) = self.review.threads.iter_mut().find(|t| t.id == thread_id) else {
            return false;
        };
        let Some(comment) = thread.comments.iter_mut().find(|c| c.id == comment_id) else {
            return false;
        };
        if comment.is_published() || comment.author != author {
            return false;
        }
        comment.body = body.to_string();
        true
    }

    /// Save the review, returning a status message describing the outcome.
    fn persist(&self, done: &str) -> Option<String> {
        // For a GitHub subject (pull request or issue) only the drafts are stored
        // under `pr_drafts[key]` — published comments are re-pulled, not saved into
        // the worktree store; drafts are sent on submit (PR) or Ctrl-S (issue).
        if self.has_subject() {
            let verb = if self.is_issue() { "send" } else { "submit" };
            return match self.save_pr_drafts() {
                Ok(()) => Some(format!("{done} — draft saved, Ctrl-S to {verb}")),
                Err(e) => Some(format!("{done}, but save failed: {e}")),
            };
        }
        match &self.store {
            Some(store) => match store.save(&self.review) {
                Ok(()) => Some(done.to_string()),
                Err(e) => Some(format!("{done}, but save failed: {e:#}")),
            },
            None => Some(format!("{done} (not saved)")),
        }
    }

    /// Persist the current PR's draft-only threads (those with an unpublished
    /// comment). This is a no-op outside a pull request.
    fn save_pr_drafts(&self) -> Result<(), String> {
        let (Some(store), Some(key)) = (&self.store, &self.pr_key) else {
            return Ok(());
        };
        store
            .save_pr_drafts(key, &self.pr_drafts())
            .map_err(|e| format!("{e:#}"))
    }

    // -- comment mutation (shared by the human keys and the control plane) ---

    /// Publish a review event to the control plane's event log.
    fn emit(&self, kind: EventKind, thread: Option<String>) {
        self.events.append(kind, thread);
    }

    /// Append a new thread with a single root comment, returning `(thread_id,
    /// comment_id)`. Emits a [`EventKind::Comment`]; the caller persists and
    /// relays out.
    fn add_thread(
        &mut self,
        anchor: Anchor,
        author: &str,
        body: &str,
        kind: CommentKind,
    ) -> (String, String) {
        let comment_id = generate_id();
        let thread_id = generate_id();
        self.review.threads.push(Thread {
            id: thread_id.clone(),
            anchor,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: comment_id.clone(),
                author: author.to_string(),
                body: body.trim_end().to_string(),
                created_at: now(),
                remote_id: None,
                kind,
            }],
        });
        self.emit(EventKind::Comment, Some(thread_id.clone()));
        (thread_id, comment_id)
    }

    /// Append a reply to `thread_id`, returning the new comment id (or `None`
    /// when the thread is gone). Emits a [`EventKind::Reply`].
    fn add_reply(
        &mut self,
        thread_id: &str,
        author: &str,
        body: &str,
        kind: CommentKind,
    ) -> Option<String> {
        let comment_id = generate_id();
        let thread = self.review.thread_mut(thread_id)?;
        thread.comments.push(Comment {
            id: comment_id.clone(),
            author: author.to_string(),
            body: body.trim_end().to_string(),
            created_at: now(),
            remote_id: None,
            kind,
        });
        self.emit(EventKind::Reply, Some(thread_id.to_string()));
        Some(comment_id)
    }

    /// The kind for a new human-authored comment: a note in a local review (never
    /// sent), a draft on a GitHub subject — queued for submit on a pull request,
    /// for Ctrl-S send on an issue.
    fn human_new_kind(&self) -> CommentKind {
        if self.has_subject() {
            CommentKind::Draft
        } else {
            CommentKind::Local
        }
    }

    /// The kind a reply inherits: local reviews are all local; on a PR, a reply
    /// continues its thread — local under a local note, draft under a draft or
    /// published thread. A reply under a **conversation** thread
    /// ([`Anchor::Review`]) is always local: it never posts, since GitHub's
    /// conversation is flat and sending it would flatten the reply into a new
    /// top-level comment, changing what it means.
    fn reply_kind(&self, thread_id: &str) -> CommentKind {
        if self.pr.is_none() {
            return CommentKind::Local;
        }
        let thread = self.review.thread(thread_id);
        if thread.is_some_and(|t| t.anchor == Anchor::Review) {
            return CommentKind::Local;
        }
        let root_local = thread.and_then(|t| t.root()).is_some_and(|c| c.is_local());
        if root_local {
            CommentKind::Local
        } else {
            CommentKind::Draft
        }
    }

    // -- control plane (server side; runs on the UI thread) -----------------

    /// Apply one control-plane request against the running review and produce a
    /// response. `hello` and `wait` are handled by the socket thread and never
    /// reach here.
    fn handle_control(&mut self, request: Request) -> Response {
        let reply = match request {
            Request::Hello { .. } => return Response::Error("unexpected hello".to_string()),
            Request::Wait(_) => {
                return Response::Error("wait is served by the socket thread".to_string());
            }
            Request::Get => Reply::Session(self.session_info()),
            Request::Context => Reply::Context(self.context_info()),
            Request::Review { include_patch } => Reply::Review(self.review_info(include_patch)),
            Request::CommentList => Reply::Threads {
                threads: self.thread_infos(),
            },
            Request::Navigate(nav) => match self.control_navigate(nav) {
                Ok(result) => Reply::Navigate(result),
                Err(message) => return Response::Error(message),
            },
            Request::Reload => match self.control_reload() {
                Ok(result) => Reply::Reload(result),
                Err(message) => return Response::Error(message),
            },
            Request::CommentAdd(add) => match self.control_comment_add(add) {
                Ok(result) => Reply::Comment(result),
                Err(message) => return Response::Error(message),
            },
            Request::CommentReply(reply) => match self.control_comment_reply(reply) {
                Ok(result) => Reply::Comment(result),
                Err(message) => return Response::Error(message),
            },
            Request::CommentEdit(edit) => match self.control_comment_edit(edit) {
                Ok(result) => Reply::Comment(result),
                Err(message) => return Response::Error(message),
            },
            Request::CommentResolve(resolve) => match self.control_comment_resolve(resolve) {
                Ok(result) => Reply::Resolve(result),
                Err(message) => return Response::Error(message),
            },
            Request::CommentRm(rm) => match self.control_comment_rm(rm) {
                Ok(result) => Reply::Removed(result),
                Err(message) => return Response::Error(message),
            },
        };
        Response::Ok(reply)
    }

    /// This session's identity and source, for `lr session get`.
    fn session_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.session_id.clone(),
            pid: std::process::id(),
            repo: self
                .repo_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            source: self.label.clone(),
            subject: self.subject_info(),
        }
    }

    /// The PR facts for `lr session get`, so an agent reads the change's intent
    /// before reviewing — built from the overview already held (no gh call). A
    /// plain diff (no PR) has no subject.
    fn subject_info(&self) -> Option<Box<SubjectInfo>> {
        let ov = self.pr_overview.as_ref()?;
        let kind = match ov.kind {
            loopreview_github::SubjectKind::Pr => "pr",
            loopreview_github::SubjectKind::Issue => "issue",
        };
        Some(Box::new(SubjectInfo {
            kind: kind.to_string(),
            number: ov.number,
            title: ov.title.clone(),
            status: ov.status.wire().to_string(),
            author: ov.author.clone(),
            base: ov.base_ref.clone(),
            head: ov.head_ref.clone(),
            body: ov.body.clone(),
            url: ov.url.clone(),
        }))
    }

    /// The human reviewer's current focus, for `lr session context`.
    fn context_info(&self) -> ContextInfo {
        let view = match self.view {
            View::Overview => "overview",
            View::Files => "files",
            View::Conversation => "conversation",
        }
        .to_string();
        let cursor = self.current_anchor();
        let thread = match self.view {
            // The Overview carries no comment cursor (read-only PR facts).
            View::Overview => None,
            View::Conversation => self
                .selected_thread()
                .map(|t| self.review.threads[t].id.clone()),
            View::Files => self
                .thread_at_cursor()
                .map(|idx| self.review.threads[idx].id.clone()),
        };
        // On a header the cursor has a file but no line; report the file anyway.
        let file = cursor.as_ref().map(|a| a.path.clone()).or_else(|| {
            self.clines
                .get(self.cursor)
                .map(|&(f, _)| self.diff.files[f].display_path().to_string())
        });
        ContextInfo {
            view,
            file,
            side: cursor
                .as_ref()
                .map(|a| if a.new_side { Side::New } else { Side::Old }),
            line: cursor.as_ref().map(|a| a.line),
            thread,
            event_seq: self.events.latest_seq(),
        }
    }

    /// The diff structure and threads, for `lr session review`.
    fn review_info(&self, include_patch: bool) -> ReviewInfo {
        ReviewInfo {
            source: self.label.clone(),
            base: self.diff.provenance.base.clone(),
            head: self.diff.provenance.head.clone(),
            files: self
                .diff
                .files
                .iter()
                .map(|f| control::file_info(f, include_patch))
                .collect(),
            threads: self.thread_infos(),
        }
    }

    /// The review's threads with their outdated flags, for `lr session review`
    /// and `comment list`.
    fn thread_infos(&self) -> Vec<protocol::ThreadInfo> {
        self.review
            .threads
            .iter()
            .map(|t| control::thread_info(t, control::anchor_outdated(&self.diff, &t.anchor)))
            .collect()
    }

    /// Move the reviewer's cursor and view (a control-plane `navigate`).
    /// Reach a diff line if the model has it, doing the state transitions itself:
    /// switch to the Files view, expand the file when it is collapsed, then place
    /// the cursor. Validates against the diff model — not the rendered rows — so a
    /// collapsed file or the Conversation view can't hide a line `comment add`
    /// would accept. Returns whether the line was reached.
    fn goto_diff_line(&mut self, file: &str, side: Side, line: u32) -> bool {
        if !line_present(&self.diff, file, side, line) {
            return false;
        }
        self.view = View::Files;
        if self.collapsed_files.remove(file) {
            self.relayout();
        }
        let target = CursorAnchor {
            path: file.to_string(),
            new_side: side == Side::New,
            line,
        };
        match self.find_anchor(&target) {
            Some(cursor) => {
                self.set_cursor(cursor);
                true
            }
            None => false,
        }
    }

    fn control_navigate(&mut self, nav: protocol::Navigate) -> Result<NavigateResult, String> {
        if let Some(thread_id) = nav.thread {
            let idx = self
                .review
                .threads
                .iter()
                .position(|t| t.id == thread_id)
                .ok_or_else(|| format!("no thread {thread_id}"))?;
            let anchor = self.review.threads[idx].anchor.clone();
            if let Anchor::Line {
                file, side, end, ..
            } = anchor
                && self.goto_diff_line(&file, side, end)
            {
                self.conv_cursor = self.thread_display_pos(idx);
                self.status = Some(format!("agent → {file}:{end}"));
                return Ok(NavigateResult {
                    moved: true,
                    file: Some(file),
                    line: Some(end),
                });
            }
            // A file/review anchor, or an outdated line: select it in the
            // Conversation view instead.
            self.view = View::Conversation;
            self.set_conv(idx);
            self.status = Some("agent → conversation".to_string());
            let file = self.review.threads[idx].anchor.file().map(str::to_string);
            return Ok(NavigateResult {
                moved: true,
                file,
                line: None,
            });
        }

        match (nav.file, nav.line) {
            (Some(file), Some(line)) => {
                let side = nav.side.unwrap_or(Side::New);
                if self.goto_diff_line(&file, side, line) {
                    self.status = Some(format!("agent → {file}:{line}"));
                    Ok(NavigateResult {
                        moved: true,
                        file: Some(file),
                        line: Some(line),
                    })
                } else {
                    Ok(NavigateResult {
                        moved: false,
                        file: Some(file),
                        line: Some(line),
                    })
                }
            }
            _ => Err("navigate needs --thread, or --file with --line".to_string()),
        }
    }

    /// Reload the current source (a control-plane `reload`). A GitHub subject (a
    /// pull request or an issue) re-pulls in the background; a git/patch source
    /// reloads synchronously.
    fn control_reload(&mut self) -> Result<ReloadResult, String> {
        if self.has_subject() {
            self.refresh();
            self.status = Some("agent: reloading…".to_string());
            return Ok(ReloadResult { started: true });
        }
        let Some(source) = self.source.clone() else {
            return Err("this source cannot be reloaded".to_string());
        };
        match source.load() {
            Ok(diff) => {
                self.reload(diff);
                self.status = Some("agent: reloaded".to_string());
                Ok(ReloadResult { started: false })
            }
            Err(e) => Err(format!("reload failed: {e:#}")),
        }
    }

    /// Add a comment thread at a line (a control-plane `comment add`).
    fn control_comment_add(
        &mut self,
        add: protocol::CommentAdd,
    ) -> Result<protocol::CommentResult, String> {
        if !self.comments_enabled() {
            return Err("comments need a git repository or a pull request".to_string());
        }
        // A conversation comment (Anchor::Review) is tied to nothing in the diff —
        // an agent's overall note (a run verdict, a summary). It takes no line.
        if add.conversation {
            if add.file.is_some() || add.line.is_some() {
                return Err("--conversation takes no --file/--line".to_string());
            }
            let kind = self.agent_kind(add.draft);
            let (thread, comment) = self.add_thread(Anchor::Review, &add.author, &add.body, kind);
            let done = self.persist("comment added").unwrap_or_default();
            self.relayout();
            self.status = Some(format!("agent: {done} (conversation)"));
            return Ok(protocol::CommentResult {
                thread,
                comment,
                draft: kind == CommentKind::Draft,
            });
        }
        // Otherwise it is a line comment, which needs a file and a line.
        let (Some(file), Some(line)) = (add.file.as_deref(), add.line) else {
            return Err(
                "a line comment needs --file and --line (or use --conversation)".to_string(),
            );
        };
        let side = add.side.unwrap_or(Side::New);
        // Locate the line so the anchor captures its commit and context.
        let file_idx = self
            .diff
            .files
            .iter()
            .position(|f| f.display_path() == file)
            .ok_or_else(|| format!("no file {file} in the current review"))?;
        let found = self.diff.files[file_idx]
            .hunks
            .iter()
            .enumerate()
            .find_map(|(hi, h)| {
                h.lines
                    .iter()
                    .position(|l| {
                        let n = if side == Side::New {
                            l.new_lineno
                        } else {
                            l.old_lineno
                        };
                        n == Some(line)
                    })
                    .map(|li| (hi, li))
            });
        let (hi, li) = found.ok_or_else(|| {
            format!(
                "line {line} ({}) is not shown in the diff for {file}",
                if side == Side::New { "new" } else { "old" },
            )
        })?;
        let commit = if side == Side::New {
            self.diff.provenance.head.clone()
        } else {
            self.diff.provenance.base.clone()
        };
        let context = context_snippet(&self.diff.files[file_idx].hunks[hi], li);
        let anchor = Anchor::Line {
            file: file.to_string(),
            side,
            start: line,
            end: line,
            commit,
            context,
        };
        let kind = self.agent_kind(add.draft);
        let (thread, comment) = self.add_thread(anchor, &add.author, &add.body, kind);
        let done = self.persist("comment added").unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done} ({file}:{line})"));
        Ok(protocol::CommentResult {
            thread,
            comment,
            draft: kind == CommentKind::Draft,
        })
    }

    /// The kind for an agent-authored comment: a local note by default (agents
    /// converse, they don't queue GitHub sends), a draft only on a GitHub subject
    /// (pull request or issue) with the explicit `--draft` flag — so an agent's
    /// note is never sent by accident, and a human still does the sending.
    fn agent_kind(&self, draft: bool) -> CommentKind {
        if self.has_subject() && draft {
            CommentKind::Draft
        } else {
            CommentKind::Local
        }
    }

    /// Reply to a thread (a control-plane `comment reply`).
    fn control_comment_reply(
        &mut self,
        reply: protocol::CommentReply,
    ) -> Result<protocol::CommentResult, String> {
        if !self.comments_enabled() {
            return Err("comments need a git repository or a pull request".to_string());
        }
        // An agent's reply is a local note unless it passes --draft (agents don't
        // queue GitHub sends implicitly, even under a draft thread).
        let kind = self.agent_kind(reply.draft);
        // A conversation reply always stays local — it never posts — so --draft on
        // one is refused, mirroring the human `t` rule.
        if kind == CommentKind::Draft
            && self
                .review
                .thread(&reply.thread)
                .is_some_and(|t| t.anchor == Anchor::Review)
        {
            return Err(
                "conversation replies stay local — post a new conversation comment instead"
                    .to_string(),
            );
        }
        // A draft reply under a local root would be stranded — it could never be
        // sent while the root stays off GitHub. Refuse it, mirroring the human
        // `t` rule; a reply without --draft inherits local as usual.
        if kind == CommentKind::Draft
            && self
                .review
                .thread(&reply.thread)
                .and_then(|t| t.root())
                .is_some_and(|c| c.disposition() == CommentKind::Local)
        {
            return Err(
                "the thread root is local — promote it first, or reply without --draft".to_string(),
            );
        }
        let comment = self
            .add_reply(&reply.thread, &reply.author, &reply.body, kind)
            .ok_or_else(|| format!("no thread {}", reply.thread))?;
        let done = self.persist("reply added").unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done}"));
        Ok(protocol::CommentResult {
            thread: reply.thread,
            comment,
            draft: kind == CommentKind::Draft,
        })
    }

    /// Edit a comment's body by id (a control-plane `comment edit`). An agent
    /// may edit only its own unpublished comment — a draft or a local note it
    /// authored. A published comment is refused (writing to GitHub is a human
    /// action), and another author's comment is refused (that would misattribute
    /// it). Works on a root or a reply, wherever the id points.
    fn control_comment_edit(
        &mut self,
        edit: protocol::CommentEdit,
    ) -> Result<protocol::CommentResult, String> {
        let id = &edit.id;
        let Some((ti, ci)) = self.review.threads.iter().enumerate().find_map(|(ti, t)| {
            t.comments
                .iter()
                .position(|c| c.id == *id)
                .map(|ci| (ti, ci))
        }) else {
            return Err(format!("no comment {id}"));
        };
        {
            let comment = &self.review.threads[ti].comments[ci];
            guard_agent_write(
                std::iter::once(comment),
                "a published comment can't be edited by an agent — writing to GitHub is a human action",
            )?;
            if comment.author != edit.author {
                return Err(format!(
                    "only the author can edit their own comment ({} can't edit {}'s)",
                    edit.author, comment.author
                ));
            }
        }
        self.review.threads[ti].comments[ci].body = edit.body.clone();
        let thread_id = self.review.threads[ti].id.clone();
        let comment_id = self.review.threads[ti].comments[ci].id.clone();
        let draft = self.review.threads[ti].comments[ci].is_draft();
        let done = self.persist("comment edited").unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done}"));
        Ok(protocol::CommentResult {
            thread: thread_id,
            comment: comment_id,
            draft,
        })
    }

    /// Resolve or reopen a thread (a control-plane `comment resolve`). Refuses a
    /// published pull-request thread: pushing that to GitHub is a human action.
    fn control_comment_resolve(
        &mut self,
        resolve: protocol::CommentResolve,
    ) -> Result<protocol::ResolveResult, String> {
        let idx = self
            .review
            .threads
            .iter()
            .position(|t| t.id == resolve.thread)
            .ok_or_else(|| format!("no thread {}", resolve.thread))?;
        guard_agent_write(
            self.review.threads[idx].root(),
            "resolving a published pull-request thread is a human action (press x in the TUI)",
        )?;
        // A draft thread has no meaningful resolve — it is still queued to send.
        if resolve.resolved
            && self.review.threads[idx]
                .root()
                .is_some_and(|c| c.disposition() == CommentKind::Draft)
        {
            return Err(
                "a draft thread can't be resolved — demote it to local or remove it".to_string(),
            );
        }
        let thread = &mut self.review.threads[idx];
        thread.state = if resolve.resolved {
            ThreadState::Resolved
        } else {
            ThreadState::Open
        };
        let resolved = thread.is_resolved();
        self.emit(EventKind::Resolve, Some(resolve.thread.clone()));
        let done = self
            .persist(if resolved { "resolved" } else { "reopened" })
            .unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done}"));
        Ok(protocol::ResolveResult {
            thread: resolve.thread,
            resolved,
        })
    }

    /// Withdraw an unpublished comment or thread by id — a draft or a local
    /// note, never a published comment. A comment id removes that comment (and
    /// its thread when it empties); a thread id removes the whole thread.
    fn control_comment_rm(
        &mut self,
        rm: protocol::CommentRm,
    ) -> Result<protocol::RemoveResult, String> {
        let id = &rm.id;
        // Resolve the id to a thread, and optionally a comment within it.
        let (ti, ci) = if let Some(ti) = self.review.threads.iter().position(|t| t.id == *id) {
            (ti, None)
        } else if let Some(found) = self.review.threads.iter().enumerate().find_map(|(ti, t)| {
            t.comments
                .iter()
                .position(|c| c.id == *id)
                .map(|ci| (ti, ci))
        }) {
            (found.0, Some(found.1))
        } else {
            return Err(format!("no comment or thread {id}"));
        };
        // Unpublished only (a draft or a local note) — never delete anything
        // published to GitHub.
        let targets: &[Comment] = match ci {
            Some(ci) => std::slice::from_ref(&self.review.threads[ti].comments[ci]),
            None => &self.review.threads[ti].comments,
        };
        guard_agent_write(
            targets,
            "a published comment can't be withdrawn — it stays on GitHub",
        )?;
        let (thread_id, removed_thread) = self.remove_draft(ti, ci);
        self.status = Some("agent: draft removed".to_string());
        Ok(protocol::RemoveResult {
            thread: thread_id,
            removed_thread,
        })
    }

    /// Remove a draft comment `ci` (or the whole thread when `ci` is `None`) from
    /// memory and the store; returns the thread id and whether the thread went.
    /// Callers must have checked the target is a draft.
    fn remove_draft(&mut self, ti: usize, ci: Option<usize>) -> (String, bool) {
        let thread_id = self.review.threads[ti].id.clone();
        let comment_id = ci.map(|ci| self.review.threads[ti].comments[ci].id.clone());
        let removed_thread = match ci {
            Some(ci) => {
                self.review.threads[ti].comments.remove(ci);
                let empty = self.review.threads[ti].comments.is_empty();
                if empty {
                    self.review.threads.remove(ti);
                }
                empty
            }
            None => {
                self.review.threads.remove(ti);
                true
            }
        };
        self.store_remove(&thread_id, comment_id.as_deref());
        self.conv_cursor = self
            .conv_cursor
            .min(self.review.threads.len().saturating_sub(1));
        self.relayout();
        (thread_id, removed_thread)
    }

    /// A targeted store deletion (not the union `save`), routed to the working
    /// tree or a GitHub subject's draft set (pull request or issue).
    fn store_remove(&self, thread_id: &str, comment_id: Option<&str>) {
        let Some(store) = &self.store else {
            return;
        };
        if self.has_subject() {
            if let Some(key) = &self.pr_key {
                let _ = store.remove_pr_draft(key, thread_id, comment_id);
            }
        } else {
            let _ = store.remove(thread_id, comment_id);
        }
    }

    /// Re-clamp the cursor and scroll after a terminal resize. A resize can flip
    /// `auto` layout to side-by-side (which has fewer rows than unified), so a
    /// scroll or cursor that was valid a moment ago may now point past the end;
    /// left unclamped, the side-by-side renderer would try to allocate a row for
    /// every position from a stale scroll to the end and panic.
    fn on_resize(&mut self, cols: u16) {
        self.body_width.set(cols as usize);
        // A resize re-evaluates the sidebar mode (drops any `b` override); if the
        // sidebar auto-hides, focus falls back to the body.
        self.sidebar_override = None;
        if self.focus == Focus::Sidebar && self.sidebar_width(cols as usize).is_none() {
            self.focus = Focus::Body;
        }
        if !self.clines.is_empty() {
            self.cursor = self.cursor.min(self.clines.len() - 1);
        }
        self.scroll = self.scroll.min(self.rows_len().saturating_sub(1));
        if !self.conv_order.is_empty() {
            self.conv_cursor = self.conv_cursor.min(self.conv_order.len() - 1);
        }
        self.conv_scroll = self.conv_scroll.min(self.conv_max_scroll());
        self.follow_cursor();
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.input.is_some()
            || self.submit.is_some()
            || self.finder.is_some()
            || self.palette.is_some()
            || self.job.is_some()
            || self.loading.is_some()
        {
            return; // a modal or a background action owns input
        }
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            // Shift + wheel scrolls horizontally (as does a trackpad's sideways swipe).
            MouseEventKind::ScrollDown if shift => self.hscroll_by(HSCROLL_STEP),
            MouseEventKind::ScrollUp if shift => self.hscroll_by(-HSCROLL_STEP),
            MouseEventKind::ScrollRight => self.hscroll_by(HSCROLL_STEP),
            MouseEventKind::ScrollLeft => self.hscroll_by(-HSCROLL_STEP),
            MouseEventKind::ScrollDown => self.scroll_wheel(mouse.column, mouse.row, 3),
            MouseEventKind::ScrollUp => self.scroll_wheel(mouse.column, mouse.row, -3),
            MouseEventKind::Down(MouseButton::Left) => self.mouse_down(mouse.column, mouse.row),
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(mouse.column, mouse.row),
            _ => {}
        }
    }

    /// A vertical wheel notch scrolls the sidebar list when the pointer is over
    /// it, otherwise the body — the Conversation thread pane (freely) or the diff
    /// (hit-tested against the last draw).
    fn scroll_wheel(&mut self, col: u16, row: u16, delta: isize) {
        if let Region::Sidebar(_) = hit_region(col, row, self.hit.get()) {
            self.scroll_sidebar(delta);
        } else {
            match self.view {
                View::Overview => {
                    let max = self.overview_max_scroll() as isize;
                    self.overview_scroll =
                        (self.overview_scroll as isize + delta).clamp(0, max) as usize;
                }
                View::Conversation => self.scroll_conv(delta),
                View::Files => self.scroll_view(delta),
            }
        }
    }

    /// Freely scroll the Conversation thread pane, clamped to its content (the
    /// same defensive clamp the side-by-side pass uses).
    fn scroll_conv(&mut self, delta: isize) {
        let max = self.conv_max_scroll() as isize;
        self.conv_scroll = (self.conv_scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Scroll the viewport without moving the cursor (wheel scrolling).
    fn scroll_view(&mut self, delta: isize) {
        let height = self.body_height.get().max(1);
        let max_scroll = self.rows_len().saturating_sub(height) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max_scroll) as usize;
    }

    /// Scroll the diff content horizontally, clamped to the current file's widest
    /// line (the line-number gutter does not move).
    fn hscroll_by(&mut self, delta: isize) {
        let max = self.max_hscroll() as isize;
        self.hscroll = (self.hscroll as isize + delta).clamp(0, max) as usize;
    }

    /// The furthest right the content may scroll. Bounded so the viewport never
    /// becomes whitespace-only: the longest line's overflow past the viewport,
    /// plus a small reading margin, but never so far that its last column
    /// scrolls off (which matters only for a very narrow viewport). Zero when
    /// the widest line already fits.
    fn max_hscroll(&self) -> usize {
        let viewport = self.content_viewport_width();
        let longest = self.max_content_width();
        if viewport == 0 || longest <= viewport {
            return 0;
        }
        (longest - viewport + HSCROLL_MARGIN).min(longest - 1)
    }

    /// Columns available for line content (the body width minus the fixed
    /// gutter) in the current layout. In split view each pane has its own
    /// gutter, and horizontal scroll windows both panes by the same offset, so
    /// the narrower single-pane content width bounds the scroll.
    fn content_viewport_width(&self) -> usize {
        let body = self.body_width.get();
        if self.sbs() {
            let pane = body.saturating_sub(1) / 2; // a 1-col divider splits the body
            pane.saturating_sub(self.num_width + 4)
        } else {
            body.saturating_sub(1 + (2 * self.num_width + 2) + 2)
        }
    }

    /// The widest line content, in display columns, of the current file.
    fn max_content_width(&self) -> usize {
        let file = self.current_file();
        self.diff
            .files
            .get(file)
            .map(|f| {
                f.hunks
                    .iter()
                    .flat_map(|h| h.lines.iter())
                    .map(|l| {
                        l.content
                            .chars()
                            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                            .sum::<usize>()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Route a left-button press by the region it lands in: a tab switches the
    /// view; a sidebar row selects and opens a file; a file header toggles its
    /// fold; a diff line moves the cursor (and arms a drag-select).
    fn mouse_down(&mut self, column: u16, row: u16) {
        match hit_region(column, row, self.hit.get()) {
            Region::LayoutToggle => self.toggle_mode(),
            Region::PrLink => self.open_pr_page(),
            Region::Tabs if self.shows_tabs() => {
                if let Some(view) = self.tab_at_column(column) {
                    self.set_view(view);
                }
            }
            Region::Sidebar(row) => {
                let idx = self.sidebar_scroll + row;
                if self.view == View::Conversation {
                    self.jump_to_thread(idx);
                } else if let Some(file) = file_at_row(&sidebar_rows(&self.file_entries()), idx) {
                    // A click on a directory header maps to no file — ignored.
                    self.sidebar_activate(file);
                }
            }
            Region::Content { col, row } => {
                // Clicking the content pane focuses it — the clicked pane takes
                // focus, so a diff-line click, drag-select, or comment-action click
                // pulls focus out of the sidebar (a sidebar click keeps it there).
                self.focus = Focus::Body;
                // Overview: a click on a link/image opens it, on a <details>
                // summary folds it. The body scroll maps a pane row to a line.
                if self.view == View::Overview {
                    let line = self.overview_scroll + row;
                    if let Some(action) = self.overview_action_at(line, col) {
                        self.run_md_action(action);
                    }
                    return;
                }
                // Conversation: a click on a body link/image opens it; else a
                // thread header toggles its fold (and selects), a body line selects.
                if self.view == View::Conversation {
                    let line = self.conv_scroll + row;
                    if let Some(action) = self.conv_action_at(line, col) {
                        self.run_conv_md_action(action);
                        return;
                    }
                    if let Some((pos, is_header)) = self.conv_hit(row) {
                        self.set_conv(pos);
                        if is_header && let Some(t) = self.selected_thread() {
                            let id = self.review.threads[t].id.clone();
                            self.toggle_collapse(id);
                        }
                    }
                    return;
                }
                // Files: an inline comment header toggles that thread's fold,
                // distinct from a diff-line click (which moves the cursor).
                if let Some(t) = self.comment_header_at(row) {
                    let id = self.review.threads[t].id.clone();
                    self.toggle_collapse(id);
                    return;
                }
                if let Some(cursor) = self.cline_at_body(col, row) {
                    if self.clines[cursor].1 == HEADER {
                        // A header click folds/unfolds the file.
                        self.set_cursor(cursor);
                        self.toggle_fold_at(self.current_file());
                    } else {
                        self.clear_selection();
                        self.set_cursor(cursor);
                        self.drag_anchor = Some(cursor);
                    }
                }
            }
            _ => {}
        }
    }

    /// Extend a range selection to the dragged-over diff line (content only).
    fn mouse_drag(&mut self, column: u16, row: u16) {
        let Region::Content { col, row } = hit_region(column, row, self.hit.get()) else {
            return;
        };
        let Some(anchor) = self.drag_anchor else {
            return;
        };
        if let Some(target) = self.cline_at_body(col, row)
            && self
                .clines
                .get(target)
                .is_some_and(|&(_, flat)| flat != HEADER)
        {
            if target != anchor {
                self.selection = Some(anchor);
            }
            self.set_cursor(target);
        }
    }

    /// The cursor line at a body cell (`column` relative to the diff, `body_row`
    /// relative to the body top), if any.
    fn cline_at_body(&self, column: u16, body_row: usize) -> Option<usize> {
        if body_row >= self.body_height.get() {
            return None;
        }
        let row_index = self.scroll + body_row;
        if row_index >= self.rows_len() {
            return None;
        }
        if self.sbs() {
            self.sbs_click(row_index, column as usize)
        } else {
            self.unified_click(row_index)
        }
    }

    fn unified_click(&self, row_index: usize) -> Option<usize> {
        match self.urows[row_index] {
            URow::Line { file, flat } => self.cline_index.get(&(file, flat)).copied(),
            URow::FileHeader(fi) => self.cline_index.get(&(fi, HEADER)).copied(),
            _ => None,
        }
    }

    fn sbs_click(&self, row_index: usize, column: usize) -> Option<usize> {
        match self.srows[row_index] {
            SRow::FileHeader(fi) => self.cline_index.get(&(fi, HEADER)).copied(),
            SRow::Pair { file, old, new } => {
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
            _ => None,
        }
    }

    /// Whether the diff body is the inactive pane (the sidebar holds focus). A
    /// hidden sidebar forces focus back to the body, so this is false then.
    fn body_dimmed(&self) -> bool {
        self.focus == Focus::Sidebar
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
        self.hscroll = 0; // a layout switch resets horizontal scroll
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
        self.hscroll = 0; // a file jump resets horizontal scroll
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
        // Browsing the body scrolls the sidebar to keep the current file in view.
        if self.focus == Focus::Body {
            self.reveal_file_in_sidebar(self.current_file());
        }
    }

    // -- render data ------------------------------------------------------

    /// Initialise a file's render cache without highlighting any lines yet: the
    /// (cheap) intra-line ranges are computed up front, but syntax highlighting
    /// is deferred to [`App::ensure_highlight`] so only shown lines are processed.
    fn ensure_render(&self, file: usize) {
        if self.render.borrow()[file].is_some() {
            return;
        }
        let f = &self.diff.files[file];
        let flat = &self.flats[file];

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
            line_highlighter: self.highlighter.line_highlighter(f.display_path()),
            highlight: Vec::with_capacity(flat.len()),
            intraline,
        });
    }

    /// Highlight (incrementally, in file order) up to and including flat line
    /// `flat` of `file`, caching results. Cheap once a line is already done, so
    /// it is safe to call for every drawn row.
    fn ensure_highlight(&self, file: usize, flat: usize) {
        self.ensure_render(file);
        let mut render = self.render.borrow_mut();
        let data = render[file].as_mut().expect("render populated");
        let flats = &self.flats[file];
        let f = &self.diff.files[file];
        let theme = self.highlighter.theme_highlighter();
        while data.highlight.len() <= flat && data.highlight.len() < flats.len() {
            let (h, l) = flats[data.highlight.len()];
            let spans = self.highlighter.highlight_next(
                &mut data.line_highlighter,
                &theme,
                &f.hunks[h].lines[l].content,
            );
            data.highlight.push(spans);
        }
    }

    // -- rendering --------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        if let Some(loading) = &self.loading {
            self.draw_loading(f, &loading.stage);
            return;
        }
        if let Some(error) = &self.load_error {
            self.draw_load_error(f, error);
            return;
        }
        // A tab bar appears for any comment-capable session (a repo diff or a
        // PR), threads or not, so the Conversation tab is always reachable; a
        // pure patch shows it only once it carries threads. A comfortable
        // terminal gives it breathing room — a blank row above and below; a
        // short one collapses that so the body keeps every row it can.
        let tabs = self.shows_tabs();
        let spacious = tabs && f.area().height >= TAB_SPACING_MIN_HEIGHT;
        let constraints: Vec<Constraint> = match (tabs, spacious) {
            (true, true) => vec![
                Constraint::Length(1), // header
                Constraint::Length(1), // gap
                Constraint::Length(1), // tabs
                Constraint::Length(1), // gap
                Constraint::Min(1),    // body
                Constraint::Length(1), // footer
            ],
            (true, false) => vec![
                Constraint::Length(1), // header
                Constraint::Length(1), // tabs
                Constraint::Min(1),    // body
                Constraint::Length(1), // footer
            ],
            (false, _) => vec![
                Constraint::Length(1), // header
                Constraint::Min(1),    // body
                Constraint::Length(1), // footer
            ],
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(f.area());
        let (header, tabs_area, body, footer) = match (tabs, spacious) {
            (true, true) => (chunks[0], Some(chunks[2]), chunks[4], chunks[5]),
            (true, false) => (chunks[0], Some(chunks[1]), chunks[2], chunks[3]),
            (false, _) => (chunks[0], None, chunks[1], chunks[2]),
        };
        // Split off the file-explorer sidebar when shown and the terminal is
        // wide enough (it auto-hides on a narrow terminal — the finder still
        // works there). In the two-pane layout each pane is framed with a
        // title, and the focused pane's frame accents so it is always obvious
        // where input goes.
        let mut content = body;
        let mut sidebar_x0 = 0u16;
        let mut sidebar_cols = 0u16;
        // The Overview tab is a single full-width pane — no file/thread sidebar.
        if self.view != View::Overview
            && let Some(sidebar_w) = self.sidebar_width(body.width as usize)
        {
            let sidebar_focused = self.focus == Focus::Sidebar;
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sidebar_w as u16 + 2), // +2 for the frame
                    Constraint::Length(1),                    // a gap between panes
                    Constraint::Min(1),
                ])
                .split(body);
            // The sidebar indexes whatever the right pane shows: files in the
            // Files view, threads in the Conversation view.
            let sb_title = if self.view == View::Conversation {
                format!(" Threads ({}) ", self.review.threads.len())
            } else {
                format!(" Files ({}) ", self.diff.files.len())
            };
            let sb_block = pane_block(sb_title, sidebar_focused);
            let sb_inner = sb_block.inner(cols[0]);
            f.render_widget(sb_block, cols[0]);
            self.draw_sidebar(f, sb_inner);

            let body_block = pane_block(self.pane_title(), !sidebar_focused);
            let body_inner = body_block.inner(cols[2]);
            f.render_widget(body_block, cols[2]);
            content = body_inner;
            sidebar_x0 = sb_inner.x;
            sidebar_cols = sb_inner.width;
        }
        self.body_width.set(content.width as usize);
        self.body_height.set(content.height as usize);

        // Draw the header first so it records the PR-link columns, then capture
        // the geometry for mouse hit-testing (inner rects; frames map to Outside).
        self.draw_header(f, header);
        let (pr_link_x0, pr_link_x1) = self.header_pr_link.get().unwrap_or((0, 0));
        self.hit.set(HitLayout {
            body_top: content.y,
            body_height: content.height,
            content_x0: content.x,
            content_w: content.width,
            sidebar_x0,
            sidebar_w: sidebar_cols,
            tabs_row: tabs_area.map(|t| t.y),
            footer_row: footer.y,
            // The layout indicator (and its click target) is absent for an issue.
            layout_end: if self.is_issue() {
                0
            } else {
                self.layout_label().chars().count() as u16
            },
            pr_link_row: self.header_pr_link.get().map(|_| header.y),
            pr_link_x0,
            pr_link_x1,
        });

        if let Some(tabs_area) = tabs_area {
            self.draw_tabs(f, tabs_area);
        }
        if self.view == View::Overview {
            self.draw_overview(f, content);
        } else if self.view == View::Conversation {
            self.draw_conversation(f, content);
        } else if self.clines.is_empty() && self.diff.files.is_empty() {
            self.draw_empty(f, content);
        } else if self.sbs() {
            self.draw_body_sbs(f, content);
        } else {
            self.draw_body_unified(f, content);
        }
        self.draw_footer(f, footer);

        if let Some(compose) = &self.input {
            self.draw_compose(f, compose);
        }
        if let Some(modal) = &self.submit {
            self.draw_submit(f, modal);
        }
        if let Some(finder) = &self.finder {
            self.draw_finder(f, finder);
        }
        if let Some(palette) = &self.palette {
            self.draw_palette(f, palette);
        }
        if let Some(picker) = &self.thread_picker {
            self.draw_thread_picker(f, picker);
        }
        if self.confirming_close {
            self.draw_close_confirm(f);
        }
        if self.confirming_delete.is_some() {
            self.draw_delete_confirm(f);
        }
        if let Some(job) = &self.job {
            self.draw_job(f, job);
        }
    }

    /// A spinner overlay shown while a background action runs.
    fn draw_job(&self, f: &mut Frame, job: &Job) {
        let spinner = SPINNER[self.tick % SPINNER.len()];
        let area = centered_rect(50, 18, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", job.title))
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(vec![
                TextLine::from(TextSpan::styled(
                    format!("  {spinner}  {}", job.stage),
                    Style::default().fg(Color::Cyan),
                )),
                TextLine::from(""),
                TextLine::from(TextSpan::styled(
                    "  q abandons and quits (the request keeps running)",
                    Style::default().fg(Color::DarkGray),
                )),
            ]),
            inner,
        );
    }

    /// The review-submission modal for a pull request.
    fn draw_submit(&self, f: &mut Frame, modal: &SubmitModal) {
        let send_only = modal.is_send_only();
        let area = centered_rect(70, 60, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(if send_only {
                " Send replies "
            } else {
                " Submit review "
            })
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let total = modal.new_count + modal.reply_count + modal.conversation_count;
        // A breakdown of what actually posts, omitting zero categories.
        let mut breakdown = Vec::new();
        if modal.new_count > 0 {
            breakdown.push(format!("{} new inline", modal.new_count));
        }
        if modal.reply_count > 0 {
            breakdown.push(format!(
                "{} repl{}",
                modal.reply_count,
                if modal.reply_count == 1 { "y" } else { "ies" }
            ));
        }
        if modal.conversation_count > 0 {
            breakdown.push(format!("{} conversation", modal.conversation_count));
        }
        let by = modal
            .authors
            .iter()
            .map(|(name, n)| format!("{n} by {name}"))
            .collect::<Vec<_>>()
            .join(" · ");
        let mut lines = vec![
            TextLine::from(TextSpan::styled(
                format!("{total} draft(s) to send ({})", breakdown.join(" · ")),
                Style::default().fg(Color::Gray),
            )),
            TextLine::from(TextSpan::styled(
                format!("  {by}"),
                Style::default().fg(Color::DarkGray),
            )),
        ];
        // These go under the human's GitHub identity — warn on any not-yours.
        if modal.foreign {
            lines.push(TextLine::from(TextSpan::styled(
                "  ⚠ includes drafts not authored by you — they send as you on GitHub",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        // A send-only batch (replies / conversation comments, no new inline
        // review comments) makes no review POST, so there is no event to choose
        // and no summary — just a send confirmation.
        if send_only {
            lines.push(TextLine::from(""));
            lines.push(TextLine::from(TextSpan::styled(
                "posts directly — no review event",
                Style::default().fg(Color::DarkGray),
            )));
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(inner);
            f.render_widget(Paragraph::new(lines), rows[0]);
            f.render_widget(
                Paragraph::new(TextLine::from(TextSpan::styled(
                    "Ctrl-S send · Esc cancel",
                    Style::default().fg(Color::DarkGray),
                ))),
                rows[1],
            );
            return;
        }

        lines.push(TextLine::from(""));
        lines.push(TextLine::from(TextSpan::styled(
            "event:",
            Style::default().fg(Color::DarkGray),
        )));
        for (i, (label, _)) in SUBMIT_EVENTS.iter().enumerate() {
            let marker = if i == modal.selected { "●" } else { "○" };
            let style = if i == modal.selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(TextLine::from(TextSpan::styled(
                format!("  {marker} {label}"),
                style,
            )));
        }
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(TextSpan::styled(
            "summary (optional):",
            Style::default().fg(Color::DarkGray),
        )));

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(lines.len() as u16),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);
        f.render_widget(Paragraph::new(lines), rows[0]);
        f.render_widget(
            Paragraph::new(modal.body.render(rows[1].width as usize, Style::default())),
            rows[1],
        );
        f.render_widget(
            Paragraph::new(TextLine::from(TextSpan::styled(
                "↑↓ event · type summary · Ctrl-S submit · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[2],
        );
    }

    /// The full-screen spinner shown while a background load runs.
    fn draw_loading(&self, f: &mut Frame, stage: &str) {
        let spinner = SPINNER[self.tick % SPINNER.len()];
        let area = centered_rect(60, 20, f.area());
        let lines = vec![
            TextLine::from(""),
            TextLine::from(TextSpan::styled(
                format!("  {spinner}  {stage}"),
                Style::default().fg(Color::Cyan),
            )),
            TextLine::from(""),
            TextLine::from(TextSpan::styled(
                "  q to cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The full-screen error shown when a background load fails.
    fn draw_load_error(&self, f: &mut Frame, error: &str) {
        let area = centered_rect(70, 40, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Could not load ")
            .border_style(Style::default().fg(Color::Red));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let mut lines: Vec<TextLine> = error
            .lines()
            .map(|l| {
                TextLine::from(TextSpan::styled(
                    l.to_string(),
                    Style::default().fg(Color::White),
                ))
            })
            .collect();
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(TextSpan::styled(
            "press q to quit",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
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
        // A GitHub subject (PR or issue) only discards its local drafts here — it
        // never deletes the shared store — so the prompt must not threaten that.
        let prompt = if self.has_subject() {
            let subject = if self.is_issue() {
                "issue"
            } else {
                "pull request"
            };
            format!("Discard your local drafts for this {subject}?")
        } else {
            format!(
                "Delete all {} thread(s) and close this review?",
                self.review.threads.len()
            )
        };
        let lines = vec![
            TextLine::from(TextSpan::styled(prompt, Style::default().fg(Color::White))),
            TextLine::from(""),
            TextLine::from(TextSpan::styled(
                "y / Enter confirm · any other key cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The confirmation modal for removing a comment with `d`.
    fn draw_delete_confirm(&self, f: &mut Frame) {
        let Some(target) = &self.confirming_delete else {
            return;
        };
        let published = target.published.is_some();
        let thread = self.review.thread(&target.thread_id);
        let label = thread.map(|t| anchor_label(&t.anchor)).unwrap_or_default();
        let what = if published {
            format!(
                "Permanently delete your published comment on {label} from GitHub? This can't be undone."
            )
        } else if target.comment_id.is_some() {
            format!("Withdraw your reply on {label}?")
        } else {
            // A whole thread — name the total it takes (the root plus any replies),
            // so a delete from the index is never a blind count. The excerpt below
            // identifies which thread, so the count leads (and never truncates off
            // the end of a labelled line on a narrow terminal).
            let total = target.also_removed + 1;
            format!(
                "Withdraw this thread ({total} comment{})?",
                if total == 1 { "" } else { "s" }
            )
        };
        // Removing this empties the thread of published comments — the local notes
        // under it go too. Warn on its own line (the message above is long and
        // truncates), since those notes are the reviewer's own to lose.
        let cascade = (target.also_removed > 0).then(|| {
            let n = target.also_removed;
            format!(
                "⚠ its {n} local repl{} will be removed too",
                if n == 1 { "y" } else { "ies" }
            )
        });
        // A one-line excerpt of the exact comment, so the delete can't misfire on
        // the wrong one. A whole-thread draft (`comment_id` is `None`) shows its
        // root; otherwise the named comment.
        let excerpt = thread
            .and_then(|t| match &target.comment_id {
                Some(cid) => t.comments.iter().find(|c| c.id == *cid),
                None => t.root(),
            })
            .map(|c| one_line_excerpt(&c.body, 56))
            .unwrap_or_default();
        let mut lines = vec![
            TextLine::from(TextSpan::styled(what, Style::default().fg(Color::White))),
            TextLine::from(""),
            TextLine::from(TextSpan::styled(
                format!("“{excerpt}”"),
                Style::default().fg(Color::Gray),
            )),
        ];
        if let Some(cascade) = cascade {
            lines.push(TextLine::from(""));
            lines.push(TextLine::from(TextSpan::styled(
                cascade,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(TextSpan::styled(
            "y / Enter confirm · any other key cancel",
            Style::default().fg(Color::DarkGray),
        )));

        // Size the modal to its content so the cascade warning is never cut off.
        let screen = f.area();
        let width = (screen.width * 60 / 100).clamp(30, screen.width);
        let height = (lines.len() as u16 + 2).min(screen.height); // + borders
        let area = Rect {
            x: (screen.width.saturating_sub(width)) / 2,
            y: (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(if published {
                " Delete from GitHub? "
            } else {
                " Remove draft? "
            })
            .border_style(Style::default().fg(if published { Color::Red } else { Color::Yellow }));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The visible tab labels, left to right, each paired with its view — shared
    /// by drawing and mouse hit-testing so their widths stay in sync. Overview is
    /// present on a pull request or an issue; an issue has no Files tab.
    fn tab_labels(&self) -> Vec<(View, String)> {
        self.visible_views()
            .into_iter()
            .map(|v| {
                let label = match v {
                    View::Overview => " Overview ".to_string(),
                    View::Files => format!(" Files ({}) ", self.diff.files.len()),
                    View::Conversation => {
                        format!(" Conversation ({}) ", self.review.threads.len())
                    }
                };
                (v, label)
            })
            .collect()
    }

    /// The tab whose label covers screen column `col` on the tab row (tabs are
    /// separated by a single space, starting at column 0).
    fn tab_at_column(&self, col: u16) -> Option<View> {
        let mut x = 0u16;
        for (v, label) in self.tab_labels() {
            let w = label.chars().count() as u16;
            if col >= x && col < x + w {
                return Some(v);
            }
            x += w + 1; // the space between tabs
        }
        None
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let active = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let idle = Style::default().fg(Color::Gray);
        let mut spans = Vec::new();
        for (i, (v, label)) in self.tab_labels().into_iter().enumerate() {
            if i > 0 {
                spans.push(TextSpan::raw(" "));
            }
            spans.push(TextSpan::styled(
                label,
                if self.view == v { active } else { idle },
            ));
        }
        spans.push(TextSpan::styled(
            "   tab to switch",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(TextLine::from(spans)), area);
    }

    /// The Overview tab's rendered lines: a small facts block (number + status
    /// badge, title, author, `base ← head`, timestamps) then the markdown body.
    fn overview_lines(&self, width: usize) -> Vec<TextLine<'static>> {
        self.overview_render(width).lines
    }

    /// Build the Overview pane: the facts preamble plus the markdown body, with
    /// its clickable regions (links, images, `<details>` toggles) offset to the
    /// full line list. Caches the regions and the details' effective fold state
    /// so a click can be resolved and a toggle can flip the right index.
    fn overview_render(&self, width: usize) -> crate::markdown::Rendered {
        let Some(ov) = &self.pr_overview else {
            self.overview_regions.replace(Vec::new());
            self.overview_effective.replace(HashMap::new());
            return crate::markdown::Rendered {
                lines: vec![TextLine::from("")],
                regions: Vec::new(),
            };
        };
        let mut lines: Vec<TextLine<'static>> = Vec::new();
        // #N + the lifecycle badge.
        lines.push(TextLine::from(vec![
            TextSpan::styled(
                format!("#{}", ov.number),
                Style::default().fg(PR_ACCENT).add_modifier(Modifier::BOLD),
            ),
            TextSpan::raw("  "),
            TextSpan::styled(
                ov.status.label(),
                Style::default()
                    .fg(subject_status_color(ov.status))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        // Title.
        lines.push(TextLine::from(TextSpan::styled(
            ov.title.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        // Author · [base ← head ·] opened [· merged/closed]. An issue has no
        // branches, so base/head are absent.
        let author = if ov.author.is_empty() {
            "unknown".to_string()
        } else {
            format!("@{}", ov.author)
        };
        let mut facts = author;
        if let (Some(base), Some(head)) = (&ov.base_ref, &ov.head_ref) {
            facts.push_str(&format!("  ·  {base} ← {head}"));
        }
        if let Some(created) = ov.created_at.as_deref() {
            facts.push_str(&format!("  ·  opened {}", date_only(created)));
        }
        if let Some(closed) = ov.closed_at.as_deref() {
            let verb = if matches!(ov.status, SubjectStatus::Pr(PrStatus::Merged)) {
                "merged"
            } else {
                "closed"
            };
            facts.push_str(&format!("  ·  {verb} {}", date_only(closed)));
        }
        lines.push(TextLine::from(TextSpan::styled(
            facts,
            Style::default().fg(Color::DarkGray),
        )));
        // A dim rule marks the boundary between the facts block and the body.
        lines.push(TextLine::from(TextSpan::styled(
            "─".repeat(width.max(1)),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(TextLine::from(""));
        // The description, or a placeholder when empty (still shown, so the tab
        // is never a blank pane).
        if ov.body.trim().is_empty() {
            lines.push(TextLine::from(TextSpan::styled(
                "No description provided.",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(TextLine::from("")); // bottom breathing room at the scroll end
            self.overview_regions.replace(Vec::new());
            self.overview_effective.replace(HashMap::new());
            return crate::markdown::Rendered {
                lines,
                regions: Vec::new(),
            };
        }
        // Render the body interactively: fold state comes from `overview_folds`
        // (an override over each `<details>`'s `open` attribute), and the effective
        // state per index is recorded so a toggle flips the current value.
        let effective = RefCell::new(HashMap::new());
        let is_open = |index: usize, default: bool| {
            let open = self.overview_folds.get(&index).copied().unwrap_or(default);
            effective.borrow_mut().insert(index, open);
            open
        };
        let mut body =
            crate::markdown::render_rich(&ov.body, Some(width.max(1)), &self.highlighter, &is_open);
        let preamble = lines.len();
        for region in &mut body.regions {
            region.line += preamble;
        }
        lines.extend(body.lines);
        lines.push(TextLine::from("")); // bottom breathing room at the scroll end
        self.overview_effective.replace(effective.into_inner());
        self.overview_regions.replace(body.regions.clone());
        crate::markdown::Rendered {
            lines,
            regions: body.regions,
        }
    }

    fn draw_overview(&self, f: &mut Frame, area: Rect) {
        // overview_render caches the click regions for mouse_down as it builds.
        let lines = self.overview_render(area.width as usize).lines;
        let height = area.height as usize;
        let start = self.overview_scroll.min(lines.len().saturating_sub(1));
        let end = (start + height).min(lines.len());
        f.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
    }

    /// The click action at absolute Overview line `line`, column `col` (pane
    /// coordinates), from the regions cached by the last render.
    fn overview_action_at(&self, line: usize, col: u16) -> Option<crate::markdown::MdAction> {
        self.overview_regions
            .borrow()
            .iter()
            .find(|r| r.line == line && col >= r.start && col < r.end)
            .map(|r| r.action.clone())
    }

    /// The click action at absolute Conversation line `line`, column `col`, from
    /// the regions cached by the last draw (comment-body links/images/details).
    fn conv_action_at(&self, line: usize, col: u16) -> Option<crate::markdown::MdAction> {
        self.conv_regions
            .borrow()
            .iter()
            .find(|r| r.line == line && col >= r.start && col < r.end)
            .map(|r| r.action.clone())
    }

    /// Run a Conversation body click: open a URL, or fold/unfold a comment-body
    /// `<details>` (the toggle index routes through `conv_details` to its thread).
    fn run_conv_md_action(&mut self, action: crate::markdown::MdAction) {
        match action {
            crate::markdown::MdAction::Open(url) => {
                self.status = Some(match (self.url_opener)(&url) {
                    Ok(()) => format!("opened {url}"),
                    Err(_) => format!("open it yourself: {url}"),
                });
            }
            crate::markdown::MdAction::ToggleDetails(global) => {
                let Some(key) = self.conv_details.borrow().get(global).cloned() else {
                    return;
                };
                let current = self
                    .conv_effective
                    .borrow()
                    .get(&key)
                    .copied()
                    .unwrap_or(false);
                self.conv_folds.insert(key, !current);
                // Re-lay the conversation with the new fold (fewer/more lines) and
                // re-clamp the scroll; the selected thread/comment is unchanged.
                self.relayout();
            }
        }
    }

    /// Run a markdown click action (shared by the Overview and Conversation): open
    /// a URL, or toggle a `<details>` fold. Only the Overview emits toggles today
    /// (comment bodies pass link/image opens only), so a toggle acts on the
    /// Overview fold state and re-clamps its scroll.
    fn run_md_action(&mut self, action: crate::markdown::MdAction) {
        match action {
            crate::markdown::MdAction::Open(url) => {
                self.status = Some(match (self.url_opener)(&url) {
                    Ok(()) => format!("opened {url}"),
                    Err(_) => format!("open it yourself: {url}"),
                });
            }
            crate::markdown::MdAction::ToggleDetails(index) => {
                let current = self
                    .overview_effective
                    .borrow()
                    .get(&index)
                    .copied()
                    .unwrap_or(false);
                self.overview_folds.insert(index, !current);
                self.overview_scroll = self.overview_scroll.min(self.overview_max_scroll());
            }
        }
    }

    fn draw_conversation(&self, f: &mut Frame, area: Rect) {
        // An empty conversation still shows the tab (so the first comment can be
        // started) — greet it with a hint instead of a blank pane.
        if self.conv_order.is_empty() {
            let key = self.keymap.key_for(Action::Comment).unwrap_or("c");
            let hint = format!("No comments yet — press {key} to start a conversation.");
            let lines = vec![
                TextLine::from(""),
                TextLine::from(TextSpan::styled(hint, Style::default().fg(Color::DarkGray))),
            ];
            f.render_widget(Paragraph::new(lines), area);
            return;
        }
        let select_bg = CONV_SELECT_BG;
        let width = area.width as usize;
        let mut lines: Vec<TextLine> = Vec::new();
        // Click regions for the composed line list, absolute so mouse_down can map
        // a scrolled row straight to an action; `details` routes a toggle index to
        // its (thread id, per-thread details index).
        let mut regions: Vec<crate::markdown::MdRegion> = Vec::new();
        let mut details: Vec<(String, usize)> = Vec::new();
        for (pos, &ti) in self.conv_order.iter().enumerate() {
            let block = &self.conv_blocks[ti];
            let thread_start = lines.len();
            let selected = pos == self.conv_cursor;
            // Within the selected thread, the cursor rests on one comment; tint
            // that comment's line range (plus the header, so the thread reads as
            // active even when the cursor is on a reply).
            let (lo, hi) = if selected {
                let starts = &self.conv_comment_starts[ti];
                let ci = self.conv_comment.min(starts.len().saturating_sub(1));
                let lo = starts.get(ci).copied().unwrap_or(0);
                let hi = starts.get(ci + 1).copied().unwrap_or(block.len());
                (lo, hi)
            } else {
                (0, 0)
            };
            for (li, line) in block.iter().enumerate() {
                // The header (first line of a block) gets a full-width band, like
                // a file header; the selected comment (and its thread's header)
                // is tinted.
                let bg = if selected && (li == 0 || (li >= lo && li < hi)) {
                    Some(select_bg)
                } else if li == 0 {
                    Some(HEADER_BG)
                } else {
                    None
                };
                match bg {
                    Some(c) => {
                        let mut spans: Vec<TextSpan> = line
                            .spans
                            .iter()
                            .map(|s| TextSpan::styled(s.content.clone(), s.style.bg(c)))
                            .collect();
                        if li == 0 {
                            let used = span_width(&spans);
                            if used < width {
                                spans.push(TextSpan::styled(
                                    " ".repeat(width - used),
                                    Style::default().bg(c),
                                ));
                            }
                        }
                        lines.push(TextLine::from(spans));
                    }
                    None => lines.push(line.clone()),
                }
            }
            // This thread's body regions, shifted to their composed line indices
            // (tinting re-styles content in place, so columns are unchanged). A
            // `ToggleDetails(per-thread index)` is remapped to a conversation-wide
            // index whose `conv_details` entry routes the click back to this thread.
            let thread_id = &self.review.threads[ti].id;
            for r in &self.conv_block_regions[ti] {
                let action = match &r.action {
                    crate::markdown::MdAction::Open(url) => {
                        crate::markdown::MdAction::Open(url.clone())
                    }
                    crate::markdown::MdAction::ToggleDetails(local) => {
                        let global = details.len();
                        details.push((thread_id.clone(), *local));
                        crate::markdown::MdAction::ToggleDetails(global)
                    }
                };
                regions.push(crate::markdown::MdRegion {
                    line: r.line + thread_start,
                    start: r.start,
                    end: r.end,
                    action,
                });
            }
            lines.push(TextLine::from("")); // a blank between threads
        }
        self.conv_regions.replace(regions);
        self.conv_details.replace(details);
        let height = area.height as usize;
        let start = self.conv_scroll.min(lines.len().saturating_sub(1));
        let end = (start + height).min(lines.len());
        f.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
    }

    /// The comment-composer modal, overlaid on the body.
    fn draw_compose(&self, f: &mut Frame, compose: &Compose) {
        let area = centered_rect(80, 50, f.area());
        f.render_widget(Clear, area);
        let title = match &compose.kind {
            ComposeKind::Edit { .. } => " Edit comment ".to_string(),
            ComposeKind::New(Anchor::Review) => " Comment — conversation ".to_string(),
            ComposeKind::Reply(thread_id) => {
                let thread = self.review.thread(thread_id);
                let who = thread
                    .and_then(|t| t.root())
                    .map(|c| c.author.as_str())
                    .unwrap_or("thread");
                // A conversation reply is structurally local — say so in the
                // title, matching the footer's `r local reply`, so the reader
                // knows it will not reach GitHub before typing.
                if thread.is_some_and(|t| t.anchor == Anchor::Review) {
                    format!(" Local reply to @{who} ")
                } else {
                    format!(" Reply to @{who} ")
                }
            }
            _ if compose.suggestion => format!(" Suggest change — {} ", compose.target),
            _ => format!(" Comment on {} ", compose.target),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
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
            // The hint always names the exact key that saves here, so the two
            // roles of Ctrl-S (save inside, submit outside) never get confused.
            let text = if self.composer_enter_saves() {
                format!(
                    "Enter {} · Shift/Alt+Enter newline · Esc cancel",
                    self.compose_save_label()
                )
            } else {
                format!(
                    "Ctrl-S {} · Enter newline · Esc cancel",
                    self.compose_save_label()
                )
            };
            TextSpan::styled(text, Style::default().fg(Color::DarkGray))
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
        let mut spans = vec![TextSpan::styled(
            " loopreview ",
            bar.fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
        // The source label. On a GitHub subject it carries "#N" (e.g. "PR #N" or
        // "issue owner/repo#N"): render the text before "#" plainly and the "#N"
        // as an underlined link, recording its columns so a click on just that
        // range opens the subject's page. Elsewhere it is plain text.
        self.header_pr_link.set(None);
        let hash = self.has_subject().then(|| self.label.find('#')).flatten();
        if let Some(hash) = hash {
            // "· PR " plainly...
            spans.push(TextSpan::styled(
                format!("· {}", &self.label[..hash]),
                bar.fg(Color::Gray),
            ));
            // ...then "#N" as an underlined link; its columns start where we are.
            let x0 = area.x + span_width(&spans) as u16;
            let n_len = self.label[hash..].chars().count() as u16;
            self.header_pr_link.set(Some((x0, x0 + n_len)));
            spans.push(TextSpan::styled(
                self.label[hash..].to_string(),
                bar.fg(PR_ACCENT).add_modifier(Modifier::UNDERLINED),
            ));
            // The lifecycle badge sits just after the #N link — a separate span,
            // so it is not underlined and not inside the recorded click columns.
            if let Some(status) = self.pr_overview.as_ref().map(|o| o.status) {
                spans.push(TextSpan::styled(
                    format!(" {}", status.label()),
                    bar.fg(subject_status_color(status)),
                ));
            }
            spans.push(TextSpan::styled(" ", bar.fg(Color::Gray)));
        } else {
            spans.push(TextSpan::styled(
                format!("· {} ", self.label),
                bar.fg(Color::Gray),
            ));
        }
        // The file / +/- counters — only when a diff exists (an issue has none, so
        // "0 files +0 -0" would be noise). The layout (unified/split) lives in the
        // footer's clickable indicator, so it is dropped here as redundant.
        if !self.is_issue() {
            spans.extend([
                TextSpan::styled(
                    format!("· {} file{} ", stats.files, plural(stats.files)),
                    bar.fg(Color::Gray),
                ),
                TextSpan::styled(format!("+{} ", stats.insertions), bar.fg(Color::Green)),
                TextSpan::styled(format!("-{} ", stats.deletions), bar.fg(Color::Red)),
            ]);
        }
        if !self.review.is_empty() {
            spans.push(TextSpan::styled(
                format!("  💬 {} open", self.review.open_count()),
                bar.fg(PR_ACCENT),
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

    /// The layout indicator shown (and clickable) in the footer.
    fn layout_label(&self) -> String {
        if self.sbs() {
            "[split]".to_string()
        } else {
            "[unified]".to_string()
        }
    }

    /// The right pane's frame title. The Files view names the file the cursor is
    /// in; the Conversation view names the review context (a PR or a local
    /// review), never a filename — the threads there span files.
    fn pane_title(&self) -> String {
        if self.view == View::Conversation {
            return match (&self.pr, &self.issue) {
                (Some(pr), _) => format!(" PR #{} — {} ", pr.number(), pr.title()),
                (_, Some(issue)) => format!(" Issue #{} — {} ", issue.number(), issue.title()),
                _ => format!(" Review — {} ", self.label),
            };
        }
        match self.diff.files.get(self.current_file()) {
            Some(file) => format!(" {} ", file_name(file.display_path())),
            None => " Diff ".to_string(),
        }
    }

    /// The footer's key hints: the top few actions that [`Self::action_available`]
    /// reports for the cursor's spot, in priority order and capped so the bar
    /// stays readable. Movement keys (`j`/`k`) are universal and omitted; submit
    /// and `? all` are added by the caller. In a visual selection the bar is a
    /// dedicated sub-mode instead.
    fn footer_ops(&self) -> String {
        use Action::*;
        let key = |a: Action| self.keymap.key_for(a).unwrap_or("?").to_string();
        // The Overview is a read-only scroll pane — name only scrolling and the
        // GitHub open (the submit / `? all` anchors are added by the caller).
        if self.view == View::Overview {
            return format!(
                "{}/{} scroll · {} open on github",
                key(MoveDown),
                key(MoveUp),
                key(OpenGithub),
            );
        }
        let in_sidebar =
            self.focus == Focus::Sidebar && self.sidebar_width(self.body_width.get()).is_some();
        // A visual line selection has its own sub-mode: extend the range (a
        // context-specific move worth naming), comment or suggest over it, cancel.
        if !in_sidebar && self.view == View::Files && self.selection.is_some() {
            let mut parts = vec![
                format!("{}/{} extend", key(MoveDown), key(MoveUp)),
                format!("{} range-comment", key(Comment)),
            ];
            if self.action_available(Suggest) {
                parts.push(format!("{} suggest", key(Suggest)));
            }
            parts.push("esc cancel".to_string());
            return parts.join(" · ");
        }
        // Priority-ordered candidates per context, in "what you'd do next here"
        // order; only the available ones show, capped so the bar stays readable.
        // On a diff line the act-on-this-line trio (comment / suggest / select)
        // leads. Wherever `reply` applies (the cursor is on a comment/thread), its
        // siblings `edit` and `delete` rank right behind it, so the three that act
        // on that comment appear together when they are all yours to run.
        let candidates: &[(Action, &str)] = if in_sidebar {
            &[(NavIn, "open"), (Fold, "fold"), (Delete, "delete")]
        } else if self.view == View::Conversation {
            &[
                (Reply, "reply"),
                (Edit, "edit"),
                (Delete, "del"),
                (Comment, "comment"),
                (ToggleKind, "kind"),
                (Resolve, "resolve"),
                (Fold, "fold"),
            ]
        } else {
            &[
                (NavIn, "open"),
                (Comment, "comment"),
                (Suggest, "suggest"),
                (Select, "select"),
                (Reply, "reply"),
                (Edit, "edit"),
                (Delete, "del"),
                (ToggleKind, "kind"),
                (Resolve, "resolve"),
                (Fold, "fold"),
            ]
        };
        // Several threads on the cursor line make thread actions ambiguous; the
        // footer flags the count on `reply` (`r reply (2)`) so it is clear a pick
        // is coming — matching the Files-view picker.
        let overlap = if !in_sidebar && self.view == View::Files {
            self.threads_at_cursor().len()
        } else {
            0
        };
        // A reply on a conversation thread stays local — name it so the behaviour
        // shows in the hint, not just after the fact.
        let conv_reply_local = self.view == View::Conversation
            && self
                .selected_thread()
                .is_some_and(|t| self.review.threads[t].anchor == Anchor::Review);
        // On a fold header — a file header (Files) or a thread's root
        // (Conversation) — Enter is the tree-convention fold key, so the fold hint
        // reads `enter fold` there. `o` still folds everywhere and leads the hint
        // off a header.
        let on_fold_header = !in_sidebar
            && match self.view {
                View::Files => self.cursor_is_header(),
                View::Conversation => self.conv_on_thread_header(),
                View::Overview => false,
            };
        let mut parts = Vec::new();
        for (action, label) in candidates {
            if parts.len() >= 6 {
                break;
            }
            if self.action_available(*action) {
                let shown = if *action == Reply {
                    if conv_reply_local {
                        "local reply".to_string()
                    } else if overlap > 1 {
                        format!("{label} ({overlap})")
                    } else {
                        (*label).to_string()
                    }
                } else {
                    (*label).to_string()
                };
                let key_label = if *action == Fold && on_fold_header {
                    "enter".to_string()
                } else {
                    key(*action)
                };
                parts.push(format!("{key_label} {shown}"));
            }
        }
        parts.join(" · ")
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let bar = Style::default().bg(BAR_BG);
        // The position index: files in the diff, threads in the conversation, and
        // nothing in the Overview (a scroll pane has no file/thread position).
        let position = match self.view {
            View::Overview => String::new(),
            View::Conversation => format!(
                " [{}/{}] ",
                self.conv_cursor + 1,
                self.review.threads.len().max(1)
            ),
            View::Files => format!(
                " [{}/{}]{} ",
                self.current_file() + 1,
                self.diff.files.len().max(1),
                self.cursor_anchor()
            ),
        };
        // A clickable layout indicator leads the footer (click toggles it) — only
        // where a diff exists; an issue has no layout to switch.
        let mut spans = Vec::new();
        if !self.is_issue() {
            spans.push(TextSpan::styled(
                self.layout_label(),
                bar.fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(TextSpan::styled(position, bar.fg(Color::Cyan)));
        // Horizontal-scroll indicator (only when scrolled and viewing the diff).
        if self.hscroll > 0 && self.view == View::Files {
            spans.push(TextSpan::styled(
                format!("→{} ", self.hscroll),
                bar.fg(PR_ACCENT),
            ));
        }
        if let Some(status) = &self.status {
            spans.push(TextSpan::styled(status.clone(), bar.fg(Color::Yellow)));
        } else {
            // A slim hint built from `action_available`, so it shows only the keys
            // useful at the cursor's exact spot (and stays in lockstep with the
            // palette's grey-out). Everything else is one `? all` away.
            spans.push(TextSpan::styled(self.footer_ops(), bar.fg(Color::DarkGray)));
            if self.action_available(Action::Submit) {
                // An issue has no submit modal — Ctrl-S sends the queued comments.
                let verb = if self.is_issue() { "send" } else { "submit" };
                spans.push(TextSpan::styled(format!(" · ^s {verb}"), bar.fg(PR_ACCENT)));
            }
            let palette_key = self.keymap.key_for(Action::Palette).unwrap_or("?");
            spans.push(TextSpan::styled(
                format!(" · {palette_key} all"),
                bar.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(Paragraph::new(TextLine::from(spans)).style(bar), area);
    }

    fn cursor_anchor(&self) -> String {
        let Some((file, flat)) = self.cursor_content() else {
            return String::new(); // on a file header
        };
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        match (line.new_lineno, line.old_lineno) {
            (Some(n), _) => format!(" new:{n}"),
            (None, Some(o)) => format!(" old:{o}"),
            _ => String::new(),
        }
    }

    fn draw_body_unified(&self, f: &mut Frame, area: Rect) {
        // Clamp the scroll: a layout change (e.g. a resize that flips to
        // side-by-side, which has fewer rows) can leave it past the last row.
        let start = self.scroll.min(self.urows.len());
        let end = (start + area.height as usize).min(self.urows.len());
        let current = self.current_file();
        let cursor_header = self.cursor_is_header().then_some(current);
        let cursor_row = self.line_urow.get(self.cursor).copied();
        let lines: Vec<TextLine> = (start..end)
            .map(|i| match &self.urows[i] {
                URow::Spacer => TextLine::from(""),
                URow::FileHeader(fi) => {
                    self.file_header_line(*fi, *fi == current, cursor_header == Some(*fi))
                }
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
        let cursor_header = self.cursor_is_header().then_some(current);

        // Clamp the scroll before computing the capacity: `end - start` would
        // underflow (a huge allocation, a "capacity overflow" panic) if the
        // scroll were past the last row — which happens when a resize flips the
        // layout from unified (more rows) to side-by-side (fewer rows).
        let start = self.scroll.min(self.srows.len());
        let end = (start + area.height as usize).min(self.srows.len());
        let mut lines = Vec::with_capacity(end.saturating_sub(start));
        for i in start..end {
            let line = match &self.srows[i] {
                SRow::Spacer => TextLine::from(""),
                SRow::FileHeader(fi) => {
                    self.file_header_line(*fi, *fi == current, cursor_header == Some(*fi))
                }
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
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let (tint, emph_bg, sign, sign_color) = kind_style(line.kind);
        let dim = self.body_dimmed();
        let bg = if is_cursor {
            Some(if dim {
                CURSOR_DIM_BG
            } else {
                tint.unwrap_or(CURSOR_BG)
            })
        } else if self.in_selection(file, flat) {
            Some(SELECTION_BG)
        } else {
            tint
        };
        let base = bg.map_or_else(Style::default, |c| Style::default().bg(c));
        // Gutter marker: the cursor bar wins; else a thin comment bar on any line
        // a thread's range covers (so a multi-line comment shows on every line).
        let commented = !is_cursor && self.line_has_comment(file, flat);
        let marker = if is_cursor {
            "▎"
        } else if commented {
            "▏"
        } else {
            " "
        };
        let marker_fg = if commented {
            COMMENT_BAR
        } else if dim {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        let number = if new_side {
            line.new_lineno
        } else {
            line.old_lineno
        };
        // A fixed gutter (marker + line number + sign), then the content scrolled
        // horizontally within the remaining width.
        let gutter_width = self.num_width + 4;
        let mut spans = vec![
            TextSpan::styled(
                marker.to_string(),
                base.fg(marker_fg).add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(
                format!("{} ", optional_number(number, self.num_width)),
                base.fg(Color::DarkGray),
            ),
            TextSpan::styled(format!("{sign} "), base.fg(sign_color)),
        ];
        let content = self.content_spans(file, flat, base, emph_bg);
        let content_width = width.saturating_sub(gutter_width);
        spans.extend(clip_spans(&content, self.hscroll, content_width, base));
        spans
    }

    fn file_header_line(&self, fi: usize, is_current: bool, is_cursor: bool) -> TextLine<'static> {
        let file = &self.diff.files[fi];
        let (added, removed) = file.line_stats();
        let path = match (&file.old_path, &file.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            _ => file.display_path().to_string(),
        };
        let collapsed = self.collapsed_files.contains(file.display_path());
        // A comment badge counts threads on this file (shown even when collapsed).
        let comments = self.file_comment_count(file.display_path());
        // A chevron shows the fold state; bold/white marks the current file. When
        // the cursor rests here, a bright bar and a fill brighter than a content
        // line's cursor make the header stand out (it anchors the whole file).
        let chevron = if collapsed { "▸ " } else { "▾ " };
        let dim = self.body_dimmed();
        let base = if is_cursor {
            let bg = if dim {
                HEADER_CURSOR_DIM_BG
            } else {
                HEADER_CURSOR_BG
            };
            Style::default().bg(bg).add_modifier(Modifier::BOLD)
        } else {
            // A faint band on every header (right_aligned_row fills it full width).
            Style::default().bg(HEADER_BG)
        };
        let marker = if is_cursor { "▎" } else { " " };
        let marker_fg = if is_cursor && dim {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        let path_style = if is_current {
            base.fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            base.fg(Color::Gray).add_modifier(Modifier::BOLD)
        };
        let left = vec![
            TextSpan::styled(
                marker.to_string(),
                base.fg(marker_fg).add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(chevron, base.fg(Color::Cyan)),
        ];
        // Right-fixed cluster: the status badge, the line stats, then a comment
        // badge — always shown in full, flush to the right edge.
        let mut right = vec![
            TextSpan::styled(
                format!("[{}]", file.status.label()),
                base.fg(status_color(file.status)),
            ),
            TextSpan::styled(format!("  +{added}"), base.fg(Color::Green)),
            TextSpan::styled(format!(" -{removed}"), base.fg(Color::Red)),
        ];
        if comments > 0 {
            right.push(TextSpan::styled(
                format!("  {comments} comment{}", plural(comments)),
                base.fg(Color::Magenta),
            ));
        }
        let spans = right_aligned_row(
            left,
            &path,
            |shown| vec![TextSpan::styled(shown.to_string(), path_style)],
            right,
            self.body_width.get(),
            base,
        );
        TextLine::from(spans)
    }

    /// Number of comment threads anchored to `path` (line- or file-anchored).
    fn file_comment_count(&self, path: &str) -> usize {
        self.review
            .threads
            .iter()
            .filter(|t| t.anchor.file() == Some(path))
            .count()
    }

    /// The sidebar width for a body `total` columns wide, or `None` when the
    /// sidebar is hidden or the terminal is too narrow to keep a usable diff.
    fn sidebar_width(&self, total: usize) -> Option<usize> {
        if !self.sidebar_wanted() {
            return None;
        }
        // A pinned width (config) is honored within bounds; otherwise the width
        // auto-fits the longest file row.
        let desired = if let Some(fixed) = self.sidebar_width_cfg {
            fixed.clamp(SIDEBAR_MIN, SIDEBAR_MAX)
        } else {
            let widest = self
                .file_entries()
                .iter()
                .map(|e| {
                    // chevron (2) + status glyph (2) + path + stats + comment badge.
                    e.path.chars().count()
                        + 4
                        + format!(" +{} -{}", e.added, e.removed).chars().count()
                        + if e.comments > 0 { 4 } else { 0 }
                })
                .max()
                .unwrap_or(SIDEBAR_MIN);
            widest.clamp(SIDEBAR_MIN, SIDEBAR_MAX)
        };
        // Framing overhead beside the diff: the sidebar's two borders, a 1-col
        // gap, and the diff pane's two borders — five columns before content.
        (total >= desired + 5 + self.sidebar_min_content).then_some(desired)
    }

    /// Whether the sidebar should be shown before the width check: a `b` override
    /// if set, otherwise the configured mode (`auto`/`open` want it, `closed`
    /// does not).
    fn sidebar_wanted(&self) -> bool {
        use crate::config::SidebarMode;
        self.sidebar_override
            .unwrap_or(self.sidebar_mode != SidebarMode::Closed)
    }

    /// One file row for the sidebar or finder: a fold chevron, the tail-truncated
    /// path (with fuzzy-match highlighting when `matched` is given), the line
    /// stats, and a comment badge — padded to `width` so a selection fills it.
    fn file_row_spans(
        &self,
        entry: &FileEntry,
        label: &str,
        width: usize,
        base: Style,
        matched: &[u32],
    ) -> Vec<TextSpan<'static>> {
        let chevron = if entry.collapsed { "▸ " } else { "  " };
        let left = vec![
            TextSpan::styled(chevron, base.fg(Color::Cyan)),
            TextSpan::styled(
                format!("{} ", status_glyph(entry.status)),
                base.fg(status_color(entry.status)),
            ),
        ];
        // Right-fixed cluster: the line stats, then a comment badge.
        let mut right = vec![
            TextSpan::styled(format!("+{}", entry.added), base.fg(Color::Green)),
            TextSpan::styled(format!(" -{}", entry.removed), base.fg(Color::Red)),
        ];
        if entry.comments > 0 {
            right.push(TextSpan::styled(
                format!(" ●{}", entry.comments),
                base.fg(Color::Magenta),
            ));
        }
        right_aligned_row(
            left,
            label,
            |shown| path_highlight_spans(shown, label, matched, base.fg(Color::Gray)),
            right,
            width,
            base,
        )
    }

    /// Draw the sidebar: a file index in the Files view, a thread index in the
    /// Conversation view (the left pane always indexes the right one).
    fn draw_sidebar(&self, f: &mut Frame, area: Rect) {
        if self.view == View::Conversation {
            self.draw_thread_index(f, area);
            return;
        }
        let entries = self.file_entries();
        // The list is grouped under directory headers (a shallow tree). Headers
        // are dim, non-selectable rows; files show their basename. The cursor and
        // scroll index display rows via the mappings, so headers never confuse
        // navigation.
        let rows = sidebar_rows(&entries);
        let width = area.width as usize;
        let height = area.height as usize;
        let current = self.current_file();
        let sidebar_focused = self.focus == Focus::Sidebar;
        let start = self.sidebar_scroll.min(rows.len());
        let end = (start + height).min(rows.len());
        // Three file states are told apart by intensity in a leading marker
        // column: the sidebar cursor while the sidebar has focus (a clear blue
        // fill + a bright white bar + bold); the file open in the body (a subtle
        // blue tint + a cyan bar, always); and — when focus is in the body — the
        // sidebar's resting cursor (a faint fill + a dim bar).
        let lines: Vec<TextLine> = rows[start..end]
            .iter()
            .map(|row| match row {
                SidebarRow::DirHeader(dir) => TextLine::from(TextSpan::styled(
                    format!("  {}", head_truncate(dir, width.saturating_sub(2))),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )),
                SidebarRow::File(fi) => {
                    let e = &entries[*fi];
                    // The directory is in the header above; show the basename.
                    let base_name = &e.path[dir_of(&e.path).len()..];
                    let is_sel = e.index == self.sidebar_cursor;
                    let is_current = e.index == current;
                    let (base, marker, marker_fg) = if sidebar_focused && is_sel {
                        (
                            Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD),
                            "▎",
                            Color::White,
                        )
                    } else if is_current {
                        (Style::default().bg(SIDEBAR_CURRENT_BG), "▎", Color::Cyan)
                    } else if is_sel {
                        (
                            Style::default().bg(SIDEBAR_SEL_DIM_BG),
                            "▎",
                            Color::DarkGray,
                        )
                    } else {
                        (Style::default(), " ", Color::Reset)
                    };
                    let mut spans = vec![TextSpan::styled(
                        marker.to_string(),
                        base.fg(marker_fg).add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(self.file_row_spans(
                        e,
                        base_name,
                        width.saturating_sub(1),
                        base,
                        &[],
                    ));
                    TextLine::from(spans)
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    /// Draw the thread index (the Conversation view's sidebar): one row per
    /// thread — status glyph, anchor label, and a right-fixed author + reply
    /// count — mirroring the file index's format and selection tiers.
    fn draw_thread_index(&self, f: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let height = area.height as usize;
        let sidebar_focused = self.focus == Focus::Sidebar;
        let n = self.conv_order.len();
        let start = self.sidebar_scroll.min(n);
        let end = (start + height).min(n);
        // Rows are display positions; each maps through `conv_order` to a thread.
        let lines: Vec<TextLine> = (start..end)
            .map(|pos| {
                let ti = self.conv_order[pos];
                let thread = &self.review.threads[ti];
                let outdated = self.thread_outdated.get(ti).copied().unwrap_or(false);
                // A non-resolvable thread (a PR conversation comment) has no
                // open/resolved state — show a neutral dot instead.
                let (glyph, glyph_fg) = if self.is_resolvable(ti) {
                    thread_status(thread, outdated)
                } else {
                    ("·", Color::DarkGray)
                };
                // The selected thread is what the right pane shows: a strong fill
                // while the sidebar has focus, a subtle one when the body does.
                let is_sel = pos == self.conv_cursor;
                let (base, marker, marker_fg) = if sidebar_focused && is_sel {
                    (
                        Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD),
                        "▎",
                        Color::White,
                    )
                } else if is_sel {
                    (Style::default().bg(SIDEBAR_CURRENT_BG), "▎", Color::Cyan)
                } else {
                    (Style::default(), " ", Color::Reset)
                };
                let left = vec![
                    TextSpan::styled(
                        marker.to_string(),
                        base.fg(marker_fg).add_modifier(Modifier::BOLD),
                    ),
                    TextSpan::styled(format!("{glyph} "), base.fg(glyph_fg)),
                ];
                // Right-fixed cluster: a compact kind badge (the narrow index
                // abbreviates the wide views' `[local]`/`[draft]`), the author,
                // then the reply count.
                let replies = thread.comments.len().saturating_sub(1);
                let author = thread.root().map(|c| c.author.as_str()).unwrap_or("");
                let mut right = Vec::new();
                if let Some((badge, fg)) = thread.root().and_then(kind_index_badge) {
                    right.push(TextSpan::styled(format!(" {badge}"), base.fg(fg)));
                }
                right.push(TextSpan::styled(
                    format!(" {author}"),
                    base.fg(Color::DarkGray),
                ));
                if replies > 0 {
                    right.push(TextSpan::styled(
                        format!(" ↩{replies}"),
                        base.fg(Color::Cyan),
                    ));
                }
                let label = thread_index_label(&thread.anchor);
                let row = right_aligned_row(
                    left,
                    &label,
                    |shown| vec![TextSpan::styled(shown.to_string(), base.fg(Color::Gray))],
                    right,
                    width,
                    base,
                );
                TextLine::from(row)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    /// Draw the fuzzy file-finder overlay.
    fn draw_finder(&self, f: &mut Frame, finder: &Finder) {
        let area = centered_rect(70, 70, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" find file (type to filter, Enter to open, Esc to close) ")
            .style(Style::default().bg(FINDER_BG));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines = vec![
            TextLine::from(vec![
                TextSpan::styled("> ", Style::default().fg(Color::Cyan)),
                TextSpan::styled(finder.query.clone(), Style::default().fg(Color::White)),
                TextSpan::styled("▏", Style::default().fg(Color::Gray)),
            ]),
            TextLine::from(""),
        ];
        let entries = self.file_entries();
        let width = inner.width as usize;
        let list_height = (inner.height as usize).saturating_sub(2);
        let start = if finder.selected >= list_height {
            finder.selected + 1 - list_height
        } else {
            0
        };
        for (row, (file, indices)) in finder
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(list_height)
        {
            let base = if row == finder.selected {
                Style::default().bg(SEL_BG)
            } else {
                Style::default().bg(FINDER_BG)
            };
            if let Some(entry) = entries.get(*file) {
                // The finder shows full paths (they carry the fuzzy-match spans).
                lines.push(TextLine::from(self.file_row_spans(
                    entry,
                    &entry.path,
                    width,
                    base,
                    indices,
                )));
            }
        }
        if finder.matches.is_empty() {
            lines.push(TextLine::from(TextSpan::styled(
                "  no matching files",
                Style::default().fg(Color::DarkGray),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// Draw the `?` command palette: each action as `key · name — description`,
    /// available actions bright and unavailable ones dimmed.
    fn draw_palette(&self, f: &mut Frame, palette: &Palette) {
        let area = centered_rect(70, 70, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" commands (type to filter, Enter to run, Esc to close) ")
            .style(Style::default().bg(FINDER_BG));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines = vec![
            TextLine::from(vec![
                TextSpan::styled("> ", Style::default().fg(Color::Cyan)),
                TextSpan::styled(palette.query.clone(), Style::default().fg(Color::White)),
                TextSpan::styled("▏", Style::default().fg(Color::Gray)),
            ]),
            TextLine::from(""),
        ];
        let key_col = 8usize;
        let list_height = (inner.height as usize).saturating_sub(2);
        let start = palette
            .selected
            .saturating_sub(list_height.saturating_sub(1));
        for (row, &action_idx) in palette
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(list_height)
        {
            let action = Action::ALL[action_idx];
            let available = self.action_available(action);
            let selected = row == palette.selected;
            let base = if selected {
                Style::default().bg(SEL_BG)
            } else {
                Style::default().bg(FINDER_BG)
            };
            let (name_fg, desc_fg, key_fg) = if available {
                (Color::White, Color::Gray, Color::Cyan)
            } else {
                (Color::DarkGray, Color::DarkGray, Color::DarkGray)
            };
            let key = self.keymap.key_for(action).unwrap_or("—");
            let key_field = format!("{key:>w$} ", w = key_col.saturating_sub(1));
            lines.push(TextLine::from(vec![
                TextSpan::styled(key_field, base.fg(key_fg)),
                TextSpan::styled(
                    format!("{:<14}", action.config_name()),
                    base.fg(name_fg).add_modifier(Modifier::BOLD),
                ),
                TextSpan::styled(format!(" {}", action.describe()), base.fg(desc_fg)),
            ]));
        }
        if palette.matches.is_empty() {
            lines.push(TextLine::from(TextSpan::styled(
                "  no matching commands",
                Style::default().fg(Color::DarkGray),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// Draw the thread-disambiguation picker: one row per candidate covering the
    /// cursor line — its range, author, kind/state badge, and a body excerpt —
    /// numbered for a direct pick.
    fn draw_thread_picker(&self, f: &mut Frame, picker: &ThreadPicker) {
        let screen = f.area();
        let rows = picker.candidates.len() as u16 + 2; // items + borders
        let width = 72.min(screen.width.saturating_sub(4));
        let height = rows.min(screen.height);
        let area = Rect {
            x: (screen.width.saturating_sub(width)) / 2,
            y: (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" pick a comment (j/k or 1-9 · Enter · Esc) ")
            .style(Style::default().bg(FINDER_BG));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines = Vec::new();
        for (row, &ti) in picker.candidates.iter().enumerate() {
            let thread = &self.review.threads[ti];
            let range = match &thread.anchor {
                Anchor::Line { start, end, .. } if start == end => format!("L{start}"),
                Anchor::Line { start, end, .. } => format!("L{start}-{end}"),
                _ => "—".to_string(),
            };
            let root = thread.root();
            let author = root.map(|c| c.author.as_str()).unwrap_or("?");
            let excerpt = {
                let first = root.and_then(|c| c.body.lines().next()).unwrap_or("");
                if first.chars().count() > 40 {
                    first.chars().take(39).collect::<String>() + "…"
                } else {
                    first.to_string()
                }
            };
            let selected = row == picker.selected;
            let base = if selected {
                Style::default().bg(SEL_BG)
            } else {
                Style::default().bg(FINDER_BG)
            };
            let mut spans = vec![
                TextSpan::styled(
                    format!(" {} ", row + 1),
                    base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                TextSpan::styled(format!("{range:<9} "), base.fg(Color::Gray)),
                TextSpan::styled(
                    format!("{author} "),
                    base.fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some((badge, color)) = root.and_then(kind_badge) {
                spans.push(TextSpan::styled(format!("{badge} "), base.fg(color)));
            }
            if thread.is_resolved() {
                spans.push(TextSpan::styled("[resolved] ", base.fg(Color::DarkGray)));
            }
            spans.push(TextSpan::styled(excerpt, base.fg(Color::Gray)));
            lines.push(TextLine::from(spans));
        }
        f.render_widget(Paragraph::new(lines), inner);
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
        let (hi, li) = self.flats[file][flat];
        let line = &self.diff.files[file].hunks[hi].lines[li];
        let (tint, emph_bg, sign, sign_color) = kind_style(line.kind);
        let dim = self.body_dimmed();
        let bg = if is_cursor {
            Some(if dim {
                CURSOR_DIM_BG
            } else {
                tint.unwrap_or(CURSOR_BG)
            })
        } else if self.in_selection(file, flat) {
            Some(SELECTION_BG)
        } else {
            tint
        };
        let base = bg.map_or_else(Style::default, |c| Style::default().bg(c));
        // Gutter marker: cursor bar wins, else a thin comment bar on covered lines.
        let commented = !is_cursor && self.line_has_comment(file, flat);
        let marker = if is_cursor {
            "▎"
        } else if commented {
            "▏"
        } else {
            " "
        };
        let marker_fg = if commented {
            COMMENT_BAR
        } else if dim {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        let old = optional_number(line.old_lineno, self.num_width);
        let new = optional_number(line.new_lineno, self.num_width);
        // A fixed gutter (marker + old/new numbers + sign), then the content
        // scrolled horizontally within the remaining width.
        let gutter_width = 1 + (2 * self.num_width + 2) + 2;
        let mut spans = vec![
            TextSpan::styled(
                marker.to_string(),
                base.fg(marker_fg).add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(format!("{old} {new} "), base.fg(Color::DarkGray)),
            TextSpan::styled(format!("{sign} "), base.fg(sign_color)),
        ];
        let content = self.content_spans(file, flat, base, emph_bg);
        let content_width = self.body_width.get().saturating_sub(gutter_width);
        spans.extend(clip_spans(&content, self.hscroll, content_width, base));
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
        self.ensure_highlight(file, flat);
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
/// The last path component (the filename).
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A titled pane frame whose border and title accent when the pane holds focus
/// (a bright bold accent) and dim otherwise, so focus is obvious at a glance.
fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default()
            .fg(FOCUS_ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(TextLine::from(TextSpan::styled(title, style)))
}

/// One changed file, as shown in the sidebar and the file finder.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEntry {
    /// Index into the diff's files.
    index: usize,
    /// The file's display path.
    path: String,
    /// How the file changed (added/deleted/modified/renamed/copied).
    status: loopreview_core::ChangeStatus,
    /// Added lines.
    added: u32,
    /// Removed lines.
    removed: u32,
    /// Number of comment threads on the file.
    comments: usize,
    /// Whether the file is currently collapsed.
    collapsed: bool,
}

/// Rank `entries` against a fuzzy `query`, best match first. An empty query
/// keeps the diff's order (every file). Returns each match's index into
/// `entries` together with the matched character positions in its path (for
/// highlighting).
/// Rank the actions for the command palette: an empty query lists them all in
/// [`Action::ALL`] order, available ones first; a query fuzzy-matches each
/// action's name and description, ranking available matches above unavailable.
/// Returns indices into [`Action::ALL`]. `available` is index-aligned to it.
fn fuzzy_actions(query: &str, available: &[bool]) -> Vec<usize> {
    let all = Action::ALL;
    if query.is_empty() {
        let mut idx: Vec<usize> = (0..all.len()).collect();
        idx.sort_by_key(|&i| !available[i]); // available (true) first, stable
        return idx;
    }
    let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, bool, u32)> = Vec::new();
    for (i, action) in all.iter().enumerate() {
        let hay = format!("{} {}", action.config_name(), action.describe());
        let haystack = Utf32Str::new(&hay, &mut buf);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            scored.push((i, available[i], score));
        }
    }
    // Available first, then higher fuzzy score.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));
    scored.into_iter().map(|(i, _, _)| i).collect()
}

fn fuzzy_files(entries: &[FileEntry], query: &str) -> Vec<(usize, Vec<u32>)> {
    if query.is_empty() {
        return (0..entries.len()).map(|i| (i, Vec::new())).collect();
    }
    let mut matcher = Matcher::new(NucleoConfig::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32, Vec<u32>)> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let haystack = Utf32Str::new(&entry.path, &mut buf);
        let mut indices = Vec::new();
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut indices) {
            indices.sort_unstable();
            indices.dedup();
            scored.push((i, score, indices));
        }
    }
    // Higher score first; break ties toward the shorter path (a closer match).
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| entries[a.0].path.len().cmp(&entries[b.0].path.len()))
    });
    scored.into_iter().map(|(i, _, idx)| (i, idx)).collect()
}

/// Render `shown` (a possibly head-truncated `path`) with the fuzzy-matched
/// characters emphasized. Highlighting is applied only when `shown` is the full
/// path (indices map directly); a truncated path renders without per-char
/// emphasis.
fn path_highlight_spans(
    shown: &str,
    full: &str,
    matched: &[u32],
    base: Style,
) -> Vec<TextSpan<'static>> {
    if matched.is_empty() || shown != full {
        return vec![TextSpan::styled(shown.to_string(), base)];
    }
    let set: HashSet<u32> = matched.iter().copied().collect();
    let hl = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_hl = false;
    for (i, ch) in shown.chars().enumerate() {
        let is_hl = set.contains(&(i as u32));
        if !run.is_empty() && is_hl != run_hl {
            spans.push(TextSpan::styled(
                std::mem::take(&mut run),
                if run_hl { hl } else { base },
            ));
        }
        run_hl = is_hl;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(TextSpan::styled(run, if run_hl { hl } else { base }));
    }
    spans
}

/// Truncate `path` to at most `max` display columns by dropping its head (with a
/// leading `…`), so the filename at the tail is always kept. Widths use
/// unicode-width, so a CJK name (2 columns per glyph) does not misalign.
fn truncate_path_head(path: &str, max: usize) -> String {
    if str_width(path) <= max {
        return path.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    // Keep as much of the tail as fits in `max - 1` columns (1 for the ellipsis),
    // never splitting a wide glyph.
    let budget = max - 1;
    let mut kept: Vec<char> = Vec::new();
    let mut w = 0;
    for ch in path.chars().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        w += cw;
        kept.push(ch);
    }
    kept.reverse();
    let tail: String = kept.into_iter().collect();
    format!("…{tail}")
}

/// Display width of a string in terminal columns (unicode-width).
fn str_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Display width of styled spans, in terminal columns.
fn span_width(spans: &[TextSpan<'static>]) -> usize {
    spans.iter().map(|s| str_width(&s.content)).sum()
}

/// Lay out a file row as `left` (marker/chevron) + a head-truncated filename + a
/// flexible gap + a right-fixed `right` cluster (stats and badges), to exactly
/// `width` display columns. The right cluster is the priority: it is shown in
/// full whenever it fits, the filename is minimized first (at least one column
/// of gap is always kept), and only when the cluster alone overflows the row is
/// it clipped. `build_path` styles the shown (possibly truncated) filename.
fn right_aligned_row(
    left: Vec<TextSpan<'static>>,
    path: &str,
    build_path: impl FnOnce(&str) -> Vec<TextSpan<'static>>,
    right: Vec<TextSpan<'static>>,
    width: usize,
    fill: Style,
) -> Vec<TextSpan<'static>> {
    let lw = span_width(&left);
    let rw = span_width(&right);
    // Reserve the cluster and a one-column gap first; the rest is the filename.
    let path_budget = width.saturating_sub(lw + rw + 1);
    let mut out = left;
    if path_budget > 0 {
        out.extend(build_path(&truncate_path_head(path, path_budget)));
    }
    let used = span_width(&out);
    if used + rw <= width {
        out.push(TextSpan::styled(" ".repeat(width - used - rw), fill));
        out.extend(right);
    } else {
        // Too narrow even for the cluster: it wins, clipped to what remains.
        out.extend(clip_spans(&right, 0, width.saturating_sub(used), fill));
    }
    out
}

/// Take the `width` display columns of `spans` starting at column `start`,
/// padding the end with `fill`. A wide (CJK) character straddling either edge is
/// replaced by spaces for its visible half, so nothing is cut in the middle.
/// Widths are measured with `unicode-width`. With `start = 0` this is a
/// width-aware `fit`; a positive `start` is the horizontal-scroll offset.
fn clip_spans(
    spans: &[TextSpan<'static>],
    start: usize,
    width: usize,
    fill: Style,
) -> Vec<TextSpan<'static>> {
    let end = start + width;
    let mut out: Vec<TextSpan<'static>> = Vec::new();
    let mut col = 0usize;
    let mut emitted = 0usize;
    'spans: for span in spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            let (cs, ce) = (col, col + w);
            col = ce;
            if ce <= start {
                continue; // entirely before the window
            }
            if cs >= end {
                if !buf.is_empty() {
                    out.push(TextSpan::styled(std::mem::take(&mut buf), span.style));
                }
                break 'spans; // entirely past the window
            }
            if cs < start || ce > end {
                // Straddles an edge: pad its visible half with spaces.
                let visible = ce.min(end) - cs.max(start);
                for _ in 0..visible {
                    buf.push(' ');
                }
                emitted += visible;
            } else {
                buf.push(ch);
                emitted += w;
            }
        }
        if !buf.is_empty() {
            out.push(TextSpan::styled(buf, span.style));
        }
    }
    if emitted < width {
        out.push(TextSpan::styled(" ".repeat(width - emitted), fill));
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

/// Clip a string to `max` characters keeping its tail (a leading `…`), so a
/// long directory header shows its most specific segments.
fn head_truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n + 1 - max).collect();
    format!("…{tail}")
}

/// The directory portion of a path (through the last `/`), or `""` at the root.
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "",
    }
}

/// Which end of the sidebar file list to jump to (`g` / `G`).
#[derive(Clone, Copy)]
enum SidebarEnd {
    First,
    Last,
}

/// One rendered row of the file sidebar. A `DirHeader` is a dim, non-selectable
/// grouping label; a `File` carries an index into the diff's files (the cursor
/// lands only on these).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SidebarRow {
    /// A directory grouping header (its path, with a trailing slash).
    DirHeader(String),
    /// A file row (index into `review`/diff files).
    File(usize),
}

/// The file sidebar as a list of display rows, grouping files under directory
/// headers: root files come first with no header, then each directory (in
/// first-appearance order) as a header followed by its files. A pure function of
/// the entries so the row model and its mappings are testable in isolation.
fn sidebar_rows(entries: &[FileEntry]) -> Vec<SidebarRow> {
    let mut root: Vec<usize> = Vec::new();
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for entry in entries {
        let dir = dir_of(&entry.path);
        if dir.is_empty() {
            root.push(entry.index);
        } else if let Some((_, files)) = groups.iter_mut().find(|(d, _)| d == dir) {
            files.push(entry.index);
        } else {
            groups.push((dir.to_string(), vec![entry.index]));
        }
    }
    let mut rows: Vec<SidebarRow> = root.into_iter().map(SidebarRow::File).collect();
    for (dir, files) in groups {
        rows.push(SidebarRow::DirHeader(dir));
        rows.extend(files.into_iter().map(SidebarRow::File));
    }
    rows
}

/// The display-row position of a file (for scroll/reveal), or `None`.
fn row_of_file(rows: &[SidebarRow], file: usize) -> Option<usize> {
    rows.iter().position(|r| *r == SidebarRow::File(file))
}

/// The file at a display row, or `None` for a header row (click routing).
fn file_at_row(rows: &[SidebarRow], row: usize) -> Option<usize> {
    match rows.get(row) {
        Some(SidebarRow::File(i)) => Some(*i),
        _ => None,
    }
}

/// The file indices in display order — the sequence `j`/`k` steps through,
/// skipping headers.
fn sidebar_file_order(rows: &[SidebarRow]) -> Vec<usize> {
    rows.iter()
        .filter_map(|r| match r {
            SidebarRow::File(i) => Some(*i),
            SidebarRow::DirHeader(_) => None,
        })
        .collect()
}

/// The one-character status marker for a file's change kind (`A`/`D`/`M`/`R`/`C`).
fn status_glyph(status: loopreview_core::ChangeStatus) -> char {
    use loopreview_core::ChangeStatus::*;
    match status {
        Added => 'A',
        Deleted => 'D',
        Modified => 'M',
        Renamed => 'R',
        Copied => 'C',
    }
}

/// Width the inline comment body wraps to (before the gutter bar).
const INLINE_COMMENT_WRAP: usize = 76;
/// Max lines in a placed thread's code excerpt (clipped tail-first beyond it).
const EXCERPT_MAX: usize = 8;

/// Render each thread's inline block (index-aligned to `review.threads`): a
/// header naming the author and state, then the root comment's body as markdown.
fn build_comment_blocks(
    review: &Review,
    highlighter: &Highlighter,
    collapsed: &HashSet<String>,
) -> Vec<Vec<TextLine<'static>>> {
    let bar = Style::default().fg(COMMENT_BAR);
    review
        .threads
        .iter()
        .map(|thread| {
            let is_collapsed = collapsed.contains(&thread.id);
            let author = thread.root().map(|c| c.author.clone()).unwrap_or_default();
            let mut header = vec![
                TextSpan::styled("  ▏ ", bar),
                TextSpan::styled(if is_collapsed { "▸ " } else { "▾ " }, bar),
                TextSpan::styled("💬 ", Style::default().fg(Color::Cyan)),
                TextSpan::styled(
                    author,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some((badge, fg)) = thread.root().and_then(kind_badge) {
                header.push(TextSpan::styled(
                    format!("  {badge}"),
                    Style::default().fg(fg),
                ));
            }
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
            let mut lines = vec![TextLine::from(header)];
            if !is_collapsed && let Some(root) = thread.root() {
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
#[allow(clippy::too_many_arguments)]
/// Each thread's rendered block, paired with the block-line index where each of
/// its comments (root first, then replies) begins — so the Conversation view can
/// highlight the one comment the cursor rests on. A collapsed thread's only stop
/// is its header line (`[0]`).
type ConversationBuild = (
    Vec<Vec<TextLine<'static>>>,
    Vec<Vec<usize>>,
    Vec<Vec<crate::markdown::MdRegion>>,
);

#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
fn build_conversation(
    review: &Review,
    diff: &Diff,
    width: usize,
    highlighter: &Highlighter,
    outdated: &[bool],
    collapsed: &HashSet<String>,
    repo_dir: Option<&Path>,
    folds: &HashMap<(String, usize), bool>,
    effective: &RefCell<HashMap<(String, usize), bool>>,
) -> ConversationBuild {
    let now = now();
    effective.borrow_mut().clear();
    let built: Vec<(
        Vec<TextLine<'static>>,
        Vec<usize>,
        Vec<crate::markdown::MdRegion>,
    )> = review
        .threads
        .iter()
        .enumerate()
        .map(|(ti, thread)| {
            let is_outdated = outdated.get(ti).copied().unwrap_or(false);
            let is_collapsed = collapsed.contains(&thread.id);
            let mut lines = Vec::new();
            // Click regions (links/images/`<details>` toggles) over this thread's
            // comment bodies, block-relative. A comment body's `<details>` folds
            // by (thread id, its 0-based index in the body), from `folds`.
            let mut regions: Vec<crate::markdown::MdRegion> = Vec::new();
            let is_open = |index: usize, default: bool| {
                let open = folds
                    .get(&(thread.id.clone(), index))
                    .copied()
                    .unwrap_or(default);
                effective
                    .borrow_mut()
                    .insert((thread.id.clone(), index), open);
                open
            };
            // An informative one-line header (the thread index's vocabulary): a
            // fold chevron, the status glyph, the anchor, the author, and the
            // reply count — so a collapsed thread still says what it is.
            let (glyph, glyph_fg) = thread_status(thread, is_outdated);
            let author = thread.root().map(|c| c.author.clone()).unwrap_or_default();
            let replies = thread.replies().len();
            let mut header = vec![
                TextSpan::styled(
                    if is_collapsed { "▸ " } else { "▾ " },
                    Style::default().fg(Color::DarkGray),
                ),
                TextSpan::styled(format!("{glyph} "), Style::default().fg(glyph_fg)),
                TextSpan::styled(
                    anchor_label(&thread.anchor),
                    Style::default().fg(Color::Gray),
                ),
                TextSpan::styled(
                    format!("  {author}"),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some((badge, fg)) = thread.root().and_then(kind_badge) {
                header.push(TextSpan::styled(
                    format!("  {badge}"),
                    Style::default().fg(fg),
                ));
            }
            if replies > 0 {
                header.push(TextSpan::styled(
                    format!("  ↩{replies}"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if is_outdated {
                header.push(TextSpan::styled(
                    "  [outdated]",
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(TextLine::from(header));

            if is_collapsed {
                // A collapsed thread has a single stop: its header line.
                return (lines, vec![0], regions);
            }

            // Track where each comment's meta line lands within the block.
            let mut comment_starts: Vec<usize> = Vec::with_capacity(thread.comments.len());

            // For an outdated thread, show the line it was left on: reconstructed
            // from history (`git show <commit>:<path>`), or the saved snippet.
            if is_outdated {
                match reconstruct_outdated(repo_dir, &thread.anchor) {
                    Some(reconstructed) => lines.extend(reconstructed),
                    None => {
                        if let Anchor::Line { context, .. } = &thread.anchor {
                            for snippet in context {
                                lines.push(TextLine::from(TextSpan::styled(
                                    format!("  │ {snippet}"),
                                    Style::default().fg(SNIPPET_FG),
                                )));
                            }
                        }
                    }
                }
            } else if matches!(thread.anchor, Anchor::Line { .. }) {
                // A placed line-anchored thread shows the code it comments on,
                // from the current diff (the same look as the reconstruction).
                lines.extend(build_excerpt(
                    diff,
                    &thread.anchor,
                    highlighter,
                    EXCERPT_MAX,
                ));
            }

            // Details indices are per-thread (offset across the root and replies)
            // so root/reply folds never collide on the same (thread id, index) key.
            let mut details_base = 0usize;

            if let Some(root) = thread.root() {
                comment_starts.push(lines.len());
                lines.push(comment_meta_line(root, now, false));
                let base = lines.len();
                let seen = std::cell::Cell::new(0usize);
                let dbase = details_base;
                let is_open_body = |local: usize, default: bool| {
                    seen.set(seen.get().max(local + 1));
                    is_open(dbase + local, default)
                };
                let rendered = crate::markdown::render_rich(
                    &root.body,
                    Some(width),
                    highlighter,
                    &is_open_body,
                );
                collect_regions(&rendered.regions, base, 0, dbase, &mut regions);
                details_base += seen.get();
                lines.extend(rendered.lines);
            }
            for reply in thread.replies() {
                comment_starts.push(lines.len());
                lines.push(comment_meta_line(reply, now, true));
                let base = lines.len();
                let seen = std::cell::Cell::new(0usize);
                let dbase = details_base;
                let is_open_body = |local: usize, default: bool| {
                    seen.set(seen.get().max(local + 1));
                    is_open(dbase + local, default)
                };
                let rendered = crate::markdown::render_rich(
                    &reply.body,
                    Some(width.saturating_sub(2)),
                    highlighter,
                    &is_open_body,
                );
                collect_regions(&rendered.regions, base, 2, dbase, &mut regions);
                details_base += seen.get();
                for line in rendered.lines {
                    let mut spans = vec![TextSpan::raw("  ")]; // a reply is indented 2 cols
                    spans.extend(line.spans);
                    lines.push(TextLine::from(spans));
                }
            }
            if comment_starts.is_empty() {
                comment_starts.push(0);
            }
            (lines, comment_starts, regions)
        })
        .collect();
    let mut blocks = Vec::with_capacity(built.len());
    let mut starts = Vec::with_capacity(built.len());
    let mut all_regions = Vec::with_capacity(built.len());
    for (b, s, r) in built {
        blocks.push(b);
        starts.push(s);
        all_regions.push(r);
    }
    (blocks, starts, all_regions)
}

/// Copy a rendered comment body's regions into `out`, offsetting each by the
/// body's `base` line in the block and a left `indent`. A `ToggleDetails(local)`
/// index is shifted by `details_base` to the per-thread index space (so a fold in
/// the root and one in a reply do not collide).
fn collect_regions(
    regions: &[crate::markdown::MdRegion],
    base: usize,
    indent: u16,
    details_base: usize,
    out: &mut Vec<crate::markdown::MdRegion>,
) {
    for region in regions {
        let action = match &region.action {
            crate::markdown::MdAction::Open(url) => crate::markdown::MdAction::Open(url.clone()),
            crate::markdown::MdAction::ToggleDetails(local) => {
                crate::markdown::MdAction::ToggleDetails(local + details_base)
            }
        };
        out.push(crate::markdown::MdRegion {
            line: region.line + base,
            start: region.start + indent,
            end: region.end + indent,
            action,
        });
    }
}

/// The author/timestamp line for a comment; replies are marked and indented.
/// A `[local]`/`[draft]` badge follows when the comment is unpublished.
fn comment_meta_line(comment: &Comment, now: u64, reply: bool) -> TextLine<'static> {
    let prefix = if reply { "  ↳ " } else { "" };
    let mut spans = vec![
        TextSpan::styled(prefix, Style::default().fg(Color::DarkGray)),
        TextSpan::styled(
            comment.author.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        TextSpan::styled(
            format!("  · {}", relative_time(comment.created_at, now)),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some((badge, fg)) = kind_badge(comment) {
        spans.push(TextSpan::styled(
            format!("  {badge}"),
            Style::default().fg(fg),
        ));
    }
    TextLine::from(spans)
}

/// A code excerpt for a placed line-anchored thread: the anchored range plus a
/// few preceding context lines from the current diff, syntax-highlighted with a
/// line-number gutter and +/- markers (the diff view's pipeline), matching the
/// outdated reconstruction's look. Empty when the file/line is not in the diff.
/// Clipped tail-first (with a leading `…`) beyond `max` lines.
fn build_excerpt(
    diff: &Diff,
    anchor: &Anchor,
    highlighter: &Highlighter,
    max: usize,
) -> Vec<TextLine<'static>> {
    let Anchor::Line {
        file,
        side,
        start,
        end,
        ..
    } = anchor
    else {
        return Vec::new();
    };
    let Some(fd) = diff.files.iter().find(|f| f.display_path() == file) else {
        return Vec::new();
    };
    let flat: Vec<&Line> = fd.hunks.iter().flat_map(|h| h.lines.iter()).collect();
    let lineno = |l: &Line| match side {
        Side::New => l.new_lineno,
        Side::Old => l.old_lineno,
    };
    let in_range = |l: &Line| lineno(l).is_some_and(|n| n >= *start && n <= *end);
    let Some(first) = flat.iter().position(|l| in_range(l)) else {
        return Vec::new(); // the anchored line is not in the current diff
    };
    let last = flat.iter().rposition(|l| in_range(l)).unwrap_or(first);
    let to = last + 1;
    let mut from = first.saturating_sub(3); // a few lines of leading context
    let clipped = to - from > max;
    if clipped {
        from = to - max; // keep the tail (the anchored range) when too long
    }
    // Warm the highlighter from the file start so multi-line constructs (strings,
    // comments) are correct at the excerpt, then emit the visible window.
    let mut state = highlighter.line_highlighter(file);
    let theme = highlighter.theme_highlighter();
    for l in &flat[..from] {
        let _ = highlighter.highlight_next(&mut state, &theme, &l.content);
    }
    let num_width = flat[from..to]
        .iter()
        .filter_map(|l| lineno(l))
        .map(digits)
        .max()
        .unwrap_or(3)
        .max(3);
    let mut out = Vec::new();
    if clipped {
        out.push(TextLine::from(TextSpan::styled(
            "  …",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for l in &flat[from..to] {
        let spans = highlighter.highlight_next(&mut state, &theme, &l.content);
        let (marker, mcolor) = match l.kind {
            LineKind::Addition => ("+", Color::Green),
            LineKind::Deletion => ("-", Color::Red),
            LineKind::Context => (" ", Color::DarkGray),
        };
        let anchored = in_range(l);
        let mut line_spans = vec![
            TextSpan::styled(
                format!("  {} ", optional_number(lineno(l), num_width)),
                Style::default().fg(Color::DarkGray),
            ),
            TextSpan::styled(format!("{marker} "), Style::default().fg(mcolor)),
        ];
        for s in &spans {
            let mut style = Style::default().fg(rgb(s.color));
            if anchored {
                style = style.bg(EXCERPT_ANCHOR_BG);
            }
            line_spans.push(TextSpan::styled(s.text.clone(), style));
        }
        out.push(TextLine::from(line_spans));
    }
    out
}

/// Reconstruct the lines around an outdated thread's anchor from history:
/// `git show <commit>:<path>`, a few lines either side of the anchored line.
/// Returns `None` when there is no commit/repo or the file can't be read.
fn reconstruct_outdated(
    repo_dir: Option<&Path>,
    anchor: &Anchor,
) -> Option<Vec<TextLine<'static>>> {
    let Anchor::Line {
        file,
        commit: Some(commit),
        start,
        ..
    } = anchor
    else {
        return None;
    };
    let content = loopreview_core::git::show_file(repo_dir?, commit, file)?;
    let all: Vec<&str> = content.lines().collect();
    if all.is_empty() {
        return None;
    }
    let center = (*start as usize).saturating_sub(1).min(all.len() - 1);
    let from = center.saturating_sub(3);
    let to = (center + 4).min(all.len());
    let mut out = Vec::with_capacity(to - from);
    for (offset, text) in all[from..to].iter().enumerate() {
        let number = from + offset + 1;
        let is_anchor = number as u32 == *start;
        let text_style = if is_anchor {
            Style::default().fg(Color::White).bg(EXCERPT_ANCHOR_BG)
        } else {
            Style::default().fg(EXCERPT_CONTEXT_FG)
        };
        out.push(TextLine::from(vec![
            TextSpan::styled(
                format!("  {} {number:>4} ", if is_anchor { "▸" } else { " " }),
                Style::default().fg(Color::DarkGray),
            ),
            TextSpan::styled(text.to_string(), text_style),
        ]));
    }
    Some(out)
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
/// The header-badge color for a PR status, following GitHub's color semantics:
/// open is green, merged is magenta/purple, closed is red, a draft is dim gray.
fn subject_status_color(status: SubjectStatus) -> Color {
    use loopreview_github::IssueStatus;
    match status {
        SubjectStatus::Pr(PrStatus::Open) => Color::Green,
        SubjectStatus::Pr(PrStatus::Merged) => Color::Magenta,
        SubjectStatus::Pr(PrStatus::Closed) => Color::Red,
        SubjectStatus::Pr(PrStatus::Draft) => Color::DarkGray,
        // An issue: open green, closed (completed) magenta, not planned dim.
        SubjectStatus::Issue(IssueStatus::Open) => Color::Green,
        SubjectStatus::Issue(IssueStatus::Closed) => Color::Magenta,
        SubjectStatus::Issue(IssueStatus::NotPlanned) => Color::DarkGray,
    }
}

/// The date portion (`YYYY-MM-DD`) of an ISO-8601 timestamp, for the Overview
/// facts — no clock time, and no date-library dependency to parse it.
fn date_only(iso: &str) -> &str {
    iso.split('T').next().unwrap_or(iso)
}

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
        // "conversation" everywhere in the UI; "changeset" stays design-internal.
        Anchor::Review => "conversation".to_string(),
    }
}

/// The display order of threads: by root-comment time (ascending), ties broken
/// by thread id for stability. Returns `review.threads` indices, so storage
/// order (which `conv_blocks` / `thread_outdated` align to) is left untouched.
fn conv_display_order(review: &Review) -> Vec<usize> {
    let mut order: Vec<usize> = (0..review.threads.len()).collect();
    order.sort_by_key(|&i| {
        let t = &review.threads[i];
        (t.root().map(|c| c.created_at).unwrap_or(0), t.id.clone())
    });
    order
}

/// A compact anchor label for the thread index: `file:line` / `file` /
/// `conversation` (no side suffix — the index is narrow).
fn thread_index_label(anchor: &Anchor) -> String {
    match anchor {
        Anchor::Line {
            file, start, end, ..
        } => {
            if start == end {
                format!("{file}:{start}")
            } else {
                format!("{file}:{start}-{end}")
            }
        }
        Anchor::File { file } => file.clone(),
        Anchor::Review => "conversation".to_string(),
    }
}

/// The first line of `body`, trimmed and clipped to `max` characters with a
/// trailing ellipsis — a compact preview of a comment (e.g. in the delete
/// confirmation, so the removal can't misfire on the wrong one).
fn one_line_excerpt(body: &str, max: usize) -> String {
    let first = body.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        let clipped: String = first.chars().take(max.saturating_sub(1)).collect();
        format!("{clipped}…")
    } else {
        first.to_string()
    }
}

/// Turn a raw `gh` write failure into an actionable status line. A permission
/// (403) or not-found (404) error on a comment write almost always means the
/// comment isn't the viewer's own, or `gh` auth has lapsed.
fn friendly_github_write_error(reason: String) -> String {
    let low = reason.to_lowercase();
    // GitHub allows only one pending review per pull request; a leftover one
    // makes a fresh comment/reply POST 422. Point at the fix (it lives on GitHub).
    if low.contains("pending") && low.contains("review") {
        return "a pending review already exists on GitHub — submit or discard it there first"
            .to_string();
    }
    if low.contains("403")
        || low.contains("forbidden")
        || low.contains("404")
        || low.contains("not found")
    {
        "GitHub refused the write — check the comment is yours and your `gh` auth is current"
            .to_string()
    } else {
        reason
    }
}

/// The single published-comment guard the agent control-plane writes share.
///
/// An agent may only touch what is not yet on GitHub: editing, resolving, or
/// removing something already published is a human action. If any of `comments`
/// is published (carries a remote id — see [`Comment::is_published`]), the write
/// is refused with `refusal`; otherwise it is allowed. Centralizing the check
/// keeps `comment edit` / `comment resolve` / `comment rm` agreeing on exactly
/// what "published" means.
fn guard_agent_write<'a>(
    comments: impl IntoIterator<Item = &'a Comment>,
    refusal: &str,
) -> Result<(), String> {
    if comments.into_iter().any(Comment::is_published) {
        Err(refusal.to_string())
    } else {
        Ok(())
    }
}

/// The disposition badge for a comment: `[local]` (subdued — never sent) or
/// `[draft]` (attention — queued to submit). A published comment (on GitHub, or
/// pulled with no addressable id) has none.
fn kind_badge(comment: &Comment) -> Option<(&'static str, Color)> {
    match comment.disposition() {
        CommentKind::Published => None,
        CommentKind::Local => Some(("[local]", BADGE_LOCAL)),
        CommentKind::Draft => Some(("[draft]", BADGE_DRAFT)),
    }
}

/// The compact index form of [`kind_badge`]: `[l]`/`[d]` for the narrow
/// sidebar, with the same colors. A published comment has none.
fn kind_index_badge(comment: &Comment) -> Option<(&'static str, Color)> {
    match comment.disposition() {
        CommentKind::Published => None,
        CommentKind::Local => Some(("[l]", BADGE_LOCAL)),
        CommentKind::Draft => Some(("[d]", BADGE_DRAFT)),
    }
}

/// The status glyph and color for a thread in the index.
fn thread_status(thread: &Thread, outdated: bool) -> (&'static str, Color) {
    if thread.is_resolved() {
        ("✓", Color::Green)
    } else if outdated {
        ("⚠", Color::Yellow)
    } else {
        ("○", Color::Cyan)
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
    fn build(
        diff: &Diff,
        review: &Review,
        block_lens: &[usize],
        collapsed_files: &HashSet<String>,
    ) -> Layouts {
        // Which line-anchored threads are present in the diff (placed); an absent
        // one is outdated. Computed from the diff, independent of collapse — a
        // collapsed file's threads are present, just hidden.
        let mut placed = vec![false; review.threads.len()];
        for (idx, thread) in review.threads.iter().enumerate() {
            if let Anchor::Line {
                file, side, end, ..
            } = &thread.anchor
            {
                placed[idx] = line_present(diff, file, *side, *end);
            }
        }

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

        // A blank line separates a file's content from the next header, but
        // collapsed headers (no content below them) stack directly — the band
        // on each header carries the separation. So a spacer is inserted only
        // after a file that showed content.
        let mut prev_had_content = false;
        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 && prev_had_content {
                urows.push(URow::Spacer);
            }
            let header_row = urows.len();
            urows.push(URow::FileHeader(fi));
            let collapsed = collapsed_files.contains(file.display_path());
            let file_threads = thread_at.get(file.display_path());
            let mut flat = Vec::new();
            let mut cof = Vec::new();

            // The file header is always a cursor stop (flat = HEADER), so j/k
            // walks headers and a collapsed or binary file stays navigable.
            file_first[fi] = Some(clines.len());
            line_urow.push(header_row);
            clines.push((fi, HEADER));

            // Whether this file shows any rows below its header (drives the
            // spacer before the next one); only a collapsed file shows none.
            prev_had_content = true;
            if file.binary {
                urows.push(URow::Note("binary file — contents not shown".to_string()));
            } else if file.hunks.is_empty() {
                urows.push(URow::Note(format!(
                    "{}, no content changes",
                    file.status.label()
                )));
            } else if collapsed {
                // A collapsed file shows only its header — no content rows (so
                // highlighting is skipped) and no line cursor stops.
                prev_had_content = false;
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    for li in 0..hunk.lines.len() {
                        flat.push((hi, li));
                    }
                }
            } else {
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    urows.push(URow::HunkHeader(fi, hi));
                    hunk_first.push(clines.len());
                    // Saturating: a hostile patch could carry counts near u32::MAX.
                    max_lineno = max_lineno
                        .max(hunk.old_start.saturating_add(hunk.old_lines))
                        .max(hunk.new_start.saturating_add(hunk.new_lines));
                    for li in 0..hunk.lines.len() {
                        let cursor = clines.len();
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

        // Side-by-side pass, using cursor_of to point each line at its row. The
        // same spacer rule as the unified pass: a blank line only after a file
        // that showed content, so collapsed headers stack.
        let mut srows = Vec::new();
        let mut line_srow = vec![0usize; clines.len()];
        let mut prev_had_content = false;
        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 && prev_had_content {
                srows.push(SRow::Spacer);
            }
            let header_srow = srows.len();
            srows.push(SRow::FileHeader(fi));
            // The header cursor stop maps to the header row on the sbs side too.
            if let Some(header_cline) = file_first[fi] {
                line_srow[header_cline] = header_srow;
            }
            let collapsed = collapsed_files.contains(file.display_path());
            prev_had_content = true;
            if file.binary {
                srows.push(SRow::Note("binary file — contents not shown".to_string()));
            } else if file.hunks.is_empty() {
                srows.push(SRow::Note(format!(
                    "{}, no content changes",
                    file.status.label()
                )));
            } else if collapsed {
                // Header only (already mapped above).
                prev_had_content = false;
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

/// Whether `line` on `side` of `file` is present in `diff`.
fn line_present(diff: &Diff, file: &str, side: Side, line: u32) -> bool {
    diff.files.iter().any(|f| {
        f.display_path() == file
            && f.hunks.iter().any(|h| {
                h.lines.iter().any(|l| {
                    let n = if side == Side::New {
                        l.new_lineno
                    } else {
                        l.old_lineno
                    };
                    n == Some(line)
                })
            })
    })
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
    fn clip_spans_offsets_and_pads() {
        let s = |t: &str| TextSpan::raw(t.to_string());
        let text = |v: &[TextSpan]| -> String { v.iter().map(|x| x.content.to_string()).collect() };
        // Offset 2, width 3 of "abcdef" → "cde".
        assert_eq!(
            text(&clip_spans(&[s("abcdef")], 2, 3, Style::default())),
            "cde"
        );
        // Past the content pads with spaces.
        assert_eq!(
            text(&clip_spans(&[s("abcdef")], 4, 4, Style::default())),
            "ef  "
        );
        // Offset 0 behaves like a width-aware fit.
        assert_eq!(
            text(&clip_spans(&[s("ab")], 0, 4, Style::default())),
            "ab  "
        );
    }

    #[test]
    fn clip_spans_never_cuts_a_wide_char() {
        // "a漢b": a at col 0, 漢 (width 2) at cols 1–2, b at col 3.
        let spans = vec![TextSpan::raw("a漢b".to_string())];
        // Starting at col 2 lands in the middle of 漢: its visible half is padded.
        let out = clip_spans(&spans, 2, 3, Style::default());
        let text: String = out.iter().map(|x| x.content.to_string()).collect();
        assert_eq!(
            text, " b ",
            "the split wide char is a space, not half a glyph"
        );
    }

    /// A file whose single context line is `width` columns of "x".
    fn wide_file(path: &str, width: usize) -> FileDiff {
        FileDiff {
            old_path: Some(path.into()),
            new_path: Some(path.into()),
            status: ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                section: None,
                lines: vec![Line {
                    kind: LineKind::Context,
                    content: "x".repeat(width),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
        }
    }

    fn wide_app(files: &[(&str, usize)]) -> App {
        let files = files.iter().map(|(p, w)| wide_file(p, *w)).collect();
        let diff = Diff {
            files,
            provenance: Provenance::default(),
        };
        let mut app = App::new(
            "t".into(),
            diff,
            Review::default(),
            None,
            "me".into(),
            Highlighter::new(),
            None,
        );
        app.mode = Mode::Unified;
        app
    }

    #[test]
    fn horizontal_scroll_clamps_and_resets_on_jump() {
        let mut app = wide_app(&[("a.rs", 200), ("b.rs", 200)]);
        app.cursor = 1; // a content line in file a
        app.hscroll_by(100);
        assert!(app.hscroll > 0);
        assert!(app.hscroll <= app.max_content_width());
        // A layout switch and a file jump both reset it.
        app.toggle_mode();
        assert_eq!(app.hscroll, 0);
        app.hscroll = 3;
        app.goto_file(1);
        assert_eq!(app.hscroll, 0);
    }

    #[test]
    fn horizontal_scroll_stops_before_whitespace_only() {
        let mut app = wide_app(&[("wide.rs", 200)]);
        app.hscroll_by(100_000); // fling all the way right
        let longest = app.max_content_width();
        let gutter = 1 + (2 * app.num_width + 2) + 2; // unified gutter
        let viewport = app.body_width.get() - gutter;
        assert_eq!(longest, 200);
        assert!(viewport < longest, "the long line overflows the viewport");
        // Never scroll so far that the viewport is (almost) all whitespace: at
        // the far-right stop at least (viewport - margin) columns are content.
        let margin = 8;
        assert!(
            longest - app.hscroll >= viewport - margin,
            "over-scrolled into whitespace: hscroll={}, longest={longest}, viewport={viewport}",
            app.hscroll,
        );
        assert_eq!(
            app.hscroll,
            longest - viewport + margin,
            "clamped to the line's overflow plus a small reading margin",
        );
    }

    #[test]
    fn right_aligned_row_pins_the_cluster_to_the_edge() {
        let sp = |t: &str| TextSpan::raw(t.to_string());
        let text = |v: &[TextSpan]| -> String { v.iter().map(|s| s.content.to_string()).collect() };
        let plain = Style::default();

        // Normal: the filename fits; the cluster is flush right past a flex gap.
        let row = right_aligned_row(
            vec![sp("  ")],
            "src/main.rs",
            |shown| vec![TextSpan::raw(shown.to_string())],
            vec![sp("+10 -2")],
            20,
            plain,
        );
        let s = text(&row);
        assert_eq!(str_width(&s), 20, "row is exactly the width: {s:?}");
        assert!(
            s.starts_with("  src/main.rs"),
            "filename on the left: {s:?}"
        );
        assert!(s.ends_with("+10 -2"), "cluster flush right: {s:?}");

        // Narrow: the cluster wins, the filename is head-truncated to a sliver.
        let row = right_aligned_row(
            vec![sp("  ")],
            "src/very/long/path/name.rs",
            |shown| vec![TextSpan::raw(shown.to_string())],
            vec![sp("+10 -2")],
            14,
            plain,
        );
        let s = text(&row);
        assert_eq!(str_width(&s), 14, "narrow row is exactly the width: {s:?}");
        assert!(s.ends_with("+10 -2"), "the cluster stays whole: {s:?}");
        assert!(s.contains('…'), "the filename is head-truncated: {s:?}");

        // CJK: width-aware — a 2-column-per-glyph name never misaligns the edge.
        let row = right_aligned_row(
            vec![sp("  ")],
            "日本語のファイル.rs",
            |shown| vec![TextSpan::raw(shown.to_string())],
            vec![sp("+1 -0")],
            20,
            plain,
        );
        let s = text(&row);
        assert_eq!(str_width(&s), 20, "CJK row is exactly the width: {s:?}");
        assert!(
            s.ends_with("+1 -0"),
            "cluster flush right with a CJK name: {s:?}"
        );
    }

    #[test]
    fn truncate_path_head_is_width_aware_for_cjk() {
        // A 2-column-per-glyph name clipped to 7 columns: "…" + a tail that fits.
        let out = truncate_path_head("あいうえお.rs", 7);
        assert!(out.starts_with('…'), "leads with an ellipsis: {out:?}");
        assert!(
            str_width(&out) <= 7,
            "fits the column budget: {out:?} = {} cols",
            str_width(&out)
        );
        assert!(out.ends_with(".rs"), "keeps the filename tail: {out:?}");
    }

    // -- control plane (handle_control against a real App) -----------------

    use loopreview_core::{ChangeStatus, FileDiff, Hunk, Line, Provenance};

    /// An App over a one-file diff (context line 1, addition line 2), with a
    /// local review store disabled (so persistence is a no-op).
    fn sample_app() -> App {
        let file = FileDiff {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            status: ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                section: None,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        content: "keep".into(),
                        old_lineno: Some(1),
                        new_lineno: Some(1),
                    },
                    Line {
                        kind: LineKind::Addition,
                        content: "added".into(),
                        old_lineno: None,
                        new_lineno: Some(2),
                    },
                ],
            }],
        };
        let diff = Diff {
            files: vec![file],
            provenance: Provenance {
                base: Some("base".into()),
                head: None,
            },
        };
        // A store is needed for comments; an explicit temp path keeps the test
        // off the real config. Persistence itself is covered by store's tests.
        let path = std::env::temp_dir().join(format!(
            "lr-ui-{}-{}/review.json",
            std::process::id(),
            app_counter()
        ));
        App::new(
            "working tree".into(),
            diff,
            Review::default(),
            Some(Store::at(path, "test-repo")),
            "tester".into(),
            Highlighter::new(),
            None,
        )
    }

    fn app_counter() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        C.fetch_add(1, Ordering::Relaxed)
    }

    /// An app whose diff ends in a pure deletion — a line present only on the old
    /// side. clines: [header, context "keep" (new 1), deletion "gone" (old 2)].
    fn deletion_app() -> App {
        let file = FileDiff {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            status: ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 1,
                section: None,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        content: "keep".into(),
                        old_lineno: Some(1),
                        new_lineno: Some(1),
                    },
                    Line {
                        kind: LineKind::Deletion,
                        content: "gone".into(),
                        old_lineno: Some(2),
                        new_lineno: None,
                    },
                ],
            }],
        };
        let diff = Diff {
            files: vec![file],
            provenance: Provenance {
                base: Some("base".into()),
                head: None,
            },
        };
        let path = std::env::temp_dir().join(format!(
            "lr-ui-{}-{}/review.json",
            std::process::id(),
            app_counter()
        ));
        App::new(
            "working tree".into(),
            diff,
            Review::default(),
            Some(Store::at(path, "test-repo")),
            "tester".into(),
            Highlighter::new(),
            None,
        )
    }

    /// An app whose file is five added lines (new 1..=5). clines row `i`
    /// (1..=5) is new line `i`, so a cursor row maps straight to a line number —
    /// handy for exercising multi-line range anchors.
    fn multi_line_app() -> App {
        let lines: Vec<Line> = (1..=5)
            .map(|n| Line {
                kind: LineKind::Addition,
                content: format!("line {n}"),
                old_lineno: None,
                new_lineno: Some(n),
            })
            .collect();
        let file = FileDiff {
            old_path: None,
            new_path: Some("a.rs".into()),
            status: ChangeStatus::Added,
            binary: false,
            hunks: vec![Hunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 5,
                section: None,
                lines,
            }],
        };
        let diff = Diff {
            files: vec![file],
            provenance: Provenance {
                base: Some("base".into()),
                head: None,
            },
        };
        let path = std::env::temp_dir().join(format!(
            "lr-ui-{}-{}/review.json",
            std::process::id(),
            app_counter()
        ));
        App::new(
            "working tree".into(),
            diff,
            Review::default(),
            Some(Store::at(path, "test-repo")),
            "tester".into(),
            Highlighter::new(),
            None,
        )
    }

    /// A new-side range anchor on `a.rs` spanning `start..=end`.
    fn range_anchor(start: u32, end: u32) -> Anchor {
        Anchor::Line {
            file: "a.rs".into(),
            side: Side::New,
            start,
            end,
            commit: None,
            context: Vec::new(),
        }
    }

    #[test]
    fn control_comment_add_creates_a_thread_and_emits_an_event() {
        let mut app = sample_app();
        let before = app.events.latest_seq();
        let response = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            body: "look here".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        }));
        match response {
            Response::Ok(Reply::Comment(result)) => {
                assert!(!result.draft, "a working-tree comment is not a draft");
                assert_eq!(app.review.threads.len(), 1);
                let thread = &app.review.threads[0];
                assert_eq!(thread.id, result.thread);
                assert_eq!(thread.comments[0].author, "agent");
                assert_eq!(thread.comments[0].body, "look here");
            }
            other => panic!("unexpected response: {other:?}"),
        }
        assert_eq!(app.events.latest_seq(), before + 1, "a Comment event fired");
    }

    #[test]
    fn control_comment_rm_removes_a_draft_thread() {
        let mut app = sample_app();
        let add = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            body: "note".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        }));
        let thread_id = match add {
            Response::Ok(Reply::Comment(r)) => r.thread,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(app.review.threads.len(), 1);
        match app.handle_control(Request::CommentRm(protocol::CommentRm {
            id: thread_id.clone(),
        })) {
            Response::Ok(Reply::Removed(r)) => {
                assert!(r.removed_thread);
                assert_eq!(r.thread, thread_id);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(app.review.threads.len(), 0, "the draft thread is gone");
    }

    #[test]
    fn guard_agent_write_refuses_only_when_something_is_published() {
        let draft = Comment {
            id: "d".into(),
            author: "agent".into(),
            body: "b".into(),
            created_at: 0,
            remote_id: None,
            kind: loopreview_core::CommentKind::Draft,
        };
        let published = Comment {
            remote_id: Some("PRRC_1".into()),
            ..draft.clone()
        };
        // All-draft passes; the message is the caller's own.
        assert!(guard_agent_write(std::iter::once(&draft), "nope").is_ok());
        assert!(guard_agent_write([&draft, &draft], "nope").is_ok());
        // Any published in the set refuses, verbatim.
        assert_eq!(
            guard_agent_write([&draft, &published], "refuse me"),
            Err("refuse me".to_string())
        );
        // An empty set (e.g. a thread with no root) is not a published write.
        assert!(guard_agent_write(std::iter::empty(), "nope").is_ok());
    }

    #[test]
    fn control_comment_rm_refuses_a_published_comment() {
        let mut app = sample_app();
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 1),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c".into(),
                author: "reviewer".into(),
                body: "b".into(),
                created_at: 0,
                remote_id: Some("PRRC_1".into()),
                kind: loopreview_core::CommentKind::Draft,
            }],
        });
        app.relayout();
        match app.handle_control(Request::CommentRm(protocol::CommentRm { id: "t".into() })) {
            Response::Error(msg) => assert!(msg.contains("published"), "friendly refusal: {msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
        assert_eq!(app.review.threads.len(), 1, "a published thread stays");
    }

    #[test]
    fn control_comment_edit_replaces_own_comment_body() {
        let mut app = sample_app();
        let add = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            body: "before".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        }));
        let comment_id = match add {
            Response::Ok(Reply::Comment(r)) => r.comment,
            other => panic!("unexpected: {other:?}"),
        };
        match app.handle_control(Request::CommentEdit(protocol::CommentEdit {
            id: comment_id.clone(),
            body: "after".into(),
            author: "agent".into(),
        })) {
            Response::Ok(Reply::Comment(r)) => assert_eq!(r.comment, comment_id),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(app.review.threads[0].root().unwrap().body, "after");
        assert_eq!(app.review.threads[0].comments.len(), 1, "still one comment");
    }

    #[test]
    fn control_comment_edit_refuses_published_and_other_authors() {
        let mut app = sample_app();
        // An agent's own local note, plus a published comment by someone else.
        let add = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            body: "mine".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        }));
        let own = match add {
            Response::Ok(Reply::Comment(r)) => r.comment,
            other => panic!("unexpected: {other:?}"),
        };
        // Another author can't edit the agent's comment.
        match app.handle_control(Request::CommentEdit(protocol::CommentEdit {
            id: own.clone(),
            body: "hijacked".into(),
            author: "someone-else".into(),
        })) {
            Response::Error(msg) => assert!(msg.contains("author"), "refusal: {msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
        assert_eq!(app.review.threads[0].root().unwrap().body, "mine");

        // A published comment can't be edited by an agent at all.
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 1),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "cpub".into(),
                author: "agent".into(),
                body: "posted".into(),
                created_at: 0,
                remote_id: Some("PRRC_1".into()),
                kind: loopreview_core::CommentKind::Draft,
            }],
        });
        app.relayout();
        match app.handle_control(Request::CommentEdit(protocol::CommentEdit {
            id: "cpub".into(),
            body: "changed".into(),
            author: "agent".into(),
        })) {
            Response::Error(msg) => assert!(msg.contains("human action"), "refusal: {msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn tui_d_removes_a_draft_thread_after_confirm() {
        let mut app = sample_app();
        let _ = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            body: "note".into(),
            author: "me".into(),
            draft: false,
            conversation: false,
        }));
        app.view = View::Conversation;
        app.conv_cursor = 0;
        // d arms the confirmation; y removes.
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(app.confirming_delete.is_some(), "d arms the delete confirm");
        app.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.confirming_delete.is_none());
        assert_eq!(app.review.threads.len(), 0, "the draft was removed");
    }

    #[test]
    fn pr_drafts_excludes_a_fully_published_thread() {
        // The store never saves a pulled/published thread as a draft — the source
        // of the old ghost contamination. Only threads with an unpublished
        // comment are kept, so the current build cannot reproduce it.
        let mut app = pr_app();
        app.review
            .threads
            .push(published_comment("c", "tester", "555"));
        app.add_thread(
            Anchor::line("a.rs", Side::New, 1),
            "tester",
            "my draft",
            CommentKind::Draft,
        );
        let drafts = app.pr_drafts();
        assert_eq!(
            drafts.threads.len(),
            1,
            "only the unpublished draft is saved"
        );
        assert!(
            drafts.threads[0].root().unwrap().remote_id.is_none(),
            "the saved thread is the draft, not the published one"
        );
    }

    #[test]
    fn submitting_clears_the_published_drafts_from_the_store() {
        let mut app = sample_app();
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test(1, "t")));
        app.pr_key = Some("owner/repo#1".into());
        // A draft thread, saved into the PR draft store.
        let (tid, _) = app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "note",
            CommentKind::Draft,
        );
        app.save_pr_drafts().unwrap();
        let key = "owner/repo#1";
        assert!(!app.pr_drafts().is_empty());
        assert!(
            !app.store
                .as_ref()
                .unwrap()
                .load_pr_drafts(key)
                .unwrap()
                .is_empty(),
            "the draft is stored before submit"
        );
        // Submitting stamps it published and clears it from the store; a repeat
        // submit would now find nothing.
        app.apply_submitted(crate::prsync::Submitted {
            published: vec![(tid, "PRRC_1".into())],
            replies: Vec::new(),
            failed_replies: 0,
            deferred_replies: 0,
        });
        assert_eq!(
            app.review.threads[0].root().unwrap().remote_id.as_deref(),
            Some("PRRC_1"),
            "the root is now published"
        );
        assert!(app.pr_drafts().is_empty(), "no drafts remain in memory");
        assert!(
            app.store
                .as_ref()
                .unwrap()
                .load_pr_drafts(key)
                .unwrap()
                .is_empty(),
            "the store's draft entry is cleared"
        );
    }

    #[test]
    fn an_unreconciled_submit_clears_the_draft_and_shows_no_badge() {
        // The review posted but its comment id could not be read back: prsync
        // stamps the pending sentinel. The root must stop being a draft (no repost,
        // no [draft] badge) and leave the store, its real id arriving on next pull.
        let mut app = sample_app();
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test(1, "t")));
        app.pr_key = Some("owner/repo#1".into());
        let (tid, _) = app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "note",
            CommentKind::Draft,
        );
        app.save_pr_drafts().unwrap();
        let key = "owner/repo#1";

        app.apply_submitted(crate::prsync::Submitted {
            published: vec![(tid, crate::prsync::PENDING_REMOTE_ID.into())],
            replies: Vec::new(),
            failed_replies: 0,
            deferred_replies: 0,
        });

        let root = app.review.threads[0].root().unwrap();
        assert!(!root.is_draft(), "a submitted root is no longer a draft");
        assert!(kind_badge(root).is_none(), "no [draft] badge remains");
        assert!(
            app.pr_drafts().is_empty(),
            "a repeat submit finds nothing to re-post"
        );
        assert!(
            app.store
                .as_ref()
                .unwrap()
                .load_pr_drafts(key)
                .unwrap()
                .is_empty(),
            "the store's draft entry is cleared"
        );
    }

    #[test]
    fn a_failed_reply_is_reported_and_left_draft() {
        let mut app = sample_app();
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test(1, "t")));
        app.pr_key = Some("owner/repo#1".into());
        let (tid, _) = app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "root",
            CommentKind::Draft,
        );
        // The root publishes, but one reply failed to post.
        app.apply_submitted(crate::prsync::Submitted {
            published: vec![(tid, "PRRC_1".into())],
            replies: Vec::new(),
            failed_replies: 1,
            deferred_replies: 0,
        });
        assert!(
            app.status.as_deref().unwrap_or("").contains("failed"),
            "a partial failure is surfaced: {:?}",
            app.status
        );
    }

    #[test]
    fn a_submit_with_unparsed_ids_publishes_pending_and_wont_resubmit() {
        // The review POST returned 2xx but its id didn't parse: the root publishes
        // under the pending sentinel. It must not stay a draft (that would let a
        // resubmit duplicate the review); a refresh reconciles the real id.
        let mut app = pr_app();
        let (tid, _) = app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "root",
            CommentKind::Draft,
        );
        app.apply_submitted(crate::prsync::Submitted {
            published: vec![(tid, crate::prsync::PENDING_REMOTE_ID.to_string())],
            replies: Vec::new(),
            failed_replies: 0,
            deferred_replies: 0,
        });
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("reconciling ids on the next refresh"),
            "status points at the refresh, not a resubmit: {:?}",
            app.status
        );
        assert!(
            app.review.threads[0].root().unwrap().is_published(),
            "the root is marked published (no [draft])"
        );
        assert_eq!(
            app.draft_summary().total(),
            0,
            "nothing left to resubmit — no duplicate POST"
        );
    }

    #[test]
    fn a_deferred_reply_is_reported_with_a_recovery_hint() {
        // A reply whose root's id wasn't read back stays draft — never silently:
        // the status names the two-step refresh-and-resubmit recovery.
        let mut app = pr_app();
        app.apply_submitted(crate::prsync::Submitted {
            published: Vec::new(),
            replies: Vec::new(),
            failed_replies: 0,
            deferred_replies: 1,
        });
        let status = app.status.as_deref().unwrap_or("");
        assert!(
            status.contains("kept as draft") && status.contains("refresh and submit again"),
            "the deferral is surfaced with a recovery hint: {status:?}"
        );
    }

    fn pr_app() -> App {
        let mut app = sample_app();
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test(1, "t")));
        app.pr_key = Some("owner/repo#1".into());
        app
    }

    /// A PR overview snapshot for header/Overview tests.
    fn overview(status: PrStatus) -> SubjectOverview {
        SubjectOverview {
            kind: loopreview_github::SubjectKind::Pr,
            number: 1,
            status: SubjectStatus::Pr(status),
            title: "Add the thing".into(),
            author: "octocat".into(),
            base_ref: Some("main".into()),
            head_ref: Some("feature".into()),
            created_at: Some("2026-07-20T10:00:00Z".into()),
            closed_at: None,
            body: String::new(),
            url: "https://github.com/owner/repo/pull/1".into(),
        }
    }

    /// An issue overview snapshot (no branches).
    fn issue_overview(status: loopreview_github::IssueStatus) -> SubjectOverview {
        SubjectOverview {
            kind: loopreview_github::SubjectKind::Issue,
            number: 5,
            status: SubjectStatus::Issue(status),
            title: "Flaky retry".into(),
            author: "octocat".into(),
            base_ref: None,
            head_ref: None,
            created_at: Some("2026-07-20T10:00:00Z".into()),
            closed_at: None,
            body: "It flakes under load.".into(),
            url: "https://github.com/owner/repo/issues/5".into(),
        }
    }

    /// An app reviewing an issue (no diff, a flat conversation).
    fn issue_app() -> App {
        let mut app = sample_app();
        app.issue = Some(Arc::new(crate::prsync::IssueHandle::for_test(
            5,
            "Flaky retry",
        )));
        app.pr_key = Some("owner/repo#5".into());
        app.pr_overview = Some(issue_overview(loopreview_github::IssueStatus::Open));
        app
    }

    #[test]
    fn an_issue_session_shows_overview_and_conversation_only() {
        let app = issue_app();
        assert!(app.is_issue());
        assert!(app.has_subject());
        assert_eq!(
            app.visible_views(),
            vec![View::Overview, View::Conversation],
            "an issue has no Files tab"
        );
        assert!(app.shows_tabs());
        assert!(!app.view_available(View::Files));
    }

    #[test]
    fn installing_an_issue_load_opens_the_overview() {
        let mut app = sample_app();
        app.view = View::Files;
        app.install_loaded(Loaded {
            label: "issue owner/repo#5".into(),
            diff: Diff::default(),
            review: Review::default(),
            pr: None,
            issue: Some(crate::prsync::IssueHandle::for_test(5, "t")),
            pr_key: Some("owner/repo#5".into()),
            stale_cleaned: 0,
        });
        assert_eq!(app.view, View::Overview, "an issue opens on its Overview");
        assert!(app.is_issue());
        assert!(app.pr_overview.is_some(), "the issue overview is installed");
    }

    #[test]
    fn the_issue_overview_renders_its_facts() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = issue_app();
        app.set_view(View::Overview);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(screen.contains("#5"), "the issue number: {screen:?}");
        assert!(screen.contains("Open"), "the status badge");
        assert!(screen.contains("Flaky retry"), "the title");
        assert!(screen.contains("@octocat"), "the author");
        assert!(!screen.contains('←'), "an issue has no base ← head");
        assert!(screen.contains("flakes under load"), "the body");
    }

    #[test]
    fn an_issue_reports_its_subject_kind() {
        let app = issue_app();
        let subject = app.subject_info().expect("an issue carries a subject");
        assert_eq!(subject.kind, "issue");
        assert_eq!(subject.number, 5);
        assert_eq!(subject.status, "open");
        assert!(
            subject.base.is_none() && subject.head.is_none(),
            "an issue has no branches"
        );
    }

    #[test]
    fn an_issue_thread_resolves_locally_but_not_when_published() {
        let mut app = issue_app();
        // A local (unpublished) conversation thread resolves locally.
        app.review.threads.push(Thread {
            id: "local".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of("c", "tester", None, CommentKind::Local)],
        });
        // A published issue comment has no GitHub resolve.
        app.review.threads.push(Thread {
            id: "pub".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of(
                "p",
                "octocat",
                Some("9"),
                CommentKind::Published,
            )],
        });
        app.relayout();
        assert!(
            app.is_resolvable(0),
            "a local issue thread resolves locally"
        );
        assert!(
            !app.is_resolvable(1),
            "a published issue comment can't resolve"
        );
    }

    #[test]
    fn t_toggles_kind_on_an_issue_conversation_root() {
        let mut app = issue_app();
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        app.review.threads.push(Thread {
            id: "conv".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of("root", "tester", None, CommentKind::Local)],
        });
        app.relayout();
        app.conv_cursor = 0;
        app.conv_comment = 0;
        // t promotes the local root to a draft (it will send on Ctrl-S).
        app.toggle_selected_kind();
        let ti = app.conv_order[app.conv_cursor];
        assert_eq!(
            app.review.threads[ti].root().unwrap().disposition(),
            CommentKind::Draft,
            "t promotes an issue conversation root to a draft"
        );
    }

    /// A one-thread draft review, for seeding another subject's draft bucket.
    fn one_draft() -> Review {
        Review {
            threads: vec![Thread {
                id: generate_id(),
                anchor: Anchor::Review,
                state: ThreadState::Open,
                comments: vec![comment_of("d", "someone", None, CommentKind::Draft)],
            }],
        }
    }

    #[test]
    fn a_new_human_comment_is_a_draft_on_an_issue() {
        // Regression: `c` on an issue used to make a local note (never sent), so
        // Ctrl-S found nothing to send and the human had to `t`-promote first. A
        // GitHub subject — pull request or issue — drafts by default.
        assert_eq!(issue_app().human_new_kind(), CommentKind::Draft);
        assert_eq!(pr_app().human_new_kind(), CommentKind::Draft);
        assert_eq!(sample_app().human_new_kind(), CommentKind::Local);
    }

    #[test]
    fn an_agent_draft_is_honored_on_an_issue() {
        // --draft queues a sendable draft on an issue too (a human still sends);
        // without it, and off any subject, an agent comment stays a local note.
        assert_eq!(issue_app().agent_kind(true), CommentKind::Draft);
        assert_eq!(issue_app().agent_kind(false), CommentKind::Local);
        assert_eq!(pr_app().agent_kind(true), CommentKind::Draft);
        assert_eq!(sample_app().agent_kind(true), CommentKind::Local);
    }

    #[test]
    fn issue_drafts_persist_in_the_pr_draft_bucket() {
        // Regression: an issue draft used to fall to store.save(&review) — lost
        // across sessions (the loader reads pr_drafts) and leaking published issue
        // comments into the worktree store. persist() must route it to
        // save_pr_drafts[key] and leave the worktree review untouched.
        let mut app = issue_app();
        app.set_view(View::Conversation);
        // A published issue comment (must NOT be stored) and a local draft (must).
        app.review.threads.push(Thread {
            id: "pub".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of(
                "p",
                "octocat",
                Some("IC_9"),
                CommentKind::Published,
            )],
        });
        app.add_thread(Anchor::Review, "tester", "my draft", CommentKind::Draft);

        let msg = app.persist("comment added").unwrap();
        assert!(
            msg.contains("Ctrl-S to send"),
            "an issue persist points at send, not submit: {msg}"
        );

        let store = app.store.as_ref().unwrap();
        let saved = store.load_pr_drafts("owner/repo#5").unwrap();
        assert_eq!(
            saved.threads.len(),
            1,
            "only the unpublished draft round-trips into the draft bucket"
        );
        assert!(saved.threads[0].root().unwrap().remote_id.is_none());
        assert!(
            store.load_or_recover().0.is_empty(),
            "the worktree review store is not polluted with issue comments"
        );
    }

    #[test]
    fn closing_an_issue_review_keeps_other_subjects_drafts() {
        // Regression (data loss): X on an issue fell to the non-subject branch and
        // deleted the whole shared store file — every other PR/issue's drafts and
        // the worktree review with it — then dropped to the hidden Files view. It
        // must clear only this issue's own bucket and land on a visible view.
        let mut app = issue_app();
        app.set_view(View::Conversation);
        // Another subject already has drafts in the same store file.
        app.store
            .as_ref()
            .unwrap()
            .save_pr_drafts("owner/repo#99", &one_draft())
            .unwrap();
        app.add_thread(Anchor::Review, "tester", "my note", CommentKind::Draft);
        assert!(!app.pr_drafts().is_empty());

        app.close_review();

        assert_eq!(
            app.view,
            View::Conversation,
            "an issue lands on a visible view, not the hidden Files"
        );
        assert!(
            app.pr_drafts().is_empty(),
            "this issue's own local drafts are discarded"
        );
        assert!(
            !app.store
                .as_ref()
                .unwrap()
                .load_pr_drafts("owner/repo#99")
                .unwrap()
                .is_empty(),
            "another subject's drafts survive — the store file was not deleted"
        );
    }

    fn add(app: &mut App, line: u32, draft: bool) {
        app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(line),
            body: "n".into(),
            author: "agent".into(),
            draft,
            conversation: false,
        }));
    }

    fn reply(app: &mut App, thread: &str, draft: bool) -> Response {
        app.handle_control(Request::CommentReply(protocol::CommentReply {
            thread: thread.to_string(),
            body: "r".into(),
            author: "agent".into(),
            draft,
        }))
    }

    #[test]
    fn control_reply_refuses_a_draft_under_a_local_root() {
        let mut app = pr_app(); // a PR, so --draft is meaningful
        // A local-note root and a draft root (both on lines in the sample diff).
        add(&mut app, 2, false); // local root
        add(&mut app, 1, true); // draft root
        let local_tid = app.review.threads[0].id.clone();
        let draft_tid = app.review.threads[1].id.clone();

        // A --draft reply under the local root is refused (it would strand a draft).
        match reply(&mut app, &local_tid, true) {
            Response::Error(msg) => assert!(msg.contains("promote"), "guard message: {msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
        assert_eq!(
            app.review.threads[0].comments.len(),
            1,
            "the local root gained no draft reply"
        );
        // Without --draft, the reply inherits local and is accepted.
        assert!(matches!(
            reply(&mut app, &local_tid, false),
            Response::Ok(Reply::Comment(_))
        ));
        assert_eq!(app.review.threads[0].comments.len(), 2);
        // Under a draft root, a --draft reply is fine (rule 2).
        assert!(matches!(
            reply(&mut app, &draft_tid, true),
            Response::Ok(Reply::Comment(_))
        ));
    }

    #[test]
    fn agent_comments_default_to_local_unless_draft() {
        // A working-tree review: an agent comment is a local note.
        let mut local = sample_app();
        add(&mut local, 2, false);
        assert!(local.review.threads[0].root().unwrap().is_local());

        // On a PR: still local by default (never sent by accident)...
        let mut pr = pr_app();
        add(&mut pr, 2, false);
        assert!(
            pr.review.threads[0].root().unwrap().is_local(),
            "agent default is local even on a PR"
        );
        // ...unless it passes --draft, which queues it for submit.
        add(&mut pr, 1, true);
        assert!(
            pr.review.threads[1].root().unwrap().is_draft(),
            "--draft queues the comment"
        );
    }

    #[test]
    fn submit_summary_counts_drafts_and_flags_other_authors() {
        // sample_app's author is "tester" — the submitting human.
        let mut app = pr_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "mine",
            CommentKind::Draft,
        );
        app.add_thread(
            Anchor::line("a.rs", Side::New, 1),
            "fable",
            "agent draft",
            CommentKind::Draft,
        );
        // A local note by an agent must not be counted (it is never sent).
        app.add_thread(
            Anchor::line("a.rs", Side::New, 1),
            "fable",
            "just a note",
            CommentKind::Local,
        );
        let summary = app.draft_summary();
        assert_eq!(
            (summary.new_inline, summary.replies, summary.conversation),
            (2, 0, 0),
            "two inline drafts, the local note excluded"
        );
        assert!(summary.authors.contains(&("tester".to_string(), 1)));
        assert!(summary.authors.contains(&("fable".to_string(), 1)));
        assert!(
            summary.foreign,
            "a draft by someone other than the human is flagged"
        );

        // Only the human's own drafts: no flag.
        let mut solo = pr_app();
        solo.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "mine",
            CommentKind::Draft,
        );
        assert!(
            !solo.draft_summary().foreign,
            "only the human's own drafts — no warning"
        );
    }

    #[test]
    fn submit_summary_counts_conversation_roots_not_conversation_replies() {
        let mut app = pr_app();
        // A new conversation comment (a draft root) posts.
        let (conv, _) = app.add_thread(Anchor::Review, "tester", "overall", CommentKind::Draft);
        // A reply under it stays local — it must not be counted.
        app.add_reply(&conv, "tester", "reply", CommentKind::Local);
        // A reply to a published inline thread posts.
        app.review.threads.push(Thread {
            id: "inline".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![
                Comment {
                    id: "root".into(),
                    author: "someone".into(),
                    body: "root".into(),
                    created_at: 0,
                    remote_id: Some("500".into()),
                    kind: CommentKind::Draft,
                },
                Comment {
                    id: "r".into(),
                    author: "tester".into(),
                    body: "my reply".into(),
                    created_at: 0,
                    remote_id: None,
                    kind: CommentKind::Draft,
                },
            ],
        });
        let s = app.draft_summary();
        assert_eq!(
            (s.new_inline, s.replies, s.conversation),
            (0, 1, 1),
            "one inline reply and one conversation root; the conversation reply is local"
        );
    }

    #[test]
    fn submit_summary_counts_a_draft_reply_under_a_draft_root() {
        // A brand-new inline thread the reviewer drafted a root and a reply on:
        // both go out in one submit (the root, then the reply via in_reply_to),
        // so the modal must count both as sends.
        let mut app = pr_app();
        let (tid, _) = app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "root",
            CommentKind::Draft,
        );
        app.add_reply(&tid, "tester", "reply", CommentKind::Draft);
        let s = app.draft_summary();
        assert_eq!(
            (s.new_inline, s.replies),
            (1, 1),
            "the draft root and its draft reply both count"
        );
    }

    #[test]
    fn submit_is_send_only_when_there_are_no_new_inline_comments() {
        // A reply-only batch: a published inline root with a draft reply, no new
        // inline drafts. The modal opens in send-only mode — no event, no summary.
        let mut app = pr_app();
        app.review.threads.push(Thread {
            id: "inline".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![
                Comment {
                    id: "root".into(),
                    author: "someone".into(),
                    body: "root".into(),
                    created_at: 0,
                    remote_id: Some("500".into()),
                    kind: CommentKind::Draft,
                },
                Comment {
                    id: "r".into(),
                    author: "tester".into(),
                    body: "reply".into(),
                    created_at: 0,
                    remote_id: None,
                    kind: CommentKind::Draft,
                },
            ],
        });
        app.open_submit();
        let modal = app.submit.as_ref().expect("the modal opened");
        assert!(modal.is_send_only(), "a reply-only batch is send-only");
        // A new inline draft flips it to the full review modal.
        let mut full = pr_app();
        full.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "new inline",
            CommentKind::Draft,
        );
        full.open_submit();
        assert!(
            !full.submit.as_ref().unwrap().is_send_only(),
            "a new inline draft needs the full modal"
        );
    }

    #[test]
    fn submit_with_nothing_to_send_reports_instead_of_opening() {
        let mut app = pr_app(); // no drafts
        app.open_submit();
        assert!(app.submit.is_none(), "no empty modal");
        assert_eq!(app.status.as_deref(), Some("no drafts to submit"));
    }

    #[test]
    fn composer_enter_makes_newlines_and_ctrl_s_saves() {
        // The default policy: Enter is always a newline (reliable everywhere),
        // and Ctrl-S saves — so multi-line comments never depend on Shift+Enter.
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.cursor = 1; // a content line, not the file header
        app.start_compose();
        assert!(app.input.is_some(), "the composer opened");

        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(
            app.input.is_some(),
            "Enter keeps composing, it does not save"
        );
        // Ctrl-S saves and closes the composer.
        app.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.input.is_none(), "Ctrl-S saves and closes the composer");

        let body = app
            .review
            .threads
            .last()
            .unwrap()
            .root()
            .unwrap()
            .body
            .clone();
        assert_eq!(body, "a\nb\nc", "the saved comment kept both newlines");
    }

    #[test]
    fn composer_enter_can_be_configured_to_save() {
        // The opt-in `composer_enter = "save"`: Enter saves, a modifier newlines.
        let mut app = sample_app();
        app.composer_enter = crate::config::ComposerEnter::Save;
        app.mode = Mode::Unified;
        app.cursor = 1;
        app.start_compose();

        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        // Shift+Enter (Kitty protocol) and Alt+Enter (fallback) insert newlines.
        app.on_key(KeyCode::Enter, KeyModifiers::SHIFT);
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.on_key(KeyCode::Enter, KeyModifiers::ALT);
        app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(
            app.input.is_some(),
            "a modified Enter is a newline, not save"
        );
        // A bare Enter saves and closes.
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.input.is_none(), "Enter saves in save mode");

        let body = app
            .review
            .threads
            .last()
            .unwrap()
            .root()
            .unwrap()
            .body
            .clone();
        assert_eq!(body, "a\nb\nc", "the saved comment kept both newlines");
    }

    #[test]
    fn ctrl_s_submits_when_no_composer_is_open() {
        // Ctrl-S has two roles kept apart by whether a composer is open: it saves
        // inside the composer (above), and opens the submit modal outside it.
        let mut pr = pr_app();
        pr.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "a draft to submit",
            CommentKind::Draft,
        );
        assert!(pr.input.is_none() && pr.submit.is_none());
        pr.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(
            pr.submit.is_some(),
            "Ctrl-S with no composer opens the submit modal"
        );
    }

    #[test]
    fn composer_save_label_tracks_context() {
        // A plain review has no local/draft split.
        let mut plain = sample_app();
        plain.cursor = 1;
        plain.start_compose();
        assert_eq!(plain.compose_save_label(), "save comment");

        // On a pull request, a new comment is a draft (queued for submit).
        let mut pr = pr_app();
        pr.cursor = 1;
        pr.start_compose();
        assert_eq!(pr.compose_save_label(), "save draft");

        // A reply under a local note stays a note, even on a pull request.
        let mut reply = pr_app();
        reply.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "agent",
            "note",
            CommentKind::Local,
        );
        reply.open_reply(0);
        assert_eq!(reply.compose_save_label(), "save note");
    }

    #[test]
    fn e_edits_your_own_unpublished_comment() {
        let mut app = sample_app(); // author "tester"
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "original",
            CommentKind::Local,
        );
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        // `e` opens the composer pre-filled with the existing body.
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(app.input.is_some(), "the editor opened");
        assert_eq!(app.input.as_ref().unwrap().area.text(), "original");
        // Append and save with Ctrl-S — the body is replaced in place.
        app.on_key(KeyCode::Char('!'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.input.is_none(), "Ctrl-S saves the edit");
        assert_eq!(app.review.threads[0].root().unwrap().body, "original!");
        // The comment count did not change — an edit, not a new comment.
        assert_eq!(app.review.threads[0].comments.len(), 1);
    }

    #[test]
    fn e_refuses_published_and_others_comments() {
        // Another author's note is not editable — that would misattribute it.
        let mut app = sample_app(); // author "tester"
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "agent",
            "theirs",
            CommentKind::Local,
        );
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(app.input.is_none(), "cannot edit another author's comment");
        assert!(app.status.as_deref().unwrap_or("").contains("your own"));

        // A published comment (it has a remote id) is not editable, even yours.
        let mut published = sample_app();
        published.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "posted",
            CommentKind::Draft,
        );
        published.review.threads[0].comments[0].remote_id = Some("R1".into());
        published.relayout();
        published.view = View::Conversation;
        published.conv_cursor = 0;
        published.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(
            published.input.is_none(),
            "a published comment with no addressable id can't be edited"
        );
        // It is mine (I authored it), but its non-numeric remote id has no GitHub
        // route — the refusal is about the id, not ownership.
        assert!(
            published
                .status
                .as_deref()
                .unwrap_or("")
                .contains("no synced API id")
        );
    }

    #[test]
    fn github_link_deep_links_a_published_comment_by_kind() {
        // On a PR the Conversation cursor on a published comment deep-links to it;
        // the anchor scheme follows the thread-id prefix the pull assigns (review
        // summary / conversation comment / inline review comment).
        let mut app = pr_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "posted",
            CommentKind::Draft,
        );
        app.review.threads[0].comments[0].remote_id = Some("123".into());
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.conv_comment = 0;
        let pr = app.pr.clone().unwrap();

        // A plain (non-prefixed) thread id is an inline review comment.
        assert_eq!(
            app.github_link(&pr),
            "https://github.com/owner/repo/pull/1#discussion_r123"
        );
        // A conversation comment and a review summary key off the prefix.
        app.review.threads[0].id = "issuecomment:9".into();
        app.relayout();
        assert_eq!(
            app.github_link(&pr),
            "https://github.com/owner/repo/pull/1#issuecomment-123"
        );
        app.review.threads[0].id = "review:9".into();
        app.relayout();
        assert_eq!(
            app.github_link(&pr),
            "https://github.com/owner/repo/pull/1#pullrequestreview-123"
        );

        // An unpublished draft under the cursor falls back to the PR page.
        app.review.threads[0].comments[0].remote_id = None;
        app.relayout();
        assert_eq!(app.github_link(&pr), "https://github.com/owner/repo/pull/1");
    }

    #[test]
    fn github_link_is_the_pr_page_off_conversation_and_absent_off_a_pr() {
        // The Files view never rests "on" a comment, so it opens the PR page even
        // over a published thread.
        let mut app = pr_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "posted",
            CommentKind::Draft,
        );
        app.review.threads[0].comments[0].remote_id = Some("123".into());
        app.relayout();
        app.view = View::Files;
        let pr = app.pr.clone().unwrap();
        assert_eq!(app.github_link(&pr), "https://github.com/owner/repo/pull/1");

        // Off a pull request there is nowhere to go — Ctrl-O just says so (and must
        // not try to launch a browser).
        let mut local = sample_app();
        local.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(local.status.as_deref(), Some("no GitHub context here"));
    }

    #[test]
    fn e_edits_a_reply_via_the_comment_cursor() {
        let mut app = sample_app(); // author "tester"
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "agent",
            "root note",
            CommentKind::Local,
        );
        let tid = app.review.threads[0].id.clone();
        app.add_reply(&tid, "tester", "my reply", CommentKind::Local);
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        // On the root (someone else's), edit is refused.
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(app.input.is_none(), "can't edit another author's root");
        // j steps to the reply (your own); edit it.
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.conv_comment, 1, "the cursor moved to the reply");
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(
            app.input.is_some(),
            "editing your own reply opens the composer"
        );
        assert_eq!(app.input.as_ref().unwrap().area.text(), "my reply");
        app.on_key(KeyCode::Char('!'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.review.threads[0].comments[1].body, "my reply!");
        assert_eq!(
            app.review.threads[0].comments[0].body, "root note",
            "the root is untouched"
        );
    }

    #[test]
    fn d_removes_a_reply_without_the_thread() {
        let mut app = sample_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "root",
            CommentKind::Local,
        );
        let tid = app.review.threads[0].id.clone();
        app.add_reply(&tid, "tester", "reply", CommentKind::Local);
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.conv_comment = 1; // the reply
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE); // arm the confirm
        assert!(app.confirming_delete.is_some());
        app.on_key(KeyCode::Char('y'), KeyModifiers::NONE); // confirm
        assert_eq!(app.review.threads.len(), 1, "the thread stays");
        assert_eq!(
            app.review.threads[0].comments.len(),
            1,
            "only the reply is gone"
        );
        assert_eq!(app.review.threads[0].comments[0].body, "root");
    }

    #[test]
    fn confirm_delete_targets_by_id_after_the_review_shifts() {
        // Arm a delete on thread B, then have an agent remove thread A before the
        // confirm — B's index shifts from 1 to 0. The id-based target must still
        // remove B (an index-based one would miss it).
        let mut app = sample_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 1),
            "me",
            "A",
            CommentKind::Local,
        );
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "B",
            CommentKind::Local,
        );
        app.relayout();
        let b_id = app.review.threads[1].id.clone();
        app.confirming_delete = Some(DeleteTarget {
            thread_id: b_id.clone(),
            comment_id: None,
            published: None,
            also_removed: 0,
        });
        // Interleave: thread A is removed, shifting B to index 0.
        app.review.threads.remove(0);
        app.relayout();
        let target = app.confirming_delete.take().unwrap();
        app.confirm_delete(target);
        assert!(
            app.review.threads.iter().all(|t| t.id != b_id),
            "B was removed by id despite the index shift"
        );
    }

    #[test]
    fn resolved_outcome_applies_by_id_after_the_review_shifts() {
        // A resolve job for B finishes after A was removed (indices shifted).
        let mut app = sample_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 1),
            "me",
            "A",
            CommentKind::Draft,
        );
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "B",
            CommentKind::Draft,
        );
        app.relayout();
        let b_id = app.review.threads[1].id.clone();
        app.review.threads.remove(0);
        app.relayout();
        app.apply_job(Ok(JobOutcome::Resolved {
            thread_id: b_id.clone(),
            resolved: true,
        }));
        assert!(
            app.review.thread(&b_id).is_some_and(|t| t.is_resolved()),
            "the resolution applied to B by id, not a stale index"
        );
    }

    fn published_thread(id: &str, anchor: Anchor) -> Thread {
        Thread {
            id: id.into(),
            anchor,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "5".into(),
                author: "tester".into(),
                body: "text".into(),
                created_at: 0,
                remote_id: Some("5".into()),
                kind: loopreview_core::CommentKind::Published,
            }],
        }
    }

    #[test]
    fn published_endpoint_routes_by_thread_id() {
        // The pulled thread id tells the three published kinds apart: a review
        // summary and an issue comment share Anchor::Review, so routing must use
        // the id prefix (a review summary through issues/comments 404s).
        let mut app = pr_app();
        app.review.threads.push(published_thread(
            "PRRT_node",
            Anchor::line("a.rs", Side::New, 1),
        ));
        app.review
            .threads
            .push(published_thread("issuecomment:5", Anchor::Review));
        app.review
            .threads
            .push(published_thread("review:5", Anchor::Review));
        assert_eq!(
            app.published_endpoint("PRRT_node", "5"),
            Some(CommentEndpoint::ReviewComment(5))
        );
        assert_eq!(
            app.published_endpoint("issuecomment:5", "5"),
            Some(CommentEndpoint::IssueComment(5))
        );
        assert_eq!(
            app.published_endpoint("review:5", "5"),
            Some(CommentEndpoint::ReviewSummary(5))
        );
    }

    #[test]
    fn a_review_summary_is_editable_but_not_deletable() {
        let mut app = pr_app(); // viewer "tester"
        app.review
            .threads
            .push(published_thread("review:99", Anchor::Review));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        // `e` opens the composer — a review summary edits via PUT /pulls/reviews.
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(app.input.is_some(), "a review summary is editable");
        app.input = None;
        // But GitHub has no delete for a submitted review — no delete offered.
        assert!(
            app.selected_delete_target().is_none(),
            "a review summary can't be deleted"
        );
    }

    fn published_comment(id: &str, author: &str, remote: &str) -> Thread {
        Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: id.into(),
                author: author.into(),
                body: "posted".into(),
                created_at: 0,
                remote_id: Some(remote.into()),
                kind: loopreview_core::CommentKind::Draft,
            }],
        }
    }

    #[test]
    fn e_opens_and_d_targets_your_own_published_comment() {
        let mut app = pr_app(); // the viewer is "tester"
        app.review
            .threads
            .push(published_comment("c", "tester", "555"));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        // Editing your own published comment opens the composer (the save itself
        // goes to GitHub in the background).
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(
            app.input.is_some(),
            "your own published comment is editable"
        );
        assert_eq!(app.input.as_ref().unwrap().area.text(), "posted");
        app.input = None; // don't trigger the network save
        // Deleting it targets that single published comment; a line-anchored
        // thread is a review comment.
        let target = app.selected_delete_target().expect("a deletable target");
        assert_eq!(target.comment_id.as_deref(), Some("c"));
        assert_eq!(target.published, Some(CommentEndpoint::ReviewComment(555)));
    }

    #[test]
    fn published_edit_and_delete_refuse_another_authors_comment() {
        let mut app = pr_app(); // the viewer is "tester"
        app.review
            .threads
            .push(published_comment("c", "someone-else", "555"));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(
            app.input.is_none(),
            "another author's comment isn't editable"
        );
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("your own published")
        );
        assert!(
            app.selected_delete_target().is_none(),
            "nor is it deletable"
        );
    }

    #[test]
    fn excerpt_and_friendly_error_helpers() {
        assert_eq!(one_line_excerpt("short", 56), "short");
        assert_eq!(one_line_excerpt("  first line\nsecond", 56), "first line");
        let clipped = one_line_excerpt(&"x".repeat(100), 10);
        assert_eq!(clipped.chars().count(), 10, "9 chars plus the ellipsis");
        assert!(clipped.ends_with('…'));

        assert!(
            friendly_github_write_error("HTTP 403: Forbidden".into())
                .contains("check the comment is yours")
        );
        assert!(friendly_github_write_error("gh: Not Found (HTTP 404)".into()).contains("auth"));
        assert!(
            friendly_github_write_error(
                "HTTP 422: A pending review already exists (create a review)".into()
            )
            .contains("pending review already exists"),
            "a pending-review 422 gets a pointed explanation"
        );
        assert_eq!(
            friendly_github_write_error("network down".into()),
            "network down",
            "an unrelated error passes through unchanged"
        );
    }

    #[test]
    fn kind_badges_reflect_disposition() {
        let mk = |kind, remote: Option<&str>| Comment {
            id: "c".into(),
            author: "a".into(),
            body: "b".into(),
            created_at: 0,
            remote_id: remote.map(str::to_string),
            kind,
        };
        // Published (it has a remote id) shows no badge, whatever the kind.
        assert_eq!(kind_badge(&mk(CommentKind::Draft, Some("R1"))), None);
        assert_eq!(kind_index_badge(&mk(CommentKind::Draft, Some("R1"))), None);
        // Local is the subdued default; draft draws attention.
        assert_eq!(
            kind_badge(&mk(CommentKind::Local, None)).unwrap().0,
            "[local]"
        );
        assert_eq!(
            kind_badge(&mk(CommentKind::Draft, None)).unwrap().0,
            "[draft]"
        );
        assert_eq!(
            kind_index_badge(&mk(CommentKind::Local, None)).unwrap().0,
            "[l]"
        );
        assert_eq!(
            kind_index_badge(&mk(CommentKind::Draft, None)).unwrap().0,
            "[d]"
        );
    }

    #[test]
    fn t_toggles_a_thread_between_local_and_draft_on_a_pr() {
        let mut app = pr_app();
        // An agent left a local note; the human adopts it as a draft with `t`.
        add(&mut app, 2, false);
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(app.review.threads[0].root().unwrap().is_local());
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].root().unwrap().is_draft(),
            "t promotes a local note to a draft"
        );
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].root().unwrap().is_local(),
            "t again demotes it back to local"
        );

        // In a local review the toggle is a no-op with an explanatory status.
        let mut local = sample_app();
        add(&mut local, 2, false);
        local.view = View::Conversation;
        local.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(local.review.threads[0].root().unwrap().is_local());
        assert!(
            local
                .status
                .as_deref()
                .unwrap_or("")
                .contains("pull request")
        );
    }

    /// Build a PR thread with a root of `root_kind` plus one reply per entry in
    /// `replies`, laid out and focused in the Conversation view. Returns the app.
    fn pr_thread(root_kind: CommentKind, replies: &[CommentKind]) -> App {
        let mut app = pr_app();
        let (tid, _) = app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "root",
            root_kind,
        );
        if root_kind == CommentKind::Published {
            app.review.threads[0].comments[0].remote_id = Some("R1".into());
        }
        for (i, &k) in replies.iter().enumerate() {
            app.add_reply(&tid, "tester", &format!("reply {i}"), k);
        }
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app
    }

    #[test]
    fn t_targets_the_reply_at_the_cursor_in_conversation() {
        // Rule 2: under a draft root a reply flips local⇄draft on its own, and `t`
        // now retargets per comment (like e/d) rather than always the root.
        let mut app = pr_thread(CommentKind::Draft, &[CommentKind::Local]);
        app.conv_comment = 1; // the reply, not the root
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[1].is_draft(),
            "the reply promotes under a draft root"
        );
        assert!(
            app.review.threads[0].root().unwrap().is_draft(),
            "and the root is left where it was"
        );
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[1].is_local(),
            "t again demotes just the reply"
        );
    }

    #[test]
    fn t_refuses_a_draft_reply_under_a_local_root() {
        // Rule 3: a reply under a local root can't become a draft — the root must
        // be promoted first, so a queued reply never dangles above a note that is
        // never sent.
        let mut app = pr_thread(CommentKind::Local, &[CommentKind::Local]);
        app.conv_comment = 1; // the reply
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[1].is_local(),
            "the reply stays local under a local root"
        );
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("promote the thread root first"),
            "the status points at the root"
        );
    }

    #[test]
    fn t_toggles_a_reply_under_a_published_root_but_never_the_root() {
        // Rule 1: replies under a published root flip freely. Rule 5: the published
        // root itself never changes kind.
        let mut app = pr_thread(CommentKind::Published, &[CommentKind::Local]);
        // The reply promotes even though the root is published.
        app.conv_comment = 1;
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[1].is_draft(),
            "a reply is free to draft under a published root"
        );
        // The published root refuses the toggle and keeps its disposition.
        app.conv_comment = 0;
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(
            app.review.threads[0].comments[0].disposition(),
            CommentKind::Published,
            "a published root can't change kind"
        );
        assert!(
            app.status.as_deref().unwrap_or("").contains("published"),
            "the status says the comment is published"
        );
    }

    #[test]
    fn demoting_a_root_to_local_drags_its_draft_replies_down() {
        // Rule 4: a root's draft→local demotion pulls every draft reply to local;
        // the reverse promotion leaves the replies where they are.
        let mut app = pr_thread(
            CommentKind::Draft,
            &[CommentKind::Draft, CommentKind::Local],
        );
        app.conv_comment = 0; // the root
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[0].is_local(),
            "the root is demoted"
        );
        assert!(
            app.review.threads[0].comments[1].is_local(),
            "the draft reply is dragged down to local"
        );
        assert!(
            app.review.threads[0].comments[2].is_local(),
            "the already-local reply stays local"
        );
        // Promote the root again: the replies are left untouched (still local).
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[0].is_draft(),
            "the root is promoted"
        );
        assert!(
            app.review.threads[0].comments[1].is_local()
                && app.review.threads[0].comments[2].is_local(),
            "promotion leaves the replies alone"
        );
    }

    #[test]
    fn t_in_the_files_view_still_targets_the_thread_root() {
        // The diff shows only the root inline, so in Files `t` stays root-scoped —
        // the cursor's line, not a per-comment target.
        let mut app = pr_thread(CommentKind::Local, &[CommentKind::Local]);
        app.view = View::Files;
        // Park the cursor on the anchored line (a.rs, new line 2).
        app.cursor = app
            .clines
            .iter()
            .position(|&(file, flat)| {
                flat != HEADER && {
                    let (hi, li) = app.flats[file][flat];
                    app.diff.files[file].hunks[hi].lines[li].new_lineno == Some(2)
                }
            })
            .expect("the anchored line is on screen");
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.review.threads[0].comments[0].is_draft(),
            "the root is promoted from the Files view"
        );
        assert!(
            app.review.threads[0].comments[1].is_local(),
            "the reply is untouched from the Files view"
        );
    }

    #[test]
    fn human_new_is_draft_on_a_pr_and_replies_inherit() {
        // A human's new comment: a note locally, a draft on a PR.
        assert!(matches!(sample_app().human_new_kind(), CommentKind::Local));
        let mut pr = pr_app();
        assert!(matches!(pr.human_new_kind(), CommentKind::Draft));
        // A reply to an *inline* thread inherits it: local under a local note,
        // draft otherwise. (Conversation-thread replies are always local — that
        // rule is covered separately.)
        let mk = |id: &str, kind: CommentKind| Thread {
            id: id.into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: format!("{id}c"),
                author: "a".into(),
                body: "b".into(),
                created_at: 0,
                remote_id: None,
                kind,
            }],
        };
        pr.review.threads.push(mk("loc", CommentKind::Local));
        pr.review.threads.push(mk("drf", CommentKind::Draft));
        assert!(matches!(pr.reply_kind("loc"), CommentKind::Local));
        assert!(matches!(pr.reply_kind("drf"), CommentKind::Draft));
    }

    #[test]
    fn c_in_conversation_starts_a_draft_conversation_comment() {
        let mut app = pr_app(); // a pull request; the viewer is "tester"
        app.view = View::Conversation;
        app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
        match &app.input.as_ref().expect("the composer opened").kind {
            ComposeKind::New(Anchor::Review) => {}
            _ => panic!("expected a conversation composer (Anchor::Review)"),
        }
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('i'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL); // Ctrl-S saves
        assert!(app.input.is_none(), "the composer closed on save");
        let convo = app
            .review
            .threads
            .iter()
            .find(|t| t.anchor == Anchor::Review)
            .expect("a conversation thread was created");
        assert_eq!(convo.root().unwrap().body, "hi");
        assert!(
            convo.root().unwrap().is_draft(),
            "a new conversation comment is a draft on a pull request"
        );
    }

    #[test]
    fn a_conversation_reply_is_always_local() {
        let mut app = pr_app();
        app.review.threads.push(Thread {
            id: "conv".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "root".into(),
                author: "someone".into(),
                body: "conversation root".into(),
                created_at: 0,
                remote_id: Some("900".into()), // a published conversation root
                kind: CommentKind::Draft,
            }],
        });
        assert_eq!(
            app.reply_kind("conv"),
            CommentKind::Local,
            "a reply under a conversation thread never becomes a draft"
        );
    }

    #[test]
    fn t_refuses_a_conversation_reply() {
        let mut app = pr_app();
        let (tid, _) = app.add_thread(Anchor::Review, "tester", "root", CommentKind::Draft);
        app.add_reply(&tid, "tester", "reply", CommentKind::Local);
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE); // step to the reply
        assert_eq!(app.conv_comment, 1);
        app.on_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("conversation replies stay local"),
            "t on a conversation reply is refused: {:?}",
            app.status
        );
        assert!(
            app.review.threads[0].comments[1].is_local(),
            "the reply stayed local"
        );
    }

    #[test]
    fn control_reply_refuses_draft_on_a_conversation_thread() {
        let mut app = pr_app();
        let (tid, _) = app.add_thread(Anchor::Review, "tester", "root", CommentKind::Draft);
        let res = app.control_comment_reply(protocol::CommentReply {
            thread: tid,
            body: "x".into(),
            author: "agent".into(),
            draft: true,
        });
        assert!(
            res.is_err(),
            "an agent draft reply on a conversation thread is refused: {res:?}"
        );
    }

    #[test]
    fn loading_demotes_a_draft_conversation_reply_to_local() {
        let mut app = pr_app();
        let (tid, _) = app.add_thread(Anchor::Review, "tester", "root", CommentKind::Draft);
        app.add_reply(&tid, "tester", "reply", CommentKind::Draft); // stale draft reply
        app.normalize_conversation_reply_drafts();
        assert!(
            app.review.threads[0].comments[1].is_local(),
            "the draft conversation reply became local"
        );
        assert!(
            app.review.threads[0].comments[0].is_draft(),
            "the root draft is untouched"
        );
    }

    #[test]
    fn footer_says_local_reply_on_a_conversation_thread() {
        let mut app = pr_app();
        app.add_thread(Anchor::Review, "tester", "root", CommentKind::Draft);
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        let f = app.footer_ops();
        assert!(
            f.contains("local reply"),
            "a conversation reply is flagged local in the footer: {f}"
        );

        // An inline thread keeps the plain `reply` wording.
        let mut inline = pr_app();
        inline.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "root",
            CommentKind::Draft,
        );
        inline.relayout();
        inline.view = View::Conversation;
        inline.conv_cursor = 0;
        let f = inline.footer_ops();
        assert!(
            f.contains("reply") && !f.contains("local reply"),
            "an inline reply is plain: {f}"
        );
    }

    #[test]
    fn reply_composer_title_marks_a_conversation_reply_local() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // A conversation reply is titled "Local reply to @<user>".
        let mut app = pr_app();
        app.add_thread(Anchor::Review, "alice", "root", CommentKind::Draft);
        app.relayout();
        app.view = View::Conversation;
        app.open_reply(0);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("Local reply to @alice"),
            "conversation reply title says local:\n{screen}"
        );

        // An inline reply is the plain "Reply to @<user>".
        let mut inline = pr_app();
        inline.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "bob",
            "root",
            CommentKind::Draft,
        );
        inline.relayout();
        inline.open_reply(0);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| inline.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("Reply to @bob") && !screen.contains("Local reply"),
            "an inline reply title is plain:\n{screen}"
        );
    }

    /// A conversation thread with a published root — `id` sets whether it looks
    /// pulled (an `issuecomment:`/`review:` prefix) or locally created.
    fn published_review_thread(id: &str, author: &str, remote: Option<&str>) -> Thread {
        Thread {
            id: id.into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c".into(),
                author: author.into(),
                body: "x".into(),
                created_at: 0,
                remote_id: remote.map(Into::into),
                kind: CommentKind::Draft,
            }],
        }
    }

    #[test]
    fn a_local_conversation_comment_routes_to_the_issue_endpoint() {
        // A conversation comment created locally with `c` keeps a plain generated
        // thread id (no `issuecomment:` prefix) but an Anchor::Review anchor. Once
        // published it must edit/delete via /issues/comments, not /pulls/comments.
        let mut app = pr_app();
        app.review
            .threads
            .push(published_review_thread("local-xyz", "tester", Some("777")));
        let endpoint = app
            .published_endpoint("local-xyz", "c")
            .expect("an endpoint");
        assert!(
            matches!(endpoint, CommentEndpoint::IssueComment(777)),
            "a local conversation comment routes to the issue endpoint: {endpoint:?}"
        );
    }

    #[test]
    fn my_own_just_submitted_comment_stays_editable_before_the_login_syncs() {
        // The Pattern A fix: a comment I drafted keeps my git name as its author
        // when it publishes, but my GitHub login differs — the login check alone
        // used to disown my own comment until the next pull.
        let mut app = sample_app(); // author (git name) "tester"
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test_with_viewer(
            1, "t", "octocat", // login != git name
        )));
        app.pr_key = Some("owner/repo#1".into());
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "mine, just sent",
            CommentKind::Draft,
        );
        app.review.threads[0].comments[0].remote_id = Some("500".into()); // just published
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(
            app.delete_target().is_ok(),
            "my just-published comment is still deletable: {:?}",
            app.delete_target()
        );
        assert!(app.can_edit_target(), "and editable");
    }

    #[test]
    fn delete_refusals_name_their_reason() {
        // Your own published review summary — GitHub has no delete for it.
        let mut app = pr_app(); // viewer "tester"
        app.review
            .threads
            .push(published_review_thread("review:42", "tester", Some("42")));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(
            app.delete_target()
                .unwrap_err()
                .contains("review summaries can't be deleted"),
            "reason: {:?}",
            app.delete_target()
        );

        // Your own published comment whose real id has not synced (a sentinel id
        // from a just-submitted comment) — recoverable by a refresh.
        let mut app = pr_app();
        app.review.threads.push(published_review_thread(
            "issuecomment:x",
            "tester",
            Some(crate::prsync::PENDING_REMOTE_ID),
        ));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(
            app.delete_target()
                .unwrap_err()
                .contains("no synced API id"),
            "reason: {:?}",
            app.delete_target()
        );

        // Someone else's published comment.
        let mut app = pr_app();
        app.review.threads.push(published_review_thread(
            "issuecomment:9",
            "someone-else",
            Some("9"),
        ));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(
            app.delete_target()
                .unwrap_err()
                .contains("your own published comment"),
            "reason: {:?}",
            app.delete_target()
        );

        // Your own published conversation comment with a real id — deletable.
        let mut app = pr_app();
        app.review.threads.push(published_review_thread(
            "issuecomment:5",
            "tester",
            Some("5"),
        ));
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(
            app.delete_target().is_ok(),
            "a real published id is deletable"
        );
    }

    /// A comment with an explicit author/kind/remote id, for delete-cascade tests.
    fn comment_of(id: &str, author: &str, remote: Option<&str>, kind: CommentKind) -> Comment {
        Comment {
            id: id.into(),
            author: author.into(),
            body: format!("body {id}"),
            created_at: 0,
            remote_id: remote.map(Into::into),
            kind,
        }
    }

    #[test]
    fn deleting_a_conversation_root_cascades_its_local_replies() {
        let mut app = pr_app();
        // A published conversation root with two local replies of my own.
        app.review.threads.push(Thread {
            id: "issuecomment:5".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![
                comment_of("root", "tester", Some("5"), CommentKind::Draft),
                comment_of("r1", "tester", None, CommentKind::Local),
                comment_of("r2", "tester", None, CommentKind::Local),
            ],
        });
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.conv_comment = 0; // the root

        // The confirmation counts the local replies that will cascade.
        let target = app.delete_target().expect("the root is deletable");
        assert_eq!(target.also_removed, 2, "both local replies are counted");

        // The post-GitHub-delete application removes the whole thread — no orphan.
        app.remove_comment_by_id("issuecomment:5", "root");
        assert!(
            app.review.threads.iter().all(|t| t.id != "issuecomment:5"),
            "the whole thread is gone — no orphan reply left behind"
        );
    }

    #[test]
    fn the_delete_prompt_states_the_cascade() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = pr_app();
        app.review.threads.push(Thread {
            id: "issuecomment:5".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![
                comment_of("root", "tester", Some("5"), CommentKind::Draft),
                comment_of("r1", "tester", None, CommentKind::Local),
            ],
        });
        app.relayout();
        app.view = View::Conversation;
        app.conv_cursor = 0;
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE); // arm the confirm
        assert!(app.confirming_delete.is_some());
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("1 local reply will be removed too"),
            "the prompt warns about the cascaded local reply:\n{screen}"
        );
    }

    #[test]
    fn d_in_the_thread_index_deletes_a_local_thread() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app(); // a worktree review: comments are local, no PR
        app.body_width.set(120);
        app.sidebar_override = Some(true);
        app.review.threads.push(Thread {
            id: "conv".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![
                comment_of("root", "tester", None, CommentKind::Local),
                comment_of("r1", "tester", None, CommentKind::Local),
                comment_of("r2", "tester", None, CommentKind::Local),
            ],
        });
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Sidebar;
        app.conv_cursor = 0;
        assert!(
            app.sidebar_width(app.body_width.get()).is_some(),
            "the thread index is shown"
        );
        assert_eq!(app.selected_thread(), Some(0));

        // d in the index arms the whole-thread delete (root ci = None), counting
        // the cascaded replies.
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        let target = app
            .confirming_delete
            .as_ref()
            .expect("d armed the confirmation");
        assert!(target.comment_id.is_none(), "the thread root is the target");
        assert_eq!(target.also_removed, 2, "its two replies are counted");

        // The prompt names the total.
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            screen_text(&term).contains("(3 comments)"),
            "the confirm names the thread's comment count"
        );

        // y confirms; the whole thread is gone.
        app.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.confirming_delete.is_none(), "the modal closed");
        assert!(
            !app.review.threads.iter().any(|t| t.id == "conv"),
            "the thread was removed"
        );
    }

    #[test]
    fn d_in_the_index_routes_a_published_root_to_github() {
        let mut app = pr_app(); // git name + viewer both "tester"
        app.body_width.set(120);
        app.sidebar_override = Some(true);
        app.review.threads.push(Thread {
            id: "issuecomment:5".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of("root", "tester", Some("5"), CommentKind::Draft)],
        });
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Sidebar;
        app.conv_cursor = 0;
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        let target = app
            .confirming_delete
            .as_ref()
            .expect("d armed the confirmation");
        assert!(
            target.published.is_some(),
            "my published root routes to a GitHub delete"
        );
        assert_eq!(target.comment_id.as_deref(), Some("root"));
    }

    #[test]
    fn d_in_the_index_refuses_someone_elses_published_root() {
        let mut app = pr_app();
        app.body_width.set(120);
        app.sidebar_override = Some(true);
        app.review.threads.push(Thread {
            id: "issuecomment:9".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of(
                "root",
                "someone-else",
                Some("9"),
                CommentKind::Draft,
            )],
        });
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Sidebar;
        app.conv_cursor = 0;
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            app.confirming_delete.is_none(),
            "someone else's published comment is not armed"
        );
        assert!(
            app.status.as_deref().unwrap_or("").contains("your own"),
            "the refusal names the reason: {:?}",
            app.status
        );
    }

    #[test]
    fn the_thread_index_footer_offers_delete_only_there() {
        let mut app = sample_app();
        app.body_width.set(120);
        app.sidebar_override = Some(true);
        app.review.threads.push(Thread {
            id: "conv".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![comment_of("root", "tester", None, CommentKind::Local)],
        });
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Sidebar;
        app.conv_cursor = 0;
        let key = app.keymap.key_for(Action::Delete).unwrap().to_string();
        assert!(
            app.footer_ops().contains(&format!("{key} delete")),
            "the thread index offers delete: {:?}",
            app.footer_ops()
        );
        // The file index (Files view sidebar) carries no comment actions.
        app.set_view(View::Files);
        assert!(
            !app.footer_ops().contains("delete"),
            "the file index has no delete: {:?}",
            app.footer_ops()
        );
    }

    #[test]
    fn deleting_one_published_comment_keeps_a_thread_that_still_has_another() {
        let mut app = pr_app();
        // An inline thread with two published comments.
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![
                comment_of("root", "tester", Some("500"), CommentKind::Draft),
                comment_of("r2", "tester", Some("600"), CommentKind::Draft),
            ],
        });
        app.relayout();
        app.remove_comment_by_id("t", "root");
        let thread = app
            .review
            .threads
            .iter()
            .find(|t| t.id == "t")
            .expect("the thread survives");
        assert_eq!(
            thread.comments.len(),
            1,
            "the other published comment anchors it"
        );
    }

    #[test]
    fn a_refresh_reports_local_notes_orphaned_by_a_deleted_thread() {
        let mut app = pr_app();
        // A published thread carrying a local note of mine.
        app.review.threads.push(Thread {
            id: "T1".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![
                comment_of("root", "someone", Some("r1"), CommentKind::Draft),
                comment_of("note", "tester", None, CommentKind::Local),
            ],
        });
        app.relayout();
        // The fresh pull no longer has the thread — it was deleted on GitHub.
        app.apply_job(Ok(JobOutcome::Refreshed {
            threads: Vec::new(),
            overview: None,
        }));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("local note(s) removed — their thread was deleted on GitHub"),
            "the refresh reports the orphan drop, not silently: {:?}",
            app.status
        );
        assert!(
            app.review.threads.iter().all(|t| t.id != "T1"),
            "the orphaned thread is gone"
        );
    }

    #[test]
    fn control_comment_add_rejects_a_line_not_in_the_diff() {
        let mut app = sample_app();
        let response = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(99),
            body: "x".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        }));
        assert!(matches!(response, Response::Error(_)));
        assert!(app.review.threads.is_empty());
    }

    #[test]
    fn control_add_creates_a_conversation_comment() {
        let mut app = pr_app();
        // Default (no --draft): a local conversation note.
        let result = app
            .control_comment_add(protocol::CommentAdd {
                file: None,
                side: None,
                line: None,
                body: "overall this reads well".into(),
                author: "agent".into(),
                draft: false,
                conversation: true,
            })
            .expect("a conversation comment is created");
        let thread = app
            .review
            .threads
            .iter()
            .find(|t| t.id == result.thread)
            .unwrap();
        assert_eq!(thread.anchor, Anchor::Review);
        assert!(
            thread.root().unwrap().is_local(),
            "an agent conversation comment defaults to local"
        );
        assert_eq!(thread.root().unwrap().body, "overall this reads well");

        // With --draft on a PR it queues for submit (same plan as the UI's `c`).
        let result = app
            .control_comment_add(protocol::CommentAdd {
                file: None,
                side: None,
                line: None,
                body: "queue this one".into(),
                author: "agent".into(),
                draft: true,
                conversation: true,
            })
            .unwrap();
        let thread = app
            .review
            .threads
            .iter()
            .find(|t| t.id == result.thread)
            .unwrap();
        assert!(thread.root().unwrap().is_draft(), "--draft queues it");
    }

    #[test]
    fn control_conversation_add_refuses_a_line_and_a_line_add_needs_one() {
        // --conversation with a line is contradictory.
        let mut app = pr_app();
        let res = app.control_comment_add(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
            body: "x".into(),
            author: "agent".into(),
            draft: false,
            conversation: true,
        });
        assert!(res.is_err(), "conversation + line is refused: {res:?}");

        // A non-conversation add with no line has nothing to anchor to.
        let mut app = sample_app();
        let res = app.control_comment_add(protocol::CommentAdd {
            file: None,
            side: None,
            line: None,
            body: "x".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        });
        assert!(res.is_err(), "a line comment needs file+line: {res:?}");
    }

    #[test]
    fn control_resolve_refuses_a_published_thread() {
        let mut app = sample_app();
        app.review.threads.push(Thread {
            id: "t1".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c1".into(),
                author: "reviewer".into(),
                body: "b".into(),
                created_at: 0,
                remote_id: Some("R1".into()),
                kind: loopreview_core::CommentKind::Draft, // published
            }],
        });
        let response = app.handle_control(Request::CommentResolve(protocol::CommentResolve {
            thread: "t1".into(),
            resolved: true,
            author: "agent".into(),
        }));
        assert!(matches!(response, Response::Error(_)));
        assert!(!app.review.threads[0].is_resolved(), "state unchanged");
    }

    #[test]
    fn a_draft_thread_is_not_resolvable_on_a_pr() {
        let mut app = pr_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "queued",
            CommentKind::Draft,
        );
        app.relayout();
        assert!(!app.is_resolvable(0), "a draft thread can't be resolved");
        // On the draft, the palette/footer drop Resolve.
        app.view = View::Conversation;
        app.conv_cursor = 0;
        assert!(
            !app.action_available(Action::Resolve),
            "x is not offered on a draft"
        );
        // The runtime guard refuses and points at the coherent moves.
        app.resolve_thread(0);
        assert!(!app.review.threads[0].is_resolved(), "the draft stays open");
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("draft can't be resolved"),
            "with guidance: {:?}",
            app.status
        );
    }

    #[test]
    fn local_and_published_threads_stay_resolvable_on_a_pr() {
        // A local note resolves — work-tracking semantics, never sent.
        let mut local = pr_app();
        local.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "agent",
            "note",
            CommentKind::Local,
        );
        assert!(local.is_resolvable(0), "a local note resolves on a pr");
        // A published line thread resolves — it syncs to GitHub.
        let mut published = pr_app();
        published.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "posted",
            CommentKind::Draft,
        );
        published.review.threads[0].comments[0].remote_id = Some("R1".into());
        assert!(
            published.is_resolvable(0),
            "a published line thread resolves"
        );
    }

    #[test]
    fn loading_reopens_a_resolved_draft() {
        let mut app = pr_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "queued",
            CommentKind::Draft,
        );
        // Stale state from before the guard: a draft marked resolved.
        app.review.threads[0].state = ThreadState::Resolved;
        app.normalize_resolved_drafts();
        assert!(
            !app.review.threads[0].is_resolved(),
            "the resolved draft is reopened"
        );
        // A resolved local note is legitimate — left as is.
        app.review.threads[0].comments[0].kind = CommentKind::Local;
        app.review.threads[0].state = ThreadState::Resolved;
        app.normalize_resolved_drafts();
        assert!(
            app.review.threads[0].is_resolved(),
            "a resolved local note stays resolved"
        );
    }

    #[test]
    fn control_resolve_refuses_a_draft_thread() {
        let mut app = sample_app();
        app.review.threads.push(Thread {
            id: "t1".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c1".into(),
                author: "agent".into(),
                body: "b".into(),
                created_at: 0,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            }],
        });
        let response = app.handle_control(Request::CommentResolve(protocol::CommentResolve {
            thread: "t1".into(),
            resolved: true,
            author: "agent".into(),
        }));
        assert!(matches!(response, Response::Error(_)));
        assert!(!app.review.threads[0].is_resolved(), "state unchanged");
    }

    #[test]
    fn control_navigate_reports_a_missing_target() {
        let mut app = sample_app();
        let response = app.handle_control(Request::Navigate(protocol::Navigate {
            thread: None,
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(999),
        }));
        match response {
            Response::Ok(Reply::Navigate(result)) => assert!(!result.moved),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn control_navigate_reaches_a_line_in_a_collapsed_file() {
        let mut app = sample_app(); // a.rs: new 1 ("keep"), new 2 ("added")
        // The file is collapsed and the Conversation view is showing — the
        // states that used to hide the line from validation.
        app.collapsed_files.insert("a.rs".into());
        app.view = View::Conversation;
        app.relayout();
        let response = app.handle_control(Request::Navigate(protocol::Navigate {
            thread: None,
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(2),
        }));
        match response {
            Response::Ok(Reply::Navigate(result)) => {
                assert!(result.moved, "navigate reaches a line in a collapsed file");
                assert_eq!(result.line, Some(2));
            }
            other => panic!("unexpected response: {other:?}"),
        }
        // It did the transitions itself: Files view, the file expanded.
        assert_eq!(app.view, View::Files);
        assert!(
            !app.collapsed_files.contains("a.rs"),
            "the file was expanded"
        );
        assert_eq!(
            clicked_line(&app),
            "added",
            "the cursor landed on new line 2"
        );
    }

    #[test]
    fn control_review_and_list_expose_the_diff_and_threads() {
        let mut app = sample_app();
        let _ = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: Some("a.rs".into()),
            side: Some(Side::New),
            line: Some(1),
            body: "note".into(),
            author: "agent".into(),
            draft: false,
            conversation: false,
        }));
        match app.handle_control(Request::Review {
            include_patch: true,
        }) {
            Response::Ok(Reply::Review(info)) => {
                assert_eq!(info.files.len(), 1);
                assert_eq!(info.files[0].hunks[0].lines.as_ref().unwrap().len(), 2);
                assert_eq!(info.threads.len(), 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
        match app.handle_control(Request::CommentList) {
            Response::Ok(Reply::Threads { threads }) => assert_eq!(threads.len(), 1),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn highlighting_is_incremental_and_reset_on_reload() {
        let mut app = sample_app();
        app.mode = Mode::Unified;
        // sample_app's file 0 has two flat lines; nothing is highlighted yet.
        assert!(app.render.borrow()[0].is_none());

        // Highlighting up to a line computes only that far, and extends on demand.
        app.ensure_highlight(0, 0);
        assert_eq!(app.render.borrow()[0].as_ref().unwrap().highlight.len(), 1);
        assert!(!app.render.borrow()[0].as_ref().unwrap().highlight[0].is_empty());
        app.ensure_highlight(0, 1);
        assert_eq!(app.render.borrow()[0].as_ref().unwrap().highlight.len(), 2);
        // Re-requesting an already-highlighted line is a no-op.
        app.ensure_highlight(0, 0);
        assert_eq!(app.render.borrow()[0].as_ref().unwrap().highlight.len(), 2);

        // A reload drops the cache so no stale highlight survives a diff change.
        let diff = app.diff.clone();
        app.reload(diff);
        assert!(app.render.borrow()[0].is_none());
    }

    // -- file collapse ------------------------------------------------------

    fn one_file(path: &str) -> FileDiff {
        FileDiff {
            old_path: Some(path.into()),
            new_path: Some(path.into()),
            status: ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                section: None,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        content: "keep".into(),
                        old_lineno: Some(1),
                        new_lineno: Some(1),
                    },
                    Line {
                        kind: LineKind::Addition,
                        content: "added".into(),
                        old_lineno: None,
                        new_lineno: Some(2),
                    },
                ],
            }],
        }
    }

    #[test]
    fn collapsing_a_file_hides_content_and_skips_highlight() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.collapsed_files.insert("a.rs".to_string());
        app.relayout_to_file_header(0);
        assert_eq!(
            app.clines.len(),
            1,
            "only the header cursor stop when collapsed"
        );
        assert_eq!(
            app.urows
                .iter()
                .filter(|r| matches!(r, URow::Line { .. }))
                .count(),
            0,
            "no content rows when collapsed"
        );

        // Drawing must not highlight a collapsed file.
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            app.render.borrow()[0].is_none(),
            "collapsed file is never highlighted"
        );

        // `o` on the collapsed file expands it.
        app.cursor = 0;
        app.toggle_fold();
        assert!(!app.collapsed_files.contains("a.rs"));
        assert_eq!(
            app.urows
                .iter()
                .filter(|r| matches!(r, URow::Line { .. }))
                .count(),
            2,
            "content rows return when expanded"
        );
    }

    #[test]
    fn collapsed_headers_stack_without_blank_lines() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.mode = Mode::Unified;
        for p in ["a.rs", "b.rs", "c.rs"] {
            app.collapsed_files.insert(p.into());
        }
        app.relayout();
        let spacers = app
            .urows
            .iter()
            .filter(|r| matches!(r, URow::Spacer))
            .count();
        assert_eq!(
            spacers, 0,
            "collapsed headers stack directly, no blank lines"
        );
        assert_eq!(app.urows.len(), 3, "exactly the three header rows");
    }

    #[test]
    fn a_blank_line_only_follows_a_file_with_content() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.mode = Mode::Unified;
        // a expanded, b collapsed: one spacer after a's content, before b.
        app.collapsed_files.insert("b.rs".into());
        app.relayout();
        assert_eq!(
            app.urows
                .iter()
                .filter(|r| matches!(r, URow::Spacer))
                .count(),
            1,
            "a blank line separates the expanded file from the next header"
        );
        // Collapse a too: now no spacer between the two stacked headers.
        app.collapsed_files.insert("a.rs".into());
        app.relayout();
        assert_eq!(
            app.urows
                .iter()
                .filter(|r| matches!(r, URow::Spacer))
                .count(),
            0,
        );
    }

    #[test]
    fn file_headers_render_as_full_width_bands() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.mode = Mode::Unified;
        app.sidebar_override = Some(false); // no frame; the header spans col 0..width
        app.cursor = 0; // a.rs header is the cursor; b.rs header is not
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let row_of = |needle: &str| -> u16 {
            for y in 0..24u16 {
                let text: String = (0..80u16).map(|x| buf[(x, y)].symbol()).collect();
                if text.contains(needle) {
                    return y;
                }
            }
            panic!("row with {needle:?} rendered");
        };
        let a_row = row_of("a.rs");
        let b_row = row_of("b.rs");
        // The cursored header is a strong band spanning the full width.
        assert_eq!(buf[(0, a_row)].bg, HEADER_CURSOR_BG);
        assert_eq!(
            buf[(79, a_row)].bg,
            HEADER_CURSOR_BG,
            "band spans full width"
        );
        // The non-cursor header still gets a faint full-width band.
        assert_eq!(buf[(0, b_row)].bg, HEADER_BG);
        assert_eq!(
            buf[(79, b_row)].bg,
            HEADER_BG,
            "faint band spans full width"
        );
        assert_ne!(HEADER_BG, HEADER_CURSOR_BG);
    }

    #[test]
    fn clicking_a_stacked_header_maps_to_its_file() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.mode = Mode::Unified;
        app.sidebar_override = Some(false);
        for p in ["a.rs", "b.rs", "c.rs"] {
            app.collapsed_files.insert(p.into());
        }
        app.relayout();
        // Headers stack at body rows 0,1,2 (no spacers). Click the third.
        app.hit.set(hit(1, 0, None, 0)); // body starts at screen row 1, no sidebar
        app.mouse_down(0, 3); // screen row 3 = body row 2 = c.rs header
        assert_eq!(
            app.current_file(),
            2,
            "the compressed layout keeps click-to-row correct"
        );
    }

    #[test]
    fn auto_collapse_triggers_over_the_file_threshold() {
        let diff = Diff {
            files: vec![one_file("a.rs"), one_file("b.rs"), one_file("c.rs")],
            provenance: Provenance::default(),
        };
        let mut app = App::new(
            "t".into(),
            diff,
            Review::default(),
            None,
            "me".into(),
            Highlighter::new(),
            None,
        );
        app.mode = Mode::Unified;
        app.auto_collapse_files = 2; // 3 files > 2
        app.auto_collapse_lines = 1_000_000;
        app.maybe_auto_collapse();
        assert_eq!(app.collapsed_files.len(), 3);
        assert_eq!(
            app.clines.len(),
            3,
            "one header cursor stop per collapsed file"
        );
    }

    #[test]
    fn a_thread_on_a_collapsed_file_stays_placed_not_outdated() {
        let diff = Diff {
            files: vec![one_file("a.rs")],
            provenance: Provenance::default(),
        };
        let review = Review {
            threads: vec![Thread {
                id: "t".into(),
                anchor: Anchor::line("a.rs", Side::New, 2),
                state: ThreadState::Open,
                comments: vec![Comment {
                    id: "c".into(),
                    author: "a".into(),
                    body: "b".into(),
                    created_at: 0,
                    remote_id: None,
                    kind: loopreview_core::CommentKind::Draft,
                }],
            }],
        };
        let mut collapsed = HashSet::new();
        collapsed.insert("a.rs".to_string());
        let layout = Layouts::build(&diff, &review, &[1], &collapsed);
        assert!(
            layout.placed[0],
            "a present line stays placed even when its file is collapsed"
        );
    }

    // -- hierarchical navigation (headers as cursor stops, h/l) -------------

    #[test]
    fn file_headers_are_cursor_stops() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        // clines: [a-header, a-line1, a-line2, b-header, b-line1, b-line2].
        assert!(app.cursor_is_header(), "starts on the first file's header");
        app.move_cursor(1);
        assert!(!app.cursor_is_header(), "j steps into the content");

        // With everything collapsed, j/k walks just the headers.
        app.collapsed_files.insert("a.rs".to_string());
        app.collapsed_files.insert("b.rs".to_string());
        app.relayout();
        assert_eq!(app.clines.len(), 2, "two headers when all collapsed");
        app.cursor = 0;
        assert!(app.cursor_is_header());
        app.move_cursor(1);
        assert!(app.cursor_is_header());
        assert_eq!(app.current_file(), 1);
    }

    #[test]
    fn l_and_h_move_in_and_out_of_a_file() {
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(false); // isolate nav from the sidebar
        app.collapsed_files.insert("a.rs".to_string());
        app.relayout();
        app.cursor = 0;
        assert!(app.cursor_is_header());

        // l expands a collapsed header (cursor stays on the header).
        app.nav_in();
        assert!(!app.collapsed_files.contains("a.rs"));
        assert!(app.cursor_is_header());
        // l again enters the file's first content line.
        app.nav_in();
        assert!(!app.cursor_is_header());
        assert_eq!(app.current_file(), 0);
        // h jumps from a line back to its header.
        app.nav_out();
        assert!(app.cursor_is_header());
        // h on a header no longer folds — folding moved to Enter / `o`. With the
        // sidebar hidden there is nowhere to step out to, so h leaves the cursor
        // on the header and the file open.
        app.nav_out();
        assert!(
            !app.collapsed_files.contains("a.rs"),
            "h does not fold an open file"
        );
        assert!(app.cursor_is_header(), "h leaves the cursor on the header");

        // Enter is what folds a file from its header (and re-expands it).
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            app.collapsed_files.contains("a.rs"),
            "Enter folds the file from its header"
        );
    }

    #[test]
    fn h_on_a_collapsed_header_focuses_the_sidebar_when_shown() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        app.collapsed_files.insert("a.rs".to_string());
        app.relayout();
        app.cursor = 0; // a's collapsed header
        app.nav_out();
        assert_eq!(app.focus, Focus::Sidebar);
    }

    #[test]
    fn h_cascade_reaches_the_sidebar_through_keys() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // A real draw wires up the sidebar; then key events drive the cascade
        // line → header → sidebar (no fold hop — folding moved to Enter),
        // checking focus at each step.
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.sidebar_override = Some(true);
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.sidebar_width(app.body_width.get()).is_some());

        let line = app.file_first_line(0).expect("file a has a content line");
        app.set_cursor(line);
        assert_eq!(app.focus, Focus::Body);
        // h: a line jumps to its own header.
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(app.cursor_is_header());
        assert_eq!(app.focus, Focus::Body);
        // h: a header steps straight out to the sidebar, without folding.
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(
            app.focus,
            Focus::Sidebar,
            "the h cascade reaches the sidebar"
        );
        assert!(
            !app.collapsed_files.contains("a.rs"),
            "h did not fold the file on its way out"
        );
    }

    #[test]
    fn enter_toggles_the_fold_on_a_file_header() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.sidebar_override = Some(false); // isolate from the sidebar
        app.relayout();
        app.cursor = 0; // a.rs header, expanded
        assert!(app.cursor_is_header());
        assert!(!app.collapsed_files.contains("a.rs"));

        // Enter on an expanded header folds the file (cursor rests on the header).
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.collapsed_files.contains("a.rs"), "Enter folds the file");
        assert!(app.cursor_is_header(), "the cursor stays on the header");

        // Enter on the now-collapsed header re-expands it (a true toggle).
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            !app.collapsed_files.contains("a.rs"),
            "Enter re-expands the collapsed file"
        );
    }

    #[test]
    fn enter_on_a_non_header_line_does_not_fold() {
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(false);
        app.relayout();
        let line = app.file_first_line(0).expect("a.rs has a content line");
        app.set_cursor(line);
        assert!(!app.cursor_is_header());

        // Enter on a diff line keeps its NavIn meaning (a no-op there): it must
        // not fold the file, and must not move the cursor.
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            !app.collapsed_files.contains("a.rs"),
            "Enter on a line does not fold the file"
        );
        assert_eq!(app.cursor, line, "Enter on a line does not move the cursor");
    }

    #[test]
    fn enter_outside_a_files_header_still_navigates_in() {
        // Enter keeps its NavIn role in the Conversation view: a collapsed thread
        // expands (the file-header fold is Files-only, header-only).
        let mut app = app_with_threads();
        app.set_view(View::Conversation);
        app.sidebar_override = Some(false);
        app.focus = Focus::Body;
        app.conv_cursor = 0;
        app.fold_selected(true);
        assert!(app.selected_collapsed(), "the thread starts collapsed");

        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            !app.selected_collapsed(),
            "Enter expands a collapsed thread (still NavIn outside a file header)"
        );
    }

    /// The background under the body's cursor bar (`▎`), scanning only the
    /// content area so the sidebar's own bars are not picked up.
    fn body_cursor_bg(
        term: &ratatui::Terminal<ratatui::backend::TestBackend>,
        content_x0: u16,
    ) -> Option<Color> {
        let buf = term.backend().buffer();
        let (w, h) = (buf.area().width, buf.area().height);
        for y in 0..h {
            for x in content_x0..w {
                if buf[(x, y)].symbol() == "▎" {
                    return Some(buf[(x, y)].bg);
                }
            }
        }
        None
    }

    fn footer_text(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buf = term.backend().buffer();
        let (w, h) = (buf.area().width, buf.area().height);
        (0..w).map(|x| buf[(x, h - 1)].symbol()).collect()
    }

    fn screen_text(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buf = term.backend().buffer();
        let (w, h) = (buf.area().width, buf.area().height);
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    /// A thread anchored to `file`:`line` with `replies` replies under `author`.
    fn thread_on(file: &str, line: u32, author: &str, replies: usize) -> Thread {
        let mut comments = vec![Comment {
            id: format!("{file}-{line}-root"),
            author: author.into(),
            body: "root".into(),
            created_at: 0,
            remote_id: None,
            kind: loopreview_core::CommentKind::Draft,
        }];
        for r in 0..replies {
            comments.push(Comment {
                id: format!("{file}-{line}-r{r}"),
                author: "reviewer".into(),
                body: "reply".into(),
                created_at: (r + 1) as u64,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            });
        }
        Thread {
            id: format!("t-{file}-{line}"),
            anchor: Anchor::line(file, Side::New, line),
            state: ThreadState::Open,
            comments,
        }
    }

    fn app_with_threads() -> App {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.sidebar_override = Some(true);
        app.review.threads.push(thread_on("a.rs", 1, "alice", 2));
        app.review.threads.push(thread_on("b.rs", 2, "bob", 0));
        app.relayout();
        app
    }

    /// A single-comment thread with a given anchor and root time.
    fn thread_with(id: &str, anchor: Anchor, created_at: u64) -> Thread {
        Thread {
            id: id.into(),
            anchor,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: format!("{id}-root"),
                author: "author".into(),
                body: "b".into(),
                created_at,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            }],
        }
    }

    /// An app whose threads are stored out of chronological order: an inline
    /// thread (newest), a review-level thread (oldest), and a file-level thread
    /// (middle) — so display order must differ from storage order.
    fn app_mixed_threads() -> App {
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(true);
        app.review.threads.push(thread_with(
            "inline",
            Anchor::line("a.rs", Side::New, 1),
            300,
        ));
        app.review
            .threads
            .push(thread_with("review", Anchor::Review, 100));
        app.review.threads.push(thread_with(
            "file",
            Anchor::File {
                file: "a.rs".into(),
            },
            200,
        ));
        app.relayout();
        app
    }

    #[test]
    fn conversation_orders_threads_by_root_time() {
        let app = app_mixed_threads();
        // Sorted by root created_at ascending: review(100), file(200), inline(300),
        // which are storage indices 1, 2, 0 — storage order is left untouched.
        assert_eq!(app.conv_order, vec![1, 2, 0]);
        assert_eq!(
            app.review.threads[0].id, "inline",
            "storage order unchanged"
        );
    }

    #[test]
    fn thread_index_selection_matches_display_order() {
        let mut app = app_mixed_threads();
        app.view = View::Conversation;
        app.body_width.set(120);
        app.hit.set(hit(1, 22, None, 0));

        // Clicking the top thread-index row selects the oldest thread (the
        // review thread, storage index 1) — the same thread drawn on that row.
        app.mouse_down(3, 1); // sidebar body row 0 = display position 0
        assert_eq!(app.conv_cursor, 0, "top row is display position 0");
        assert_eq!(
            app.selected_thread(),
            Some(1),
            "row 0 selects the oldest thread"
        );

        // j moves down the display order to the middle then newest thread.
        app.focus = Focus::Sidebar;
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.selected_thread(), Some(2), "next is the file thread");
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.selected_thread(), Some(0), "last is the newest inline");
    }

    /// A single thread whose body is taller than any small viewport.
    fn app_with_tall_thread() -> App {
        let mut app = multi_file_app(&["a.rs"]);
        let mut comments = vec![Comment {
            id: "root".into(),
            author: "a".into(),
            body: "root".into(),
            created_at: 0,
            remote_id: None,
            kind: loopreview_core::CommentKind::Draft,
        }];
        for i in 0..40u64 {
            comments.push(Comment {
                id: format!("r{i}"),
                author: "b".into(),
                body: format!("reply {i}"),
                created_at: i + 1,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            });
        }
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments,
        });
        app.relayout();
        app.view = View::Conversation;
        app
    }

    #[test]
    fn conversation_free_scroll_reaches_a_tall_thread() {
        let mut app = app_with_tall_thread();
        app.body_height.set(10);
        let max = app.conv_max_scroll();
        assert!(max > 0, "the thread is taller than the viewport");
        // g/G scroll to the ends without moving the selection.
        app.conversation_action(Action::Bottom);
        assert_eq!(app.conv_scroll, max, "G reaches the content end");
        app.scroll_conv(10_000);
        assert_eq!(app.conv_scroll, max, "the wheel clamps at the end");
        app.scroll_conv(-10_000);
        assert_eq!(app.conv_scroll, 0, "and at the top");
        assert_eq!(app.conv_cursor, 0, "free scroll leaves the selection alone");
    }

    #[test]
    fn review_anchor_reads_conversation_everywhere() {
        let anchor = Anchor::Review;
        assert_eq!(anchor_label(&anchor), "conversation");
        assert_eq!(thread_index_label(&anchor), "conversation");
    }

    fn thread_state(id: &str, anchor: Anchor, state: ThreadState) -> Thread {
        let mut t = thread_with(id, anchor, 0);
        t.state = state;
        t
    }

    #[test]
    fn resolved_threads_start_collapsed_manual_override_wins() {
        let mut app = multi_file_app(&["a.rs"]);
        app.review
            .threads
            .push(thread_state("open1", Anchor::Review, ThreadState::Open));
        app.review
            .threads
            .push(thread_state("res1", Anchor::Review, ThreadState::Resolved));
        app.relayout(); // re-derives the default folds
        assert!(!app.collapsed.contains("open1"), "open starts expanded");
        assert!(app.collapsed.contains("res1"), "resolved starts collapsed");
        // A hand unfold of the resolved thread survives a re-derive.
        app.toggle_collapse("res1".into());
        assert!(!app.collapsed.contains("res1"));
        app.relayout();
        assert!(
            !app.collapsed.contains("res1"),
            "manual override is not clobbered by defaults"
        );
    }

    #[test]
    fn resolving_a_thread_folds_it() {
        let mut app = multi_file_app(&["a.rs"]);
        app.review.threads.push(thread_state(
            "t",
            Anchor::line("a.rs", Side::New, 2),
            ThreadState::Open,
        ));
        app.relayout();
        assert!(!app.collapsed.contains("t"));
        app.resolve_thread(0);
        assert!(
            app.collapsed.contains("t") && app.review.threads[0].is_resolved(),
            "resolve folds the thread"
        );
        app.resolve_thread(0);
        assert!(!app.collapsed.contains("t"), "reopen expands it");
    }

    #[test]
    fn conversation_enter_folds_h_moves_out_l_expands() {
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        app.review
            .threads
            .push(thread_state("t", Anchor::Review, ThreadState::Open));
        app.relayout();
        app.view = View::Conversation;
        app.focus = Focus::Body;
        app.conv_cursor = 0;
        app.conv_comment = 0;
        assert!(!app.selected_collapsed());

        // Enter on the thread header toggles its fold (folding moved off h).
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            app.selected_collapsed(),
            "Enter folds the thread from its header"
        );
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.selected_collapsed(), "Enter re-expands it");

        // h steps straight out to the thread index — pure movement, no fold.
        app.conversation_action(Action::NavOut);
        assert_eq!(app.focus, Focus::Sidebar, "h goes to the thread index");
        assert!(!app.selected_collapsed(), "h did not fold on the way out");

        // l expands a collapsed thread.
        app.focus = Focus::Body;
        app.fold_selected(true);
        app.conversation_action(Action::NavIn);
        assert!(!app.selected_collapsed(), "l expands a collapsed thread");
    }

    #[test]
    fn conversation_enter_on_a_reply_does_not_fold() {
        // Enter folds only from the thread header; on a reply it is a no-op, the
        // same as Enter on a diff line in the Files view.
        let mut app = app_with_threads(); // first thread has two replies
        app.sidebar_override = Some(false);
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        app.conv_cursor = 0;
        app.conv_comment = 1; // down on a reply, not the root/header
        assert!(!app.conv_on_thread_header());
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            !app.selected_collapsed(),
            "Enter on a reply does not fold the thread"
        );
    }

    #[test]
    fn conversation_g_moves_to_the_first_and_last_comment() {
        let mut app = app_with_threads(); // one thread has a root + two replies
        app.body_height.set(4);
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        app.conv_cursor = 0;
        app.conv_comment = 1; // start mid-thread

        // G selects the last comment of the last thread, and the pane follows.
        app.conversation_action(Action::Bottom);
        let last = app.conv_order.len() - 1;
        assert_eq!(app.conv_cursor, last, "G lands on the last thread");
        assert_eq!(
            app.conv_comment,
            app.selected_comment_count() - 1,
            "and on its last comment"
        );
        let ti = app.conv_order[app.conv_cursor];
        let within = app.conv_comment_starts[ti][app.conv_comment];
        let target = app.conv_offsets()[app.conv_cursor] + within;
        let h = app.body_height.get();
        assert!(
            (app.conv_scroll..app.conv_scroll + h).contains(&target),
            "the selected comment is on screen after G"
        );

        // g jumps back to the very first comment; the pane follows up to it.
        let scroll_at_bottom = app.conv_scroll;
        app.conversation_action(Action::Top);
        assert_eq!(app.conv_cursor, 0);
        assert_eq!(app.conv_comment, 0);
        let ti0 = app.conv_order[0];
        let first = app.conv_offsets()[0] + app.conv_comment_starts[ti0][0];
        assert_eq!(app.conv_scroll, first, "g scrolls up to the first comment");
        assert!(
            app.conv_scroll < scroll_at_bottom,
            "g scrolled the pane back up"
        );
    }

    #[test]
    fn conversation_g_last_handles_a_collapsed_final_thread() {
        let mut app = app_with_threads();
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        let last = app.conv_order.len() - 1;
        app.conv_cursor = last;
        app.fold_selected(true); // collapse the final thread
        assert!(app.selected_collapsed());
        app.conv_cursor = 0;
        app.conv_comment = 0;

        // G still lands on the collapsed thread, on its single stop (the header).
        app.conversation_action(Action::Bottom);
        assert_eq!(app.conv_cursor, last);
        assert_eq!(app.conv_comment, 0, "a collapsed thread has one stop");
    }

    #[test]
    fn c_on_a_file_header_guides_instead_of_no_op() {
        // A silent no-op is a project no-no: pressing c with nothing to anchor to
        // (a file header) explains the two ways to comment.
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.sidebar_override = Some(false);
        app.cursor = 0; // the file header — no line to anchor a comment to
        assert!(app.cursor_is_header());
        app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.input.is_none(), "no composer opens on a header");
        let status = app.status.as_deref().unwrap_or("");
        assert!(
            status.contains("diff line") && status.contains("Tab"),
            "the status points at both ways to comment: {status:?}"
        );
    }

    #[test]
    fn pr_conversation_comments_are_not_resolvable() {
        let mut app = multi_file_app(&["a.rs"]);
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test(1, "t")));
        app.review
            .threads
            .push(thread_with("conv", Anchor::Review, 0));
        app.review
            .threads
            .push(thread_with("line", Anchor::line("a.rs", Side::New, 1), 1));
        // Local notes (not drafts), so resolvability turns purely on the anchor —
        // a draft would be unresolvable regardless (covered separately).
        app.review.threads[0].comments[0].kind = CommentKind::Local;
        app.review.threads[1].comments[0].kind = CommentKind::Local;
        app.relayout();
        assert!(
            !app.is_resolvable(0),
            "a PR conversation comment can't resolve"
        );
        assert!(app.is_resolvable(1), "a PR review thread can resolve");

        // Resolving the conversation comment is a no-op with a friendly status.
        app.resolve_thread(0);
        assert!(!app.review.threads[0].is_resolved());
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("can't be resolved"),
            "a friendly status, not an error: {:?}",
            app.status
        );
        // The review thread resolves as usual.
        app.resolve_thread(1);
        assert!(
            app.review.threads[1].is_resolved(),
            "a review thread resolves"
        );

        // A local review (no PR) resolves everything, conversation comments too.
        app.pr = None;
        assert!(app.is_resolvable(0), "local reviews resolve everything");
    }

    #[test]
    fn the_sidebar_indexes_files_or_threads_by_view() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = app_with_threads();
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();

        // Files view: a file index and a Files frame title.
        app.set_view(View::Files);
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("Files (2)"),
            "sidebar titled Files: {screen:?}"
        );
        assert!(screen.contains("a.rs"), "the file index is shown");

        // Conversation view: a thread index, a Threads frame title, and a
        // review-context right-pane title (not a filename).
        app.set_view(View::Conversation);
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("Threads (2)"),
            "sidebar titled Threads: {screen:?}"
        );
        assert!(
            screen.contains("Review —"),
            "review-context pane title: {screen:?}"
        );
        assert!(screen.contains('○'), "an open-thread status glyph is shown");
    }

    #[test]
    fn conversation_pane_title_is_the_review_context() {
        let mut app = multi_file_app(&["a.rs"]);
        app.label = "working tree".into();
        app.view = View::Conversation;
        assert!(
            app.pane_title().contains("Review"),
            "local review title: {:?}",
            app.pane_title()
        );
        app.view = View::Files;
        assert!(
            app.pane_title().contains("a.rs"),
            "files view names the file: {:?}",
            app.pane_title()
        );
    }

    #[test]
    fn a_repo_source_shows_tabs_without_comments() {
        // A store-backed source (worktree / lr diff / lr show) shows the tabs
        // even before the first comment, so the Conversation tab is reachable.
        let mut app = sample_app();
        assert!(!app.has_review(), "no threads yet");
        assert!(app.comments_enabled(), "but comments are enabled (a store)");
        assert!(app.shows_tabs(), "so the tab structure is shown");
        // Tab switches into the Conversation view before any comment exists.
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Conversation);
    }

    #[test]
    fn a_patch_source_has_no_tabs_until_a_comment_exists() {
        // multi_file_app carries no store and no PR — the stdin/file patch case:
        // a lightweight pager with no comment surface and no tabs.
        let mut app = multi_file_app(&["a.rs"]);
        assert!(!app.comments_enabled(), "a patch cannot take comments");
        assert!(!app.shows_tabs(), "so it stays tab-less");
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Files, "Tab does nothing without tabs");
        // Shift+Tab is inert too, in both terminal forms.
        app.on_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Files, "BackTab is inert without tabs");
        app.on_key(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(app.view, View::Files, "Tab+SHIFT is inert without tabs");
    }

    #[test]
    fn shift_tab_cycles_the_tabs_in_reverse() {
        let mut app = sample_app(); // a repo source: tabs are shown
        assert_eq!(app.view, View::Files);
        // BackTab (one way a terminal reports Shift+Tab) cycles a tab back.
        app.on_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(
            app.view,
            View::Conversation,
            "Shift+Tab from Files reaches Conversation"
        );
        app.on_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Files);
        // The other form: Tab carrying SHIFT is reverse as well.
        app.on_key(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(app.view, View::Conversation, "Tab+SHIFT is also reverse");
        // A plain Tab still cycles forward.
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Files);
    }

    #[test]
    fn view_cycle_wraps_over_visible_tabs() {
        // Off a PR: two tabs, each direction wraps to the other.
        let mut app = multi_file_app(&["a.rs"]);
        app.view = View::Files;
        assert_eq!(app.cycle_view(true), View::Conversation);
        assert_eq!(app.cycle_view(false), View::Conversation);
        app.view = View::Conversation;
        assert_eq!(app.cycle_view(true), View::Files);

        // On a PR: three tabs, Overview leftmost. Files → forward Conversation,
        // back Overview; the cycle wraps.
        let mut pr = pr_app();
        pr.view = View::Files;
        assert_eq!(pr.cycle_view(true), View::Conversation);
        assert_eq!(pr.cycle_view(false), View::Overview);
        pr.view = View::Overview;
        assert_eq!(
            pr.cycle_view(false),
            View::Conversation,
            "wraps back to the end"
        );
        assert_eq!(pr.cycle_view(true), View::Files);
    }

    #[test]
    fn overview_tab_exists_only_for_a_pr() {
        let pr = pr_app();
        assert_eq!(
            pr.visible_views(),
            vec![View::Overview, View::Files, View::Conversation]
        );
        assert!(pr.shows_tabs());
        // A repo diff (store, no PR): two tabs, no Overview.
        let repo = sample_app();
        assert_eq!(repo.visible_views(), vec![View::Files, View::Conversation]);
        // A patch (no store, no PR): no tabs drawn.
        let patch = multi_file_app(&["a.rs"]);
        assert!(!patch.shows_tabs());
    }

    #[test]
    fn tab_and_shift_tab_reach_overview_and_conversation() {
        let mut app = pr_app();
        app.body_width.set(120);
        assert_eq!(app.view, View::Files, "the session opens on Files");
        // Shift+Tab (back) from Files → Overview; Tab forward wraps around.
        app.on_key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Overview);
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Files);
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Conversation);
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.view, View::Overview, "forward wraps back to Overview");
    }

    #[test]
    fn the_overview_renders_facts_and_body() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = pr_app();
        let mut ov = overview(PrStatus::Open);
        ov.body = "## Summary\n\nDoes the thing.".into();
        app.pr_overview = Some(ov);
        app.set_view(View::Overview);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(screen.contains("#1"), "the PR number: {screen:?}");
        assert!(screen.contains("Open"), "the status badge");
        assert!(screen.contains("Add the thing"), "the title");
        assert!(screen.contains("@octocat"), "the author");
        assert!(screen.contains("main ← feature"), "base ← head");
        assert!(screen.contains("opened 2026-07-20"), "the opened date");
        assert!(screen.contains("Summary"), "the markdown body heading");
        assert!(screen.contains("Does the thing"), "the body text");
    }

    #[test]
    fn the_overview_shows_a_placeholder_for_an_empty_body() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = pr_app();
        app.pr_overview = Some(overview(PrStatus::Open)); // body empty
        app.set_view(View::Overview);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(screen_text(&term).contains("No description provided."));
    }

    #[test]
    fn refresh_updates_the_overview_and_keeps_it_on_failure() {
        let mut app = pr_app();
        app.pr_overview = Some(overview(PrStatus::Open));
        // A fresh overview (open → merged) is applied on refresh.
        let mut merged = overview(PrStatus::Merged);
        merged.closed_at = Some("2026-07-22T00:00:00Z".into());
        app.apply_job(Ok(JobOutcome::Refreshed {
            threads: Vec::new(),
            overview: Some(Box::new(merged)),
        }));
        assert_eq!(
            app.pr_overview.as_ref().unwrap().status,
            SubjectStatus::Pr(PrStatus::Merged)
        );
        // A failed re-fetch (None) keeps the current overview.
        app.apply_job(Ok(JobOutcome::Refreshed {
            threads: Vec::new(),
            overview: None,
        }));
        assert_eq!(
            app.pr_overview.as_ref().unwrap().status,
            SubjectStatus::Pr(PrStatus::Merged),
            "a failed metadata re-fetch keeps the last overview"
        );
    }

    #[test]
    fn the_overview_is_read_only() {
        let mut app = pr_app();
        app.pr_overview = Some(overview(PrStatus::Open));
        app.set_view(View::Overview);
        // c opens no composer — the Overview carries no comment actions.
        app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.input.is_none(), "c does nothing in the Overview");
        let footer = app.footer_ops();
        assert!(
            footer.contains("scroll"),
            "footer offers scroll: {footer:?}"
        );
        assert!(
            !footer.contains("comment") && !footer.contains("reply"),
            "no comment ops in the Overview footer: {footer:?}"
        );
    }

    #[test]
    fn the_overview_scrolls_within_its_content() {
        let mut app = pr_app();
        let mut ov = overview(PrStatus::Open);
        ov.body = (0..40).map(|i| format!("line {i}\n")).collect();
        app.pr_overview = Some(ov);
        app.set_view(View::Overview);
        app.body_height.set(5);
        app.body_width.set(60);
        assert_eq!(app.overview_scroll, 0);
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.overview_scroll, 1);
        app.on_key(KeyCode::Char('G'), KeyModifiers::NONE);
        let bottom = app.overview_scroll;
        assert!(bottom > 1, "G jumps toward the bottom");
        app.on_key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.overview_scroll, 0, "g returns to the top");
        app.on_key(KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(app.overview_scroll, bottom, "G stops at the last line");
    }

    #[test]
    fn conversation_c_starts_a_review_comment_without_a_pr() {
        // The chicken-and-egg fix: on a repo diff (no PR), the Conversation `c`
        // creates a review-level (Anchor::Review) comment from an empty session.
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.sidebar_override = Some(false);
        app.set_view(View::Conversation);
        app.on_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.input.is_some(), "c opens the conversation composer");
        for ch in "hello".chars() {
            app.on_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Char('s'), KeyModifiers::CONTROL); // save
        assert!(app.input.is_none(), "Ctrl-S saves and closes");
        let thread = app.review.threads.last().expect("a thread was created");
        assert_eq!(
            thread.anchor,
            Anchor::Review,
            "it is a review-level comment"
        );
        assert_eq!(thread.root().unwrap().body, "hello");
    }

    #[test]
    fn an_empty_conversation_shows_a_placeholder() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app();
        app.set_view(View::Conversation);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("No comments yet"),
            "the empty conversation is greeted with a hint: {screen:?}"
        );
        assert!(screen.contains("press c"), "and names the key: {screen:?}");
    }

    #[test]
    fn thread_index_selects_and_jumps() {
        let mut app = app_with_threads();
        app.view = View::Conversation;
        app.focus = Focus::Sidebar;
        app.conv_cursor = 0;

        // j / k move the thread selection.
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.conv_cursor, 1, "j selects the next thread");
        app.sidebar_action(Action::MoveUp);
        assert_eq!(app.conv_cursor, 0);

        // l jumps into the thread: focus to the body, still the Conversation view.
        app.sidebar_action(Action::NavIn);
        assert_eq!(app.focus, Focus::Body);
        assert_eq!(app.view, View::Conversation);
        assert_eq!(app.conv_cursor, 0);

        // h steps straight out to the thread index — pure movement, no fold.
        app.body_width.set(120);
        app.conversation_action(Action::NavOut);
        assert_eq!(app.focus, Focus::Sidebar, "h goes to the thread index");
        assert!(!app.selected_collapsed(), "h did not fold on the way out");
    }

    #[test]
    fn sidebar_jk_scrolls_the_diff_to_the_selected_file() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        app.relayout();
        app.body_height.set(3);
        app.body_width.set(120);
        app.focus_sidebar();
        assert_eq!(app.focus, Focus::Sidebar);
        let cursor_before = app.cursor;
        assert_eq!(
            app.scroll, 0,
            "entering the sidebar does not scroll the diff"
        );

        // j down to the third file: the diff pane follows, anchoring that file's
        // header at the top of the viewport (clamped to the last scroll row).
        app.sidebar_action(Action::MoveDown);
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.sidebar_cursor, 2, "j moved the sidebar selection");

        let header = app.file_first[2].expect("file 2 has a header");
        let max = app.rows_len().saturating_sub(app.body_height.get());
        let want = app.line_urow[header].min(max);
        assert!(want > 0, "the third file is below the fold (real follow)");
        assert_eq!(app.scroll, want, "the diff scrolled to the selected file");
        assert_eq!(
            app.cursor, cursor_before,
            "a peek does not move the body cursor"
        );
        assert_eq!(
            app.focus,
            Focus::Sidebar,
            "a peek keeps focus in the sidebar"
        );

        // g jumps to the first file and the diff follows back to the top.
        app.sidebar_action(Action::Top);
        assert_eq!(app.sidebar_cursor, 0);
        assert_eq!(app.scroll, 0, "g follows the diff back to the first file");
    }

    #[test]
    fn sidebar_preview_peeks_but_enter_confirms() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.collapsed_files.insert("c.rs".into());
        app.relayout();
        app.body_height.set(3);
        app.focus_sidebar();
        let cursor_before = app.cursor;

        // Peek at c.rs: the selection and the diff scroll move, focus/cursor stay.
        app.sidebar_action(Action::MoveDown);
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.sidebar_cursor, 2);
        assert_eq!(
            app.focus,
            Focus::Sidebar,
            "a peek keeps focus in the sidebar"
        );
        assert_eq!(
            app.cursor, cursor_before,
            "a peek does not move the body cursor"
        );

        // Enter confirms: c.rs expands, the body takes focus, the cursor lands in it.
        app.sidebar_action(Action::NavIn);
        assert_eq!(app.focus, Focus::Body, "Enter hands focus to the body");
        assert_eq!(
            app.current_file(),
            2,
            "the cursor lands in the confirmed file"
        );
        assert!(
            !app.collapsed_files.contains("c.rs"),
            "Enter expands the collapsed file"
        );
    }

    #[test]
    fn a_body_focused_diff_is_unaffected_by_the_sidebar_follow() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.relayout();
        app.body_height.set(3);
        app.focus = Focus::Body;

        // With the diff focused, the scroll tracks the body cursor (reveal with a
        // scrolloff margin) — never the sidebar's file-header anchor.
        app.set_cursor(app.clines.len() - 1);
        let row = app.cursor_row();
        assert!(
            (app.scroll..app.scroll + app.body_height.get()).contains(&row),
            "the body cursor stays in view while the diff has focus"
        );
        assert_eq!(
            app.focus,
            Focus::Body,
            "browsing the diff does not change focus"
        );
    }

    #[test]
    fn a_sidebar_click_in_conversation_jumps_to_the_thread() {
        let mut app = app_with_threads();
        app.view = View::Conversation;
        app.body_width.set(120);
        app.hit.set(hit(1, 22, None, 0));
        // Sidebar body row 1 (screen row 2) is the second thread.
        app.mouse_down(3, 2);
        assert_eq!(
            app.conv_cursor, 1,
            "a thread-index click selects that thread"
        );
        assert_eq!(app.focus, Focus::Body);
    }

    #[test]
    fn conversation_clicks_toggle_headers_and_select_bodies() {
        let mut app = app_with_threads();
        app.view = View::Conversation;
        app.body_height.set(40);
        app.hit.set(hit(1, 0, None, 0)); // no sidebar; body starts at screen row 1
        let id0 = app.review.threads[app.conv_order[0]].id.clone();
        assert!(!app.collapsed.contains(&id0));

        // A body-line click (conv line 1, inside the open first block) selects
        // the thread but does not fold it.
        app.conv_cursor = 1;
        app.mouse_down(2, 2); // body row 1 = conv line 1
        assert_eq!(app.conv_cursor, 0, "a body click selects the thread");
        assert!(!app.collapsed.contains(&id0), "a body click does not fold");

        // A header click (conv line 0) folds the thread; clicking again expands.
        app.mouse_down(2, 1);
        assert!(
            app.collapsed.contains(&id0),
            "a header click folds the thread"
        );
        app.mouse_down(2, 1);
        assert!(
            !app.collapsed.contains(&id0),
            "a header click again expands"
        );
    }

    #[test]
    fn files_inline_comment_header_click_toggles_fold() {
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.sidebar_override = Some(false);
        app.review.threads.push(thread_on("a.rs", 2, "alice", 1));
        app.relayout();
        app.body_height.set(40);
        app.scroll = 0;
        app.hit.set(hit(1, 0, None, 0));
        let hdr = app
            .urows
            .iter()
            .position(|r| matches!(r, URow::Comment(0, 0)))
            .expect("an inline comment header row");
        // The header row hit-tests as a comment header; a diff line does not.
        assert_eq!(app.comment_header_at(hdr), Some(0));
        let line_row = app
            .urows
            .iter()
            .position(|r| matches!(r, URow::Line { .. }))
            .expect("a diff line row");
        assert_eq!(
            app.comment_header_at(line_row),
            None,
            "a diff line is not a header"
        );
        // Clicking the header folds the thread.
        let id = app.review.threads[0].id.clone();
        app.mouse_down(2, (1 + hdr) as u16);
        assert!(
            app.collapsed.contains(&id),
            "clicking the inline comment header folds the thread"
        );
    }

    /// A one-file diff with five context lines `line1`..`line5` (new 1..5).
    fn excerpt_diff() -> Diff {
        let lines: Vec<Line> = (1..=5u32)
            .map(|n| Line {
                kind: LineKind::Context,
                content: format!("line{n}"),
                old_lineno: Some(n),
                new_lineno: Some(n),
            })
            .collect();
        let file = FileDiff {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            status: ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 5,
                new_start: 1,
                new_lines: 5,
                section: None,
                lines,
            }],
        };
        Diff {
            files: vec![file],
            provenance: Provenance::default(),
        }
    }

    fn excerpt_text(lines: &[TextLine]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn excerpt_shows_placed_code_and_skips_file_anchor() {
        let h = Highlighter::new();
        let diff = excerpt_diff();
        // Single line: the anchored line plus preceding context.
        let lines = build_excerpt(&diff, &Anchor::line("a.rs", Side::New, 3), &h, 8);
        let text = excerpt_text(&lines);
        assert!(text.contains("line3"), "shows the anchored line: {text}");
        assert!(text.contains("line2"), "shows preceding context: {text}");
        // A file anchor and a line not in the diff produce no excerpt.
        assert!(
            build_excerpt(
                &diff,
                &Anchor::File {
                    file: "a.rs".into()
                },
                &h,
                8
            )
            .is_empty(),
            "a file anchor has no excerpt"
        );
        assert!(
            build_excerpt(&diff, &Anchor::line("a.rs", Side::New, 99), &h, 8).is_empty(),
            "an off-diff line has no excerpt"
        );
    }

    #[test]
    fn excerpt_clips_a_long_range_tail_first() {
        let h = Highlighter::new();
        let diff = excerpt_diff();
        let anchor = Anchor::Line {
            file: "a.rs".into(),
            side: Side::New,
            start: 1,
            end: 5,
            commit: None,
            context: Vec::new(),
        };
        let lines = build_excerpt(&diff, &anchor, &h, 3);
        let text = excerpt_text(&lines);
        assert!(text.contains('…'), "clipped with an ellipsis: {text}");
        assert!(
            text.contains("line5"),
            "keeps the tail (anchor end): {text}"
        );
        assert!(
            !text.contains("line1"),
            "drops the head when clipped: {text}"
        );
    }

    #[test]
    fn body_cursor_dims_when_the_sidebar_has_focus() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs"]);
        app.mode = Mode::Unified;
        app.sidebar_override = Some(true);
        let line = app.file_first_line(0).expect("a content line");
        app.set_cursor(line);
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();

        // Body focused: the cursor bar is the bright cursor color.
        app.focus = Focus::Body;
        term.draw(|f| app.draw(f)).unwrap();
        let x0 = app.hit.get().content_x0;
        assert_eq!(
            body_cursor_bg(&term, x0),
            Some(CURSOR_BG),
            "the active body cursor is bright"
        );

        // Sidebar focused: the same cursor dims.
        app.focus = Focus::Sidebar;
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            body_cursor_bg(&term, x0),
            Some(CURSOR_DIM_BG),
            "the inactive body cursor dims"
        );
        assert_ne!(CURSOR_BG, CURSOR_DIM_BG);
    }

    #[test]
    fn pane_frames_accent_the_focused_pane() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(true);
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();

        // Sidebar focused: its frame accents, the diff frame dims.
        app.focus = Focus::Sidebar;
        term.draw(|f| app.draw(f)).unwrap();
        let hit = app.hit.get();
        let border_row = hit.body_top - 1; // the top border sits above the content
        let sidebar_border_x = hit.sidebar_x0 - 1;
        let body_border_x = hit.content_x0 - 1;
        {
            let buf = term.backend().buffer();
            assert_eq!(
                buf[(sidebar_border_x, border_row)].fg,
                FOCUS_ACCENT,
                "the focused sidebar frame is accented"
            );
            assert_eq!(
                buf[(body_border_x, border_row)].fg,
                Color::DarkGray,
                "the unfocused diff frame is dim"
            );
        }

        // Body focused: the accent moves to the diff frame.
        app.focus = Focus::Body;
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        assert_eq!(
            buf[(body_border_x, border_row)].fg,
            FOCUS_ACCENT,
            "the focused diff frame is accented"
        );
        assert_eq!(
            buf[(sidebar_border_x, border_row)].fg,
            Color::DarkGray,
            "the unfocused sidebar frame is dim"
        );
    }

    #[test]
    fn tab_bar_padding_collapses_on_a_short_terminal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "c",
            CommentKind::Local,
        );
        app.relayout();
        app.sidebar_override = Some(false); // single pane, so content == body

        // Tall: blank rows pad the tab bar (header · gap · tabs · gap · body).
        let mut tall = Terminal::new(TestBackend::new(80, 24)).unwrap();
        tall.draw(|f| app.draw(f)).unwrap();
        let hit = app.hit.get();
        let tabs_row = hit.tabs_row.expect("the tab bar is shown");
        assert_eq!(
            tabs_row, 2,
            "a gap row sits between the header and the tabs"
        );
        assert_eq!(
            hit.body_top,
            tabs_row + 2,
            "a gap row sits between tabs and body"
        );
        assert_eq!(hit_region(0, tabs_row, hit), Region::Tabs);
        assert!(
            matches!(
                hit_region(hit.content_x0, hit.body_top, hit),
                Region::Content { .. }
            ),
            "the first body row is still content, at its shifted position"
        );

        // Short: the padding collapses (header · tabs · body) so the body keeps
        // its rows and the hit geometry stays consistent.
        let mut short = Terminal::new(TestBackend::new(80, 12)).unwrap();
        short.draw(|f| app.draw(f)).unwrap();
        let hit = app.hit.get();
        let tabs_row = hit.tabs_row.expect("the tab bar is shown");
        assert_eq!(tabs_row, 1, "no header/tabs gap when short");
        assert_eq!(hit.body_top, tabs_row + 1, "no tabs/body gap when short");
    }

    #[test]
    fn footer_hint_follows_focus() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(true);
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();

        app.focus = Focus::Sidebar;
        term.draw(|f| app.draw(f)).unwrap();
        let sidebar = footer_text(&term);
        assert!(sidebar.contains("l open"), "the sidebar shows its own hint");
        assert!(sidebar.contains("? all"), "and the palette anchor");

        // Body focus, cursor on a file header (index 0): move/open/fold, but no
        // `comment` (nothing on a header to comment on).
        app.focus = Focus::Body;
        app.cursor = 0;
        assert!(app.cursor_is_header());
        term.draw(|f| app.draw(f)).unwrap();
        let header = footer_text(&term);
        assert!(
            header.contains("enter fold"),
            "the header shows the Enter fold hint: {header:?}"
        );
        assert!(
            !header.contains("comment"),
            "and no comment hint on a header"
        );
    }

    // -- file explorer (sidebar + finder) -----------------------------------

    fn multi_file_app(paths: &[&str]) -> App {
        let files = paths.iter().map(|p| one_file(p)).collect();
        let diff = Diff {
            files,
            provenance: Provenance::default(),
        };
        let mut app = App::new(
            "t".into(),
            diff,
            Review::default(),
            None,
            "me".into(),
            Highlighter::new(),
            None,
        );
        app.mode = Mode::Unified;
        app
    }

    fn entry(index: usize, path: &str) -> FileEntry {
        FileEntry {
            index,
            path: path.into(),
            status: ChangeStatus::Modified,
            added: 1,
            removed: 0,
            comments: 0,
            collapsed: false,
        }
    }

    #[test]
    fn sidebar_width_honors_a_pinned_config() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        // A pinned width is used as-is when it fits and is within bounds.
        app.sidebar_width_cfg = Some(40);
        assert_eq!(app.sidebar_width(200), Some(40));
        // Out-of-bounds values clamp to the sensible range.
        app.sidebar_width_cfg = Some(100);
        assert_eq!(app.sidebar_width(200), Some(SIDEBAR_MAX));
        app.sidebar_width_cfg = Some(1);
        assert_eq!(app.sidebar_width(200), Some(SIDEBAR_MIN));
    }

    fn sidebar_entry(i: usize, p: &str) -> FileEntry {
        FileEntry {
            index: i,
            path: p.into(),
            status: ChangeStatus::Modified,
            added: 0,
            removed: 0,
            comments: 0,
            collapsed: false,
        }
    }

    #[test]
    fn sidebar_rows_group_files_under_directory_headers() {
        // Root files first (no header), then each directory once with its files;
        // note b.rs and c.rs are in the same dir but not adjacent in diff order.
        let entries = vec![
            sidebar_entry(0, "src/a/b.rs"),
            sidebar_entry(1, "top.rs"),
            sidebar_entry(2, "src/a/c.rs"),
            sidebar_entry(3, "tests/d.rs"),
        ];
        let rows = sidebar_rows(&entries);
        assert_eq!(
            rows,
            vec![
                SidebarRow::File(1), // root file, no header
                SidebarRow::DirHeader("src/a/".into()),
                SidebarRow::File(0),
                SidebarRow::File(2), // gathered into its directory group
                SidebarRow::DirHeader("tests/".into()),
                SidebarRow::File(3),
            ]
        );
    }

    #[test]
    fn sidebar_navigation_skips_directory_headers() {
        // Display rows: File(0=root), DirHeader(src/), File(1), File(2).
        let mut app = multi_file_app(&["z_root.rs", "src/a.rs", "src/b.rs"]);
        app.toggle_sidebar();
        assert_eq!(app.focus, Focus::Sidebar);
        app.sidebar_cursor = 0;
        // j moves to the next file row (1), stepping over the directory header.
        app.sidebar_action(Action::MoveDown);
        assert_eq!(
            app.sidebar_cursor, 1,
            "j skipped the header to the next file"
        );
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.sidebar_cursor, 2);
        // At the end, j holds; G/g reach the last/first file.
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.sidebar_cursor, 2, "j clamps at the last file");
        app.sidebar_action(Action::Top);
        assert_eq!(app.sidebar_cursor, 0);
        app.sidebar_action(Action::Bottom);
        assert_eq!(app.sidebar_cursor, 2);
    }

    #[test]
    fn conversation_sidebar_wheel_reaches_all_threads() {
        // One file, seven threads: the thread index must scroll to the last
        // thread, not be clamped by the file count (the old bug used files.len()
        // as the wheel bound, so the thread index couldn't reach past it).
        let mut app = sample_app();
        for i in 0..7 {
            app.add_thread(
                Anchor::line("a.rs", Side::New, 2),
                "me",
                &format!("c{i}"),
                CommentKind::Local,
            );
        }
        app.relayout();
        app.view = View::Conversation;
        app.body_height.set(3);
        for _ in 0..20 {
            app.scroll_sidebar(1);
        }
        // 7 threads, viewport 3 → the last reachable scroll is 7 - 3 = 4.
        assert_eq!(
            app.sidebar_scroll, 4,
            "the thread index scrolls to reveal the last thread"
        );
    }

    #[test]
    fn sidebar_row_mappings_are_bidirectional() {
        let entries = vec![
            sidebar_entry(0, "src/a.rs"),
            sidebar_entry(1, "top.rs"),
            sidebar_entry(2, "src/b.rs"),
        ];
        let rows = sidebar_rows(&entries);
        // rows: [File(1), DirHeader(src/), File(0), File(2)]
        assert_eq!(sidebar_file_order(&rows), vec![1, 0, 2]);
        // cursor (a file) -> its display row, and back.
        assert_eq!(row_of_file(&rows, 0), Some(2));
        assert_eq!(row_of_file(&rows, 1), Some(0));
        assert_eq!(file_at_row(&rows, 2), Some(0));
        // A header row maps to no file (non-selectable, non-clickable).
        assert_eq!(file_at_row(&rows, 1), None);
        assert_eq!(dir_of("src/a.rs"), "src/");
        assert_eq!(dir_of("top.rs"), "");
    }

    #[test]
    fn status_glyph_marks_each_change_kind() {
        use loopreview_core::ChangeStatus::*;
        assert_eq!(status_glyph(Added), 'A');
        assert_eq!(status_glyph(Deleted), 'D');
        assert_eq!(status_glyph(Modified), 'M');
        assert_eq!(status_glyph(Renamed), 'R');
        assert_eq!(status_glyph(Copied), 'C');
    }

    #[test]
    fn fuzzy_files_ranks_filters_and_reports_indices() {
        let entries = vec![
            entry(0, "src/main.rs"),
            entry(1, "src/ui/mod.rs"),
            entry(2, "README.md"),
        ];
        // Empty query keeps every file in order.
        let all = fuzzy_files(&entries, "");
        assert_eq!(all.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1, 2]);
        // "main" matches only main.rs, with match positions for highlighting.
        let m = fuzzy_files(&entries, "main");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, 0);
        assert!(!m[0].1.is_empty());
        // Smart case: "readme" finds README.md.
        assert_eq!(fuzzy_files(&entries, "readme")[0].0, 2);
        // No match.
        assert!(fuzzy_files(&entries, "zzzzz").is_empty());
    }

    #[test]
    fn finder_filters_and_jumps() {
        let mut app = multi_file_app(&["src/a.rs", "src/b.rs"]);
        app.open_finder();
        assert!(app.finder.is_some());
        app.on_key_finder(KeyCode::Char('b'), KeyModifiers::NONE);
        {
            let f = app.finder.as_ref().unwrap();
            assert_eq!(f.matches.len(), 1);
            assert_eq!(f.matches[0].0, 1);
        }
        // Enter opens the file and closes the finder.
        app.on_key_finder(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.finder.is_none());
        assert_eq!(app.current_file(), 1);
    }

    #[test]
    fn palette_opens_filters_and_runs_an_action() {
        let mut app = sample_app(); // has a store, so Comment can open the composer
        app.mode = Mode::Unified;
        app.cursor = 1; // a content line
        // `?` opens the palette through the normal key path.
        app.on_key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(app.palette.is_some(), "? opens the command palette");
        assert_eq!(
            app.palette.as_ref().unwrap().matches.len(),
            Action::ALL.len(),
            "every action is listed with no query"
        );
        // Type to filter down to the comment action, which tops the list.
        for c in "comment".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let top = app.palette.as_ref().unwrap().matches[0];
        assert_eq!(Action::ALL[top], Action::Comment);
        // Enter runs it (opens the composer) and closes the palette.
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.palette.is_none());
        assert!(app.input.is_some(), "running Comment opened the composer");
    }

    #[test]
    fn palette_availability_reflects_context() {
        let mut app = sample_app();
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "me",
            "c",
            CommentKind::Local,
        );
        app.relayout();
        // Files view, cursor on a content line: Comment applies (it would not on a
        // header), the Conversation-only CloseReview does not.
        app.view = View::Files;
        app.cursor = 1; // a content line, not the file header
        assert!(!app.cursor_is_header());
        assert!(app.action_available(Action::Comment));
        assert!(!app.action_available(Action::CloseReview));
        // On the file header, Comment no longer applies (nothing to comment on).
        app.cursor = 0;
        assert!(app.cursor_is_header());
        assert!(!app.action_available(Action::Comment));
        app.cursor = 1;
        // Conversation view: Comment applies (a new conversation comment, given a
        // store), and Reply and CloseReview do too.
        app.view = View::Conversation;
        assert!(app.action_available(Action::Comment));
        assert!(app.action_available(Action::CloseReview));
        // Submit is available only on a pull request.
        assert!(!app.action_available(Action::Submit));
        app.pr = Some(std::sync::Arc::new(crate::prsync::PrHandle::for_test(
            1, "t",
        )));
        assert!(app.action_available(Action::Submit));
        // Running an unavailable action reports it instead of silently no-op'ing.
        // `next_file` is Files-only, so in the Conversation view it tops the
        // filter yet is greyed and refused.
        app.view = View::Conversation;
        app.open_palette();
        for c in "next_file".chars() {
            app.on_key_palette(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let top = app.palette.as_ref().unwrap().matches[0];
        assert_eq!(Action::ALL[top], Action::NextFile);
        app.on_key_palette(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.palette.is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("isn't available")
        );
    }

    #[test]
    fn footer_reflects_the_cursor_target() {
        // Files, a bare line (no thread): `comment` shows, `reply` does not.
        let mut app = sample_app(); // author "tester"
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 1; // a content line with no thread
        let f = app.footer_ops();
        assert!(f.contains("comment"), "a bare line can be commented: {f}");
        assert!(!f.contains("reply"), "no thread here to reply to: {f}");

        // A thread of my own on line 2: over it, `reply` joins the act-on-this-
        // line ops. (Editing your own comment lives in the palette — it is a
        // lower-priority Files action, crowded out of the slim bar here.)
        app.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "mine",
            CommentKind::Local,
        );
        app.relayout();
        app.cursor = 2; // the addition line, where the thread is anchored
        let f = app.footer_ops();
        assert!(f.contains("reply"), "a thread here can be replied to: {f}");
        assert!(f.contains("suggest"), "and a change suggested: {f}");

        // Conversation, my draft comment on a PR: `kind` and `edit` show.
        let mut draft = pr_app(); // viewer "tester"
        draft.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "d",
            CommentKind::Draft,
        );
        draft.relayout();
        draft.view = View::Conversation;
        draft.conv_cursor = 0;
        let f = draft.footer_ops();
        assert!(
            f.contains("kind"),
            "an unpublished comment toggles kind: {f}"
        );
        assert!(f.contains("edit"), "and can be edited: {f}");

        // My published comment: no `kind` (can't change), but `edit` remains.
        let mut published = pr_app();
        published
            .review
            .threads
            .push(published_comment("c", "tester", "555"));
        published.relayout();
        published.view = View::Conversation;
        published.conv_cursor = 0;
        let f = published.footer_ops();
        assert!(
            !f.contains("kind"),
            "a published comment can't toggle kind: {f}"
        );
        assert!(
            f.contains("edit"),
            "but I can edit my own published one: {f}"
        );

        // Someone else's comment: no `edit`/`del`, but `reply` still applies.
        let mut other = pr_app();
        other
            .review
            .threads
            .push(published_comment("c", "someone-else", "555"));
        other.relayout();
        other.view = View::Conversation;
        other.conv_cursor = 0;
        let f = other.footer_ops();
        assert!(!f.contains("edit"), "not my comment — no edit: {f}");
        assert!(!f.contains("del"), "nor delete: {f}");
        assert!(f.contains("reply"), "but there's a thread to reply to: {f}");
    }

    #[test]
    fn footer_omits_the_move_prefix_and_groups_comment_actions() {
        // Movement keys are universal — the bar no longer spends space naming
        // them (outside the visual-selection sub-mode's `extend`).
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 1;
        assert!(
            !app.footer_ops().contains("j/k"),
            "no j/k movement prefix: {}",
            app.footer_ops()
        );

        // Where `reply` applies (on your own comment), `edit` and `delete` show
        // alongside it — the freed slot lets the three comment actions group.
        let mut pr = pr_app(); // viewer "tester"
        pr.add_thread(
            Anchor::line("a.rs", Side::New, 2),
            "tester",
            "mine",
            CommentKind::Draft,
        );
        pr.relayout();
        pr.view = View::Conversation;
        pr.conv_cursor = 0;
        let f = pr.footer_ops();
        assert!(f.contains("reply"), "reply shows: {f}");
        assert!(f.contains("edit"), "edit shows alongside it: {f}");
        assert!(f.contains("del"), "delete shows alongside it: {f}");
        assert!(!f.contains("j/k"), "and still no movement prefix: {f}");
    }

    #[test]
    fn footer_offers_select_and_suggest_then_switches_in_selection() {
        let mut app = sample_app(); // author "tester"
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 1; // a new-side content line
        assert!(app.cursor_targets_new_side());
        let f = app.footer_ops();
        assert!(
            f.contains("select"),
            "a diff line can start a selection: {f}"
        );
        assert!(f.contains("suggest"), "and offer a suggestion: {f}");

        // In a visual selection the bar becomes the range sub-mode.
        app.start_selection();
        let f = app.footer_ops();
        assert!(f.contains("extend"), "j/k extends the range: {f}");
        assert!(f.contains("range-comment"), "c comments on the range: {f}");
        assert!(f.contains("esc cancel"), "esc leaves the selection: {f}");
        assert!(
            !f.contains("move"),
            "the move label is replaced by extend: {f}"
        );

        // On a pure deletion (old side) `suggest` is not offered.
        let mut del = deletion_app();
        del.mode = Mode::Unified;
        del.view = View::Files;
        del.cursor = 2; // the deletion line
        let f = del.footer_ops();
        assert!(
            !f.contains("suggest"),
            "no suggestion on an old-side line: {f}"
        );
        assert!(f.contains("comment"), "but a comment still applies: {f}");
    }

    #[test]
    fn suggest_prefills_the_new_side_and_refuses_the_old() {
        // A single line: the block holds that line's current new-side text.
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 2; // the addition "added"
        app.start_suggest();
        let compose = app.input.as_ref().expect("the suggest composer opens");
        assert!(compose.suggestion);
        assert_eq!(compose.area.text(), "```suggestion\nadded\n```\n");
        assert_eq!(compose.target, "a.rs:2");

        // A range: every selected new-side line is prefilled, in order.
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 1;
        app.start_selection();
        app.move_cursor(1); // extend over lines 1..2
        app.start_suggest();
        let compose = app
            .input
            .as_ref()
            .expect("the range suggest composer opens");
        assert_eq!(compose.area.text(), "```suggestion\nkeep\nadded\n```\n");
        assert_eq!(compose.target, "a.rs:1-2");
        assert!(
            app.selection.is_none(),
            "opening the composer clears the selection"
        );

        // A pure deletion (old side only) is refused with guidance.
        let mut app = deletion_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 2; // the deletion line
        assert!(!app.cursor_targets_new_side());
        app.start_suggest();
        assert!(
            app.input.is_none(),
            "no composer opens for an old-side target"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("suggestions apply to the new side")
        );
    }

    #[test]
    fn suggest_titles_itself_and_saves_the_block_as_the_body() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // The composer names itself a suggestion and shows the target.
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.cursor = 2;
        app.start_suggest();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("Suggest change"),
            "the composer titles itself a suggestion:\n{screen}"
        );
        assert!(screen.contains("a.rs:2"), "and shows the target line");

        // Saving keeps the ```suggestion block verbatim as the comment body.
        app.submit_compose();
        let thread = app.review.threads.last().expect("a thread is created");
        let body = &thread.root().expect("the thread has a root").body;
        assert!(
            body.starts_with("```suggestion\n"),
            "the saved body is a suggestion block: {body:?}"
        );
        assert!(body.contains("added"), "holding the line's new-side text");
    }

    #[test]
    fn drawing_the_palette_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app();
        app.open_palette();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let mut text = String::new();
        for y in 0..24u16 {
            for x in 0..80u16 {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(text.contains("commands"), "the palette title rendered");
        assert!(text.contains("cursor_down"), "an action name rendered");
    }

    #[test]
    fn drawing_the_thread_picker_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_line_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.add_thread(
            range_anchor(2, 4),
            "tester",
            "a long body that should be truncated to keep the picker row tidy indeed",
            CommentKind::Local,
        );
        app.add_thread(range_anchor(2, 3), "tester", "C", CommentKind::Local);
        app.relayout();
        app.cursor = 2;
        let hits = app.threads_at_cursor();
        app.open_thread_picker(hits, Action::Reply);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("pick a comment"),
            "the picker title rendered:\n{screen}"
        );
        assert!(screen.contains("L2-4"), "a candidate's range rendered");
    }

    #[test]
    fn jump_to_file_expands_a_collapsed_file() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.collapsed_files.insert("b.rs".to_string());
        app.relayout();
        app.jump_to_file(1);
        assert!(!app.collapsed_files.contains("b.rs"), "jumping expands it");
        assert_eq!(app.current_file(), 1);
        assert_eq!(app.focus, Focus::Body);
    }

    #[test]
    fn sidebar_mode_and_override_control_visibility() {
        use crate::config::SidebarMode;
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        // Auto (the default) shows when the terminal is wide, hides when narrow.
        assert!(app.sidebar_width(120).is_some());
        assert!(app.sidebar_width(50).is_none());
        // Closed hides it by default.
        app.sidebar_mode = SidebarMode::Closed;
        assert!(app.sidebar_width(120).is_none());
        // A `b` override forces it on...
        app.sidebar_override = Some(true);
        assert!(app.sidebar_width(120).is_some());
        // ...and a resize clears the override (back to the Closed default).
        app.on_resize(120);
        assert!(app.sidebar_width(120).is_none());
    }

    #[test]
    fn a_remapped_key_triggers_its_action() {
        let mut over = std::collections::HashMap::new();
        over.insert("cursor_down".to_string(), "z".to_string());
        let mut app = multi_file_app(&["a.rs"]);
        app.keymap = crate::keys::Keymap::from_overrides(&over).unwrap();
        app.cursor = 0;
        // `z` now moves the cursor down.
        app.on_key(KeyCode::Char('z'), KeyModifiers::NONE);
        assert_eq!(app.cursor, 1);
        // The old default `j` is unbound after the remap.
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn sidebar_activate_jumps_without_folding() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        for p in ["a.rs", "b.rs", "c.rs"] {
            app.collapsed_files.insert(p.to_string());
        }
        app.relayout();
        // The sidebar is auto-visible at the test's default width; `b` focuses it.
        app.toggle_sidebar();
        assert_eq!(app.focus, Focus::Sidebar);
        app.sidebar_action(Action::MoveDown);
        app.sidebar_action(Action::MoveDown);
        assert_eq!(app.sidebar_cursor, 2);
        // l on a collapsed file expands it and jumps the body onto its header.
        app.sidebar_action(Action::NavIn);
        assert_eq!(app.current_file(), 2);
        assert_eq!(app.focus, Focus::Body);
        assert!(app.cursor_is_header(), "the jump lands on the file header");
        assert!(!app.collapsed_files.contains("c.rs"));
        // Activating the now-open file again does NOT fold it — it just re-jumps
        // (navigate, never toggle). Folding is the diff pane's Enter on the header.
        app.focus_sidebar();
        app.sidebar_action(Action::NavIn);
        assert!(
            !app.collapsed_files.contains("c.rs"),
            "re-activating an open file does not fold it"
        );
        assert_eq!(app.focus, Focus::Body, "activating jumps into the body");
        assert!(app.cursor_is_header());
    }

    #[test]
    fn hit_region_maps_across_layouts() {
        // No tabs, no sidebar: content fills the body from column 0, row 1.
        let h = HitLayout {
            body_top: 1,
            body_height: 20,
            content_x0: 0,
            content_w: 100,
            sidebar_x0: 0,
            sidebar_w: 0,
            tabs_row: None,
            footer_row: 21,
            layout_end: 0,
            ..HitLayout::default()
        };
        assert_eq!(hit_region(5, 0, h), Region::Outside); // header
        assert_eq!(hit_region(5, 1, h), Region::Content { col: 5, row: 0 });
        assert_eq!(hit_region(5, 3, h), Region::Content { col: 5, row: 2 });

        // Tabs present: body at screen row 2, tab bar at row 1.
        let h = HitLayout {
            body_top: 2,
            tabs_row: Some(1),
            ..h
        };
        assert_eq!(hit_region(5, 1, h), Region::Tabs);
        assert_eq!(hit_region(5, 2, h), Region::Content { col: 5, row: 0 });
        assert_eq!(hit_region(5, 4, h), Region::Content { col: 5, row: 2 });

        // Two framed panes: sidebar inner [1,23), the diff inner starts at 25.
        // The frames and the gap between them (columns 0, 23, 24) map to Outside.
        let h = HitLayout {
            body_top: 1,
            tabs_row: None,
            sidebar_x0: 1,
            sidebar_w: 22,
            content_x0: 25,
            ..h
        };
        assert_eq!(hit_region(5, 1, h), Region::Sidebar(0));
        assert_eq!(hit_region(0, 1, h), Region::Outside); // sidebar left border
        assert_eq!(hit_region(23, 1, h), Region::Outside); // sidebar right border
        assert_eq!(hit_region(24, 1, h), Region::Outside); // gap / body border
        assert_eq!(hit_region(27, 3, h), Region::Content { col: 2, row: 2 });
    }

    #[test]
    fn the_header_pr_number_is_a_clickable_link() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = pr_app();
        app.label = "PR #1".to_string(); // as the real PR load sets it
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let (x0, x1) = app
            .header_pr_link
            .get()
            .expect("a PR-link range was recorded");
        assert_eq!(
            (x1 - x0) as usize,
            "#1".chars().count(),
            "the link spans '#1'"
        );
        // The '#1' columns hit the link; the 'PR ' before and the space after don't.
        assert_eq!(hit_region(x0, 0, app.hit.get()), Region::PrLink);
        assert_eq!(hit_region(x1 - 1, 0, app.hit.get()), Region::PrLink);
        assert_eq!(
            hit_region(x0 - 1, 0, app.hit.get()),
            Region::Outside,
            "the 'PR ' prefix is not the link"
        );
        assert_eq!(
            hit_region(x1, 0, app.hit.get()),
            Region::Outside,
            "past the number is not the link"
        );
        assert!(screen_text(&term).contains("#1"), "the number renders");
    }

    #[test]
    fn a_non_pr_header_has_no_link() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let app = sample_app(); // a working-tree review, no PR
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            app.header_pr_link.get().is_none(),
            "off a pull request there is no link"
        );
        assert_eq!(hit_region(15, 0, app.hit.get()), Region::Outside);
    }

    #[test]
    fn the_header_shows_the_pr_status_badge() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = pr_app();
        app.label = "PR #1".to_string();
        app.pr_overview = Some(overview(PrStatus::Merged));
        let mut term = Terminal::new(TestBackend::new(120, 6)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let header: String = (0..120u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            header.contains("#1 Merged"),
            "the badge sits just after #1: {header:?}"
        );

        // The badge is not part of the clickable/underlined link — the recorded
        // columns still cover only "#1".
        let (x0, x1) = app.header_pr_link.get().expect("the #N link is recorded");
        assert_eq!(
            (x1 - x0) as usize,
            "#1".chars().count(),
            "the link spans only #1, not the badge"
        );

        // The badge carries the status color (merged = magenta).
        let merged_col = (0..114u16)
            .find(|&x| (x..x + 6).map(|c| buf[(c, 0)].symbol()).collect::<String>() == "Merged")
            .expect("the badge text is rendered");
        assert_eq!(
            buf[(merged_col, 0)].fg,
            Color::Magenta,
            "the merged badge uses the merged color"
        );
    }

    #[test]
    fn the_header_issue_number_is_a_clickable_link_with_a_badge() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // Regression: the #N link and the status badge were PR-gated, so an issue
        // header showed a plain label with no link and no badge.
        let mut app = issue_app();
        app.label = "Issue #5".to_string(); // as the real issue load sets it
        app.set_view(View::Overview);
        let mut term = Terminal::new(TestBackend::new(120, 6)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let (x0, x1) = app
            .header_pr_link
            .get()
            .expect("an issue records the #N link too");
        assert_eq!(
            (x1 - x0) as usize,
            "#5".chars().count(),
            "the link spans '#5'"
        );
        assert_eq!(hit_region(x0, 0, app.hit.get()), Region::PrLink);
        let buf = term.backend().buffer();
        let header: String = (0..120u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            header.contains("#5 Open"),
            "the issue status badge sits just after #5: {header:?}"
        );
        // The badge is green for an open issue, and not inside the click columns.
        let open_col = (0..114u16)
            .find(|&x| (x..x + 4).map(|c| buf[(c, 0)].symbol()).collect::<String>() == "Open")
            .expect("the badge text is rendered");
        assert_eq!(
            buf[(open_col, 0)].fg,
            Color::Green,
            "an open issue is green"
        );
    }

    #[test]
    fn tab_at_column_maps_clicks_to_tabs() {
        // A click on a tab's label lands on that tab; the single space separating
        // two tabs belongs to neither.
        let app = pr_app(); // Overview | Files | Conversation
        let labels = app.tab_labels();
        assert_eq!(labels.len(), 3, "a PR shows all three tabs");
        let mut x = 0u16;
        for (view, label) in &labels {
            let w = label.chars().count() as u16;
            assert_eq!(
                app.tab_at_column(x),
                Some(*view),
                "the left edge of {view:?}"
            );
            assert_eq!(
                app.tab_at_column(x + w - 1),
                Some(*view),
                "the right edge of {view:?}"
            );
            x += w;
            assert_eq!(
                app.tab_at_column(x),
                None,
                "the gap after {view:?} hits nothing"
            );
            x += 1; // the separator space
        }
    }

    /// The plain text of a rendered markdown line list.
    fn md_text(r: &crate::markdown::Rendered) -> String {
        r.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_overview_link_click_opens_it() {
        let mut app = issue_app();
        // Capture the URL instead of launching a real browser (never open a real
        // process from a test).
        let opened = std::rc::Rc::new(RefCell::new(Vec::<String>::new()));
        let sink = opened.clone();
        app.url_opener = Box::new(move |url: &str| {
            sink.borrow_mut().push(url.to_string());
            Ok(())
        });
        if let Some(ov) = app.pr_overview.as_mut() {
            ov.body = "See https://example.com/xyz here".into();
        }
        let rendered = app.overview_render(60);
        let link = rendered
            .regions
            .iter()
            .find(|r| matches!(&r.action, crate::markdown::MdAction::Open(u) if u.contains("xyz")))
            .expect("a link region in the overview body");
        // Resolve the click through the cached regions (with scroll offset 0) and
        // run it — the injected opener records the URL.
        let action = app
            .overview_action_at(link.line, link.start)
            .expect("the region is hit at its own coordinates");
        app.run_md_action(action);
        assert_eq!(
            opened.borrow().as_slice(),
            &["https://example.com/xyz".to_string()],
            "the click routed the url to the opener (no real browser launch)"
        );
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("example.com/xyz"),
            "the status names the opened url: {:?}",
            app.status
        );
    }

    #[test]
    fn an_overview_details_click_toggles_the_fold() {
        let mut app = issue_app();
        if let Some(ov) = app.pr_overview.as_mut() {
            ov.body = "<details>\n<summary>More</summary>\n\nhidden body\n\n</details>".into();
        }
        // Closed by default: the summary shows with a toggle, the body is hidden.
        let rendered = app.overview_render(60);
        let toggle = rendered
            .regions
            .iter()
            .find(|r| r.action == crate::markdown::MdAction::ToggleDetails(0))
            .expect("a details toggle region");
        let closed = md_text(&rendered);
        assert!(
            closed.contains("▸ More") && !closed.contains("hidden body"),
            "folded by default: {closed:?}"
        );
        // Clicking the summary flips the fold; a re-render shows the body.
        let action = app
            .overview_action_at(toggle.line, toggle.start)
            .expect("the toggle is hit");
        app.run_md_action(action);
        assert_eq!(app.overview_folds.get(&0), Some(&true), "now open");
        let opened = md_text(&app.overview_render(60));
        assert!(
            opened.contains("▾ More") && opened.contains("hidden body"),
            "the body shows after unfolding: {opened:?}"
        );
    }

    #[test]
    fn issue_display_matches_the_pr_shape() {
        let mut app = issue_app();
        app.set_view(View::Conversation);
        // Frame title: `Issue #N — <title>`, symmetric with a PR.
        assert_eq!(app.pane_title(), " Issue #5 — Flaky retry ");
        // The subject label is capitalized and slug-less.
        assert_eq!(app.issue.as_ref().unwrap().number(), 5);
        // The composer no longer falls to the plain-review "save comment".
        assert_ne!(app.compose_save_label(), "save comment");
    }

    #[test]
    fn the_issue_close_modal_discards_drafts_not_delete_all() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = issue_app();
        app.confirming_close = true;
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("Discard your local drafts for this issue?"),
            "the issue close prompt is about drafts: {screen:?}"
        );
        assert!(
            !screen.contains("Delete all"),
            "no scary delete-all wording for an issue: {screen:?}"
        );
    }

    #[test]
    fn the_issue_footer_says_send_not_submit() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = issue_app();
        app.set_view(View::Conversation);
        app.status = None; // so the footer shows the action hints, not a status
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            screen.contains("^s send"),
            "issue footer says send: {screen:?}"
        );
        assert!(!screen.contains("^s submit"), "not submit: {screen:?}");
    }

    #[test]
    fn the_overview_shows_a_rule_below_the_facts() {
        // Both a PR and an issue Overview separate the facts block from the body
        // with a dim, full-width rule.
        let mut pr = pr_app();
        pr.pr_overview = Some(overview(PrStatus::Open));
        let issue = issue_app();
        for app in [&pr, &issue] {
            let lines = app.overview_lines(40);
            assert!(
                lines.iter().any(|l| {
                    let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                    !t.is_empty() && t.chars().all(|c| c == '─')
                }),
                "a rule line separates facts from body"
            );
        }
    }

    #[test]
    fn a_conversation_body_link_is_clickable() {
        let mut app = pr_app();
        app.review.threads.push(Thread {
            id: "t1".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c1".into(),
                author: "octo".into(),
                body: "see https://example.org/deep here".into(),
                created_at: 0,
                remote_id: Some("IC_1".into()),
                kind: CommentKind::Published,
            }],
        });
        app.collapsed.clear();
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        // Draw to compose and cache the click regions for the scrolled line list.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let region = app
            .conv_regions
            .borrow()
            .iter()
            .find(|r| {
                matches!(&r.action, crate::markdown::MdAction::Open(u) if u.contains("example.org"))
            })
            .cloned()
            .expect("a conversation body link region");
        // Route the click through an injected recorder (never a real browser).
        let opened = std::rc::Rc::new(RefCell::new(Vec::<String>::new()));
        let sink = opened.clone();
        app.url_opener = Box::new(move |u: &str| {
            sink.borrow_mut().push(u.to_string());
            Ok(())
        });
        let action = app
            .conv_action_at(region.line, region.start)
            .expect("the region is hit at its coordinates");
        app.run_md_action(action);
        assert_eq!(
            opened.borrow().as_slice(),
            &["https://example.org/deep".to_string()],
            "the conversation link click routed to the opener"
        );
    }

    #[test]
    fn a_conversation_details_folds_on_click() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = pr_app();
        app.review.threads.push(Thread {
            id: "td".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c1".into(),
                author: "octo".into(),
                body: "<details>\n<summary>More</summary>\n\nhidden line\n\n</details>".into(),
                created_at: 0,
                remote_id: Some("IC_1".into()),
                kind: CommentKind::Published,
            }],
        });
        app.collapsed.clear();
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        let draw = |app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(80, 30)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            screen_text(&term)
        };
        // Closed by default: the ▸ summary shows, the body is hidden.
        let closed = draw(&mut app);
        assert!(closed.contains("▸ More"), "folded by default: {closed:?}");
        assert!(!closed.contains("hidden line"), "body hidden: {closed:?}");
        // Clicking the summary's toggle region opens it (a re-draw shows the body).
        let toggle = app
            .conv_regions
            .borrow()
            .iter()
            .find(|r| matches!(r.action, crate::markdown::MdAction::ToggleDetails(_)))
            .cloned()
            .expect("a details toggle region in the conversation");
        let action = app
            .conv_action_at(toggle.line, toggle.start)
            .expect("the toggle is hit");
        app.run_conv_md_action(action);
        let opened = draw(&mut app);
        assert!(
            opened.contains("▾ More") && opened.contains("hidden line"),
            "the body shows after unfolding: {opened:?}"
        );
    }

    #[test]
    fn conversation_non_link_clicks_fall_through_to_selection() {
        // No-regression: a plain comment body registers no link regions, so a
        // content click still reaches the existing selection / header-fold path.
        let mut app = pr_app();
        app.review.threads.push(Thread {
            id: "t1".into(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c1".into(),
                author: "octo".into(),
                body: "just plain text".into(),
                created_at: 0,
                remote_id: Some("IC_1".into()),
                kind: CommentKind::Published,
            }],
        });
        app.collapsed.clear();
        app.relayout();
        app.set_view(View::Conversation);
        app.focus = Focus::Body;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            app.conv_regions.borrow().is_empty(),
            "a plain body has no link regions"
        );
        assert!(app.conv_action_at(0, 0).is_none());
    }

    #[test]
    fn the_overview_body_ends_with_bottom_padding() {
        // A trailing blank so the last body line doesn't sit on the pane's floor.
        let app = issue_app();
        let lines = app.overview_lines(40);
        assert!(
            lines
                .last()
                .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty())),
            "the overview ends with a blank line"
        );
    }

    #[test]
    fn pr_header_keeps_files_but_drops_the_layout_label() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut pr = pr_app();
        pr.label = "PR #1".into();
        pr.pr_overview = Some(overview(PrStatus::Open));
        let mut term = Terminal::new(TestBackend::new(120, 6)).unwrap();
        term.draw(|f| pr.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let header: String = (0..120u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            header.contains("file"),
            "PR header keeps the file counter: {header:?}"
        );
        assert!(
            !header.contains("unified") && !header.contains("split"),
            "the layout label is dropped from the header: {header:?}"
        );
    }

    #[test]
    fn issue_header_and_footer_drop_diff_only_chrome() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut issue = issue_app();
        issue.label = "Issue #5".into();
        issue.set_view(View::Overview);
        issue.status = None;
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| issue.draw(f)).unwrap();
        let screen = screen_text(&term);
        assert!(
            !screen.contains("0 file") && !screen.contains("+0 "),
            "no file counters for a no-diff issue: {screen:?}"
        );
        assert!(
            !screen.contains("unified") && !screen.contains("split"),
            "no layout indicator for an issue: {screen:?}"
        );
        assert!(
            !screen.contains("[1/"),
            "no position index in the Overview: {screen:?}"
        );
    }

    #[test]
    fn an_overview_click_off_any_region_is_a_noop() {
        // A plain-text body has no regions, so a content click resolves to nothing
        // (and never touches diff/conversation state).
        let mut app = issue_app();
        if let Some(ov) = app.pr_overview.as_mut() {
            ov.body = "just plain text".into();
        }
        let _ = app.overview_render(60);
        assert!(app.overview_action_at(0, 0).is_none());
        assert!(app.overview_action_at(99, 99).is_none());
    }

    /// Empirical: render, read the buffer to find the screen row of a known
    /// line, click it, and confirm the cursor lands on that exact line — with the
    /// tab bar present (the reported "one row down" case).
    #[test]
    fn a_click_selects_the_line_under_it_with_tabs() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app(); // a.rs: new 1 (context "keep"), new 2 ("added")
        app.mode = Mode::Unified;
        // A thread makes the tab bar appear (review context).
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 1),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c".into(),
                author: "a".into(),
                body: "note".into(),
                created_at: 0,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            }],
        });
        app.relayout();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        // Find the screen row whose text contains the addition "added".
        let buffer = term.backend().buffer();
        let mut added_row = None;
        for y in 0..24u16 {
            let text: String = (0..80u16).map(|x| buffer[(x, y)].symbol()).collect();
            if text.contains("added") {
                added_row = Some(y);
                break;
            }
        }
        let added_row = added_row.expect("the addition line is rendered");

        // Click that row in the diff area (past the default sidebar); the cursor
        // must land on the addition (new line 2), not one line below.
        let click_col = app.hit.get().sidebar_w + 5;
        app.mouse_down(click_col, added_row);
        assert_eq!(clicked_line(&app), "added", "with the sidebar shown");

        // Same, with the sidebar hidden (tabs still shift the body down a row).
        app.cursor = 0;
        app.sidebar_override = Some(false);
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.hit.get().sidebar_w, 0);
        let buffer = term.backend().buffer();
        let mut y2 = None;
        for y in 0..24u16 {
            let text: String = (0..80u16).map(|x| buffer[(x, y)].symbol()).collect();
            if text.contains("added") {
                y2 = Some(y);
                break;
            }
        }
        app.mouse_down(5, y2.expect("addition rendered"));
        assert_eq!(clicked_line(&app), "added", "with the sidebar hidden");
    }

    fn clicked_line(app: &App) -> String {
        let (file, flat) = app.cursor_content().expect("cursor on a content line");
        let (h, l) = app.flats[file][flat];
        app.diff.files[file].hunks[h].lines[l].content.clone()
    }

    /// The diff text at a `clines` index (content lines only).
    fn cline_text(app: &App, cidx: usize) -> String {
        let (file, flat) = app.clines[cidx];
        let (h, l) = app.flats[file][flat];
        app.diff.files[file].hunks[h].lines[l].content.clone()
    }

    /// Reported symptom, drag half: press on one diff line, drag to another with
    /// the tab bar present, and confirm the selection spans exactly the pressed
    /// and dragged rows — not one row below either end.
    #[test]
    fn a_drag_selects_the_lines_under_it_with_tabs() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app(); // a.rs: new 1 (context "keep"), new 2 ("added")
        app.mode = Mode::Unified;
        // A thread makes the tab bar appear (review context).
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 1),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c".into(),
                author: "a".into(),
                body: "note".into(),
                created_at: 0,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            }],
        });
        app.relayout();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        let row_of = |term: &Terminal<TestBackend>, needle: &str| -> u16 {
            let buffer = term.backend().buffer();
            for y in 0..24u16 {
                let text: String = (0..80u16).map(|x| buffer[(x, y)].symbol()).collect();
                if text.contains(needle) {
                    return y;
                }
            }
            panic!("row containing {needle:?} is rendered");
        };
        let keep_row = row_of(&term, "keep");
        let added_row = row_of(&term, "added");
        assert!(
            added_row > keep_row,
            "the addition renders below the context"
        );

        // Press on "keep", drag down to "added". Both must be selected; the
        // range must resolve to exactly those two lines, off-by-one and all.
        let col = app.hit.get().sidebar_w + 5;
        app.mouse_down(col, keep_row);
        app.mouse_drag(col, added_row);
        let (lo, hi) = app
            .selection_range()
            .expect("dragging across two lines forms a selection");
        assert_eq!(
            cline_text(&app, lo),
            "keep",
            "selection starts at the press row"
        );
        assert_eq!(
            cline_text(&app, hi),
            "added",
            "selection ends at the drag row"
        );
    }

    #[test]
    fn a_diff_click_or_drag_pulls_focus_out_of_the_sidebar() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.hit.get().sidebar_w > 0, "the sidebar is shown");
        // As if the reviewer had been browsing the file index.
        app.focus = Focus::Sidebar;

        let row_of = |term: &Terminal<TestBackend>, needle: &str| -> u16 {
            let buffer = term.backend().buffer();
            for y in 0..24u16 {
                let text: String = (0..120u16).map(|x| buffer[(x, y)].symbol()).collect();
                if text.contains(needle) {
                    return y;
                }
            }
            panic!("row containing {needle:?} is rendered");
        };
        let keep_row = row_of(&term, "keep");
        let added_row = row_of(&term, "added");
        let col = app.hit.get().sidebar_w + 5;

        // A press on a diff line focuses the body and lands the cursor there —
        // the clicked pane takes focus.
        app.mouse_down(col, keep_row);
        assert_eq!(app.focus, Focus::Body, "a diff click focuses the body");
        assert_eq!(clicked_line(&app), "keep");

        // A drag to another content line keeps the focus in the body and selects.
        app.mouse_drag(col, added_row);
        assert_eq!(app.focus, Focus::Body, "the drag stays in the body");
        assert!(app.selection.is_some(), "the drag builds a selection");
    }

    fn hit(body_top: u16, sidebar_w: u16, tabs_row: Option<u16>, _files_end: u16) -> HitLayout {
        HitLayout {
            body_top,
            body_height: 20,
            content_x0: if sidebar_w > 0 { sidebar_w + 1 } else { 0 },
            content_w: 100,
            sidebar_x0: 0,
            sidebar_w,
            tabs_row,
            footer_row: body_top + 20,
            layout_end: 0,
            ..HitLayout::default()
        }
    }

    #[test]
    fn mouse_click_on_a_header_toggles_the_fold() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.mode = Mode::Unified;
        app.hit.set(hit(1, 0, None, 0));
        // Body row 0 (screen row 1) is the first file's header.
        app.mouse_down(0, 1);
        assert!(app.collapsed_files.contains("a.rs"), "a header click folds");
        app.mouse_down(0, 1);
        assert!(!app.collapsed_files.contains("a.rs"), "and unfolds");
    }

    #[test]
    fn clicking_the_layout_indicator_toggles_it() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs"]);
        app.mode = Mode::Unified;
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let hit = app.hit.get();
        assert!(hit.layout_end > 0);
        let before = app.sbs();
        app.mouse_down(0, hit.footer_row); // the layout indicator
        assert_ne!(
            app.sbs(),
            before,
            "clicking the indicator toggles the layout"
        );
    }

    #[test]
    fn mouse_click_on_a_tab_switches_the_view() {
        let mut app = multi_file_app(&["a.rs"]);
        app.review.threads.push(Thread {
            id: "t".into(),
            anchor: Anchor::line("a.rs", Side::New, 2),
            state: ThreadState::Open,
            comments: vec![Comment {
                id: "c".into(),
                author: "a".into(),
                body: "b".into(),
                created_at: 0,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            }],
        });
        app.relayout();
        let labels = app.tab_labels();
        let files_end = labels[0].1.chars().count() as u16;
        app.hit.set(hit(2, 0, Some(1), files_end));
        app.mouse_down(files_end + 2, 1); // in the Conversation tab
        assert_eq!(app.view, View::Conversation);
        app.mouse_down(1, 1); // in the Files tab
        assert_eq!(app.view, View::Files);
    }

    #[test]
    fn mouse_click_in_the_sidebar_jumps_to_the_file() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.mode = Mode::Unified;
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        app.collapsed_files.insert("c.rs".to_string());
        app.relayout();
        app.hit.set(hit(1, 22, None, 0));
        // Sidebar body row 2 (screen row 3) is the third file, collapsed:
        // clicking it expands and opens the file in the body (on its header).
        app.mouse_down(3, 3);
        assert_eq!(
            app.current_file(),
            2,
            "a click on a collapsed file opens it"
        );
        assert_eq!(app.focus, Focus::Body);
        assert!(!app.collapsed_files.contains("c.rs"));
        // Clicking the now-open file does not collapse it — the table of contents
        // navigates, it never toggles; the click just re-jumps into the body.
        app.mouse_down(3, 3);
        assert!(
            !app.collapsed_files.contains("c.rs"),
            "a click on an open file does not fold it"
        );
        assert_eq!(app.focus, Focus::Body);
    }

    #[test]
    fn sidebar_shows_cursor_and_current_file_distinctly() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        // Current file is a.rs (0); the sidebar cursor rests on c.rs (2).
        app.focus = Focus::Sidebar;
        app.sidebar_cursor = 2;
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let x = app.hit.get().sidebar_x0; // the marker column, inside the frame
        let buf = term.backend().buffer();
        let mut sel_row = None;
        let mut current_row = None;
        for y in 0..24u16 {
            match buf[(x, y)].bg {
                SEL_BG => sel_row = Some(y),
                SIDEBAR_CURRENT_BG => current_row = Some(y),
                _ => {}
            }
        }
        let sel_row = sel_row.expect("the sidebar cursor row is filled with the selection color");
        let current_row =
            current_row.expect("the current-file row is filled with the current-file color");
        assert_ne!(
            sel_row, current_row,
            "cursor and current file are drawn on different rows"
        );
        assert_ne!(
            SEL_BG, SIDEBAR_CURRENT_BG,
            "the two states use visibly distinct colors"
        );
    }

    #[test]
    fn header_cursor_uses_a_stronger_fill_than_a_line() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs"]);
        app.mode = Mode::Unified;
        app.sidebar_override = Some(false);
        app.cursor = 0; // the file header is the first cursor stop
        assert!(app.cursor_is_header());
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let mut header_row = None;
        for y in 0..24u16 {
            let text: String = (0..80u16).map(|x| buf[(x, y)].symbol()).collect();
            if text.contains("a.rs") {
                header_row = Some(y);
                break;
            }
        }
        let header_row = header_row.expect("the file header is rendered");
        assert_eq!(
            buf[(0, header_row)].bg,
            HEADER_CURSOR_BG,
            "the cursored header is filled brighter than a content line ({CURSOR_BG:?})"
        );
        assert_ne!(HEADER_CURSOR_BG, CURSOR_BG, "stronger than a line cursor");
    }

    #[test]
    fn drawing_the_sidebar_and_finder_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&[
            "src/aaaa.rs",
            "verylong/path/to/some/deeply/nested/module/file.rs",
        ]);
        app.sidebar_override = Some(true);
        app.focus = Focus::Sidebar;
        let mut wide = Terminal::new(TestBackend::new(120, 30)).unwrap();
        wide.draw(|f| app.draw(f)).unwrap();
        // With the finder open and a query (exercises match highlighting).
        app.open_finder();
        app.on_key_finder(KeyCode::Char('a'), KeyModifiers::NONE);
        wide.draw(|f| app.draw(f)).unwrap();
        // A narrow terminal auto-hides the sidebar but must still draw.
        let mut narrow = Terminal::new(TestBackend::new(50, 20)).unwrap();
        narrow.draw(|f| app.draw(f)).unwrap();
        assert!(app.sidebar_width(50).is_none(), "sidebar hides when narrow");
        assert!(app.sidebar_width(120).is_some(), "sidebar shows when wide");
    }

    /// Build an app with `n` short files named `fNN.rs`.
    fn many_file_app(n: usize) -> App {
        let paths: Vec<String> = (0..n).map(|i| format!("f{i:02}.rs")).collect();
        let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        multi_file_app(&refs)
    }

    #[test]
    fn body_navigation_scrolls_the_sidebar_to_the_current_file() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = many_file_app(40);
        app.sidebar_override = Some(true);
        let mut term = Terminal::new(TestBackend::new(120, 12)).unwrap();
        term.draw(|f| app.draw(f)).unwrap(); // captures body width/height
        // Jump the body cursor deep into the list; the sidebar must follow.
        let target = app.file_first[30].expect("file 30 has a header");
        app.set_cursor(target);
        assert_eq!(app.current_file(), 30);
        let h = app.body_height.get();
        assert!(
            app.sidebar_scroll <= 30 && 30 < app.sidebar_scroll + h,
            "the current file is within the sidebar window (scroll={}, height={h})",
            app.sidebar_scroll,
        );
        // A redraw shows that file's sidebar row without panicking.
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let mut shown = String::new();
        for y in 0..12u16 {
            for x in 0..24u16 {
                shown.push_str(buf[(x, y)].symbol());
            }
            shown.push('\n');
        }
        assert!(
            shown.contains("f30.rs"),
            "the current file appears in the sidebar list"
        );
    }

    #[test]
    fn wheel_scrolls_the_sidebar_or_the_body_by_region() {
        let mut app = many_file_app(40);
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        app.body_height.set(10);
        app.hit.set(hit(1, 22, None, 0)); // sidebar 22 wide, body below row 1
        // A wheel notch over the sidebar (col 3) scrolls the sidebar list.
        app.scroll_wheel(3, 5, 3);
        assert!(app.sidebar_scroll > 0, "wheel over the sidebar scrolls it");
        let sidebar_at = app.sidebar_scroll;
        let body_at = app.scroll;
        // A wheel notch over the body (col 40) scrolls the body, not the sidebar.
        app.scroll_wheel(40, 5, 3);
        assert_eq!(
            app.sidebar_scroll, sidebar_at,
            "a body wheel leaves the sidebar where it was"
        );
        assert!(app.scroll > body_at, "wheel over the body scrolls the body");
    }

    // -- multi-line range selection -----------------------------------------

    #[test]
    fn single_line_compose_target_without_selection() {
        let mut app = sample_app();
        app.mode = Mode::Unified;
        // clines: [header, new line 1, new line 2] — index 2 is the addition.
        app.cursor = 2;
        let (anchor, target) = app.compose_target().unwrap();
        match anchor {
            Anchor::Line {
                start, end, side, ..
            } => assert_eq!((start, end, side), (2, 2, Side::New)),
            other => panic!("unexpected anchor {other:?}"),
        }
        assert_eq!(target, "a.rs:2");
    }

    #[test]
    fn selection_produces_a_range_anchor_and_clears_on_compose() {
        let mut app = sample_app(); // clines: [header, new 1, new 2]
        app.mode = Mode::Unified;
        app.cursor = 1; // new line 1 (a content line, not the header)
        app.start_selection();
        assert!(app.selection.is_some());
        app.move_cursor(1); // extend to new line 2
        let (anchor, target) = app.compose_target().unwrap();
        match anchor {
            Anchor::Line {
                file,
                side,
                start,
                end,
                ..
            } => {
                assert_eq!(file, "a.rs");
                assert_eq!(side, Side::New);
                assert_eq!((start, end), (1, 2));
            }
            other => panic!("unexpected anchor {other:?}"),
        }
        assert_eq!(target, "a.rs:1-2");
        app.start_compose();
        assert!(
            app.selection.is_none(),
            "opening the composer captures the range and clears the selection"
        );
    }

    #[test]
    fn selection_stays_within_one_file() {
        let mut app = multi_file_app(&["a.rs", "b.rs"]);
        app.cursor = 1; // a content line in file a (index 0 is its header)
        app.start_selection();
        app.cursor = app.clines.len() - 1; // last line, file b
        let (lo, hi) = app.selection_range().unwrap();
        assert_eq!(app.clines[lo].0, 0);
        assert_eq!(
            app.clines[hi].0, 0,
            "the range clamps to the selection's file"
        );
        match app.compose_target().unwrap().0 {
            Anchor::Line { file, .. } => assert_eq!(file, "a.rs"),
            other => panic!("unexpected anchor {other:?}"),
        }
    }

    // -- range-comment targeting and overlap picker -------------------------

    #[test]
    fn a_range_comment_is_reachable_from_any_line_it_covers() {
        let mut app = multi_line_app(); // clines row i (1..=5) == new line i
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.add_thread(
            range_anchor(2, 4),
            "tester",
            "spans 2-4",
            CommentKind::Local,
        );
        app.relayout();
        // The end line (4) always worked; the fix is the start and middle lines.
        for row in [2usize, 3, 4] {
            app.cursor = row;
            assert_eq!(app.threads_at_cursor(), vec![0], "reachable at row {row}");
            let (f, flat) = app.clines[row];
            assert!(app.line_has_comment(f, flat), "the gutter marks row {row}");
        }
        // Lines outside the range are untouched.
        for row in [1usize, 5] {
            app.cursor = row;
            assert!(app.threads_at_cursor().is_empty(), "row {row} is outside");
            let (f, flat) = app.clines[row];
            assert!(!app.line_has_comment(f, flat), "no marker on row {row}");
        }
    }

    #[test]
    fn overlapping_ranges_open_a_picker_ordered_by_end_line() {
        let mut app = multi_line_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        // E's example: A=2-4, B=3-5, C=2-3 — all cover line 3.
        let (a, _) = app.add_thread(range_anchor(2, 4), "tester", "A", CommentKind::Local);
        let (_b, _) = app.add_thread(range_anchor(3, 5), "tester", "B", CommentKind::Local);
        let (_c, _) = app.add_thread(range_anchor(2, 3), "tester", "C", CommentKind::Local);
        app.relayout();
        app.cursor = 3;
        // Ordered end ascending, then start ascending: C(3), A(4), B(5).
        let order: Vec<String> = app
            .threads_at_cursor()
            .iter()
            .map(|&ti| app.review.threads[ti].root().unwrap().body.clone())
            .collect();
        assert_eq!(order, vec!["C", "A", "B"], "end asc, then start asc");
        // The footer flags the multiplicity on reply.
        assert!(
            app.footer_ops().contains("reply (3)"),
            "footer shows the count: {}",
            app.footer_ops()
        );
        // Pressing `r` opens the picker rather than choosing silently.
        app.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(app.input.is_none(), "no composer opened yet");
        let picker = app.thread_picker.as_ref().expect("the picker opened");
        assert_eq!(picker.candidates.len(), 3);
        // Pick #2 (A, the middle of C/A/B) — the reply targets thread A.
        app.on_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert!(app.thread_picker.is_none(), "the picker closed");
        match &app.input.as_ref().expect("a reply composer opened").kind {
            ComposeKind::Reply(id) => assert_eq!(id, &a, "reply went to the picked thread A"),
            _ => panic!("expected a reply composer for the picked thread"),
        }
    }

    #[test]
    fn the_thread_picker_cancels_on_esc_without_acting() {
        let mut app = multi_line_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.add_thread(range_anchor(2, 4), "tester", "A", CommentKind::Local);
        app.add_thread(range_anchor(2, 3), "tester", "C", CommentKind::Local);
        app.relayout();
        app.cursor = 2; // both A and C cover line 2
        app.on_key(KeyCode::Char('x'), KeyModifiers::NONE); // resolve → picker
        assert!(app.thread_picker.is_some(), "two threads here → a picker");
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.thread_picker.is_none(), "esc closes the picker");
        assert!(
            !app.review.threads[0].is_resolved() && !app.review.threads[1].is_resolved(),
            "cancelling resolves nothing"
        );
    }

    #[test]
    fn a_single_line_anchor_still_targets_only_its_line() {
        let mut app = multi_line_app();
        app.mode = Mode::Unified;
        app.view = View::Files;
        app.add_thread(range_anchor(3, 3), "tester", "one", CommentKind::Local);
        app.relayout();
        app.cursor = 3;
        assert_eq!(app.threads_at_cursor(), vec![0], "on its own line");
        app.cursor = 2;
        assert!(app.threads_at_cursor().is_empty(), "not the line above");
        app.cursor = 4;
        assert!(app.threads_at_cursor().is_empty(), "not the line below");
    }

    #[test]
    fn esc_cancels_a_selection_instead_of_quitting() {
        let mut app = sample_app();
        app.mode = Mode::Unified;
        app.cursor = 1; // a content line (not the header)
        app.start_selection();
        assert!(app.selection.is_some());
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.selection.is_none());
        assert!(
            !app.quit,
            "the first Esc cancels the selection, not the app"
        );
    }

    // -- robustness (resize, hostile input, truncation) ---------------------

    #[test]
    fn truncate_path_head_keeps_the_filename() {
        // Fits: unchanged.
        assert_eq!(truncate_path_head("src/lib.rs", 20), "src/lib.rs");
        // Too long: the head is dropped and the filename survives.
        let out = truncate_path_head("very/deep/nested/path/to/file.rs", 12);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("file.rs"), "kept the filename: {out}");
        assert_eq!(out.chars().count(), 12);
        // Degenerate widths.
        assert_eq!(truncate_path_head("abcdef", 1), "…");
    }

    #[test]
    fn on_resize_clamps_a_scroll_past_the_end() {
        let mut app = sample_app();
        app.mode = Mode::SideBySide;
        app.scroll = 10_000;
        app.cursor = 10_000;
        app.on_resize(200);
        assert!(app.scroll <= app.rows_len());
        assert!(app.cursor < app.clines.len().max(1));
    }

    #[test]
    fn drawing_side_by_side_with_a_stale_scroll_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let app = {
            let mut app = sample_app();
            app.mode = Mode::SideBySide;
            // A scroll left over from a taller unified layout, past every sbs row.
            app.scroll = 10_000;
            app
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        // Without the clamp + saturating capacity this panics ("capacity overflow").
        terminal.draw(|f| app.draw(f)).unwrap();
    }

    #[test]
    fn layout_line_numbers_saturate_on_a_hostile_hunk() {
        let file = FileDiff {
            old_path: Some("a".into()),
            new_path: Some("a".into()),
            status: ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: u32::MAX,
                old_lines: u32::MAX,
                new_start: u32::MAX,
                new_lines: u32::MAX,
                section: None,
                lines: vec![Line {
                    kind: LineKind::Context,
                    content: "x".into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
        };
        let diff = Diff {
            files: vec![file],
            provenance: Provenance::default(),
        };
        // Must not overflow-panic; the ceiling saturates.
        let layout = Layouts::build(&diff, &Review::default(), &[], &HashSet::new());
        assert_eq!(layout.max_lineno, u32::MAX);
    }
}
