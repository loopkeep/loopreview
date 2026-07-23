//! The bridge between the review UI and the `loopreview-github` crate.
//!
//! This is the only cli module that knows about GitHub: it builds a PR query,
//! fetches a PR's diff and threads, and — once a PR is open — syncs resolutions
//! and submits reviews. The UI holds an opaque [`PrHandle`] and calls these
//! methods on background threads; everything returns plain data or a string
//! error so the rest of the UI stays GitHub-agnostic.

use std::path::PathBuf;

use loopreview_core::{Anchor, Comment, Diff, DiffSource, Review, Thread};
use loopreview_github::{
    CommentEndpoint, GithubClient, IssueStatus, PrQuery, PrStatus, ResolvedIssue, ResolvedPr,
    ReviewEvent, Subject, SubjectKind,
};

/// Build a [`PrQuery`] from the CLI arguments, or an error message.
pub fn query(text: Option<String>, detect: bool) -> Result<PrQuery, String> {
    if detect {
        return Ok(PrQuery::Detect);
    }
    let text = text.ok_or(
        "give a pull request (number, URL, owner/repo#N, or #N), or pass --detect".to_string(),
    )?;
    PrQuery::parse(&text).ok_or_else(|| {
        format!("`{text}` is not a pull request number, URL, owner/repo#N, or #N reference")
    })
}

/// What opening a GitHub reference yields once its true type is known — a pull
/// request (with its diff) or an issue (conversation only, no diff).
pub enum Opened {
    Pr {
        handle: PrHandle,
        label: String,
        diff: Diff,
        threads: Vec<Thread>,
    },
    Issue {
        handle: IssueHandle,
        label: String,
        threads: Vec<Thread>,
    },
}

/// Resolve a reference to its true type and fetch it — a pull request (diff +
/// threads) or an issue (its flat conversation). The type is decided by the API
/// ([`GithubClient::resolve_subject`]), never the reference's look.
///
/// Once the subject is resolved, the independent legs of the load run
/// concurrently on scoped threads: for a PR the diff (a `git fetch`), the comment
/// threads (`gh`), and the viewer login (`gh`) share no state and gate on nothing
/// but the resolved PR, so they need not be serial. Error semantics are
/// unchanged: every leg still runs to completion, and the failure surfaced is the
/// one the old serial order would have shown first (the diff, then the threads);
/// no partial [`Opened`] is built unless every leg succeeded.
pub fn fetch_subject(
    dir: PathBuf,
    query: PrQuery,
    progress: &dyn Fn(&str),
) -> Result<Opened, String> {
    let client = GithubClient::new(dir);
    progress("resolving…");
    match client.resolve_subject(&query).map_err(|e| e.to_string())? {
        Subject::Pr(pr) => {
            let label = pr.label();
            // One aggregated progress line: the legs share no progress sink (the
            // callback is not `Sync`, so it cannot be handed to several threads),
            // and one settled message reads better than three racing ones.
            progress(&format!("fetching {label} diff & comments…"));
            let (diff, threads, viewer) = std::thread::scope(|scope| {
                let diff = scope.spawn(|| client.pr_source(&pr).load().map_err(|e| e.to_string()));
                let threads = scope.spawn(|| client.pull(&pr).map_err(|e| e.to_string()));
                let viewer = scope.spawn(|| client.viewer_login().ok());
                (joined(diff), joined(threads), viewer.join().unwrap_or(None))
            });
            let diff = diff?;
            let threads = threads?;
            Ok(Opened::Pr {
                handle: PrHandle { client, pr, viewer },
                label,
                diff,
                threads,
            })
        }
        Subject::Issue(issue) => {
            let label = issue.label();
            progress(&format!("fetching {label} comments…"));
            let (threads, viewer) = std::thread::scope(|scope| {
                let threads = scope.spawn(|| client.pull_issue(&issue).map_err(|e| e.to_string()));
                let viewer = scope.spawn(|| client.viewer_login().ok());
                (joined(threads), viewer.join().unwrap_or(None))
            });
            let threads = threads?;
            Ok(Opened::Issue {
                handle: IssueHandle {
                    client,
                    issue,
                    viewer,
                },
                label,
                threads,
            })
        }
    }
}

