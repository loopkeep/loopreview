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
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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
    ReviewInfo, SessionInfo,
};

use loopreview_core::{
    Anchor, Comment, CommentKind, Diff, DiffSource, Line, LineKind, Review, Segment, Side, Thread,
    ThreadState, word_diff,
};

use crate::control::{self, UiRequest};
use crate::highlight::{Highlighter, LineHighlighter, Span as HlSpan};
use crate::keys::Action;
use crate::prsync::PrHandle;
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
/// The line cursor when the body is the inactive pane (sidebar focused): a
/// faint fill so it reads as "not the active target".
const CURSOR_DIM_BG: Color = Color::Rgb(30, 33, 42);
/// Faint full-width band behind every file header (even without the cursor), so
/// headers read as list dividers and inter-file blank lines can be dropped.
const HEADER_BG: Color = Color::Rgb(28, 31, 40);
/// Background of the file header the cursor rests on — brighter than a content
/// line's cursor, since headers are the diff's few anchors and must stand out.
const HEADER_CURSOR_BG: Color = Color::Rgb(54, 64, 92);
/// The header cursor when the body is the inactive pane (sidebar focused).
const HEADER_CURSOR_DIM_BG: Color = Color::Rgb(40, 45, 62);
/// Background marking the sidebar file currently shown in the body (a subtle
/// blue tint under a cyan bar), distinct from the stronger selection color.
const SIDEBAR_CURRENT_BG: Color = Color::Rgb(33, 43, 62);
/// The sidebar's resting selection when the sidebar is not the focused pane —
/// a faint fill, dimmer than the focused selection and the current-file tint.
const SIDEBAR_SEL_DIM_BG: Color = Color::Rgb(31, 35, 47);
/// Accent used on the focused pane's divider (dim when it is not focused).
const FOCUS_ACCENT: Color = Color::Cyan;
/// Background of a side-by-side cell with no line (the other side changed).
const ABSENT_BG: Color = Color::Rgb(22, 24, 30);
/// The bar background used for the header and footer.
const BAR_BG: Color = Color::Rgb(30, 33, 40);

/// Rows of context kept above/below the cursor when scrolling.
const SCROLLOFF: usize = 3;
/// Columns moved per horizontal-scroll step.
const HSCROLL_STEP: isize = 8;
/// Trailing whitespace allowed past the longest line at the far-right scroll
/// stop — a small reading margin so the tail is not glued to the edge.
const HSCROLL_MARGIN: usize = 8;
/// Sidebar width bounds (the minimum diff width kept beside it is configurable).
const SIDEBAR_MIN: usize = 22;
const SIDEBAR_MAX: usize = 44;
/// Background of a selected sidebar / finder row — a clear blue, distinct at a
/// glance from the current-file and cursor tints.
const SEL_BG: Color = Color::Rgb(48, 66, 106);
/// Background of lines in a range selection (for a multi-line comment).
const SELECTION_BG: Color = Color::Rgb(38, 48, 74);
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
    /// A diff with more files than this opens with every file collapsed.
    pub auto_collapse_files: usize,
    /// A diff with more changed lines than this opens with every file collapsed.
    pub auto_collapse_lines: usize,
    /// When the file-explorer sidebar is shown by default.
    pub sidebar_mode: crate::config::SidebarMode,
    /// Minimum diff width kept beside the sidebar.
    pub sidebar_min_content: usize,
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
    /// The store key for this PR's drafts (`owner/repo#number`), if a PR.
    pub pr_key: Option<String>,
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
    /// A re-pulled thread list to merge with local drafts.
    Refreshed(Vec<Thread>),
    /// Thread at `index` had its resolution synced.
    Resolved { index: usize, resolved: bool },
    /// A submitted review's id stamps.
    Submitted(crate::prsync::Submitted),
}

