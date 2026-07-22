//! Mapping loopreview drafts onto GitHub write requests (the push direction).
//!
//! Three shapes of local change are pushed differently, matching GitHub's own
//! API surface:
//!
//! * **new inline threads** — draft threads anchored to a line — are submitted
//!   together as one review (`POST /pulls/{n}/reviews`) carrying a `comments[]`
//!   array, an optional summary `body`, and an `event`
//!   (comment / approve / request-changes / pending).
//! * **draft replies** — a draft comment appended to an already-published thread
//!   — are posted individually with `in_reply_to` (no line info needed, so they
//!   work on outdated and resolved threads).
//! * **resolve / unresolve** — a GraphQL mutation on the thread's node id.
//!
//! The planning here (which threads become comments, how a [`Side`] maps to
//! GitHub's `RIGHT`/`LEFT`, and how the created-comment ids are matched back to
//! the drafts that produced them) is pure and unit-tested; the actual `gh` calls
//! live in [`crate::GithubClient`].

use loopreview_core::{Anchor, Side, Thread};
use serde::{Deserialize, Serialize};

/// What kind of review to submit alongside a batch of inline comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    /// A plain comment review (`COMMENT`).
    Comment,
    /// Approve the pull request (`APPROVE`).
    Approve,
    /// Request changes (`REQUEST_CHANGES`).
    RequestChanges,
    /// Leave the review pending/unsubmitted (empty event).
    Pending,
}

impl ReviewEvent {
    /// The GitHub `event` string; empty for a pending review.
    pub fn as_api(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Pending => "",
        }
    }
}

/// GitHub's side string for a diff [`Side`]: `RIGHT` for the new side, `LEFT`
/// for the old side.
pub(crate) fn api_side(side: Side) -> &'static str {
    match side {
        Side::New => "RIGHT",
        Side::Old => "LEFT",
    }
}

/// One inline comment in a review-submission payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewCommentInput {
    /// The file the comment is on.
    pub path: String,
    /// The last line on `side` the comment anchors to (GitHub's `line`).
    pub line: u64,
    /// `RIGHT` (new side) or `LEFT` (old side).
    pub side: String,
    /// The first line of a multi-line comment (GitHub's `start_line`); `None`
    /// for a single-line comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    /// The side of `start_line` (GitHub's `start_side`); `None` for single-line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    /// The comment body.
    pub body: String,
}

/// A planned inline comment paired with the draft thread that produced it, so
/// the created remote id can be matched back after the POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedComment {
    /// The local id of the draft thread this comment came from.
    pub thread_id: String,
    /// The request payload for this comment.
    pub input: ReviewCommentInput,
}

/// The full payload for `POST /pulls/{n}/reviews`.
#[derive(Debug, Serialize)]
pub(crate) struct ReviewPayload<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    pub event: &'a str,
    pub body: &'a str,
    pub comments: &'a [ReviewCommentInput],
}

/// The payload for a threaded reply (`POST /pulls/{n}/comments`).
#[derive(Debug, Serialize)]
pub(crate) struct ReplyPayload<'a> {
    pub body: &'a str,
    pub in_reply_to: u64,
}

/// The payload for editing a comment's body (`PATCH .../comments/{id}`).
#[derive(Debug, Serialize)]
pub(crate) struct BodyPayload<'a> {
    pub body: &'a str,
}

/// The REST endpoint path for editing or deleting one published comment by its
/// numeric id. An inline review comment and a PR conversation (issue) comment
/// live under different collections, so `review` picks between them.
pub(crate) fn comment_endpoint(owner: &str, repo: &str, id: u64, review: bool) -> String {
    let kind = if review { "pulls" } else { "issues" };
    format!("repos/{owner}/{repo}/{kind}/comments/{id}")
}