/// Flatten a scoped join: a panicking leg becomes an `Err` so a crashed load
/// task fails the load cleanly rather than tearing down the loader thread.
fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T, String>>) -> Result<T, String> {
    handle
        .join()
        .unwrap_or_else(|_| Err("a background load task panicked".to_string()))
}

/// A subject's lifecycle status — a pull request's or an issue's — for the badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectStatus {
    Pr(PrStatus),
    Issue(IssueStatus),
}

impl SubjectStatus {
    /// The badge text.
    pub fn label(self) -> &'static str {
        match self {
            SubjectStatus::Pr(s) => s.label(),
            SubjectStatus::Issue(s) => s.label(),
        }
    }

    /// The lowercase machine-readable status for the control plane — a PR is
    /// `draft`/`open`/`merged`/`closed`, an issue adds `not_planned`.
    pub fn wire(self) -> &'static str {
        match self {
            SubjectStatus::Pr(PrStatus::Draft) => "draft",
            SubjectStatus::Pr(PrStatus::Open) => "open",
            SubjectStatus::Pr(PrStatus::Merged) => "merged",
            SubjectStatus::Pr(PrStatus::Closed) => "closed",
            SubjectStatus::Issue(IssueStatus::Open) => "open",
            SubjectStatus::Issue(IssueStatus::Closed) => "closed",
            SubjectStatus::Issue(IssueStatus::NotPlanned) => "not_planned",
        }
    }
}

/// The facts the Overview tab shows for the subject under review — a pull request
/// or an issue: its status, header facts, and markdown description. A plain
/// snapshot, refreshed on Ctrl-R.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectOverview {
    /// `pr` or `issue` — drives `subject.kind` and the no-diff layout.
    pub kind: SubjectKind,
    pub number: u64,
    pub status: SubjectStatus,
    pub title: String,
    /// The author's login (may be empty when unknown).
    pub author: String,
    /// The base branch — a pull request only (absent for an issue).
    pub base_ref: Option<String>,
    /// The head branch — a pull request only.
    pub head_ref: Option<String>,
    pub created_at: Option<String>,
    /// The terminal timestamp: a PR's merge time, or an issue's close time.
    pub closed_at: Option<String>,
    /// The description (markdown).
    pub body: String,
    /// The canonical URL.
    pub url: String,
}

impl SubjectOverview {
    fn from_pr(pr: &ResolvedPr) -> SubjectOverview {
        SubjectOverview {
            kind: SubjectKind::Pr,
            number: pr.number,
            status: SubjectStatus::Pr(pr.status()),
            title: pr.title.clone(),
            author: pr.author_login().to_string(),
            base_ref: Some(pr.base_ref.clone()),
            head_ref: Some(pr.head_ref.clone()),
            created_at: pr.created_at.clone(),
            closed_at: pr.merged_at.clone(),
            body: pr.body.clone(),
            url: pr.url.clone(),
        }
    }

    fn from_issue(issue: &ResolvedIssue) -> SubjectOverview {
        SubjectOverview {
            kind: SubjectKind::Issue,
            number: issue.number,
            status: SubjectStatus::Issue(issue.status()),
            title: issue.title.clone(),
            author: issue.author.clone(),
            base_ref: None,
            head_ref: None,
            created_at: issue.created_at.clone(),
            closed_at: issue.closed_at.clone(),
            body: issue.body.clone(),
            url: issue.url.clone(),
        }
    }
}

/// A resolved issue plus a client — the no-diff analogue of [`PrHandle`]. An
/// issue has no diff and no review threads; its conversation is a flat comment
/// timeline, and there is no review to submit (a draft posts directly).
pub struct IssueHandle {
    client: GithubClient,
    issue: ResolvedIssue,
    /// The authenticated GitHub login — gates editing/deleting the viewer's own
    /// published comments.
    viewer: Option<String>,
}

impl IssueHandle {
    /// The store key for this issue's drafts, `owner/repo#number` — the same
    /// keyspace as a PR (a number is either a PR or an issue, never both).
    pub fn draft_key(&self) -> String {
        format!("{}#{}", self.issue.slug(), self.issue.number)
    }

    /// The issue number.
    pub fn number(&self) -> u64 {
        self.issue.number
    }

    /// The issue title.
    pub fn title(&self) -> &str {
        &self.issue.title
    }

