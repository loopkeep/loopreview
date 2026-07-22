//! The review model: comment threads anchored to a diff.
//!
//! This is the data behind loopreview's local review (M2a) and, later, GitHub
//! sync (M2b). It is deliberately flat and serde-serializable — the store is a
//! JSON document per repository — while the UI presents it as a tree (a thread
//! with nested replies). The model owns no I/O; loading and saving live in the
//! CLI layer.
//!
//! A [`Thread`] is pinned to an [`Anchor`]. A line anchor records the commit and
//! a context snippet at creation time so an outdated thread (one whose line has
//! since moved) can be relocated later.

use serde::{Deserialize, Serialize};

use crate::model::Side;

/// Where a thread is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Anchor {
    /// A range of lines within a file, on one side of the diff.
    Line {
        /// Path of the file the anchor refers to.
        file: String,
        /// Which version of the file the range is measured on.
        side: Side,
        /// First line of the range (1-based, inclusive).
        start: u32,
        /// Last line of the range (inclusive; equals `start` for one line).
        end: u32,
        /// The commit the anchored side was at when the thread was created, for
        /// relocating the thread against history when it goes outdated.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        commit: Option<String>,
        /// The line texts captured at creation, for fuzzy relocation.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        context: Vec<String>,
    },
    /// The file as a whole (not a specific line).
    File {
        /// Path of the file.
        file: String,
    },
    /// The whole changeset — a conversation not tied to any file or line.
    Review,
}

impl Anchor {
    /// A single-line anchor on `side` of `file` at `line`.
    pub fn line(file: impl Into<String>, side: Side, line: u32) -> Anchor {
        Anchor::Line {
            file: file.into(),
            side,
            start: line,
            end: line,
            commit: None,
            context: Vec::new(),
        }
    }

    /// The file this anchor concerns, if any (`None` for [`Anchor::Review`]).
    pub fn file(&self) -> Option<&str> {
        match self {
            Anchor::Line { file, .. } | Anchor::File { file } => Some(file),
            Anchor::Review => None,
        }
    }
}

/// Whether a thread is still open or has been resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadState {
    /// The thread is unresolved.
    #[default]
    Open,
    /// The thread has been marked resolved.
    Resolved,
}

/// What a comment is *for*, independent of whether it has been published. A
/// `local` note is agent-conversation only and is never sent to GitHub; a
/// `draft` is queued for the next `review submit`; a `published` comment came
/// from GitHub and is never a send target. Publication is normally tracked by a
/// `remote_id`, but a comment pulled from GitHub with no addressable id (a null
/// `databaseId`) is marked `Published` so it can never be mistaken for a draft
/// and re-posted. Old stores (no `kind`) deserialize as `Draft`, preserving the
/// historical "everything is a draft" behavior; the store's loader downgrades a
/// working-tree review to `Local` (a non-PR review sends nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentKind {
    /// A note for the agent conversation — never sent to GitHub.
    Local,
    /// Queued for the next `review submit` (the historical default).
    #[default]
    Draft,
    /// Pulled from GitHub (already on the remote) — never a send target, even
    /// when it carries no addressable `remote_id`.
    Published,
}

impl CommentKind {
    /// The wire/display token for this disposition: `local`, `draft`, or
    /// `published`.
    pub fn as_str(self) -> &'static str {
        match self {
            CommentKind::Local => "local",
            CommentKind::Draft => "draft",
            CommentKind::Published => "published",
        }
    }
}

/// A single comment: the root of a thread, or a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// Stable unique id.
    pub id: String,
    /// Display name of the author.
    pub author: String,
    /// The comment body (markdown).
    pub body: String,
    /// Creation time, seconds since the Unix epoch (sorts chronologically).
    pub created_at: u64,
    /// The remote (GitHub) comment id once published; `None` while unpublished.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_id: Option<String>,
    /// Whether this is a local note or a draft to submit. Defaults to `Draft` for
    /// data written before this field existed.
    #[serde(default)]
    pub kind: CommentKind,
}

impl Comment {
    /// True while this comment has not been published to a remote.
    pub fn is_published(&self) -> bool {
        self.remote_id.is_some()
    }

    /// True for a local note (never sent to GitHub).
    pub fn is_local(&self) -> bool {
        self.kind == CommentKind::Local
    }

    /// True while this comment is an unpublished draft queued to submit — not a
    /// local note, not pulled from GitHub, and not yet on the remote.
    pub fn is_draft(&self) -> bool {
        self.remote_id.is_none() && self.kind == CommentKind::Draft
    }

    /// The comment's disposition — `Local`, `Draft`, or `Published` — as a single
    /// source of truth for the badges and the control protocol. A comment is
    /// published when it carries a `remote_id` or was pulled from GitHub with none
    /// (`CommentKind::Published`); a local note stays local; anything else is a
    /// draft queued to submit.
    pub fn disposition(&self) -> CommentKind {
        if self.kind == CommentKind::Local {
            CommentKind::Local
        } else if self.is_published() || self.kind == CommentKind::Published {
            CommentKind::Published
        } else {
            CommentKind::Draft
        }
    }
}

