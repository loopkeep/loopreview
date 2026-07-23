//! The bridge between the review UI and the `loopreview-github` crate.
//!
//! This is the only cli module that knows about GitHub: it builds a PR query,
//! fetches a PR's diff and threads, and — once a PR is open — syncs resolutions
//! and submits reviews. The UI holds an opaque [`PrHandle`] and calls these
//! methods on background threads; everything returns plain data or a string
//! error so the rest of the UI stays GitHub-agnostic.

use std::path::PathBuf;

use loopreview_core::{Anchor, Diff, DiffSource, Review, Thread};
use loopreview_github::{GithubClient, PrQuery, ResolvedPr, ReviewEvent};

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

/// Resolve a PR and fetch its diff and threads, reporting progress. Returns a
/// handle for later syncs, the header label, the diff, and the pulled threads.
pub fn fetch(
    dir: PathBuf,
    query: PrQuery,
    progress: &dyn Fn(&str),
) -> Result<(PrHandle, String, Diff, Vec<Thread>), String> {
    let client = GithubClient::new(dir);
    progress("resolving pull request…");
    let pr = client.resolve_pr(&query).map_err(|e| e.to_string())?;
    let label = pr.label();
    progress(&format!("fetching {label} diff…"));
    let diff = client.pr_source(&pr).load().map_err(|e| e.to_string())?;
    progress("fetching comments…");
    let threads = client.pull(&pr).map_err(|e| e.to_string())?;
    // The viewer's login gates editing/deleting their own published comments.
    // Best-effort: a failure here just means those affordances stay off.
    let viewer = client.viewer_login().ok();
    Ok((PrHandle { client, pr, viewer }, label, diff, threads))
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
                url: String::new(),
            },
            viewer: Some("tester".into()),
        }
    }

    /// The authenticated GitHub login, when known.
    pub fn viewer(&self) -> Option<&str> {
        self.viewer.as_deref()
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

    /// Edit a published comment's body on GitHub. `review` selects the inline
    /// review-comment endpoint over the PR conversation one.
    pub fn edit_published(&self, remote_id: u64, review: bool, body: &str) -> Result<(), String> {
        self.client
            .edit_comment(&self.pr, remote_id, review, body)
            .map_err(|e| e.to_string())
    }

    /// Delete a published comment on GitHub (irreversible — confirm first).
    pub fn delete_published(&self, remote_id: u64, review: bool) -> Result<(), String> {
        self.client
            .delete_comment(&self.pr, remote_id, review)
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
/// published thread by id. Returns the merged threads and the number of stale
/// draft ghosts dropped (see below).
pub fn merge_drafts(previous: &Review, fresh: Vec<Thread>) -> (Vec<Thread>, usize) {
    let mut result = fresh;
    let mut cleaned = 0usize;
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
        // A published thread: carry over any draft replies it accumulated,
        // matching the fresh thread by id. A thread published normally keeps the
        // GraphQL node id it was pulled under, so this matches. The exception is a
        // locally-created thread whose root was submitted but left id-pending (its
        // root carries the `PENDING_REMOTE_ID` sentinel): its id is a local id, not
        // the GraphQL node id the fresh pull carries, so no fresh thread matches
        // and any draft reply under it is intentionally dropped here — it was never
        // sent (a reply cannot attach to an unreconciled root) and the real thread
        // arrives fresh from the pull. This is a rare, accepted edge.
        let drafts: Vec<_> = old
            .comments
            .iter()
            .filter(|c| c.remote_id.is_none())
            .cloned()
            .collect();
        if !drafts.is_empty()
            && let Some(fresh_thread) = result.iter_mut().find(|t| t.id == old.id)
        {
            fresh_thread.comments.extend(drafts);
        }
    }
    (result, cleaned)
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

        let (merged, cleaned) = merge_drafts(&previous, fresh);
        assert_eq!(cleaned, 0, "nothing stale to clean");
        assert_eq!(merged.len(), 2);
        let t1 = merged.iter().find(|t| t.id == "T1").unwrap();
        assert_eq!(t1.comments.len(), 2, "draft reply re-attached");
        assert!(merged.iter().any(|t| t.id == "local"), "local thread kept");
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

        let (merged, cleaned) = merge_drafts(&previous, fresh);
        assert_eq!(cleaned, 1, "the ghost draft was cleaned");
        assert_eq!(merged.len(), 1, "only the real published thread remains");
        assert!(
            merged.iter().all(|t| t.root().unwrap().remote_id.is_some()),
            "no draft ghost survives"
        );
    }
}