    /// The canonical issue URL — the page to open, and a published comment's
    /// deep-link base.
    pub fn url(&self) -> &str {
        &self.issue.url
    }

    /// The authenticated GitHub login, when known.
    pub fn viewer(&self) -> Option<&str> {
        self.viewer.as_deref()
    }

    /// The issue overview (status + facts + description), for the header badge and
    /// the Overview tab.
    pub fn overview(&self) -> SubjectOverview {
        SubjectOverview::from_issue(&self.issue)
    }

    /// Re-fetch the issue overview — facts follow a close or a body edit on Ctrl-R.
    pub fn fetch_overview(&self) -> Result<SubjectOverview, String> {
        let fresh = self
            .client
            .refresh_issue(&self.issue)
            .map_err(|e| e.to_string())?;
        Ok(SubjectOverview::from_issue(&fresh))
    }

    /// Re-pull the issue's conversation.
    pub fn pull(&self) -> Result<Vec<Thread>, String> {
        self.client
            .pull_issue(&self.issue)
            .map_err(|e| e.to_string())
    }

    /// Post a new comment to the issue (the send path — an issue has no review to
    /// batch into, so a draft sends directly).
    pub fn create_comment(&self, body: &str) -> Result<Comment, String> {
        self.client
            .create_issue_comment(&self.issue, body)
            .map_err(|e| e.to_string())
    }

    /// Edit a published issue comment on GitHub (the viewer's own only).
    pub fn edit_published(&self, endpoint: CommentEndpoint, body: &str) -> Result<(), String> {
        self.client
            .edit_issue_comment(&self.issue, endpoint, body)
            .map_err(|e| e.to_string())
    }

    /// Delete a published issue comment on GitHub (the viewer's own, confirmed).
    pub fn delete_published(&self, endpoint: CommentEndpoint) -> Result<(), String> {
        self.client
            .delete_issue_comment(&self.issue, endpoint)
            .map_err(|e| e.to_string())
    }

    /// An offline handle for tests (no network calls are made).
    #[cfg(test)]
    pub fn for_test(number: u64, title: &str) -> IssueHandle {
        IssueHandle {
            client: GithubClient::new(std::env::temp_dir()),
            issue: ResolvedIssue {
                owner: "owner".into(),
                repo: "repo".into(),
                number,
                title: title.into(),
                state: "OPEN".into(),
                state_reason: None,
                author: "author".into(),
                created_at: None,
                closed_at: None,
                body: String::new(),
                url: format!("https://github.com/owner/repo/issues/{number}"),
            },
            viewer: Some("tester".into()),
        }
    }
}

/// A resolved pull request plus a client, for syncing back to GitHub.
pub struct PrHandle {
    client: GithubClient,
    pr: ResolvedPr,
    /// The authenticated GitHub login, when known — for gating published-comment
    /// edits/deletes to the viewer's own comments.
    viewer: Option<String>,
}

impl PrHandle {
    /// The store key for this PR's drafts, `owner/repo#number`.
    pub fn pr_key(&self) -> String {
        format!("{}#{}", self.pr.slug(), self.pr.number)
    }

    /// The pull request number (for UI labels; never the private slug).
    pub fn number(&self) -> u64 {
        self.pr.number
    }

    /// The pull request title.
    pub fn title(&self) -> &str {
        &self.pr.title
    }

    /// The canonical pull-request URL — the page to open in a browser, and the
    /// base a published comment's [`CommentEndpoint::anchor`] deep-links onto.
    pub fn url(&self) -> &str {
        &self.pr.url
    }

    /// A handle with an offline client, for tests that only need PR mode plus
    /// the number/title (no network calls are made).
    #[cfg(test)]
    pub fn for_test(number: u64, title: &str) -> PrHandle {
        PrHandle {
            client: GithubClient::new(std::env::temp_dir()),
            pr: ResolvedPr {
                owner: "owner".into(),
                repo: "repo".into(),
                number,
                title: title.into(),
                base_ref: "main".into(),
                head_ref: "feature".into(),
                state: "OPEN".into(),
                is_draft: false,
                merged_at: None,
                merge_commit: None,
                created_at: None,
                author: None,
                body: String::new(),
                url: format!("https://github.com/owner/repo/pull/{number}"),
            },
            viewer: Some("tester".into()),
        }
    }