/// Decide which draft threads become inline review comments.
///
/// A thread contributes exactly one comment when it is a brand-new inline draft:
/// anchored to a line, with a root comment that has not been published yet
/// (`remote_id` is `None`). A multi-line anchor (`start != end`) carries GitHub's
/// `start_line` / `start_side` alongside the last-line `line` / `side`; a
/// single-line anchor omits them. File- and review-anchored drafts are not inline
/// comments and are skipped here.
pub(crate) fn plan_inline_comments(threads: &[Thread]) -> Vec<PlannedComment> {
    let mut planned = Vec::new();
    for thread in threads {
        let Some(root) = thread.comments.first() else {
            continue;
        };
        if !root.is_draft() {
            continue;
        }
        let Anchor::Line {
            file,
            side,
            start,
            end,
            ..
        } = &thread.anchor
        else {
            continue;
        };
        let (start_line, start_side) = if start != end {
            (Some(*start as u64), Some(api_side(*side).to_string()))
        } else {
            (None, None)
        };
        planned.push(PlannedComment {
            thread_id: thread.id.clone(),
            input: ReviewCommentInput {
                path: file.clone(),
                line: *end as u64,
                side: api_side(*side).to_string(),
                start_line,
                start_side,
                body: root.body.clone(),
            },
        });
    }
    planned
}

/// A draft reply to post against an already-published thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedReply {
    /// The local id of the thread being replied to.
    pub thread_id: String,
    /// The local id of the draft comment being posted.
    pub comment_id: String,
    /// The remote comment id to reply under (`in_reply_to`).
    pub in_reply_to: u64,
    /// The reply body.
    pub body: String,
}

/// Decide which draft comments are replies to existing GitHub threads.
///
/// A reply is a draft comment (no `remote_id`) that follows a published root
/// (the thread's first comment carries a numeric `remote_id`). Each such draft
/// is posted with `in_reply_to` set to that root's remote id.
pub(crate) fn plan_replies(threads: &[Thread]) -> Vec<PlannedReply> {
    let mut replies = Vec::new();
    for thread in threads {
        let Some(root) = thread.comments.first() else {
            continue;
        };
        // Only threads that already exist on GitHub can take a threaded reply.
        let Some(in_reply_to) = root
            .remote_id
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok())
        else {
            continue;
        };
        for comment in thread.comments.iter().skip(1) {
            if comment.is_draft() {
                replies.push(PlannedReply {
                    thread_id: thread.id.clone(),
                    comment_id: comment.id.clone(),
                    in_reply_to,
                    body: comment.body.clone(),
                });
            }
        }
    }
    replies
}

/// One comment as returned by `GET /pulls/{n}/reviews/{id}/comments` after a
/// review is submitted, used to reconcile remote ids back onto the drafts.
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedComment {
    /// The new comment's REST id (its `remote_id`).
    pub id: u64,
    /// The file the comment landed on.
    #[serde(default)]
    pub path: String,
    /// `RIGHT` or `LEFT`, when GitHub reports it.
    #[serde(default)]
    pub side: Option<String>,
    /// The comment's current line.
    #[serde(default)]
    pub line: Option<u64>,
    /// The comment's original line (used when `line` is null).
    #[serde(default)]
    pub original_line: Option<u64>,
}