/// A background action worker: reports progress, then yields an outcome.
type JobWorker = Box<dyn FnOnce(&dyn Fn(&str)) -> Result<JobOutcome, String> + Send>;

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
    app.keymap = keymap;
    app.status = notice;
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
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = app.event_loop(
        &mut terminal,
        updates,
        control.as_ref().map(|c| &c.requests),
    );
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
    /// The tab bar row, when the tabs are shown.
    tabs_row: Option<u16>,
    /// The Files tab occupies columns `[0, tab_files_end)`.
    tab_files_end: u16,
    /// The Conversation tab occupies `[tab_files_end + 1, tab_conv_end)`.
    tab_conv_end: u16,
    /// The footer row.
    footer_row: u16,
    /// The layout indicator occupies `[0, layout_end)` on the footer row.
    layout_end: u16,
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
    /// Which pane has the keyboard focus.
    focus: Focus,
    /// Selected file index in the sidebar.
    sidebar_cursor: usize,
    /// Scroll offset (in rows) of the sidebar.
    sidebar_scroll: usize,
    /// The fuzzy file-finder overlay, when open.
    finder: Option<Finder>,
    /// The cursor line where a line-range selection began (`V` or a drag).
    selection: Option<usize>,
    /// The cursor line a mouse-drag started on (to distinguish a click).
    drag_anchor: Option<usize>,
    /// Screen geometry from the last draw, for mouse hit-testing.
    hit: Cell<HitLayout>,
    /// Horizontal scroll offset, in display columns, of the diff content (the
    /// line-number gutter stays fixed). Reset on a layout switch or file jump.
    hscroll: usize,
    /// The resolved key bindings (defaults plus config overrides).
    keymap: crate::keys::Keymap,
    /// Selected position within `conv_order` (the Conversation view / thread
    /// index), not a `review.threads` index — map through `conv_order`.
    conv_cursor: usize,
    /// Scroll offset (in lines) of the Conversation view.
    conv_scroll: usize,
    /// Minimum body width for `auto` layout to choose side-by-side.
    split_min_width: usize,
    /// True while awaiting confirmation to close (delete) the review.
    confirming_close: bool,
    /// The draft thread index awaiting confirmation to remove, when `d` is armed.
    confirming_delete: Option<usize>,
    /// An in-progress background load (spinner), for `lr pr`.
    loading: Option<Loading>,
    /// A fatal load error to show instead of the diff.
    load_error: Option<String>,
    /// A short-lived background action against GitHub, when one is running.
    job: Option<Job>,
    /// The pull-request handle, when reviewing a PR (enables sync/submit).
    pr: Option<Arc<PrHandle>>,
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
        let conv_blocks = build_conversation(
            &review,
            &diff,
            CONV_DEFAULT_WIDTH,
            &highlighter,
            &outdated,
            &collapsed,
            repo_dir.as_deref(),
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
            thread_outdated: outdated,
            conv_order,
            collapsed,
            manual_fold: HashSet::new(),
            collapsed_files: HashSet::new(),
            auto_collapse_files: 50,
            auto_collapse_lines: 20_000,
            view: View::Files,
            sidebar_mode: crate::config::SidebarMode::Auto,
            sidebar_override: None,
            sidebar_min_content: 44,
            focus: Focus::Body,
            sidebar_cursor: 0,
            sidebar_scroll: 0,
            finder: None,
            selection: None,
            drag_anchor: None,
            hit: Cell::new(HitLayout::default()),
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
        self.pr = loaded.pr.map(Arc::new);
        self.pr_key = loaded.pr_key;
        self.apply_layout(loaded.diff);
        self.cursor = 0;
        self.scroll = 0;
        self.conv_cursor = 0;
        self.conv_scroll = 0;
        self.loading = None;
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
            Ok(JobOutcome::Refreshed(threads)) => {
                self.review.threads = crate::prsync::merge_drafts(&self.review, threads);
                self.status = Some("refreshed from GitHub".to_string());
                self.relayout();
            }
            Ok(JobOutcome::Resolved { index, resolved }) => {
                let mut changed = None;
                if let Some(thread) = self.review.threads.get_mut(index) {
                    thread.state = if resolved {
                        ThreadState::Resolved
                    } else {
                        ThreadState::Open
                    };
                    changed = Some(thread.id.clone());
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
            Err(reason) => self.status = Some(format!("failed: {reason}")),
        }
    }

    /// Re-pull the PR's threads (keeping local drafts).
    fn refresh(&mut self) {
        let Some(pr) = self.pr.clone() else {
            return;
        };
        self.start_job(
            "Refreshing",
            Box::new(move |progress| {
                progress("fetching comments…");
                Ok(JobOutcome::Refreshed(pr.pull()?))
            }),
        );
    }

    /// Count the local drafts that a submit would post: new inline threads and
    /// replies (comments without a remote id).
    fn draft_counts(&self) -> (usize, usize) {
        let mut new_inline = 0;
        let mut replies = 0;
        for thread in &self.review.threads {
            for (i, comment) in thread.comments.iter().enumerate() {
                if comment.remote_id.is_some() {
                    continue;
                }
                if i == 0 {
                    if matches!(thread.anchor, Anchor::Line { .. }) {
                        new_inline += 1;
                    }
                } else {
                    replies += 1;
                }
            }
        }
        (new_inline, replies)
    }

    /// Open the review-submission modal (pull requests only).
    fn open_submit(&mut self) {
        if self.pr.is_none() {
            return;
        }
        let (new_count, reply_count) = self.draft_counts();
        self.submit = Some(SubmitModal {
            selected: 0,
            body: TextArea::default(),
            new_count,
            reply_count,
        });
    }

    /// Route a key while the submit modal is open.
    fn on_key_submit(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let Some(modal) = self.submit.as_mut() else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.submit = None;
                self.status = Some("submit cancelled".to_string());
            }
            KeyCode::Char('s') if ctrl => self.confirm_submit(),
            KeyCode::Up => modal.selected = modal.selected.saturating_sub(1),
            KeyCode::Down => modal.selected = (modal.selected + 1).min(SUBMIT_EVENTS.len() - 1),
            _ if ctrl => {}
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
        let event = SUBMIT_EVENTS[modal.selected].1;
        let body = modal.body.text().trim().to_string();
        let threads = self.review.threads.clone();
        self.start_job(
            "Submitting review",
            Box::new(move |progress| {
                progress("submitting review…");
                Ok(JobOutcome::Submitted(pr.submit(event, &body, &threads)?))
            }),
        );
    }

    /// Stamp remote ids from a submitted review onto the local threads.
    fn apply_submitted(&mut self, submitted: crate::prsync::Submitted) {
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
        self.status = Some(if submitted.failed_replies > 0 {
            format!(
                "review submitted — {} repl{} failed, still draft",
                submitted.failed_replies,
                if submitted.failed_replies == 1 {
                    "y"
                } else {
                    "ies"
                }
            )
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
        self.conv_blocks = build_conversation(
            &self.review,
            &diff,
            conv_width,
            &self.highlighter,
            &self.thread_outdated,
            &self.collapsed,
            self.repo_dir.as_deref(),
        );
        self.conv_order = conv_display_order(&self.review);
        self.conv_cursor = self
            .conv_cursor
            .min(self.conv_order.len().saturating_sub(1));
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

    /// Whether the review has any threads (and so the tab bar is shown).
    fn has_review(&self) -> bool {
        !self.review.threads.is_empty()
    }

    /// Switch the top-level view, re-syncing the sidebar so its index scroll
    /// tracks the new view's selection (the current file, or the selected
    /// thread).
    fn set_view(&mut self, view: View) {
        self.view = view;
        let sel = if view == View::Conversation {
            self.conv_cursor
        } else {
            self.current_file()
        };
        self.reveal_in_sidebar(sel);
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
        // Modals take keys next: the composer, the submit modal, the finder.
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
        // While confirming a draft delete: y/Enter removes, anything else cancels.
        if let Some(idx) = self.confirming_delete.take() {
            if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
                if idx < self.review.threads.len() {
                    self.remove_draft(idx, None);
                    self.status = Some("draft removed".to_string());
                }
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
            (KeyCode::Tab, _) if self.has_review() => {
                let next = match self.view {
                    View::Files => View::Conversation,
                    View::Conversation => View::Files,
                };
                self.set_view(next);
                return;
            }
            _ => {}
        }
        self.status = None;

        // Resolve the remappable action; dispatch globals, then the active context.
        let Some(action) = self.keymap.action(code, mods) else {
            return;
        };
        match action {
            Action::ToggleSidebar => return self.toggle_sidebar(),
            Action::FileFinder => return self.open_finder(),
            Action::Refresh if self.pr.is_some() => return self.refresh(),
            Action::Submit if self.pr.is_some() => return self.open_submit(),
            _ => {}
        }
        if in_sidebar {
            self.sidebar_action(action);
        } else if self.view == View::Conversation {
            self.conversation_action(action);
        } else {
            self.files_action(action);
        }
    }

    fn files_action(&mut self, action: Action) {
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
            Action::Reply => self.start_reply(),
            Action::Resolve => self.toggle_resolve(),
            Action::Fold => self.toggle_fold(),
            Action::NavIn => self.nav_in(),
            Action::NavOut => self.nav_out(),
            Action::ScrollLeft => self.hscroll_by(-HSCROLL_STEP),
            Action::ScrollRight => self.hscroll_by(HSCROLL_STEP),
            Action::Select => self.start_selection(),
            Action::Delete => self.request_delete(),
            _ => {}
        }
    }

    /// Arm the delete confirmation for the draft thread the selection points at.
    fn request_delete(&mut self) {
        match self.selected_draft_thread() {
            Some(idx) => self.confirming_delete = Some(idx),
            None => self.status = Some("no draft thread here to remove".to_string()),
        }
    }

    /// The thread the selection points at (Conversation: the selected thread;
    /// Files: the thread at the cursor line), if it is unpublished — a local note
    /// or a draft the reviewer can withdraw. A published thread cannot be removed.
    fn selected_draft_thread(&self) -> Option<usize> {
        let idx = if self.view == View::Conversation {
            self.selected_thread()?
        } else {
            self.thread_at_cursor()?
        };
        self.review.threads[idx]
            .root()
            .is_some_and(|c| !c.is_published())
            .then_some(idx)
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
        if self.store.is_none() && self.pr.is_none() {
            self.status = Some("comments need a git repository or a pull request".to_string());
            return;
        }
        let Some((anchor, target)) = self.compose_target() else {
            return;
        };
        self.input = Some(Compose {
            area: TextArea::default(),
            kind: ComposeKind::New(anchor),
            target,
            confirming_discard: false,
        });
        // The range is captured in the anchor; drop the visual selection.
        self.clear_selection();
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
        self.status = Some("visual line — j/k extend · c comment · Esc cancel".to_string());
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
    /// local concept); on a pull request only review threads (line/file-anchored)
    /// resolve — conversation comments (issue comments and review bodies, which
    /// anchor at [`Anchor::Review`]) have no resolve affordance on GitHub.
    fn is_resolvable(&self, idx: usize) -> bool {
        self.pr.is_none()
            || !matches!(
                self.review.threads.get(idx).map(|t| &t.anchor),
                Some(Anchor::Review)
            )
    }

    /// Toggle the resolved state of thread `idx`. A published thread in a PR
    /// syncs to GitHub in the background; a local thread just toggles and saves.
    fn resolve_thread(&mut self, idx: usize) {
        if !self.is_resolvable(idx) {
            self.status = Some("conversation comments can't be resolved on GitHub".to_string());
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
                        index: idx,
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

    /// `h` in the body: go one level out, in a hierarchy (nvim-tree style). On a
    /// line, jump to the file's own header; an expanded header collapses; a
    /// collapsed header moves focus to the sidebar (when it is showing). `b` is
    /// the direct jump to the sidebar for when the cascade is more than you want.
    fn nav_out(&mut self) {
        let file = self.current_file();
        if !self.cursor_is_header() {
            if let Some(header) = self.file_first.get(file).copied().flatten() {
                self.set_cursor(header);
            }
            return;
        }
        let collapsed = self
            .diff
            .files
            .get(file)
            .is_some_and(|f| self.collapsed_files.contains(f.display_path()));
        if collapsed {
            if self.sidebar_width(self.body_width.get()).is_some() {
                self.focus_sidebar();
            }
        } else {
            self.set_file_collapsed(file, Some(true));
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
        self.reveal_in_sidebar(self.sidebar_cursor);
    }

    /// Scroll the sidebar's viewport so file index `row` is within it.
    fn reveal_in_sidebar(&mut self, row: usize) {
        let height = self.body_height.get().max(1);
        if row < self.sidebar_scroll {
            self.sidebar_scroll = row;
        } else if row >= self.sidebar_scroll + height {
            self.sidebar_scroll = row + 1 - height;
        }
    }

    /// Scroll the sidebar list under the wheel (independent of the selection),
    /// clamped so the last file stays reachable.
    fn scroll_sidebar(&mut self, delta: isize) {
        let files = self.diff.files.len();
        let height = self.body_height.get().max(1);
        let max = files.saturating_sub(height) as isize;
        self.sidebar_scroll = (self.sidebar_scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Route a key while the sidebar has focus. In the Conversation view the
    /// sidebar drives the thread index instead of the file index.
    fn sidebar_action(&mut self, action: Action) {
        if self.view == View::Conversation {
            self.thread_index_action(action);
            return;
        }
        let files = self.diff.files.len();
        match action {
            Action::MoveDown => {
                if self.sidebar_cursor + 1 < files {
                    self.sidebar_cursor += 1;
                    self.follow_sidebar();
                }
            }
            Action::MoveUp => {
                self.sidebar_cursor = self.sidebar_cursor.saturating_sub(1);
                self.follow_sidebar();
            }
            Action::Top => {
                self.sidebar_cursor = 0;
                self.follow_sidebar();
            }
            Action::Bottom => {
                self.sidebar_cursor = files.saturating_sub(1);
                self.follow_sidebar();
            }
            // `l` / Enter toggle the file (expand + jump, or collapse in place);
            // `o` is a pure fold toggle; `h` is a no-op (the outermost level).
            Action::NavIn => self.sidebar_activate(self.sidebar_cursor),
            Action::Fold => self.toggle_fold_at(self.sidebar_cursor),
            _ => {}
        }
    }

    /// Activate a file from the sidebar (`l` / Enter / click): a collapsed file
    /// expands and the body jumps to it (focus follows into the body); an
    /// already-open file collapses in place, focus staying in the sidebar.
    fn sidebar_activate(&mut self, file: usize) {
        if file >= self.diff.files.len() {
            return;
        }
        self.sidebar_cursor = file;
        let collapsed = self
            .diff
            .files
            .get(file)
            .is_some_and(|f| self.collapsed_files.contains(f.display_path()));
        if collapsed {
            self.jump_to_file(file);
        } else {
            self.set_file_collapsed(file, Some(true));
            self.focus = Focus::Sidebar;
            self.follow_sidebar();
        }
    }

    /// Route a key while the thread index (Conversation sidebar) has focus. The
    /// grammar mirrors the file index: j/k select, l/Enter jump into the thread,
    /// o folds it.
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

    /// Toggle collapse of the selected Conversation thread.
    fn toggle_collapse_conv(&mut self) {
        if let Some(t) = self.selected_thread() {
            let id = self.review.threads[t].id.clone();
            self.toggle_collapse(id);
        }
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
            // j/k keep selecting threads (the scroll snaps to follow); the wheel
            // and the page/end keys scroll the pane freely without changing it.
            Action::MoveDown => self.move_conv(1),
            Action::MoveUp => self.move_conv(-1),
            Action::Top => self.conv_scroll = 0,
            Action::Bottom => self.conv_scroll = self.conv_max_scroll(),
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
            Action::Delete => self.request_delete(),
            Action::Fold => self.toggle_collapse_conv(),
            // l: a collapsed thread expands; an open one scrolls to its top.
            Action::NavIn => {
                if self.selected_collapsed() {
                    self.fold_selected(false);
                }
                self.follow_conv();
            }
            // h: an open thread collapses; a collapsed one steps out to the
            // thread index — the same cascade as a file header in the Files view.
            Action::NavOut => {
                if self.selected_thread().is_some() && !self.selected_collapsed() {
                    self.fold_selected(true);
                } else if self.sidebar_width(self.body_width.get()).is_some() {
                    self.focus_sidebar();
                }
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

    /// Close the review. For a pull request this discards the local drafts
    /// (published comments stay); otherwise it deletes the local review store.
    fn close_review(&mut self) {
        if self.pr.is_some() {
            // Drop fully-local draft threads and draft replies; keep published.
            self.review
                .threads
                .retain(|t| t.root().is_some_and(|c| c.remote_id.is_some()));
            for thread in &mut self.review.threads {
                thread.comments.retain(|c| c.remote_id.is_some());
            }
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
        self.view = View::Files;
        self.relayout();
    }

    /// The `review.threads` index of the selected thread (Conversation view).
    fn selected_thread(&self) -> Option<usize> {
        self.conv_order.get(self.conv_cursor).copied()
    }

    /// The display position (within `conv_order`) of thread `storage`.
    fn thread_display_pos(&self, storage: usize) -> usize {
        self.conv_order
            .iter()
            .position(|&t| t == storage)
            .unwrap_or(0)
    }

    fn move_conv(&mut self, delta: isize) {
        if self.conv_order.is_empty() {
            return;
        }
        let last = (self.conv_order.len() - 1) as isize;
        let next = (self.conv_cursor as isize + delta).clamp(0, last);
        self.set_conv(next as usize);
    }

    fn set_conv(&mut self, index: usize) {
        if self.conv_order.is_empty() {
            return;
        }
        self.conv_cursor = index.min(self.conv_order.len() - 1);
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
    fn thread_at_cursor(&self) -> Option<usize> {
        let (file, flat) = self.cursor_content()?;
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
        };
        self.relayout();
    }

    /// Save the review, returning a status message describing the outcome.
    fn persist(&self, done: &str) -> Option<String> {
        // In a pull request, only the drafts are stored (published comments are
        // re-pulled); they are sent on submit.
        if self.pr.is_some() {
            return match self.save_pr_drafts() {
                Ok(()) => Some(format!("{done} — draft saved, Ctrl-S to submit")),
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
    /// sent), a draft in a pull request (queued for submit).
    fn human_new_kind(&self) -> CommentKind {
        if self.pr.is_some() {
            CommentKind::Draft
        } else {
            CommentKind::Local
        }
    }

    /// The kind a reply inherits: local reviews are all local; on a PR, a reply
    /// continues its thread — local under a local note, draft under a draft or
    /// published thread.
    fn reply_kind(&self, thread_id: &str) -> CommentKind {
        if self.pr.is_none() {
            return CommentKind::Local;
        }
        let root_local = self
            .review
            .thread(thread_id)
            .and_then(|t| t.root())
            .is_some_and(|c| c.is_local());
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
        }
    }

    /// The human reviewer's current focus, for `lr session context`.
    fn context_info(&self) -> ContextInfo {
        let view = match self.view {
            View::Files => "files",
            View::Conversation => "conversation",
        }
        .to_string();
        let cursor = self.current_anchor();
        let thread = match self.view {
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

    /// Reload the current source (a control-plane `reload`). A pull request
    /// re-pulls in the background; a git/patch source reloads synchronously.
    fn control_reload(&mut self) -> Result<ReloadResult, String> {
        if self.pr.is_some() {
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
        if self.store.is_none() && self.pr.is_none() {
            return Err("comments need a git repository or a pull request".to_string());
        }
        // Locate the line so the anchor captures its commit and context.
        let file_idx = self
            .diff
            .files
            .iter()
            .position(|f| f.display_path() == add.file)
            .ok_or_else(|| format!("no file {} in the current review", add.file))?;
        let found = self.diff.files[file_idx]
            .hunks
            .iter()
            .enumerate()
            .find_map(|(hi, h)| {
                h.lines
                    .iter()
                    .position(|l| {
                        let n = if add.side == Side::New {
                            l.new_lineno
                        } else {
                            l.old_lineno
                        };
                        n == Some(add.line)
                    })
                    .map(|li| (hi, li))
            });
        let (hi, li) = found.ok_or_else(|| {
            format!(
                "line {} ({}) is not shown in the diff for {}",
                add.line,
                if add.side == Side::New { "new" } else { "old" },
                add.file
            )
        })?;
        let commit = if add.side == Side::New {
            self.diff.provenance.head.clone()
        } else {
            self.diff.provenance.base.clone()
        };
        let context = context_snippet(&self.diff.files[file_idx].hunks[hi], li);
        let anchor = Anchor::Line {
            file: add.file.clone(),
            side: add.side,
            start: add.line,
            end: add.line,
            commit,
            context,
        };
        let kind = self.agent_kind(add.draft);
        let (thread, comment) = self.add_thread(anchor, &add.author, &add.body, kind);
        let done = self.persist("comment added").unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done} ({}:{})", add.file, add.line));
        Ok(protocol::CommentResult {
            thread,
            comment,
            draft: kind == CommentKind::Draft,
        })
    }

    /// The kind for an agent-authored comment: a local note by default (agents
    /// converse, they don't queue GitHub sends), a draft only on a PR with the
    /// explicit `--draft` flag — so an agent's note is never sent by accident.
    fn agent_kind(&self, draft: bool) -> CommentKind {
        if self.pr.is_some() && draft {
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
        if self.store.is_none() && self.pr.is_none() {
            return Err("comments need a git repository or a pull request".to_string());
        }
        // An agent's reply is a local note unless it passes --draft (agents don't
        // queue GitHub sends implicitly, even under a draft thread).
        let kind = self.agent_kind(reply.draft);
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
        let published = self.review.threads[idx]
            .root()
            .is_some_and(|c| c.remote_id.is_some());
        if published {
            return Err(
                "resolving a published pull-request thread is a human action (press x in the TUI)"
                    .to_string(),
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

    /// Withdraw a draft comment or thread by id (drafts only). A comment id
    /// removes that draft (and its thread when it empties); a thread id removes
    /// the whole draft thread. Published comments are refused.
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
        // Drafts only — never delete anything published to GitHub.
        let published = match ci {
            Some(ci) => self.review.threads[ti].comments[ci].remote_id.is_some(),
            None => self.review.threads[ti]
                .comments
                .iter()
                .any(|c| c.remote_id.is_some()),
        };
        if published {
            return Err(
                "only drafts can be removed — published comments stay on GitHub".to_string(),
            );
        }
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
    /// tree or the PR draft set.
    fn store_remove(&self, thread_id: &str, comment_id: Option<&str>) {
        let Some(store) = &self.store else {
            return;
        };
        if self.pr.is_some() {
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
        } else if self.view == View::Conversation {
            self.scroll_conv(delta);
        } else {
            self.scroll_view(delta);
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
            Region::Tabs if self.has_review() => {
                let hit = self.hit.get();
                if column < hit.tab_files_end {
                    self.set_view(View::Files);
                } else if column > hit.tab_files_end && column < hit.tab_conv_end {
                    self.set_view(View::Conversation);
                }
            }
            Region::Sidebar(row) => {
                let idx = self.sidebar_scroll + row;
                if self.view == View::Conversation {
                    self.jump_to_thread(idx);
                } else {
                    self.sidebar_activate(idx);
                }
            }
            Region::Content { col, row } => {
                // Conversation: a thread header toggles its fold (and selects);
                // a body line just selects.
                if self.view == View::Conversation {
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
            self.reveal_in_sidebar(self.current_file());
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
        // Split off the file-explorer sidebar when shown and the terminal is
        // wide enough (it auto-hides on a narrow terminal — the finder still
        // works there). In the two-pane layout each pane is framed with a
        // title, and the focused pane's frame accents so it is always obvious
        // where input goes.
        let mut content = body;
        let mut sidebar_x0 = 0u16;
        let mut sidebar_cols = 0u16;
        if let Some(sidebar_w) = self.sidebar_width(body.width as usize) {
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

        // Record the geometry for mouse hit-testing (inner rects; frames map to
        // Outside).
        let (files_label, conv_label) = self.tab_labels();
        let files_w = files_label.chars().count() as u16;
        self.hit.set(HitLayout {
            body_top: content.y,
            body_height: content.height,
            content_x0: content.x,
            content_w: content.width,
            sidebar_x0,
            sidebar_w: sidebar_cols,
            tabs_row: tabs.then(|| chunks[1].y),
            tab_files_end: files_w,
            tab_conv_end: files_w + 1 + conv_label.chars().count() as u16,
            footer_row: footer.y,
            layout_end: self.layout_label().chars().count() as u16,
        });

        self.draw_header(f, header);
        if tabs {
            self.draw_tabs(f, chunks[1]);
        }
        if self.view == View::Conversation {
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
        let area = centered_rect(70, 60, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Submit review ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines = vec![
            TextLine::from(TextSpan::styled(
                format!(
                    "{} new comment(s), {} repl(y/ies) to send",
                    modal.new_count, modal.reply_count
                ),
                Style::default().fg(Color::Gray),
            )),
            TextLine::from(""),
            TextLine::from(TextSpan::styled(
                "event:",
                Style::default().fg(Color::DarkGray),
            )),
        ];
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
        let prompt = if self.pr.is_some() {
            "Discard your local drafts for this pull request?".to_string()
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

    /// The confirmation modal for withdrawing a draft thread with `d`.
    fn draw_delete_confirm(&self, f: &mut Frame) {
        let Some(idx) = self.confirming_delete else {
            return;
        };
        let area = centered_rect(60, 22, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Remove draft? ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let label = self
            .review
            .threads
            .get(idx)
            .map(|t| anchor_label(&t.anchor))
            .unwrap_or_default();
        let lines = vec![
            TextLine::from(TextSpan::styled(
                format!("Withdraw your draft thread on {label}?"),
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

    /// The two tab labels (Files, Conversation), shared by drawing and mouse
    /// hit-testing so their widths stay in sync.
    fn tab_labels(&self) -> (String, String) {
        (
            format!(" Files ({}) ", self.diff.files.len()),
            format!(" Conversation ({}) ", self.review.threads.len()),
        )
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let active = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let idle = Style::default().fg(Color::Gray);
        let (files, conv) = self.tab_labels();
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
        let width = area.width as usize;
        let mut lines: Vec<TextLine> = Vec::new();
        for (pos, &ti) in self.conv_order.iter().enumerate() {
            let block = &self.conv_blocks[ti];
            let selected = pos == self.conv_cursor;
            for (li, line) in block.iter().enumerate() {
                // The header (first line of a block) gets a full-width band, like
                // a file header; the selected thread tints its whole block.
                let bg = if selected {
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
            return match &self.pr {
                Some(pr) => format!(" PR #{} — {} ", pr.number(), pr.title()),
                None => format!(" Review — {} ", self.label),
            };
        }
        match self.diff.files.get(self.current_file()) {
            Some(file) => format!(" {} ", file_name(file.display_path())),
            None => " Diff ".to_string(),
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
        // A clickable layout indicator leads the footer (click toggles it).
        let mut spans = vec![
            TextSpan::styled(
                self.layout_label(),
                bar.fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TextSpan::styled(position, bar.fg(Color::Cyan)),
        ];
        // Horizontal-scroll indicator (only when scrolled and viewing the diff).
        if self.hscroll > 0 && self.view == View::Files {
            spans.push(TextSpan::styled(
                format!("→{} ", self.hscroll),
                bar.fg(Color::Rgb(120, 160, 220)),
            ));
        }
        if let Some(status) = &self.status {
            spans.push(TextSpan::styled(status.clone(), bar.fg(Color::Yellow)));
        } else {
            // The hint switches with focus and with what the cursor rests on, so
            // the keys shown are always the ones that act right now.
            let help = if self.focus == Focus::Sidebar {
                "j/k move · l open · o fold · esc body · ^p find · q quit"
            } else if self.view == View::Conversation {
                // `x` (resolve) only appears when the selected thread can resolve.
                if self
                    .selected_thread()
                    .is_some_and(|i| self.is_resolvable(i))
                {
                    "j/k thread · l open · h fold · r reply · x resolve · b index · tab diff · q quit"
                } else {
                    "j/k thread · l open · h fold · r reply · b index · tab diff · q quit"
                }
            } else if self.cursor_is_header() {
                "h fold · l open · j/k move · b sidebar · ^p find · q quit"
            } else {
                "j/k move · h header · c comment · r reply · o fold · b sidebar · q quit"
            };
            spans.push(TextSpan::styled(help, bar.fg(Color::DarkGray)));
            if self.pr.is_some() {
                spans.push(TextSpan::styled(
                    "  · ^r refresh · ^s submit",
                    bar.fg(Color::Rgb(120, 160, 220)),
                ));
            }
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
        let marker = if is_cursor { "▎" } else { " " };
        let marker_fg = if dim { Color::DarkGray } else { Color::Cyan };
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
        let widest = self
            .file_entries()
            .iter()
            .map(|e| {
                e.path.chars().count()
                    + 2
                    + format!(" +{} -{}", e.added, e.removed).chars().count()
                    + if e.comments > 0 { 4 } else { 0 }
            })
            .max()
            .unwrap_or(SIDEBAR_MIN);
        let desired = widest.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
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
        width: usize,
        base: Style,
        matched: &[u32],
    ) -> Vec<TextSpan<'static>> {
        let chevron = if entry.collapsed { "▸ " } else { "  " };
        let left = vec![TextSpan::styled(chevron, base.fg(Color::Cyan))];
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
            &entry.path,
            |shown| path_highlight_spans(shown, &entry.path, matched, base.fg(Color::Gray)),
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
        let width = area.width as usize;
        let height = area.height as usize;
        let current = self.current_file();
        let sidebar_focused = self.focus == Focus::Sidebar;
        let start = self.sidebar_scroll.min(entries.len());
        let end = (start + height).min(entries.len());
        // Three states are shown and must be told apart by intensity, carried in
        // a leading marker column: the sidebar cursor while the sidebar has focus
        // (a clear blue fill + a bright white bar + bold); the file open in the
        // body (a subtle blue tint + a cyan bar, always); and — when focus is in
        // the body — the sidebar's resting cursor (a faint fill + a dim bar).
        let lines: Vec<TextLine> = entries[start..end]
            .iter()
            .map(|e| {
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
                spans.extend(self.file_row_spans(e, width.saturating_sub(1), base, &[]));
                TextLine::from(spans)
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
                // Right-fixed cluster: the author, then the reply count.
                let replies = thread.comments.len().saturating_sub(1);
                let author = thread.root().map(|c| c.author.as_str()).unwrap_or("");
                let mut right = vec![TextSpan::styled(
                    format!(" {author}"),
                    base.fg(Color::DarkGray),
                )];
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
            .style(Style::default().bg(Color::Rgb(20, 22, 28)));
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
                Style::default().bg(Color::Rgb(20, 22, 28))
            };
            if let Some(entry) = entries.get(*file) {
                lines.push(TextLine::from(
                    self.file_row_spans(entry, width, base, indices),
                ));
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
        let marker = if is_cursor { "▎" } else { " " };
        let marker_fg = if dim { Color::DarkGray } else { Color::Cyan };
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

/// The left gutter bar drawn beside an inline comment thread.
const COMMENT_BAR: Color = Color::Rgb(90, 130, 200);
/// Width the inline comment body wraps to (before the gutter bar).
const INLINE_COMMENT_WRAP: usize = 76;
/// Max lines in a placed thread's code excerpt (clipped tail-first beyond it).
const EXCERPT_MAX: usize = 8;
/// Subtle background on the anchored line(s) within a thread's code excerpt.
const EXCERPT_ANCHOR_BG: Color = Color::Rgb(46, 42, 30);

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
fn build_conversation(
    review: &Review,
    diff: &Diff,
    width: usize,
    highlighter: &Highlighter,
    outdated: &[bool],
    collapsed: &HashSet<String>,
    repo_dir: Option<&Path>,
) -> Vec<Vec<TextLine<'static>>> {
    let now = now();
    review
        .threads
        .iter()
        .enumerate()
        .map(|(ti, thread)| {
            let is_outdated = outdated.get(ti).copied().unwrap_or(false);
            let is_collapsed = collapsed.contains(&thread.id);
            let mut lines = Vec::new();
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
                return lines;
            }

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
                                    Style::default().fg(Color::Rgb(90, 90, 100)),
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
            Style::default().fg(Color::White).bg(Color::Rgb(52, 46, 28))
        } else {
            Style::default().fg(Color::Rgb(120, 120, 130))
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

    #[test]
    fn control_comment_add_creates_a_thread_and_emits_an_event() {
        let mut app = sample_app();
        let before = app.events.latest_seq();
        let response = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: "a.rs".into(),
            side: Side::New,
            line: 2,
            body: "look here".into(),
            author: "agent".into(),
            draft: false,
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
            file: "a.rs".into(),
            side: Side::New,
            line: 2,
            body: "note".into(),
            author: "agent".into(),
            draft: false,
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
            Response::Error(msg) => assert!(msg.contains("drafts"), "friendly refusal: {msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
        assert_eq!(app.review.threads.len(), 1, "a published thread stays");
    }

    #[test]
    fn tui_d_removes_a_draft_thread_after_confirm() {
        let mut app = sample_app();
        let _ = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: "a.rs".into(),
            side: Side::New,
            line: 2,
            body: "note".into(),
            author: "me".into(),
            draft: false,
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
        });
        assert!(
            app.status.as_deref().unwrap_or("").contains("failed"),
            "a partial failure is surfaced: {:?}",
            app.status
        );
    }

    fn pr_app() -> App {
        let mut app = sample_app();
        app.pr = Some(Arc::new(crate::prsync::PrHandle::for_test(1, "t")));
        app.pr_key = Some("owner/repo#1".into());
        app
    }

    fn add(app: &mut App, line: u32, draft: bool) {
        app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: "a.rs".into(),
            side: Side::New,
            line,
            body: "n".into(),
            author: "agent".into(),
            draft,
        }));
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
    fn human_new_is_draft_on_a_pr_and_replies_inherit() {
        // A human's new comment: a note locally, a draft on a PR.
        assert!(matches!(sample_app().human_new_kind(), CommentKind::Local));
        let mut pr = pr_app();
        assert!(matches!(pr.human_new_kind(), CommentKind::Draft));
        // A reply inherits its thread: local under a local note, draft otherwise.
        let mk = |id: &str, kind: CommentKind| Thread {
            id: id.into(),
            anchor: Anchor::Review,
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
    fn control_comment_add_rejects_a_line_not_in_the_diff() {
        let mut app = sample_app();
        let response = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: "a.rs".into(),
            side: Side::New,
            line: 99,
            body: "x".into(),
            author: "agent".into(),
            draft: false,
        }));
        assert!(matches!(response, Response::Error(_)));
        assert!(app.review.threads.is_empty());
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
            file: "a.rs".into(),
            side: Side::New,
            line: 1,
            body: "note".into(),
            author: "agent".into(),
            draft: false,
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
        // h on an expanded header collapses the file (the hierarchy cascade).
        app.nav_out();
        assert!(
            app.collapsed_files.contains("a.rs"),
            "h collapses an open file"
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
        // line → header → fold → sidebar, checking focus at each step.
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
        // h: an expanded header collapses the file.
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(app.collapsed_files.contains("a.rs"));
        assert_eq!(app.focus, Focus::Body);
        // h: a collapsed header moves focus to the sidebar.
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(
            app.focus,
            Focus::Sidebar,
            "the h cascade reaches the sidebar"
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
    fn conversation_h_l_fold_expand_and_focus() {
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
        assert!(!app.selected_collapsed());
        // h collapses an open thread (focus stays in the body).
        app.conversation_action(Action::NavOut);
        assert!(app.selected_collapsed(), "h folds an open thread");
        assert_eq!(app.focus, Focus::Body);
        // h again (now collapsed) focuses the thread index.
        app.conversation_action(Action::NavOut);
        assert_eq!(app.focus, Focus::Sidebar, "h on a collapsed thread → index");
        // l expands a collapsed thread.
        app.focus = Focus::Body;
        app.conversation_action(Action::NavIn);
        assert!(!app.selected_collapsed(), "l expands a collapsed thread");
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

        // h folds the open thread (focus stays); h again steps out to the index.
        app.body_width.set(120);
        app.conversation_action(Action::NavOut);
        assert!(app.selected_collapsed(), "h folds the open thread first");
        app.conversation_action(Action::NavOut);
        assert_eq!(app.focus, Focus::Sidebar, "h on a collapsed thread → index");
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
    fn footer_hint_follows_focus() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = multi_file_app(&["a.rs"]);
        app.sidebar_override = Some(true);
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();

        app.focus = Focus::Sidebar;
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            footer_text(&term).contains("esc body"),
            "the sidebar shows its own hint"
        );

        // Body focus, cursor on a file header (index 0).
        app.focus = Focus::Body;
        app.cursor = 0;
        assert!(app.cursor_is_header());
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            footer_text(&term).contains("h fold"),
            "the body header shows the fold/open hint"
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
            added: 1,
            removed: 0,
            comments: 0,
            collapsed: false,
        }
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
        over.insert("cursor_down".to_string(), "s".to_string());
        let mut app = multi_file_app(&["a.rs"]);
        app.keymap = crate::keys::Keymap::from_overrides(&over).unwrap();
        app.cursor = 0;
        // `s` now moves the cursor down.
        app.on_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(app.cursor, 1);
        // The old default `j` is unbound after the remap.
        app.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn sidebar_toggle_selects_and_jumps() {
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
        // l on a collapsed file expands it and jumps the body into it.
        app.sidebar_action(Action::NavIn);
        assert_eq!(app.current_file(), 2);
        assert_eq!(app.focus, Focus::Body);
        assert!(!app.collapsed_files.contains("c.rs"));
        // Activating the now-open file again collapses it, focus staying put.
        app.focus_sidebar();
        app.sidebar_action(Action::NavIn);
        assert!(app.collapsed_files.contains("c.rs"));
        assert_eq!(
            app.focus,
            Focus::Sidebar,
            "collapsing from the sidebar keeps focus there"
        );
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
            tab_files_end: 0,
            tab_conv_end: 0,
            footer_row: 21,
            layout_end: 0,
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

    fn hit(body_top: u16, sidebar_w: u16, tabs_row: Option<u16>, files_end: u16) -> HitLayout {
        HitLayout {
            body_top,
            body_height: 20,
            content_x0: if sidebar_w > 0 { sidebar_w + 1 } else { 0 },
            content_w: 100,
            sidebar_x0: 0,
            sidebar_w,
            tabs_row,
            tab_files_end: files_end,
            tab_conv_end: files_end + 1 + 20,
            footer_row: body_top + 20,
            layout_end: 0,
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
        let files_end = app.tab_labels().0.chars().count() as u16;
        app.hit.set(hit(2, 0, Some(1), files_end));
        app.mouse_down(files_end + 2, 1); // in the Conversation tab
        assert_eq!(app.view, View::Conversation);
        app.mouse_down(1, 1); // in the Files tab
        assert_eq!(app.view, View::Files);
    }

    #[test]
    fn mouse_click_in_the_sidebar_toggles_the_file() {
        let mut app = multi_file_app(&["a.rs", "b.rs", "c.rs"]);
        app.mode = Mode::Unified;
        app.sidebar_override = Some(true);
        app.body_width.set(120);
        app.collapsed_files.insert("c.rs".to_string());
        app.relayout();
        app.hit.set(hit(1, 22, None, 0));
        // Sidebar body row 2 (screen row 3) is the third file, collapsed:
        // clicking it expands and opens the file in the body.
        app.mouse_down(3, 3);
        assert_eq!(
            app.current_file(),
            2,
            "a click on a collapsed file opens it"
        );
        assert_eq!(app.focus, Focus::Body);
        assert!(!app.collapsed_files.contains("c.rs"));
        // Clicking the now-open file collapses it, focus moving to the sidebar.
        app.mouse_down(3, 3);
        assert!(app.collapsed_files.contains("c.rs"));
        assert_eq!(app.focus, Focus::Sidebar);
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
