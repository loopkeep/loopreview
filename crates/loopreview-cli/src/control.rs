//! The session control plane, server side.
//!
//! While the review UI runs, this hosts a local socket, registers the session,
//! and turns each incoming control connection into a request the UI thread
//! answers. Reads and mutations are forwarded to the UI over an mpsc channel (so
//! they touch the same `App` the human is looking at); `wait` is served straight
//! from the shared [`EventLog`] without disturbing the UI. The mapping from the
//! core diff/review model to the wire types lives here too.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loopreview_control::ControlError;
use loopreview_control::events::EventLog;
use loopreview_control::protocol::{
    AnchorInfo, CommentInfo, FileInfo, Hello, HunkInfo, LineInfo, PROTOCOL_VERSION, Reply, Request,
    Response, ThreadInfo, WaitResult,
};
use loopreview_control::registry::{self, SessionRecord};
use loopreview_control::transport::{self, Connection};

use loopreview_core::{Anchor, FileDiff, Hunk, Line, LineKind, Side, Thread, ThreadState};

/// A control request forwarded to the UI thread, with a channel for its reply.
pub struct UiRequest {
    /// The request to apply against the running `App`.
    pub request: Request,
    /// Where the UI thread sends the response.
    pub reply: Sender<Response>,
}

/// A live control plane owned by the UI: the request stream to drain, the event
/// log to publish to, and enough to deregister on exit.
pub struct Control {
    /// Requests from connected clients, drained by the UI event loop.
    pub requests: Receiver<UiRequest>,
    /// The event log the UI appends to and `wait` blocks on.
    pub events: Arc<EventLog>,
    /// This session's id.
    pub session_id: String,
    /// The sessions directory, for deregistration.
    sessions_dir: std::path::PathBuf,
}

impl Control {
    /// Remove this session's registry record and socket (best-effort, on exit).
    pub fn deregister(&self) {
        registry::remove(&self.sessions_dir, &self.session_id);
    }
}

/// Start the control plane for a session. Returns `None` — a degraded run with
/// no control plane, but a fully working UI — when no config directory is known
/// or the socket cannot be created.
pub fn start(sessions_dir: Option<&Path>, repo: Option<&Path>, source: &str) -> Option<Control> {
    let sessions_dir = sessions_dir?.to_path_buf();
    let _ = std::fs::create_dir_all(&sessions_dir);
    let session_id = new_id();
    let socket = transport::socket_id(&session_id);
    let listener = transport::listen(&socket).ok()?;

    let events = Arc::new(EventLog::new());
    let (ui_tx, requests) = mpsc::channel();
    {
        let events = events.clone();
        let id = session_id.clone();
        thread::spawn(move || {
            for stream in transport::incoming(&listener) {
                let ui_tx = ui_tx.clone();
                let events = events.clone();
                let id = id.clone();
                thread::spawn(move || handle(Connection::new(stream), id, ui_tx, events));
            }
        });
    }

    let record = SessionRecord {
        id: session_id.clone(),
        pid: std::process::id(),
        socket,
        repo: repo.map(|p| p.to_string_lossy().into_owned()),
        source: source.to_string(),
        started_at: now_secs(),
    };
    // A failed registration only means `session list` won't discover us; a direct
    // connect still works, so keep running.
    let _ = registry::register(&sessions_dir, &record);

    Some(Control {
        requests,
        events,
        session_id,
        sessions_dir,
    })
}

