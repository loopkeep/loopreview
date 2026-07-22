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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use loopreview_control::events::EventLog;
use loopreview_control::protocol::{
    self, ContextInfo, EventKind, NavigateResult, ReloadResult, Reply, Request, Response,
    ReviewInfo, SessionInfo,
};

use loopreview_core::{
    Anchor, Comment, Diff, DiffSource, LineKind, Review, Segment, Side, Thread, ThreadState,
    word_diff,
};

use crate::control::{self, UiRequest};
use crate::highlight::{Highlighter, Span as HlSpan};
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
    /// The repository directory, for reconstructing outdated comment lines from
    /// history (`git show <commit>:<path>`). `None` for patch sources.
    pub repo_dir: Option<PathBuf>,
    /// A background loader (used by `lr pr`): when present, the UI opens on a
    /// spinner and this runs off-thread to produce the diff and threads.
    pub loader: Option<Loader>,
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
        repo_dir,
        loader,
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
    /// The active review-submission modal, when submitting a PR review.
    submit: Option<SubmitModal>,
    /// Rendered inline block per thread, index-aligned to `review.threads`.
    comment_blocks: Vec<Vec<TextLine<'static>>>,
    /// Rendered Conversation block per thread (root, replies), same order.
    conv_blocks: Vec<Vec<TextLine<'static>>>,
    /// Thread ids whose inline/Conversation body is collapsed to its header.
    collapsed: HashSet<String>,
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
        let collapsed = HashSet::new();
        let comment_blocks = build_comment_blocks(&review, &highlighter, &collapsed);
        let block_lens: Vec<usize> = comment_blocks.iter().map(Vec::len).collect();
        let layout = Layouts::build(&diff, &review, &block_lens);
        let outdated = outdated_flags(&review, &layout.placed);
        let conv_blocks = build_conversation(
            &review,
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
            collapsed: HashSet::new(),
            view: View::Files,
            conv_cursor: 0,
            conv_scroll: 0,
            split_min_width: 160,
            confirming_close: false,
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
        // Published drafts are now remote; drop them from the store.
        let _ = self.save_pr_drafts();
        self.emit(EventKind::Submit, None);
        self.status = Some("review submitted".to_string());
        self.relayout();
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
        self.comment_blocks =
            build_comment_blocks(&self.review, &self.highlighter, &self.collapsed);
        let block_lens: Vec<usize> = self.comment_blocks.iter().map(Vec::len).collect();
        let layout = Layouts::build(&diff, &self.review, &block_lens);
        let outdated = outdated_flags(&self.review, &layout.placed);
        let conv_width = self.body_width.get().clamp(40, 120);
        self.conv_blocks = build_conversation(
            &self.review,
            conv_width,
            &self.highlighter,
            &outdated,
            &self.collapsed,
            self.repo_dir.as_deref(),
        );
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
        self.emit(EventKind::Reload, None);
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
        // Modals take keys next: the composer, then the submit modal.
        if self.input.is_some() {
            self.on_key_compose(code, mods);
            return;
        }
        if self.submit.is_some() {
            self.on_key_submit(code, mods);
            return;
        }
        // PR sync shortcuts, available in either view.
        if mods.contains(KeyModifiers::CONTROL) && self.pr.is_some() {
            match code {
                KeyCode::Char('r') => {
                    self.refresh();
                    return;
                }
                KeyCode::Char('s') => {
                    self.open_submit();
                    return;
                }
                _ => {}
            }
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
            (KeyCode::Char('o'), false) => self.toggle_collapse_files(),
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
        if self.store.is_none() && self.pr.is_none() {
            self.status = Some("comments need a git repository or a pull request".to_string());
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

    /// Toggle the resolved state of thread `idx`. A published thread in a PR
    /// syncs to GitHub in the background; a local thread just toggles and saves.
    fn resolve_thread(&mut self, idx: usize) {
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
        self.emit(EventKind::Resolve, Some(id));
        self.status = self.persist(if resolved { "resolved" } else { "reopened" });
        self.relayout();
    }

    /// Toggle collapse of the thread at the cursor line (Files view).
    fn toggle_collapse_files(&mut self) {
        if let Some(idx) = self.thread_at_cursor() {
            let id = self.review.threads[idx].id.clone();
            self.toggle_collapse(id);
        }
    }

    /// Toggle collapse of the selected Conversation thread.
    fn toggle_collapse_conv(&mut self) {
        if let Some(thread) = self.review.threads.get(self.conv_cursor) {
            let id = thread.id.clone();
            self.toggle_collapse(id);
        }
    }

    fn toggle_collapse(&mut self, id: String) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
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
            (KeyCode::Char('o'), false) => self.toggle_collapse_conv(),
            _ => {}
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
        let author = self.author.clone();
        let body = compose.area.text();
        self.status = match compose.kind {
            ComposeKind::New(anchor) => {
                self.add_thread(anchor, &author, &body);
                self.persist("comment added")
            }
            ComposeKind::Reply(thread_id) => match self.add_reply(&thread_id, &author, &body) {
                Some(_) => self.persist("reply added"),
                None => Some("the thread is gone".to_string()),
            },
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
        let drafts = Review {
            threads: self
                .review
                .threads
                .iter()
                .filter(|t| t.comments.iter().any(|c| c.remote_id.is_none()))
                .cloned()
                .collect(),
        };
        store
            .save_pr_drafts(key, &drafts)
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
    fn add_thread(&mut self, anchor: Anchor, author: &str, body: &str) -> (String, String) {
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
            }],
        });
        self.emit(EventKind::Comment, Some(thread_id.clone()));
        (thread_id, comment_id)
    }

    /// Append a reply to `thread_id`, returning the new comment id (or `None`
    /// when the thread is gone). Emits a [`EventKind::Reply`].
    fn add_reply(&mut self, thread_id: &str, author: &str, body: &str) -> Option<String> {
        let comment_id = generate_id();
        let thread = self.review.thread_mut(thread_id)?;
        thread.comments.push(Comment {
            id: comment_id.clone(),
            author: author.to_string(),
            body: body.trim_end().to_string(),
            created_at: now(),
            remote_id: None,
        });
        self.emit(EventKind::Reply, Some(thread_id.to_string()));
        Some(comment_id)
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
                .review
                .threads
                .get(self.conv_cursor)
                .map(|t| t.id.clone()),
            View::Files => self
                .thread_at_cursor()
                .map(|idx| self.review.threads[idx].id.clone()),
        };
        ContextInfo {
            view,
            file: cursor.as_ref().map(|a| a.path.clone()),
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
            {
                let target = CursorAnchor {
                    path: file.clone(),
                    new_side: side == Side::New,
                    line: end,
                };
                if let Some(cursor) = self.find_anchor(&target) {
                    self.view = View::Files;
                    self.set_cursor(cursor);
                    self.conv_cursor = idx;
                    self.status = Some(format!("agent → {file}:{end}"));
                    return Ok(NavigateResult {
                        moved: true,
                        file: Some(file),
                        line: Some(end),
                    });
                }
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
                let target = CursorAnchor {
                    path: file.clone(),
                    new_side: side == Side::New,
                    line,
                };
                match self.find_anchor(&target) {
                    Some(cursor) => {
                        self.view = View::Files;
                        self.set_cursor(cursor);
                        self.status = Some(format!("agent → {file}:{line}"));
                        Ok(NavigateResult {
                            moved: true,
                            file: Some(file),
                            line: Some(line),
                        })
                    }
                    None => Ok(NavigateResult {
                        moved: false,
                        file: Some(file),
                        line: Some(line),
                    }),
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
        let (thread, comment) = self.add_thread(anchor, &add.author, &add.body);
        let draft = self.pr.is_some();
        let done = self.persist("comment added").unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done} ({}:{})", add.file, add.line));
        Ok(protocol::CommentResult {
            thread,
            comment,
            draft,
        })
    }

    /// Reply to a thread (a control-plane `comment reply`).
    fn control_comment_reply(
        &mut self,
        reply: protocol::CommentReply,
    ) -> Result<protocol::CommentResult, String> {
        if self.store.is_none() && self.pr.is_none() {
            return Err("comments need a git repository or a pull request".to_string());
        }
        let comment = self
            .add_reply(&reply.thread, &reply.author, &reply.body)
            .ok_or_else(|| format!("no thread {}", reply.thread))?;
        let draft = self.pr.is_some();
        let done = self.persist("reply added").unwrap_or_default();
        self.relayout();
        self.status = Some(format!("agent: {done}"));
        Ok(protocol::CommentResult {
            thread: reply.thread,
            comment,
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

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.input.is_some()
            || self.submit.is_some()
            || self.job.is_some()
            || self.loading.is_some()
        {
            return; // a modal or a background action owns input
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
        if let Some(modal) = &self.submit {
            self.draw_submit(f, modal);
        }
        if self.confirming_close {
            self.draw_close_confirm(f);
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
                "j/k thread · o fold · r reply · x resolve · X close · tab files · q quit"
            } else {
                "j/k move · n/p file · c comment · r reply · x resolve · o fold · q quit"
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
            let mut header = vec![
                TextSpan::styled(
                    if is_collapsed { "▸ " } else { "▾ " },
                    Style::default().fg(Color::DarkGray),
                ),
                TextSpan::styled(
                    anchor_label(&thread.anchor),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
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
    fn control_comment_add_rejects_a_line_not_in_the_diff() {
        let mut app = sample_app();
        let response = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: "a.rs".into(),
            side: Side::New,
            line: 99,
            body: "x".into(),
            author: "agent".into(),
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
                remote_id: Some("R1".into()), // published
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
    fn control_review_and_list_expose_the_diff_and_threads() {
        let mut app = sample_app();
        let _ = app.handle_control(Request::CommentAdd(protocol::CommentAdd {
            file: "a.rs".into(),
            side: Side::New,
            line: 1,
            body: "note".into(),
            author: "agent".into(),
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
}