    /// Like [`for_test`](Self::for_test) but with a chosen viewer login — for
    /// tests that exercise a git-name / GitHub-login mismatch.
    #[cfg(test)]
    pub fn for_test_with_viewer(number: u64, title: &str, viewer: &str) -> PrHandle {
        let mut handle = PrHandle::for_test(number, title);
        handle.viewer = Some(viewer.to_string());
        handle
    }

    /// The authenticated GitHub login, when known.
    pub fn viewer(&self) -> Option<&str> {
        self.viewer.as_deref()
    }

    /// The PR overview (status + facts + description), for the header badge and
    /// the Overview tab — as resolved at load.
    pub fn overview(&self) -> SubjectOverview {
        SubjectOverview::from_pr(&self.pr)
    }

    /// Re-fetch the PR overview from GitHub — the refresh path, so the badge and
    /// the Overview follow a transition (open → merged) or a description edit on
    /// Ctrl-R.
    pub fn fetch_overview(&self) -> Result<SubjectOverview, String> {
        let fresh = self
            .client
            .refresh_pr(&self.pr)
            .map_err(|e| e.to_string())?;
        Ok(SubjectOverview::from_pr(&fresh))
    }

    /// Re-pull the PR's threads from GitHub.
    pub fn pull(&self) -> Result<Vec<Thread>, String> {
        self.client.pull(&self.pr).map_err(|e| e.to_string())
    }

    /// Mark a review thread resolved or unresolved on GitHub (`node_id` is the
    /// thread's GraphQL id, which is the pulled [`Thread::id`]).
    pub fn set_resolved(&self, node_id: &str, resolved: bool) -> Result<(), String> {
        let result = if resolved {
            self.client.resolve_thread(node_id)
        } else {
            self.client.unresolve_thread(node_id)
        };
        result.map_err(|e| e.to_string())
    }

    /// Edit a published comment's body on GitHub. The endpoint carries the id and
    /// the route (inline review comment / issue comment / review summary).
    pub fn edit_published(&self, endpoint: CommentEndpoint, body: &str) -> Result<(), String> {
        self.client
            .edit_comment(&self.pr, endpoint, body)
            .map_err(|e| e.to_string())
    }

    /// Delete a published comment on GitHub (irreversible — confirm first).
    pub fn delete_published(&self, endpoint: CommentEndpoint) -> Result<(), String> {
        self.client
            .delete_comment(&self.pr, endpoint)
            .map_err(|e| e.to_string())
    }

    /// Submit a review (new inline drafts + optional summary/event) and post any
    /// draft replies. Returns the id stamps to apply to the local model.
    pub fn submit(
        &self,
        event: SubmitEvent,
        body: &str,
        threads: &[Thread],
    ) -> Result<Submitted, String> {
        let outcome = self
            .client
            .submit_review(&self.pr, event.into(), body, threads)
            .map_err(|e| e.to_string())?;
        // Stamp the just-published roots onto a working copy before planning
        // replies, so a draft reply under a newly-published root is recognized as
        // a reply to an existing thread — not left behind (the roots publish in
        // the review POST above; without this its children would be orphaned).
        // Only reconciled roots (a known remote id) are stamped here; a root whose
        // id was not read back yet cannot take an in_reply_to reply, so its replies
        // stay drafts until the next pull recovers the id.
        let mut threads = threads.to_vec();
        for (thread_id, remote_id) in &outcome.published {
            if let (Some(remote_id), Some(root)) = (
                remote_id.as_deref(),
                threads
                    .iter_mut()
                    .find(|t| t.id == *thread_id)
                    .and_then(|t| t.comments.first_mut()),
            ) {
                root.remote_id = Some(remote_id.to_string());
            }
        }
        // A draft reply whose root was submitted but not reconciled (its id not
        // read back) has no id to attach to yet — it stays draft this round.
        // Count these so the UI can prompt a refresh-then-resubmit, not go silent.
        let unreconciled: std::collections::HashSet<&str> = outcome
            .published
            .iter()
            .filter(|(_, id)| id.is_none())
            .map(|(tid, _)| tid.as_str())
            .collect();
        let deferred_replies: usize = threads
            .iter()
            .filter(|t| unreconciled.contains(t.id.as_str()))
            .map(|t| t.comments.iter().skip(1).filter(|c| c.is_draft()).count())
            .sum();
        let (replies, failed_replies) = self
            .client
            .submit_replies(&self.pr, &threads)
            .map_err(|e| e.to_string())?;
        // Drafts under the PR conversation (issue comments / review summaries) are
        // posted as new conversation comments, never as inline in_reply_to replies.
        let (conversation, failed_conversation) = self
            .client
            .submit_conversation_comments(&self.pr, &threads)
            .map_err(|e| e.to_string())?;
        let stamp = |r: loopreview_github::ReplyOutcome| ReplyStamp {
            thread_id: r.thread_id,
            comment_id: r.comment_id,
            remote_id: r.comment.remote_id.unwrap_or_default(),
        };
        // A submitted-but-unreconciled root (id not read back) is still marked
        // published so it never re-posts and shows no [draft] badge; the sentinel
        // is a non-numeric placeholder that no edit/delete path mistakes for a real
        // id, and the next pull replaces it with the true remote comment.
        let published = outcome
            .published
            .into_iter()
            .map(|(thread_id, remote_id)| {
                (
                    thread_id,
                    remote_id.unwrap_or_else(|| PENDING_REMOTE_ID.to_string()),
                )
            })
            .collect();
        Ok(Submitted {
            published,
            replies: replies.into_iter().chain(conversation).map(stamp).collect(),
            failed_replies: failed_replies + failed_conversation,
            deferred_replies,
        })
    }
}