/// Serve one connection: handshake, then a single request. `wait` is answered
/// from the event log here; everything else is forwarded to the UI thread.
fn handle<S: std::io::Read + std::io::Write>(
    mut conn: Connection<S>,
    session_id: String,
    ui_tx: Sender<UiRequest>,
    events: Arc<EventLog>,
) {
    match conn.read::<Request>() {
        Ok(Request::Hello { version }) => {
            if version != PROTOCOL_VERSION {
                let _ = conn.write(&Response::Error(format!(
                    "unsupported protocol version {version}; this session speaks {PROTOCOL_VERSION}"
                )));
                return;
            }
            if conn
                .write(&Response::Ok(Reply::Hello(Hello {
                    protocol: PROTOCOL_VERSION,
                    session: session_id,
                })))
                .is_err()
            {
                return;
            }
        }
        _ => {
            let _ = conn.write(&Response::Error(
                "expected a hello handshake first".to_string(),
            ));
            return;
        }
    }

    let request = match conn.read::<Request>() {
        Ok(request) => request,
        Err(ControlError::LineTooLong) => {
            let _ = conn.write(&Response::Error("request too large".to_string()));
            return;
        }
        Err(_) => return,
    };
    let response = match request {
        Request::Hello { .. } => Response::Error("already greeted".to_string()),
        Request::Wait(wait) => {
            // Without an explicit floor, wait for the next event after now.
            let after = wait.after.unwrap_or_else(|| events.latest_seq());
            // Cap the wait so a disconnected client's handler thread cannot park
            // forever: even an open-ended wait wakes within MAX_WAIT and exits.
            let timeout = capped_wait(wait.timeout_ms);
            let (event, event_seq) = events.wait(&wait.events, after, Some(timeout));
            Response::Ok(Reply::Wait(WaitResult { event, event_seq }))
        }
        other => {
            let (tx, rx) = mpsc::channel();
            if ui_tx
                .send(UiRequest {
                    request: other,
                    reply: tx,
                })
                .is_err()
            {
                Response::Error("the session is shutting down".to_string())
            } else {
                rx.recv()
                    .unwrap_or_else(|_| Response::Error("the session did not respond".to_string()))
            }
        }
    };
    let _ = conn.write(&response);
}

/// The ceiling on a single `wait`, whether or not the client asked for a
/// timeout. It bounds how long a handler thread can stay parked after its client
/// has gone away (a socket that is merely half-open cannot be detected without
/// consuming protocol bytes, so a hard cap is the reliable bound); the client can
/// simply wait again with `--after` to continue.
const MAX_WAIT: Duration = Duration::from_secs(600);

/// Resolve the effective wait timeout: the requested one clamped to [`MAX_WAIT`],
/// or [`MAX_WAIT`] when none was given.
fn capped_wait(timeout_ms: Option<u64>) -> Duration {
    match timeout_ms {
        Some(ms) => Duration::from_millis(ms).min(MAX_WAIT),
        None => MAX_WAIT,
    }
}

/// A process-unique session id (pid, a timestamp, and a counter, hex-encoded).
fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}-{nanos:x}-{n:x}", std::process::id())
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -- core model → wire mapping -------------------------------------------------

/// Map a file diff to its wire shape; include line-level patch text on request.
pub fn file_info(file: &FileDiff, include_patch: bool) -> FileInfo {
    let (added, removed) = file.line_stats();
    let path = file.display_path().to_string();
    FileInfo {
        old_path: file.old_path.clone().filter(|old| old != &path),
        path,
        status: file.status.label().to_string(),
        binary: file.binary,
        added,
        removed,
        hunks: file
            .hunks
            .iter()
            .map(|hunk| hunk_info(hunk, include_patch))
            .collect(),
    }
}

/// Map a hunk to its wire shape; include its lines on request.
pub fn hunk_info(hunk: &Hunk, include_patch: bool) -> HunkInfo {
    HunkInfo {
        header: hunk.header(),
        old_start: hunk.old_start,
        old_lines: hunk.old_lines,
        new_start: hunk.new_start,
        new_lines: hunk.new_lines,
        section: hunk.section.clone(),
        lines: include_patch.then(|| hunk.lines.iter().map(line_info).collect()),
    }
}

/// Map one diff line to its wire shape.
pub fn line_info(line: &Line) -> LineInfo {
    let kind = match line.kind {
        LineKind::Context => "context",
        LineKind::Addition => "addition",
        LineKind::Deletion => "deletion",
    };
    LineInfo {
        kind: kind.to_string(),
        old: line.old_lineno,
        new: line.new_lineno,
        text: line.content.clone(),
    }
}

