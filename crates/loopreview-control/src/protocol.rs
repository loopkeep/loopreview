//! The wire protocol for loopreview's control plane.
//!
//! Messages are JSON Lines: one JSON object per line, in both directions. A
//! connection opens with a [`Request::Hello`] carrying [`PROTOCOL_VERSION`]; the
//! session answers with [`Reply::Hello`] before any other exchange. After the
//! handshake the client sends one request and reads one [`Response`].
//!
//! These types are the contract between a running review UI (the server) and the
//! `lr session` verbs (the client); keeping them in this crate lets both sides
//! share one definition and lets an external tool speak the protocol directly.

use serde::{Deserialize, Serialize};

use loopreview_core::Side;

/// The protocol version, negotiated in the [`Request::Hello`] handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request from a control client to a review session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// The handshake, sent first on every connection.
    Hello {
        /// The client's protocol version.
        version: u32,
    },
    /// The session's identity and diff source.
    Get,
    /// The human reviewer's current focus (cursor position and view).
    Context,
    /// The diff structure and the review's threads.
    Review {
        /// Include each hunk's lines (the raw diff text), not just its shape.
        #[serde(default)]
        include_patch: bool,
    },
    /// Move the human reviewer's cursor and view.
    Navigate(Navigate),
    /// Reload the session's current diff source.
    Reload,
    /// Add a new comment thread at a line.
    CommentAdd(CommentAdd),
    /// Reply to an existing thread.
    CommentReply(CommentReply),
    /// Resolve or reopen a thread (local reviews only).
    CommentResolve(CommentResolve),
    /// List the review's threads.
    CommentList,
    /// Block until a matching event occurs, or a timeout elapses.
    Wait(Wait),
}

/// Where to move the reviewer's view: to a thread, or to a file line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Navigate {
    /// Jump to the line a thread is anchored to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thread: Option<String>,
    /// Jump to a line in this file (with [`Navigate::line`]).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub file: Option<String>,
    /// Which side [`Navigate::line`] is measured on (defaults to the new side).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub side: Option<Side>,
    /// The 1-based line number to move to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<u32>,
}

/// Add a new comment thread anchored to a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentAdd {
    /// The file the line belongs to.
    pub file: String,
    /// Which side the line is measured on.
    pub side: Side,
    /// The 1-based line number.
    pub line: u32,
    /// The comment body (markdown).
    pub body: String,
    /// The comment author (required; agent comments are attributed).
    pub author: String,
}

/// Reply to an existing thread by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentReply {
    /// The thread to reply to.
    pub thread: String,
    /// The reply body (markdown).
    pub body: String,
    /// The reply author.
    pub author: String,
}

/// Resolve or reopen a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentResolve {
    /// The thread to change.
    pub thread: String,
    /// True to resolve, false to reopen.
    pub resolved: bool,
    /// The actor requesting the change (for the status line).
    pub author: String,
}

/// Wait for review events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    /// The event kinds to wait for; empty means any kind.
    #[serde(default)]
    pub events: Vec<EventKind>,
    /// Report only events after this sequence number; when omitted, waits for
    /// the next event after the moment the request is received.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub after: Option<u64>,
    /// Give up after this many milliseconds; when omitted, waits indefinitely.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout_ms: Option<u64>,
}

/// A response to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// The request succeeded, with a typed payload.
    Ok(Reply),
    /// The request failed, with a human-readable reason.
    Error(String),
}

/// The payload of a successful [`Response`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// Answer to the handshake.
    Hello(Hello),
    /// Answer to [`Request::Get`].
    Session(SessionInfo),
    /// Answer to [`Request::Context`].
    Context(ContextInfo),
    /// Answer to [`Request::Review`].
    Review(ReviewInfo),
    /// Answer to [`Request::Navigate`].
    Navigate(NavigateResult),
    /// Answer to [`Request::Reload`].
    Reload(ReloadResult),
    /// Answer to [`Request::CommentAdd`] / [`Request::CommentReply`].
    Comment(CommentResult),
    /// Answer to [`Request::CommentResolve`].
    Resolve(ResolveResult),
    /// Answer to [`Request::CommentList`]. A struct variant (not a newtype over
    /// the `Vec`) because an internally-tagged enum cannot serialize a newtype
    /// variant wrapping a sequence.
    Threads {
        /// The review's threads.
        threads: Vec<ThreadInfo>,
    },
    /// Answer to [`Request::Wait`].
    Wait(WaitResult),
}

/// The handshake acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The session's protocol version.
    pub protocol: u32,
    /// The session id.
    pub session: String,
}

/// A session's identity and source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The session id.
    pub id: String,
    /// The process id hosting the session.
    pub pid: u32,
    /// The repository root, when the session is git-backed.
    pub repo: Option<String>,
    /// A human-readable description of the diff source.
    pub source: String,
}

