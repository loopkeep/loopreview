//! The bridge between the review UI and the `loopreview-github` crate.
//!
//! This is the only cli module that knows about GitHub: it builds a PR query,
//! fetches a PR's diff and threads, and — once a PR is open — syncs resolutions
//! and submits reviews. The UI holds an opaque [`PrHandle`] and calls these
//! methods on background threads; everything returns plain data or a string
//! error so the rest of the UI stays GitHub-agnostic.

use std::path::PathBuf;

use loopreview_core::{Diff, DiffSource, Review, Thread};
use loopreview_github::{GithubClient, PrQuery, ResolvedPr, ReviewEvent};

/// Build a [`PrQuery`] from the CLI arguments, or an error message.
pub fn query(text: Option<String>, detect: bool) -> Result<PrQuery, String> {
    if detect {
        return Ok(PrQuery::Detect);
    }
    let text = text.ok_or(
        "give a pull request (number, URL, or owner/repo#N), or pass --detect".to_string(),
    )?;
    PrQuery::parse(&text).ok_or_else(|| {
        format!("`{text}` is not a pull request number, URL, or owner/repo#N reference")
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
    Ok((PrHandle { client, pr }, label, diff, threads))
}

/// A resolved pull request plus a client, for syncing back to GitHub.
pub struct PrHandle {
    client: GithubClient,
    pr: ResolvedPr,
}

impl PrHandle {
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
        let replies = self
            .client
            .submit_replies(&self.pr, threads)
            .map_err(|e| e.to_string())?;
        Ok(Submitted {
            published: outcome.published,
            replies: replies
                .into_iter()
                .map(|r| ReplyStamp {
                    thread_id: r.thread_id,
                    comment_id: r.comment_id,
                    remote_id: r.comment.remote_id.unwrap_or_default(),
                })
                .collect(),
        })
    }
}

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
}

/// One published reply's id stamp.
pub struct ReplyStamp {
    pub thread_id: String,
    pub comment_id: String,
    pub remote_id: String,
}

/// Merge local drafts into a freshly-pulled thread list: keep fully-local draft
/// threads, and re-attach draft replies (comments with no remote id) to their
/// published thread by id.
pub fn merge_drafts(previous: &Review, fresh: Vec<Thread>) -> Vec<Thread> {
    let mut result = fresh;
    for old in &previous.threads {
        let root_published = old.root().is_some_and(|c| c.remote_id.is_some());
        if !root_published {
            // A thread the user started locally — keep it as-is.
            result.push(old.clone());
            continue;
        }
        // A published thread: carry over any draft replies it accumulated.
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
    result
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
        }
    }

    fn thread(id: &str, comments: Vec<Comment>) -> Thread {
        Thread {
            id: id.to_string(),
            anchor: Anchor::line("f", Side::New, 1),
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
                // A fully-local draft thread.
                thread("local", vec![comment("l1", None)]),
            ],
        };
        // A fresh pull returns the published thread (without the draft reply).
        let fresh = vec![thread("T1", vec![comment("c1", Some("r1"))])];

        let merged = merge_drafts(&previous, fresh);
        assert_eq!(merged.len(), 2);
        let t1 = merged.iter().find(|t| t.id == "T1").unwrap();
        assert_eq!(t1.comments.len(), 2, "draft reply re-attached");
        assert!(merged.iter().any(|t| t.id == "local"), "local thread kept");
    }
}