/// Map a thread to its wire shape, given whether its anchor is outdated.
pub fn thread_info(thread: &Thread, outdated: bool) -> ThreadInfo {
    let state = match thread.state {
        ThreadState::Open => "open",
        ThreadState::Resolved => "resolved",
    };
    ThreadInfo {
        id: thread.id.clone(),
        anchor: anchor_info(&thread.anchor),
        state: state.to_string(),
        outdated,
        draft: thread.root().is_some_and(|c| c.is_draft()),
        comments: thread
            .comments
            .iter()
            .map(|c| CommentInfo {
                id: c.id.clone(),
                author: c.author.clone(),
                body: c.body.clone(),
                created_at: c.created_at,
                kind: c.disposition().as_str().to_string(),
            })
            .collect(),
    }
}

/// Map a thread anchor to its wire shape.
pub fn anchor_info(anchor: &Anchor) -> AnchorInfo {
    match anchor {
        Anchor::Line {
            file,
            side,
            start,
            end,
            ..
        } => AnchorInfo {
            kind: "line".to_string(),
            file: Some(file.clone()),
            side: Some(*side),
            start: Some(*start),
            end: Some(*end),
        },
        Anchor::File { file } => AnchorInfo {
            kind: "file".to_string(),
            file: Some(file.clone()),
            side: None,
            start: None,
            end: None,
        },
        Anchor::Review => AnchorInfo {
            kind: "review".to_string(),
            file: None,
            side: None,
            start: None,
            end: None,
        },
    }
}