/// The placeholder `remote_id` stamped on an inline draft that was submitted but
/// whose created-comment id could not be read back. It marks the comment
/// published (no `[draft]` badge, never re-posted) while being deliberately
/// non-numeric, so every edit/delete path — which parses a real id as `u64` —
/// treats it as unaddressable until the next pull recovers the true id.
pub const PENDING_REMOTE_ID: &str = "pending-unreconciled";

/// The kind of review to submit (UI-side mirror of the crate's event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitEvent {
    Comment,
    Approve,
    RequestChanges,
    Pending,
}

impl From<SubmitEvent> for ReviewEvent {
    fn from(event: SubmitEvent) -> ReviewEvent {
        match event {
            SubmitEvent::Comment => ReviewEvent::Comment,
            SubmitEvent::Approve => ReviewEvent::Approve,
            SubmitEvent::RequestChanges => ReviewEvent::RequestChanges,
            SubmitEvent::Pending => ReviewEvent::Pending,
        }
    }
}

/// What a submit published, for stamping remote ids onto the local model.
pub struct Submitted {
    /// `(local thread id, remote comment id)` for each published inline draft.
    pub published: Vec<(String, String)>,
    /// Published draft replies.
    pub replies: Vec<ReplyStamp>,
    /// How many draft replies failed to post (they stay draft, re-sendable).
    pub failed_replies: usize,
    /// How many draft replies could not be posted this round because their root
    /// was submitted but its id was not read back — they stay draft, and a
    /// refresh-then-resubmit sends them once the root's real id is known.
    pub deferred_replies: usize,
}

/// One published reply's id stamp.
pub struct ReplyStamp {
    pub thread_id: String,
    pub comment_id: String,
    pub remote_id: String,
}

/// Match two anchors by their identifying location, ignoring the `Line` anchor's
/// `commit`/`context` (which can differ between a stale saved draft and a fresh
/// pull of the same comment).
fn same_anchor_location(a: &Anchor, b: &Anchor) -> bool {
    match (a, b) {
        (
            Anchor::Line {
                file: f1,
                side: s1,
                start: st1,
                end: e1,
                ..
            },
            Anchor::Line {
                file: f2,
                side: s2,
                start: st2,
                end: e2,
                ..
            },
        ) => f1 == f2 && s1 == s2 && st1 == st2 && e1 == e2,
        (Anchor::File { file: f1 }, Anchor::File { file: f2 }) => f1 == f2,
        (Anchor::Review, Anchor::Review) => true,
        _ => false,
    }
}