/// The human reviewer's current focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextInfo {
    /// The active view: `files` or `conversation`.
    pub view: String,
    /// The file under the cursor, if any.
    pub file: Option<String>,
    /// The side the cursor line is measured on.
    pub side: Option<Side>,
    /// The line under the cursor, if any.
    pub line: Option<u32>,
    /// The id of the thread at the cursor, if any.
    pub thread: Option<String>,
    /// The latest event sequence number, for chaining a [`Wait`] without gaps.
    pub event_seq: u64,
}

/// The diff structure and threads of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInfo {
    /// A human-readable description of the diff source.
    pub source: String,
    /// The base commit, when known.
    pub base: Option<String>,
    /// The head commit, when known.
    pub head: Option<String>,
    /// The changed files.
    pub files: Vec<FileInfo>,
    /// The review's threads.
    pub threads: Vec<ThreadInfo>,
}

/// One changed file's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    /// The path shown for the file (the new path, or old for a deletion).
    pub path: String,
    /// The pre-change path, when the file was renamed or copied.
    pub old_path: Option<String>,
    /// How the file changed: added / deleted / modified / renamed / copied.
    pub status: String,
    /// True when the file is binary (no line-by-line diff).
    pub binary: bool,
    /// Added-line count.
    pub added: u32,
    /// Removed-line count.
    pub removed: u32,
    /// The file's hunks.
    pub hunks: Vec<HunkInfo>,
}

/// One hunk's shape, and optionally its lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkInfo {
    /// The `@@ … @@` header.
    pub header: String,
    /// 1-based first old-side line.
    pub old_start: u32,
    /// Old-side line span.
    pub old_lines: u32,
    /// 1-based first new-side line.
    pub new_start: u32,
    /// New-side line span.
    pub new_lines: u32,
    /// The text after the `@@ … @@` marker, if any.
    pub section: Option<String>,
    /// The hunk's lines, present only when the patch was requested.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lines: Option<Vec<LineInfo>>,
}

/// One diff line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineInfo {
    /// `context`, `addition`, or `deletion`.
    pub kind: String,
    /// Old-side line number, when present.
    pub old: Option<u32>,
    /// New-side line number, when present.
    pub new: Option<u32>,
    /// The line text (without its newline).
    pub text: String,
}

/// One review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    /// The thread id.
    pub id: String,
    /// Where the thread is anchored.
    pub anchor: AnchorInfo,
    /// `open` or `resolved`.
    pub state: String,
    /// True when the anchor line is not present in the current diff.
    pub outdated: bool,
    /// The comments, oldest first.
    pub comments: Vec<CommentInfo>,
}

/// Where a thread is anchored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorInfo {
    /// `line`, `file`, or `review`.
    pub kind: String,
    /// The file, for line and file anchors.
    pub file: Option<String>,
    /// The side, for line anchors.
    pub side: Option<Side>,
    /// The first anchored line, for line anchors.
    pub start: Option<u32>,
    /// The last anchored line, for line anchors.
    pub end: Option<u32>,
}

/// One comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentInfo {
    /// The comment id.
    pub id: String,
    /// The author's display name.
    pub author: String,
    /// The body (markdown).
    pub body: String,
    /// Creation time, seconds since the Unix epoch.
    pub created_at: u64,
    /// True while the comment is an unpublished draft.
    pub draft: bool,
}

/// The outcome of a [`Request::Navigate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigateResult {
    /// True when the target was found and the view moved.
    pub moved: bool,
    /// The file moved to, if any.
    pub file: Option<String>,
    /// The line moved to, if any.
    pub line: Option<u32>,
}

/// The outcome of a [`Request::Reload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadResult {
    /// True when a background reload was started (a pull request); false when the
    /// diff was reloaded synchronously.
    pub started: bool,
}

/// The outcome of a comment mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentResult {
    /// The affected thread id.
    pub thread: String,
    /// The new comment id.
    pub comment: String,
    /// True when the comment is a draft (a pull-request review).
    pub draft: bool,
}

/// The outcome of a [`Request::CommentResolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveResult {
    /// The affected thread id.
    pub thread: String,
    /// The thread's state after the change.
    pub resolved: bool,
}

/// The outcome of a [`Request::Wait`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitResult {
    /// The matching event, or `None` when the wait timed out.
    pub event: Option<Event>,
    /// The latest event sequence number (to chain the next wait with `after`).
    pub event_seq: u64,
}

/// A kind of review event that a client can wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A new comment thread was created.
    Comment,
    /// A reply was added to a thread.
    Reply,
    /// A thread was resolved or reopened.
    Resolve,
    /// A pull-request review was submitted.
    Submit,
    /// The diff was reloaded (a watch refresh or an explicit reload).
    Reload,
}

impl EventKind {
    /// The lowercase name used on the wire and in the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Comment => "comment",
            EventKind::Reply => "reply",
            EventKind::Resolve => "resolve",
            EventKind::Submit => "submit",
            EventKind::Reload => "reload",
        }
    }
}