/// True when a line anchor's line is not present on its side in `diff` (so the
/// thread would show as outdated). File and review anchors are never outdated.
pub fn anchor_outdated(diff: &loopreview_core::Diff, anchor: &Anchor) -> bool {
    let Anchor::Line {
        file, side, end, ..
    } = anchor
    else {
        return false;
    };
    let present = diff.files.iter().any(|f| {
        f.display_path() == file
            && f.hunks.iter().any(|h| {
                h.lines.iter().any(|l| {
                    let n = if *side == Side::New {
                        l.new_lineno
                    } else {
                        l.old_lineno
                    };
                    n == Some(*end)
                })
            })
    });
    !present
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopreview_control::client::Client;
    use loopreview_control::protocol::{EventKind, SessionInfo, Wait};
    use loopreview_core::{Comment, Diff, Provenance};

    fn temp_sessions_dir() -> std::path::PathBuf {
        use std::sync::atomic::AtomicU32;
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lr-ctl-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The full server path in-process: a real socket, the handshake, a request
    /// forwarded to a stand-in "UI thread", and a `wait` served from the log.
    #[test]
    fn server_forwards_requests_and_serves_wait() {
        let dir = temp_sessions_dir();
        let control = start(
            Some(&dir),
            Some(std::path::Path::new("/repo")),
            "working tree",
        )
        .expect("start the control plane");
        let session_id = control.session_id.clone();
        let socket = transport::socket_id(&session_id);
        let events = control.events.clone();
        let requests = control.requests;

        // A stand-in for the UI thread: answer Get, reject everything else.
        let ui_id = session_id.clone();
        thread::spawn(move || {
            while let Ok(req) = requests.recv() {
                let response = match req.request {
                    Request::Get => Response::Ok(Reply::Session(SessionInfo {
                        id: ui_id.clone(),
                        pid: std::process::id(),
                        repo: Some("/repo".into()),
                        source: "working tree".into(),
                    })),
                    _ => Response::Error("unsupported in test".into()),
                };
                let _ = req.reply.send(response);
            }
        });

        // The session is discoverable in the registry.
        let listed = registry::list(&dir);
        assert!(listed.iter().any(|s| s.id == session_id));

        // A Get is forwarded to the UI and answered.
        let mut client = Client::connect(&socket).expect("connect");
        assert_eq!(client.session(), session_id);
        match client.call(&Request::Get).expect("Get") {
            Reply::Session(info) => assert_eq!(info.source, "working tree"),
            other => panic!("unexpected reply {other:?}"),
        }

        // A wait blocks on the socket thread until the log gets a matching event.
        let socket2 = socket.clone();
        let waiter = thread::spawn(move || {
            let mut client = Client::connect(&socket2).expect("connect waiter");
            client
                .call(&Request::Wait(Wait {
                    events: vec![EventKind::Reload],
                    after: Some(0),
                    timeout_ms: Some(2000),
                }))
                .expect("wait")
        });
        thread::sleep(Duration::from_millis(30));
        events.append(EventKind::Reload, None);
        match waiter.join().unwrap() {
            Reply::Wait(result) => {
                assert_eq!(result.event.map(|e| e.kind), Some(EventKind::Reload));
            }
            other => panic!("unexpected reply {other:?}"),
        }

        registry::remove(&dir, &session_id);
        assert!(registry::list(&dir).iter().all(|s| s.id != session_id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn modified_file() -> FileDiff {
        FileDiff {
            old_path: Some("src/lib.rs".into()),
            new_path: Some("src/lib.rs".into()),
            status: loopreview_core::ChangeStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                section: Some("fn main".into()),
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
    fn capped_wait_bounds_open_ended_and_oversized_timeouts() {
        // No timeout is capped, an oversized one is clamped, a small one is kept.
        assert_eq!(capped_wait(None), MAX_WAIT);
        assert_eq!(capped_wait(Some(10_000_000_000)), MAX_WAIT);
        assert_eq!(capped_wait(Some(1_500)), Duration::from_millis(1_500));
    }

    #[test]
    fn file_info_omits_lines_without_patch() {
        let info = file_info(&modified_file(), false);
        assert_eq!(info.path, "src/lib.rs");
        assert_eq!(info.old_path, None); // same path → omitted
        assert_eq!(info.status, "modified");
        assert_eq!((info.added, info.removed), (1, 0));
        assert!(info.hunks[0].lines.is_none());
        assert_eq!(info.hunks[0].header, "@@ -1,1 +1,2 @@");
    }

    #[test]
    fn file_info_includes_lines_with_patch() {
        let info = file_info(&modified_file(), true);
        let lines = info.hunks[0].lines.as_ref().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].kind, "addition");
        assert_eq!(lines[1].new, Some(2));
        assert_eq!(lines[1].text, "added");
    }

    #[test]
    fn renamed_file_reports_its_old_path() {
        let file = FileDiff {
            old_path: Some("old.rs".into()),
            new_path: Some("new.rs".into()),
            status: loopreview_core::ChangeStatus::Renamed,
            binary: false,
            hunks: vec![],
        };
        let info = file_info(&file, false);
        assert_eq!(info.path, "new.rs");
        assert_eq!(info.old_path.as_deref(), Some("old.rs"));
    }

    #[test]
    fn thread_info_maps_anchor_state_and_drafts() {
        let thread = Thread {
            id: "t1".into(),
            anchor: Anchor::line("src/lib.rs", Side::New, 2),
            state: ThreadState::Resolved,
            comments: vec![Comment {
                id: "c1".into(),
                author: "agent".into(),
                body: "here".into(),
                created_at: 5,
                remote_id: None,
                kind: loopreview_core::CommentKind::Draft,
            }],
        };
        let info = thread_info(&thread, false);
        assert_eq!(info.state, "resolved");
        assert_eq!(info.anchor.kind, "line");
        assert_eq!(info.anchor.side, Some(Side::New));
        assert_eq!(info.anchor.end, Some(2));
        assert_eq!(info.comments[0].kind, "draft");
        assert!(info.draft, "a draft root marks the thread as a draft");
    }

    #[test]
    fn anchor_outdated_tracks_presence_in_the_diff() {
        let diff = Diff {
            files: vec![modified_file()],
            provenance: Provenance::default(),
        };
        // Line 2 (new side) is present; line 9 is not.
        assert!(!anchor_outdated(
            &diff,
            &Anchor::line("src/lib.rs", Side::New, 2)
        ));
        assert!(anchor_outdated(
            &diff,
            &Anchor::line("src/lib.rs", Side::New, 9)
        ));
        // A file anchor is never outdated.
        assert!(!anchor_outdated(
            &diff,
            &Anchor::File {
                file: "src/lib.rs".into()
            }
        ));
    }
}