/// Merge local drafts into a freshly-pulled thread list: keep fully-local draft
/// threads, and re-attach draft replies (comments with no remote id) to their
/// published thread by id. Returns the merged threads, the number of stale draft
/// ghosts dropped, and the number of local notes dropped because their published
/// thread was deleted on GitHub (so the UI can say so rather than lose them
/// silently).
pub fn merge_drafts(previous: &Review, fresh: Vec<Thread>) -> (Vec<Thread>, usize, usize) {
    let mut result = fresh;
    let mut cleaned = 0usize;
    let mut orphans = 0usize;
    for old in &previous.threads {
        let root_published = old.root().is_some_and(|c| c.remote_id.is_some());
        if !root_published {
            // Defense against a pre-F2 store: a saved "local draft" thread that
            // duplicates a pulled published thread (same anchored location and the
            // same root body) is a stale ghost — a published comment a buggy build
            // once wrote back as a draft. Drop it so it does not reappear as a
            // [draft] and risk a duplicate submit. A genuine new draft is affected
            // only if its body exactly matches a published comment on the same
            // line, which would merely avoid posting an identical duplicate.
            let is_ghost = old.root().is_some_and(|old_root| {
                result.iter().any(|t| {
                    t.root().is_some_and(|r| {
                        r.remote_id.is_some()
                            && r.body == old_root.body
                            && same_anchor_location(&t.anchor, &old.anchor)
                    })
                })
            });
            if is_ghost {
                cleaned += 1;
            } else {
                // A thread the user started locally — keep it as-is.
                result.push(old.clone());
            }
            continue;
        }
        // A published thread: carry its draft/local notes over onto the fresh
        // thread. A normally-published thread keeps the GraphQL node id it was
        // pulled under, so it matches by id. A locally-created thread whose root was
        // submitted but left id-pending (a `PENDING_REMOTE_ID` sentinel) has a
        // *local* id that won't match the fresh pull's GraphQL id — so re-home its
        // notes by anchor + root body instead. Without this the draft reply that the
        // "refresh and submit again" prompt tells the user to send would be lost on
        // the very refresh it asks for (a data-loss self-contradiction).
        let root_pending =
            old.root().and_then(|c| c.remote_id.as_deref()) == Some(PENDING_REMOTE_ID);
        let drafts: Vec<_> = old
            .comments
            .iter()
            .filter(|c| c.remote_id.is_none())
            .cloned()
            .collect();
        if !drafts.is_empty() {
            let target = result.iter().position(|t| t.id == old.id).or_else(|| {
                root_pending
                    .then(|| {
                        let old_body = old.root().map(|c| c.body.as_str());
                        result.iter().position(|t| {
                            same_anchor_location(&t.anchor, &old.anchor)
                                && t.root().map(|c| c.body.as_str()) == old_body
                        })
                    })
                    .flatten()
            });
            match target {
                Some(i) => result[i].comments.extend(drafts),
                // No fresh thread by id (nor by anchor+body for a sentinel): the
                // root is gone from GitHub. The notes have nowhere to live — dropped
                // and counted so the refresh reports it rather than losing it silently.
                None => orphans += drafts.len(),
            }
        }
    }
    (result, cleaned, orphans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopreview_core::{Anchor, Comment, Side, ThreadState};

    fn comment(id: &str, remote: Option<&str>) -> Comment {
        Comment {
            id: id.to_string(),
            author: "me".to_string(),
            body: "b".to_string(),
            created_at: 0,
            remote_id: remote.map(str::to_string),
            kind: loopreview_core::CommentKind::Draft,
        }
    }

    fn thread(id: &str, comments: Vec<Comment>) -> Thread {
        thread_at(id, Anchor::line("f", Side::New, 1), comments)
    }

    fn thread_at(id: &str, anchor: Anchor, comments: Vec<Comment>) -> Thread {
        Thread {
            id: id.to_string(),
            anchor,
            state: ThreadState::Open,
            comments,
        }
    }

    #[test]
    fn query_requires_input_without_detect() {
        assert!(query(None, false).is_err());
        assert_eq!(query(None, true), Ok(PrQuery::Detect));
        assert!(query(Some("42".into()), false).is_ok());
        assert!(query(Some("nonsense".into()), false).is_err());
    }

    #[test]
    fn merge_keeps_local_threads_and_reattaches_draft_replies() {
        let previous = Review {
            threads: vec![
                // A published thread that gained a draft reply.
                thread(
                    "T1",
                    vec![comment("c1", Some("r1")), comment("c2-draft", None)],
                ),
                // A genuinely local draft — a distinct line, not a copy of a pull.
                thread_at(
                    "local",
                    Anchor::line("other.rs", Side::New, 9),
                    vec![comment("l1", None)],
                ),
            ],
        };
        // A fresh pull returns the published thread (without the draft reply).
        let fresh = vec![thread("T1", vec![comment("c1", Some("r1"))])];

        let (merged, cleaned, orphans) = merge_drafts(&previous, fresh);
        assert_eq!(cleaned, 0, "nothing stale to clean");
        assert_eq!(orphans, 0, "the published thread is still there");
        assert_eq!(merged.len(), 2);
        let t1 = merged.iter().find(|t| t.id == "T1").unwrap();
        assert_eq!(t1.comments.len(), 2, "draft reply re-attached");
        assert!(merged.iter().any(|t| t.id == "local"), "local thread kept");
    }

    #[test]
    fn merge_reports_local_notes_orphaned_by_a_deleted_remote_thread() {
        // A published thread carried a local note, but the fresh pull no longer
        // has it — the thread was deleted on GitHub. The note has no home; it is
        // dropped and counted so the refresh can say so, not lose it silently.
        let previous = Review {
            threads: vec![thread(
                "T1",
                vec![comment("root", Some("r1")), comment("note", None)],
            )],
        };
        let fresh: Vec<Thread> = Vec::new(); // the thread is gone remotely

        let (merged, cleaned, orphans) = merge_drafts(&previous, fresh);
        assert_eq!(cleaned, 0);
        assert_eq!(orphans, 1, "the orphaned local note is counted");
        assert!(merged.is_empty(), "and not carried into the merged review");
    }

    #[test]
    fn merge_rehomes_a_sentinel_threads_reply_onto_the_fresh_thread() {
        // A locally-submitted root left id-pending (sentinel) with a draft reply.
        // On pull the real published root arrives under a *different* (GraphQL) id,
        // so the id match misses — the reply must be re-homed by anchor+body, not
        // dropped (that would lose the very reply "refresh and submit again" wants).
        let previous = Review {
            threads: vec![thread_at(
                "local-id",
                Anchor::line("f", Side::New, 1),
                vec![
                    comment("root", Some(PENDING_REMOTE_ID)),
                    comment("reply", None),
                ],
            )],
        };
        let fresh = vec![thread_at(
            "PRRT_real",
            Anchor::line("f", Side::New, 1),
            vec![comment("c-real", Some("999"))],
        )];

        let (merged, cleaned, orphans) = merge_drafts(&previous, fresh);
        assert_eq!((cleaned, orphans), (0, 0), "nothing lost");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "PRRT_real", "kept the fresh thread");
        assert_eq!(
            merged[0].comments.len(),
            2,
            "the draft reply re-homed under the now-published root"
        );
        assert!(
            merged[0].comments[1].remote_id.is_none(),
            "the reply is still a draft (kind preserved)"
        );
        // The reply now sits under a root that carries a real numeric id — exactly
        // the shape plan_replies needs, so a resubmit will send it.
        assert!(
            merged[0].root().unwrap().remote_id.as_deref() == Some("999"),
            "under a real-id published root"
        );
    }

    #[test]
    fn merge_drops_a_stale_published_ghost_saved_as_a_draft() {
        // A pre-F2 build saved a published comment as a "local draft": same line
        // and same body as the pulled published thread. It must be dropped, not
        // reappear as a [draft] (which would risk a duplicate submit).
        let previous = Review {
            threads: vec![thread("ghost", vec![comment("g", None)])],
        };
        // The real published thread on the same line, same body ("b"), arrives fresh.
        let fresh = vec![thread("T9", vec![comment("real", Some("r9"))])];

        let (merged, cleaned, _orphans) = merge_drafts(&previous, fresh);
        assert_eq!(cleaned, 1, "the ghost draft was cleaned");
        assert_eq!(merged.len(), 1, "only the real published thread remains");
        assert!(
            merged.iter().all(|t| t.root().unwrap().remote_id.is_some()),
            "no draft ghost survives"
        );
    }
}