/// A review event, tagged with a monotonic sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The monotonic sequence number (starts at 1, increases with each event).
    pub seq: u64,
    /// The kind of event.
    pub kind: EventKind,
    /// The thread the event concerns, when applicable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thread: Option<String>,
    /// When the event occurred, seconds since the Unix epoch.
    pub at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let line = serde_json::to_string(value).unwrap();
        assert!(!line.contains('\n'), "a message must be one line: {line}");
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn requests_round_trip() {
        let requests = [
            Request::Hello { version: 1 },
            Request::Get,
            Request::Context,
            Request::Review {
                include_patch: true,
            },
            Request::Navigate(Navigate {
                thread: None,
                file: Some("src/lib.rs".into()),
                side: Some(Side::New),
                line: Some(42),
            }),
            Request::Reload,
            Request::CommentAdd(CommentAdd {
                file: "a.rs".into(),
                side: Side::New,
                line: 3,
                body: "look here".into(),
                author: "agent".into(),
            }),
            Request::CommentReply(CommentReply {
                thread: "t1".into(),
                body: "and here".into(),
                author: "agent".into(),
            }),
            Request::CommentResolve(CommentResolve {
                thread: "t1".into(),
                resolved: true,
                author: "agent".into(),
            }),
            Request::CommentList,
            Request::Wait(Wait {
                events: vec![EventKind::Reply, EventKind::Resolve],
                after: Some(7),
                timeout_ms: Some(5000),
            }),
        ];
        for request in &requests {
            assert_eq!(&round_trip(request), request);
        }
    }

    #[test]
    fn responses_round_trip() {
        let ok = Response::Ok(Reply::Wait(WaitResult {
            event: Some(Event {
                seq: 3,
                kind: EventKind::Reply,
                thread: Some("t1".into()),
                at: 1000,
            }),
            event_seq: 3,
        }));
        assert_eq!(round_trip(&ok), ok);
        let err = Response::Error("no such thread".into());
        assert_eq!(round_trip(&err), err);
    }

    /// Every `Reply` variant must serialize: the internal `reply` tag rules out
    /// newtype variants wrapping a sequence, so each is exercised here.
    #[test]
    fn every_reply_variant_serializes() {
        let anchor = AnchorInfo {
            kind: "line".into(),
            file: Some("a.rs".into()),
            side: Some(Side::New),
            start: Some(1),
            end: Some(1),
        };
        let thread = ThreadInfo {
            id: "t1".into(),
            anchor: anchor.clone(),
            state: "open".into(),
            outdated: false,
            comments: vec![CommentInfo {
                id: "c1".into(),
                author: "agent".into(),
                body: "hi".into(),
                created_at: 1,
                draft: true,
            }],
        };
        let replies = [
            Reply::Hello(Hello {
                protocol: 1,
                session: "s".into(),
            }),
            Reply::Session(SessionInfo {
                id: "s".into(),
                pid: 1,
                repo: None,
                source: "working tree".into(),
            }),
            Reply::Context(ContextInfo {
                view: "files".into(),
                file: Some("a.rs".into()),
                side: Some(Side::New),
                line: Some(1),
                thread: None,
                event_seq: 0,
            }),
            Reply::Review(ReviewInfo {
                source: "working tree".into(),
                base: None,
                head: None,
                files: vec![FileInfo {
                    path: "a.rs".into(),
                    old_path: None,
                    status: "modified".into(),
                    binary: false,
                    added: 1,
                    removed: 0,
                    hunks: vec![HunkInfo {
                        header: "@@ -1 +1 @@".into(),
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 1,
                        section: None,
                        lines: Some(vec![LineInfo {
                            kind: "addition".into(),
                            old: None,
                            new: Some(1),
                            text: "x".into(),
                        }]),
                    }],
                }],
                threads: vec![thread.clone()],
            }),
            Reply::Navigate(NavigateResult {
                moved: true,
                file: Some("a.rs".into()),
                line: Some(1),
            }),
            Reply::Reload(ReloadResult { started: false }),
            Reply::Comment(CommentResult {
                thread: "t1".into(),
                comment: "c2".into(),
                draft: true,
            }),
            Reply::Resolve(ResolveResult {
                thread: "t1".into(),
                resolved: true,
            }),
            Reply::Threads {
                threads: vec![thread],
            },
            Reply::Wait(WaitResult {
                event: None,
                event_seq: 0,
            }),
        ];
        for reply in replies {
            let response = Response::Ok(reply);
            assert_eq!(round_trip(&response), response);
        }
    }

    #[test]
    fn request_tag_is_the_op_field() {
        let json = serde_json::to_string(&Request::Get).unwrap();
        assert_eq!(json, r#"{"op":"get"}"#);
    }

    #[test]
    fn response_ok_and_error_are_externally_tagged() {
        let err = serde_json::to_string(&Response::Error("boom".into())).unwrap();
        assert_eq!(err, r#"{"error":"boom"}"#);
        let ok = serde_json::to_string(&Response::Ok(Reply::Hello(Hello {
            protocol: 1,
            session: "s".into(),
        })))
        .unwrap();
        assert_eq!(ok, r#"{"ok":{"reply":"hello","protocol":1,"session":"s"}}"#);
    }
}