/// Match created review comments back to the drafts that produced them.
///
/// Returns `(thread_id, remote_id)` pairs. Matching is on `(path, side, line)`
/// and consumes each planned comment once, so duplicate anchors are paired in
/// order. A created comment with no matching plan (e.g. one authored elsewhere)
/// is skipped.
pub(crate) fn match_created_comments(
    planned: &[PlannedComment],
    created: &[CreatedComment],
) -> Vec<(String, String)> {
    let mut used = vec![false; planned.len()];
    let mut out = Vec::new();
    for c in created {
        let c_side = c.side.as_deref().unwrap_or("RIGHT");
        let c_line = c.line.or(c.original_line);
        for (i, p) in planned.iter().enumerate() {
            if used[i] {
                continue;
            }
            if p.input.path == c.path && p.input.side == c_side && Some(p.input.line) == c_line {
                used[i] = true;
                out.push((p.thread_id.clone(), c.id.to_string()));
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopreview_core::{Comment, ThreadState};

    fn draft_comment(id: &str, body: &str) -> Comment {
        Comment {
            id: id.to_string(),
            author: "me".to_string(),
            body: body.to_string(),
            created_at: 0,
            remote_id: None,
            kind: loopreview_core::CommentKind::Draft,
        }
    }

    fn published_comment(id: &str, remote: &str, body: &str) -> Comment {
        Comment {
            id: id.to_string(),
            author: "me".to_string(),
            body: body.to_string(),
            created_at: 0,
            remote_id: Some(remote.to_string()),
            kind: loopreview_core::CommentKind::Draft,
        }
    }

    fn line_thread(id: &str, side: Side, line: u32, root: Comment) -> Thread {
        Thread {
            id: id.to_string(),
            anchor: Anchor::Line {
                file: "src/a.rs".to_string(),
                side,
                start: line,
                end: line,
                commit: None,
                context: Vec::new(),
            },
            state: ThreadState::Open,
            comments: vec![root],
        }
    }

    #[test]
    fn event_strings_match_github() {
        assert_eq!(ReviewEvent::Comment.as_api(), "COMMENT");
        assert_eq!(ReviewEvent::Approve.as_api(), "APPROVE");
        assert_eq!(ReviewEvent::RequestChanges.as_api(), "REQUEST_CHANGES");
        assert_eq!(ReviewEvent::Pending.as_api(), "");
    }

    #[test]
    fn comment_endpoint_picks_the_collection() {
        // An inline review comment vs a PR conversation (issue) comment.
        assert_eq!(
            comment_endpoint("o", "r", 42, true),
            "repos/o/r/pulls/comments/42"
        );
        assert_eq!(
            comment_endpoint("o", "r", 42, false),
            "repos/o/r/issues/comments/42"
        );
    }

    #[test]
    fn edit_payload_is_just_the_body() {
        let json = serde_json::to_string(&BodyPayload { body: "revised" }).unwrap();
        assert_eq!(json, r#"{"body":"revised"}"#);
    }

    #[test]
    fn plans_only_new_inline_drafts() {
        let new_draft = line_thread("t1", Side::New, 10, draft_comment("c1", "fix this"));
        let old_side = line_thread("t2", Side::Old, 5, draft_comment("c2", "on old"));
        let published = line_thread(
            "t3",
            Side::New,
            7,
            published_comment("c3", "999", "already up"),
        );
        let review_anchored = Thread {
            id: "t4".to_string(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![draft_comment("c4", "conversation draft")],
        };

        let planned = plan_inline_comments(&[new_draft, old_side, published, review_anchored]);
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].thread_id, "t1");
        assert_eq!(planned[0].input.side, "RIGHT");
        assert_eq!(planned[0].input.line, 10);
        assert_eq!(planned[0].input.body, "fix this");
        // A single-line comment omits the multi-line range fields.
        assert_eq!(planned[0].input.start_line, None);
        assert_eq!(planned[0].input.start_side, None);
        assert_eq!(planned[1].input.side, "LEFT");
    }

    #[test]
    fn multi_line_anchor_carries_start_line_and_side() {
        let thread = Thread {
            id: "t".to_string(),
            anchor: Anchor::Line {
                file: "src/a.rs".to_string(),
                side: Side::New,
                start: 10,
                end: 14,
                commit: None,
                context: Vec::new(),
            },
            state: ThreadState::Open,
            comments: vec![draft_comment("c", "range note")],
        };
        let planned = plan_inline_comments(&[thread]);
        assert_eq!(planned[0].input.line, 14);
        assert_eq!(planned[0].input.side, "RIGHT");
        assert_eq!(planned[0].input.start_line, Some(10));
        assert_eq!(planned[0].input.start_side, Some("RIGHT".to_string()));
        // The range fields serialize under GitHub's names.
        let json = serde_json::to_string(&planned[0].input).unwrap();
        assert!(json.contains("\"start_line\":10"));
        assert!(json.contains("\"start_side\":\"RIGHT\""));
    }

    #[test]
    fn plans_replies_to_published_threads_only() {
        let published = Thread {
            id: "t1".to_string(),
            anchor: Anchor::Review,
            state: ThreadState::Open,
            comments: vec![
                published_comment("c1", "500", "root"),
                draft_comment("c2", "my reply"),
            ],
        };
        // A fully-draft thread has no published root to reply under.
        let all_draft = line_thread("t2", Side::New, 3, draft_comment("c3", "brand new"));

        let replies = plan_replies(&[published, all_draft]);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].thread_id, "t1");
        assert_eq!(replies[0].comment_id, "c2");
        assert_eq!(replies[0].in_reply_to, 500);
        assert_eq!(replies[0].body, "my reply");
    }

    #[test]
    fn a_reply_becomes_plannable_once_its_root_is_published() {
        // A draft root plus a draft reply. Before publishing there is nothing to
        // reply under, so the reply is not planned (and would be orphaned if the
        // root published in the review POST without re-planning).
        let mut thread = line_thread("t", Side::New, 4, draft_comment("root", "new root"));
        thread.comments.push(draft_comment("reply", "under it"));
        assert!(
            plan_replies(&[thread.clone()]).is_empty(),
            "a draft root's reply has nothing to attach to yet"
        );
        // Once submit() stamps the root's new remote id before planning replies,
        // the reply attaches to it — not left behind.
        thread.comments[0].remote_id = Some("777".to_string());
        let planned = plan_replies(&[thread]);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].comment_id, "reply");
        assert_eq!(planned[0].in_reply_to, 777);
    }

    #[test]
    fn matches_created_comments_back_to_drafts() {
        let planned = vec![
            PlannedComment {
                thread_id: "t1".to_string(),
                input: ReviewCommentInput {
                    path: "src/a.rs".to_string(),
                    line: 10,
                    side: "RIGHT".to_string(),
                    start_line: None,
                    start_side: None,
                    body: "one".to_string(),
                },
            },
            PlannedComment {
                thread_id: "t2".to_string(),
                input: ReviewCommentInput {
                    path: "src/a.rs".to_string(),
                    line: 5,
                    side: "LEFT".to_string(),
                    start_line: None,
                    start_side: None,
                    body: "two".to_string(),
                },
            },
        ];
        let created = vec![
            CreatedComment {
                id: 8001,
                path: "src/a.rs".to_string(),
                side: Some("LEFT".to_string()),
                line: Some(5),
                original_line: None,
            },
            CreatedComment {
                id: 8002,
                path: "src/a.rs".to_string(),
                side: Some("RIGHT".to_string()),
                line: Some(10),
                original_line: None,
            },
        ];
        let matched = match_created_comments(&planned, &created);
        // Order follows the created list; each draft paired to its remote id.
        assert_eq!(
            matched,
            vec![
                ("t2".to_string(), "8001".to_string()),
                ("t1".to_string(), "8002".to_string()),
            ]
        );
    }

    #[test]
    fn review_payload_omits_empty_event() {
        let comments: Vec<ReviewCommentInput> = Vec::new();
        let pending = ReviewPayload {
            event: ReviewEvent::Pending.as_api(),
            body: "wip",
            comments: &comments,
        };
        let json = serde_json::to_string(&pending).unwrap();
        assert!(!json.contains("event"));

        let commented = ReviewPayload {
            event: ReviewEvent::Comment.as_api(),
            body: "done",
            comments: &comments,
        };
        assert!(
            serde_json::to_string(&commented)
                .unwrap()
                .contains("\"event\":\"COMMENT\"")
        );
    }
}
