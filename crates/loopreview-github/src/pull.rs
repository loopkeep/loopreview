//! Mapping GitHub's review data onto loopreview's [`Thread`] model (the pull
//! direction of the sync).
//!
//! Three GitHub sources are folded into one flat list of threads, mirroring how
//! GitHub itself stores them:
//!
//! * **review threads** (GraphQL `reviewThreads`) — inline, line-anchored
//!   conversations. Each becomes one [`Thread`] with an [`Anchor::Line`] (or
//!   [`Anchor::File`] for a file-level or unplaceable outdated thread), carrying
//!   the resolved state, the original commit, and the diff hunk as its context
//!   snippet.
//! * **issue comments** (the PR conversation) — each becomes a single-comment
//!   [`Thread`] anchored at [`Anchor::Review`].
//! * **submitted reviews** — each non-empty summary body becomes a single-comment
//!   [`Thread`] anchored at [`Anchor::Review`].
//!
//! Every remote id is preserved: a comment's `databaseId` becomes its
//! [`Comment::remote_id`], and a review thread's node id becomes its
//! [`Thread::id`] (the token the resolve/unresolve mutations need). All the
//! functions here are pure — they take already-deserialized responses — so they
//! are unit-tested against fixtures shaped like real `gh` output.

use loopreview_core::{Anchor, Comment, CommentKind, Side, Thread, ThreadState};
use serde::Deserialize;

use crate::error::GithubError;

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