/// A comment thread: a root comment plus chronological replies, pinned to an
/// [`Anchor`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Stable unique id.
    pub id: String,
    /// Where the thread is attached.
    pub anchor: Anchor,
    /// Open or resolved.
    #[serde(default)]
    pub state: ThreadState,
    /// The comments, oldest first; `comments[0]` is the root.
    pub comments: Vec<Comment>,
}

impl Thread {
    /// The root comment, if the thread has one.
    pub fn root(&self) -> Option<&Comment> {
        self.comments.first()
    }

    /// The replies to the root (all comments after the first).
    pub fn replies(&self) -> &[Comment] {
        self.comments.get(1..).unwrap_or(&[])
    }

    /// True when the thread is resolved.
    pub fn is_resolved(&self) -> bool {
        self.state == ThreadState::Resolved
    }
}

/// The whole local review for one repository: every comment thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// The threads, in creation order.
    pub threads: Vec<Thread>,
}

impl Review {
    /// True when there are no threads.
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// The number of threads that are still open.
    pub fn open_count(&self) -> usize {
        self.threads.iter().filter(|t| !t.is_resolved()).count()
    }

    /// Find a thread by id.
    pub fn thread(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    /// Find a thread by id, mutably.
    pub fn thread_mut(&mut self, id: &str) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.id == id)
    }

    /// Remove a thread by id, returning whether one was removed.
    pub fn remove_thread(&mut self, id: &str) -> bool {
        let before = self.threads.len();
        self.threads.retain(|t| t.id != id);
        self.threads.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, body: &str) -> Comment {
        Comment {
            id: id.to_string(),
            author: "tester".to_string(),
            body: body.to_string(),
            created_at: 1_000,
            remote_id: None,
            kind: CommentKind::Draft,
        }
    }

    fn thread(id: &str) -> Thread {
        Thread {
            id: id.to_string(),
            anchor: Anchor::line("src/lib.rs", Side::New, 42),
            state: ThreadState::Open,
            comments: vec![comment("c1", "root"), comment("c2", "reply")],
        }
    }

    #[test]
    fn root_and_replies_split_the_comments() {
        let t = thread("t1");
        assert_eq!(t.root().unwrap().body, "root");
        assert_eq!(t.replies().len(), 1);
        assert_eq!(t.replies()[0].body, "reply");
    }

    #[test]
    fn line_anchor_has_a_file() {
        let anchor = Anchor::line("a.rs", Side::Old, 3);
        assert_eq!(anchor.file(), Some("a.rs"));
        assert_eq!(Anchor::Review.file(), None);
    }

    #[test]
    fn missing_kind_deserializes_as_draft() {
        // Data written before `kind` existed must load as a draft (it was queued
        // to submit), never as a silent local note that would not be sent.
        let json = r#"{"id":"c","author":"a","body":"b","created_at":0}"#;
        let c: Comment = serde_json::from_str(json).unwrap();
        assert_eq!(c.kind, CommentKind::Draft);
        assert!(c.is_draft() && !c.is_local());
        // A local note round-trips.
        let local = Comment {
            kind: CommentKind::Local,
            ..c
        };
        let back: Comment = serde_json::from_str(&serde_json::to_string(&local).unwrap()).unwrap();
        assert!(back.is_local() && !back.is_draft());
    }

    #[test]
    fn published_kind_is_never_a_draft() {
        // A comment pulled from GitHub with no addressable id: marked Published so
        // it is never a send target, even though it carries no remote_id.
        let pulled = Comment {
            kind: CommentKind::Published,
            remote_id: None,
            ..comment("c", "from github")
        };
        assert!(!pulled.is_draft(), "a pulled comment is never a draft");
        assert!(!pulled.is_local());
        assert_eq!(pulled.disposition(), CommentKind::Published);

        // The ordinary dispositions.
        let draft = comment("d", "queued");
        assert_eq!(draft.disposition(), CommentKind::Draft);
        let local = Comment {
            kind: CommentKind::Local,
            ..comment("l", "note")
        };
        assert_eq!(local.disposition(), CommentKind::Local);
        // A remote_id alone (the common pulled case) reads as published too.
        let remote = Comment {
            remote_id: Some("123".into()),
            ..comment("r", "on github")
        };
        assert_eq!(remote.disposition(), CommentKind::Published);
        assert_eq!(CommentKind::Published.as_str(), "published");
    }

    #[test]
    fn open_count_ignores_resolved_threads() {
        let mut review = Review {
            threads: vec![thread("t1"), thread("t2")],
        };
        assert_eq!(review.open_count(), 2);
        review.thread_mut("t1").unwrap().state = ThreadState::Resolved;
        assert_eq!(review.open_count(), 1);
        assert!(review.remove_thread("t2"));
        assert_eq!(review.threads.len(), 1);
    }

    #[test]
    fn review_round_trips_through_json() {
        let review = Review {
            threads: vec![thread("t1")],
        };
        let json = serde_json::to_string(&review).unwrap();
        let back: Review = serde_json::from_str(&json).unwrap();
        assert_eq!(review, back);
    }

    #[test]
    fn line_anchor_serializes_with_a_type_tag() {
        let anchor = Anchor::line("f", Side::New, 1);
        let json = serde_json::to_string(&anchor).unwrap();
        assert!(json.contains("\"type\":\"line\""));
        assert!(json.contains("\"side\":\"new\""));
    }
}