/// The `reviewThreads` GraphQL response for one pull request.
#[derive(Debug, Deserialize)]
struct ReviewThreadsResponse {
    data: ReviewThreadsData,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsData {
    repository: Option<ReviewThreadsRepo>,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsRepo {
    #[serde(rename = "pullRequest")]
    pull_request: Option<ReviewThreadsPr>,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsPr {
    #[serde(rename = "reviewThreads")]
    review_threads: ReviewThreadNodes,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadNodes {
    nodes: Vec<ReviewThread>,
}

/// One inline review thread (GraphQL `PullRequestReviewThread`).
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewThread {
    /// The GraphQL node id — the token `resolveReviewThread` needs. Becomes the
    /// mapped [`Thread::id`].
    pub id: String,
    /// Whether the thread has been marked resolved.
    #[serde(rename = "isResolved", default)]
    pub is_resolved: bool,
    /// The file the thread is anchored to.
    #[serde(default)]
    pub path: Option<String>,
    /// The current line the thread points at (null once outdated).
    #[serde(default)]
    pub line: Option<u32>,
    /// The line at the original commit (used when `line` is null).
    #[serde(rename = "originalLine", default)]
    pub original_line: Option<u32>,
    /// The first line of a multi-line thread (null for a single-line thread).
    #[serde(rename = "startLine", default)]
    pub start_line: Option<u32>,
    /// The first line at the original commit (used when `startLine` is null).
    #[serde(rename = "originalStartLine", default)]
    pub original_start_line: Option<u32>,
    /// `LEFT` or `RIGHT` — which side of the diff the thread sits on.
    #[serde(rename = "diffSide", default)]
    pub diff_side: Option<String>,
    /// The side of the thread's start line (falls back for the anchor side).
    #[serde(rename = "startDiffSide", default)]
    pub start_diff_side: Option<String>,
    /// `LINE` or `FILE`.
    #[serde(rename = "subjectType", default)]
    pub subject_type: Option<String>,
    /// The thread's comments, oldest first.
    pub comments: ReviewCommentNodes,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReviewCommentNodes {
    nodes: Vec<ReviewThreadComment>,
}

/// One comment within a review thread (GraphQL `PullRequestReviewComment`).
#[derive(Debug, Clone, Deserialize)]
struct ReviewThreadComment {
    #[serde(rename = "databaseId")]
    database_id: Option<u64>,
    #[serde(default)]
    body: String,
    #[serde(rename = "createdAt", default)]
    created_at: String,
    #[serde(rename = "diffHunk", default)]
    diff_hunk: String,
    #[serde(rename = "originalCommit", default)]
    original_commit: Option<CommitOid>,
    #[serde(default)]
    author: Option<Actor>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitOid {
    #[serde(default)]
    oid: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Actor {
    #[serde(default)]
    login: String,
}

/// A PR-conversation (issue) comment (`GET /repos/{o}/{r}/issues/{n}/comments`).
#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    #[serde(default, rename = "user")]
    user: Option<Actor>,
}

/// A submitted PR review (`GET /repos/{o}/{r}/pulls/{n}/reviews`).
#[derive(Debug, Clone, Deserialize)]
pub struct SubmittedReview {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default, rename = "user")]
    user: Option<Actor>,
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the `reviewThreads` GraphQL response into its thread nodes.
///
/// A missing repository or pull request (a permissions or 404 case) yields an
/// empty list rather than an error, matching how GitHub returns `null` data.
pub(crate) fn parse_review_threads(json: &str) -> Result<Vec<ReviewThread>, GithubError> {
    let resp: ReviewThreadsResponse =
        serde_json::from_str(json).map_err(|e| GithubError::parse("review threads", e))?;
    Ok(resp
        .data
        .repository
        .and_then(|r| r.pull_request)
        .map(|pr| pr.review_threads.nodes)
        .unwrap_or_default())
}

/// Parse the issue-comments REST response.
pub(crate) fn parse_issue_comments(json: &str) -> Result<Vec<IssueComment>, GithubError> {
    serde_json::from_str(json).map_err(|e| GithubError::parse("issue comments", e))
}

/// Parse the submitted-reviews REST response.
pub(crate) fn parse_reviews(json: &str) -> Result<Vec<SubmittedReview>, GithubError> {
    serde_json::from_str(json).map_err(|e| GithubError::parse("PR reviews", e))
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

/// Fold all three GitHub sources into one flat list of threads.
///
/// Inline (line-anchored) threads come first in GitHub's own order, then the
/// conversation — issue comments and non-empty review summaries — sorted oldest
/// first so it reads as a timeline.
pub(crate) fn build_threads(
    review_threads: &[ReviewThread],
    issue_comments: &[IssueComment],
    reviews: &[SubmittedReview],
) -> Vec<Thread> {
    let mut threads: Vec<Thread> = review_threads.iter().map(map_review_thread).collect();

    let mut conversation: Vec<Thread> = Vec::new();
    conversation.extend(issue_comments.iter().map(map_issue_comment));
    conversation.extend(reviews.iter().filter_map(map_review_summary));
    // Stable sort keeps issue/review insertion order among equal timestamps.
    conversation.sort_by_key(conversation_created_at);

    threads.extend(conversation);
    threads
}

/// The root comment's creation time, for ordering the conversation timeline.
fn conversation_created_at(thread: &Thread) -> u64 {
    thread.comments.first().map(|c| c.created_at).unwrap_or(0)
}

/// Map one inline review thread onto a [`Thread`].
pub(crate) fn map_review_thread(thread: &ReviewThread) -> Thread {
    let comments: Vec<Comment> = thread
        .comments
        .nodes
        .iter()
        .enumerate()
        .map(|(index, comment)| map_thread_comment(comment, &thread.id, index))
        .collect();

    let anchor = thread_anchor(thread);
    let state = if thread.is_resolved {
        ThreadState::Resolved
    } else {
        ThreadState::Open
    };

    Thread {
        id: thread.id.clone(),
        anchor,
        state,
        comments,
    }
}

/// Derive the anchor for an inline thread.
///
/// A `FILE` subject, or a thread whose current and original lines are both
/// missing (a fully unplaceable outdated thread), anchors on the file. Anything
/// else anchors on the resolved line, preferring the current line and falling
/// back to the original line for outdated threads. The original commit and diff
/// hunk are taken from the root comment so the outdated-display path has a
/// snippet to reconstruct from.
fn thread_anchor(thread: &ReviewThread) -> Anchor {
    let Some(path) = thread.path.clone() else {
        return Anchor::Review;
    };

    if thread.subject_type.as_deref() == Some("FILE") {
        return Anchor::File { file: path };
    }

    let line = thread.line.or(thread.original_line).filter(|&l| l > 0);
    let Some(end) = line else {
        // Outdated with no resolvable line: keep it visible on the file.
        return Anchor::File { file: path };
    };

    // A multi-line thread reports its first line; single-line threads report the
    // same as the last line. Clamp the start below the end defensively.
    let start = thread
        .start_line
        .or(thread.original_start_line)
        .filter(|&l| l > 0)
        .unwrap_or(end)
        .min(end);

    let side = match thread
        .diff_side
        .as_deref()
        .or(thread.start_diff_side.as_deref())
    {
        Some("LEFT") => Side::Old,
        _ => Side::New,
    };

    let root = thread.comments.nodes.first();
    let commit = root
        .and_then(|c| c.original_commit.as_ref())
        .map(|c| c.oid.clone())
        .filter(|s| !s.is_empty());
    let context = root
        .map(|c| snippet_lines(&c.diff_hunk))
        .unwrap_or_default();

    Anchor::Line {
        file: path,
        side,
        start,
        end,
        commit,
        context,
    }
}

/// Split a diff hunk into its lines for storage as a context snippet.
fn snippet_lines(diff_hunk: &str) -> Vec<String> {
    if diff_hunk.is_empty() {
        Vec::new()
    } else {
        diff_hunk.lines().map(str::to_string).collect()
    }
}

/// Map one review-thread comment onto a [`Comment`], preserving its remote id.
///
/// Every pulled comment is [`CommentKind::Published`]: it already lives on
/// GitHub and must never be treated as a draft to re-post. GitHub occasionally
/// reports a null `databaseId` (a comment with no addressable id); such a comment
/// carries no `remote_id`, so it is keyed by a synthetic id unique within the
/// thread (`{thread_id}#{index}`) rather than a shared `"unknown"` that would
/// collide. Being `Published`, it is still never a send target.
fn map_thread_comment(comment: &ReviewThreadComment, thread_id: &str, index: usize) -> Comment {
    let remote_id = comment.database_id.map(|id| id.to_string());
    Comment {
        id: remote_id
            .clone()
            .unwrap_or_else(|| format!("{thread_id}#{index}")),
        author: actor_login(&comment.author),
        body: comment.body.clone(),
        created_at: iso8601_to_epoch(&comment.created_at),
        remote_id,
        kind: CommentKind::Published,
    }
}

/// Map one issue (conversation) comment onto a single-comment Review thread.
pub(crate) fn map_issue_comment(comment: &IssueComment) -> Thread {
    let remote_id = comment.id.to_string();
    Thread {
        id: format!("issuecomment:{}", comment.id),
        anchor: Anchor::Review,
        state: ThreadState::Open,
        comments: vec![Comment {
            id: remote_id.clone(),
            author: actor_login(&comment.user),
            body: comment.body.clone(),
            created_at: iso8601_to_epoch(&comment.created_at),
            remote_id: Some(remote_id),
            kind: CommentKind::Published,
        }],
    }
}

/// Map one submitted review's summary body onto a Review thread.
///
/// Returns [`None`] for reviews with an empty body (a bare approve/request with
/// no prose is a state change, not a comment).
pub(crate) fn map_review_summary(review: &SubmittedReview) -> Option<Thread> {
    if review.body.trim().is_empty() {
        return None;
    }
    let remote_id = review.id.to_string();
    Some(Thread {
        id: format!("review:{}", review.id),
        anchor: Anchor::Review,
        state: ThreadState::Open,
        comments: vec![Comment {
            id: remote_id.clone(),
            author: actor_login(&review.user),
            body: review.body.clone(),
            created_at: review
                .submitted_at
                .as_deref()
                .map(iso8601_to_epoch)
                .unwrap_or(0),
            remote_id: Some(remote_id),
            kind: CommentKind::Published,
        }],
    })
}

/// The login of an actor, or an empty string when GitHub returned a null author
/// (a deleted user).
fn actor_login(actor: &Option<Actor>) -> String {
    actor.as_ref().map(|a| a.login.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Time parsing
// ---------------------------------------------------------------------------

/// Parse an ISO-8601 UTC timestamp like `2026-07-21T15:24:08Z` into epoch
/// seconds, clamped to zero. Returns `0` when it does not match that basic shape
/// (comment ordering degrades gracefully rather than failing the whole pull).
pub(crate) fn iso8601_to_epoch(s: &str) -> u64 {
    parse_iso8601(s).map(|secs| secs.max(0) as u64).unwrap_or(0)
}

/// Parse an ISO-8601 UTC timestamp into signed epoch seconds, or `None` when it
/// does not match the basic `YYYY-MM-DDTHH:MM:SS` shape.
fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Reject an out-of-range time so a malformed stamp degrades to `0` rather than
    // silently skewing the ordering. A leap second (60) is tolerated.
    if !(0..=23).contains(&hour) || !(0..=59).contains(&min) || !(0..=60).contains(&sec) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + min * 60 + sec)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixture shaped exactly like `gh api graphql` output for reviewThreads,
    // with one open inline thread (a root plus a reply) and one resolved,
    // outdated thread whose current line is gone.
    const REVIEW_THREADS_JSON: &str = r#"{
      "data": {
        "repository": {
          "pullRequest": {
            "reviewThreads": {
              "nodes": [
                {
                  "id": "PRRT_open1",
                  "isResolved": false,
                  "isOutdated": false,
                  "path": "src/lib.rs",
                  "line": 42,
                  "originalLine": 40,
                  "diffSide": "RIGHT",
                  "subjectType": "LINE",
                  "comments": {
                    "nodes": [
                      {
                        "databaseId": 1001,
                        "body": "This looks off.",
                        "createdAt": "2026-07-21T15:24:08Z",
                        "diffHunk": "@@ -40,3 +40,4 @@\n context\n+added",
                        "originalCommit": { "oid": "abc123" },
                        "author": { "login": "octocat" }
                      },
                      {
                        "databaseId": 1002,
                        "body": "Agreed, will fix.",
                        "createdAt": "2026-07-21T16:00:00Z",
                        "diffHunk": "@@ -40,3 +40,4 @@\n context\n+added",
                        "originalCommit": { "oid": "abc123" },
                        "author": { "login": "hubber" }
                      }
                    ]
                  }
                },
                {
                  "id": "PRRT_outdated",
                  "isResolved": true,
                  "isOutdated": true,
                  "path": "src/old.rs",
                  "line": null,
                  "originalLine": 12,
                  "diffSide": "LEFT",
                  "subjectType": "LINE",
                  "comments": {
                    "nodes": [
                      {
                        "databaseId": 2001,
                        "body": "Historic note.",
                        "createdAt": "2026-07-20T09:00:00Z",
                        "diffHunk": "@@ -10,4 +10,3 @@\n-removed",
                        "originalCommit": { "oid": "def456" },
                        "author": { "login": "octocat" }
                      }
                    ]
                  }
                }
              ]
            }
          }
        }
      }
    }"#;

    #[test]
    fn parses_and_maps_review_threads() {
        let threads = parse_review_threads(REVIEW_THREADS_JSON).unwrap();
        assert_eq!(threads.len(), 2);

        let open = map_review_thread(&threads[0]);
        assert_eq!(open.id, "PRRT_open1");
        assert_eq!(open.state, ThreadState::Open);
        assert_eq!(open.comments.len(), 2);
        assert_eq!(open.comments[0].remote_id.as_deref(), Some("1001"));
        assert_eq!(open.comments[0].author, "octocat");
        assert_eq!(open.comments[1].body, "Agreed, will fix.");
        // Newer reply sorts after the root by created_at.
        assert!(open.comments[1].created_at > open.comments[0].created_at);

        match &open.anchor {
            Anchor::Line {
                file,
                side,
                start,
                end,
                commit,
                context,
            } => {
                assert_eq!(file, "src/lib.rs");
                assert_eq!(*side, Side::New);
                assert_eq!((*start, *end), (42, 42));
                assert_eq!(commit.as_deref(), Some("abc123"));
                // The diff hunk is preserved as the context snippet, line by line.
                assert_eq!(context.len(), 3);
                assert_eq!(context[0], "@@ -40,3 +40,4 @@");
            }
            other => panic!("expected line anchor, got {other:?}"),
        }
    }

    #[test]
    fn multi_line_thread_restores_its_range() {
        let json = r#"{
          "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [
            {
              "id": "PRRT_range",
              "isResolved": false,
              "path": "src/lib.rs",
              "line": 48,
              "originalLine": 46,
              "startLine": 44,
              "originalStartLine": 42,
              "diffSide": "RIGHT",
              "startDiffSide": "RIGHT",
              "subjectType": "LINE",
              "comments": { "nodes": [
                { "databaseId": 3001, "body": "range note",
                  "createdAt": "2026-07-21T15:24:08Z",
                  "diffHunk": "@@ -44,5 +44,5 @@",
                  "originalCommit": { "oid": "aaa" },
                  "author": { "login": "octocat" } }
              ]}
            }
          ]}}}}
        }"#;
        let threads = parse_review_threads(json).unwrap();
        match &map_review_thread(&threads[0]).anchor {
            Anchor::Line {
                side, start, end, ..
            } => {
                assert_eq!(*side, Side::New);
                assert_eq!((*start, *end), (44, 48), "the multi-line range is restored");
            }
            other => panic!("expected a line anchor, got {other:?}"),
        }
    }

    #[test]
    fn outdated_thread_falls_back_to_original_line_on_left_side() {
        let threads = parse_review_threads(REVIEW_THREADS_JSON).unwrap();
        let outdated = map_review_thread(&threads[1]);
        assert_eq!(outdated.state, ThreadState::Resolved);
        match &outdated.anchor {
            Anchor::Line {
                side, start, end, ..
            } => {
                assert_eq!(*side, Side::Old);
                assert_eq!((*start, *end), (12, 12));
            }
            other => panic!("expected line anchor on original line, got {other:?}"),
        }
    }

    #[test]
    fn file_subject_maps_to_file_anchor() {
        let json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[
          {"id":"PRRT_file","isResolved":false,"isOutdated":false,"path":"README.md",
           "line":null,"originalLine":null,"diffSide":"RIGHT","subjectType":"FILE",
           "comments":{"nodes":[{"databaseId":3001,"body":"whole-file note",
             "createdAt":"2026-07-19T00:00:00Z","diffHunk":"","originalCommit":null,
             "author":{"login":"octocat"}}]}}
        ]}}}}}"#;
        let threads = parse_review_threads(json).unwrap();
        let t = map_review_thread(&threads[0]);
        assert_eq!(
            t.anchor,
            Anchor::File {
                file: "README.md".to_string()
            }
        );
    }

    #[test]
    fn missing_repository_yields_no_threads() {
        let json = r#"{"data":{"repository":null}}"#;
        assert!(parse_review_threads(json).unwrap().is_empty());
    }

    #[test]
    fn maps_issue_comments_and_review_summaries() {
        let issues = r#"[
          {"id":5001,"body":"General thoughts.","created_at":"2026-07-21T10:00:00Z","user":{"login":"octocat"}}
        ]"#;
        let reviews = r#"[
          {"id":6001,"body":"LGTM overall.","submitted_at":"2026-07-21T11:00:00Z","user":{"login":"hubber"}},
          {"id":6002,"body":"","submitted_at":"2026-07-21T11:05:00Z","user":{"login":"botrev"}}
        ]"#;
        let issue_comments = parse_issue_comments(issues).unwrap();
        let submitted = parse_reviews(reviews).unwrap();

        let threads = build_threads(&[], &issue_comments, &submitted);
        // The empty-body review is dropped; issue comment + non-empty review kept.
        assert_eq!(threads.len(), 2);
        assert!(threads.iter().all(|t| t.anchor == Anchor::Review));
        // Sorted oldest first: issue comment (10:00) then review (11:00).
        assert_eq!(threads[0].comments[0].body, "General thoughts.");
        assert_eq!(threads[0].comments[0].remote_id.as_deref(), Some("5001"));
        assert_eq!(threads[1].comments[0].body, "LGTM overall.");
        assert_eq!(threads[1].id, "review:6001");
    }

    #[test]
    fn build_threads_orders_inline_before_conversation() {
        let review_threads = parse_review_threads(REVIEW_THREADS_JSON).unwrap();
        let issues = parse_issue_comments(
            r#"[{"id":1,"body":"hi","created_at":"2026-07-21T10:00:00Z","user":{"login":"x"}}]"#,
        )
        .unwrap();
        let all = build_threads(&review_threads, &issues, &[]);
        assert_eq!(all.len(), 3);
        // Inline threads first, then the Review-anchored conversation.
        assert!(matches!(all[0].anchor, Anchor::Line { .. }));
        assert_eq!(all[2].anchor, Anchor::Review);
    }

    #[test]
    fn null_database_id_comments_are_published_not_drafts() {
        // GitHub can report a null databaseId. Such comments must never look like
        // a local draft (which would re-post them on the next submit) and must not
        // collide on a shared "unknown" id.
        let json = r#"{
          "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [
            {
              "id": "PRRT_nullids",
              "isResolved": false,
              "path": "src/lib.rs",
              "line": 5,
              "originalLine": 5,
              "diffSide": "RIGHT",
              "subjectType": "LINE",
              "comments": { "nodes": [
                { "databaseId": null, "body": "root without an id",
                  "createdAt": "2026-07-21T15:24:08Z", "diffHunk": "@@ -5 +5 @@",
                  "originalCommit": { "oid": "aaa" }, "author": { "login": "octocat" } },
                { "databaseId": null, "body": "reply without an id",
                  "createdAt": "2026-07-21T16:00:00Z", "diffHunk": "@@ -5 +5 @@",
                  "originalCommit": { "oid": "aaa" }, "author": { "login": "hubber" } }
              ]}
            }
          ]}}}}
        }"#;
        let threads = parse_review_threads(json).unwrap();
        let thread = map_review_thread(&threads[0]);

        // Neither comment is a draft, so nothing is ever queued to submit.
        assert!(thread.comments.iter().all(|c| !c.is_draft()));
        assert!(
            thread
                .comments
                .iter()
                .all(|c| c.disposition() == CommentKind::Published)
        );
        // Synthetic ids are unique within the thread, not a shared "unknown".
        assert_ne!(thread.comments[0].id, thread.comments[1].id);
        assert!(thread.comments[0].id.starts_with("PRRT_nullids#"));

        // The plan is empty: no inline comment and no reply is produced for it.
        assert!(crate::push::plan_inline_comments(std::slice::from_ref(&thread)).is_empty());
        assert!(crate::push::plan_replies(std::slice::from_ref(&thread)).is_empty());
    }

    #[test]
    fn parses_iso8601_epoch() {
        assert_eq!(parse_iso8601("2024-04-25T19:55:42Z"), Some(1714074942));
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert!(parse_iso8601("not-a-date").is_none());
        assert!(parse_iso8601("2024-13-01T00:00:00Z").is_none());
        // An out-of-range time component is rejected, not folded in.
        assert!(parse_iso8601("2024-04-25T24:00:00Z").is_none());
        assert!(parse_iso8601("2024-04-25T19:60:42Z").is_none());
        assert!(parse_iso8601("2024-04-25T19:55:99Z").is_none());
        // A leap second is accepted.
        assert!(parse_iso8601("2024-06-30T23:59:60Z").is_some());
        // Pre-epoch times clamp to zero for the unsigned model field.
        assert_eq!(iso8601_to_epoch("1969-01-01T00:00:00Z"), 0);
    }
}
