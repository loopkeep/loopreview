//! loopreview-github: GitHub pull-request sync for loopreview, a review-first
//! diff TUI.
//!
//! This crate is the bridge between a GitHub pull request and loopreview-core's
//! review model. It does three things:
//!
//! * **resolves** a pull request from a number, URL, `owner/repo#N`, or the
//!   current branch (`--detect`) — see [`PrQuery`] and [`GithubClient::resolve_pr`];
//! * **shows** its diff through a [`DiffSource`](loopreview_core::DiffSource)
//!   that never checks the PR out — see [`PrSource`];
//! * **syncs comments** both ways: [`GithubClient::pull`] maps GitHub's review
//!   threads, conversation, and review summaries onto core [`Thread`]s, and the
//!   push methods ([`submit_review`](GithubClient::submit_review),
//!   [`reply`](GithubClient::reply),
//!   [`resolve_thread`](GithubClient::resolve_thread),
//!   [`unresolve_thread`](GithubClient::unresolve_thread)) send local drafts and
//!   resolutions back.
//!
//! Every GitHub call is a `gh` CLI subprocess: authentication and rate limiting
//! are `gh`'s job, and this crate never handles a token. The pure mapping,
//! query-building, and response-parsing logic lives in [`pull`] and [`push`] and
//! is unit-tested against fixtures; only [`GithubClient`] performs I/O.

mod cmd;
mod error;
mod git;
mod pr;
mod pull;
mod push;
mod source;

use std::path::{Path, PathBuf};

use loopreview_core::{Comment, Thread};

pub use error::GithubError;
pub use pr::{PrQuery, PrRef, ResolvedPr, parse_pr_query};
pub use push::{ReviewCommentInput, ReviewEvent};
pub use source::PrSource;

/// The outcome of submitting a review: the drafts that were published, updated
/// with the remote ids GitHub assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// GitHub's id for the created review.
    pub review_id: u64,
    /// `(local thread id, remote comment id)` for each inline draft that was
    /// published, so the caller can stamp `remote_id` onto its stored model.
    pub published: Vec<(String, String)>,
}

/// The outcome of posting one draft reply: which local comment was published,
/// and the resulting remote [`Comment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyOutcome {
    /// The local id of the thread the reply belongs to.
    pub thread_id: String,
    /// The local id of the draft comment that was posted.
    pub comment_id: String,
    /// The published comment, with its `remote_id` set.
    pub comment: Comment,
}

/// A GitHub client scoped to one local repository working directory.
///
/// The directory is the `cwd` for every `gh` and `git` invocation, which is how
/// `gh` infers the default repository and how the diff fetches find the remote.
pub struct GithubClient {
    dir: PathBuf,
}

impl GithubClient {
    /// Create a client that runs `gh`/`git` inside `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> GithubClient {
        GithubClient { dir: dir.into() }
    }

    /// The repository directory `gh`/`git` run in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // -- PR resolution ------------------------------------------------------

    /// Resolve a [`PrQuery`] into a fully-populated [`ResolvedPr`].
    ///
    /// For [`PrQuery::Detect`] the pull request for the current branch is looked
    /// up; for a [`PrQuery::Ref`] the number is used directly, honouring an
    /// explicit `owner/repo` when the query carried one.
    pub fn resolve_pr(&self, query: &PrQuery) -> Result<ResolvedPr, GithubError> {
        match query {
            PrQuery::Detect => self.resolve_detected(),
            PrQuery::Ref(pr_ref) => self.resolve_ref(pr_ref),
        }
    }

    /// Resolve the pull request for the branch currently checked out.
    fn resolve_detected(&self) -> Result<ResolvedPr, GithubError> {
        let out = cmd::run(
            "gh",
            &["pr", "view", "--json", &pr_view_fields()],
            &self.dir,
            None,
        )?;
        if !out.ok() {
            return Err(GithubError::NoPrForBranch);
        }
        let mut pr: ResolvedPr =
            serde_json::from_str(&out.stdout).map_err(|e| GithubError::parse("gh pr view", e))?;
        self.fill_repo_slug(&mut pr, None)?;
        Ok(pr)
    }

    /// Resolve an explicit reference, using its `owner/repo` when present.
    fn resolve_ref(&self, pr_ref: &PrRef) -> Result<ResolvedPr, GithubError> {
        let number = pr_ref.number.to_string();
        let slug = match (&pr_ref.owner, &pr_ref.repo) {
            (Some(owner), Some(repo)) => Some(format!("{owner}/{repo}")),
            _ => None,
        };

        let mut args = vec!["pr", "view", &number, "--json"];
        let fields = pr_view_fields();
        args.push(&fields);
        if let Some(slug) = &slug {
            args.push("--repo");
            args.push(slug);
        }

        let out = cmd::run("gh", &args, &self.dir, None)?;
        if !out.ok() {
            return Err(GithubError::PrNotFound {
                query: describe_ref(pr_ref),
            });
        }
        let mut pr: ResolvedPr =
            serde_json::from_str(&out.stdout).map_err(|e| GithubError::parse("gh pr view", e))?;
        self.fill_repo_slug(&mut pr, Some(pr_ref))?;
        Ok(pr)
    }

    /// Populate `owner`/`repo` on a resolved PR — from the explicit reference
    /// when it carried them, otherwise from the current repository.
    fn fill_repo_slug(
        &self,
        pr: &mut ResolvedPr,
        pr_ref: Option<&PrRef>,
    ) -> Result<(), GithubError> {
        if let Some(pr_ref) = pr_ref
            && let (Some(owner), Some(repo)) = (&pr_ref.owner, &pr_ref.repo)
        {
            pr.owner = owner.clone();
            pr.repo = repo.clone();
            return Ok(());
        }
        let (owner, repo) = self.current_repo_slug()?;
        pr.owner = owner;
        pr.repo = repo;
        Ok(())
    }

    /// `owner/repo` for the repository at the client's directory.
    fn current_repo_slug(&self) -> Result<(String, String), GithubError> {
        let out = cmd::run_ok(
            "gh",
            &[
                "repo",
                "view",
                "--json",
                "owner,name",
                "-q",
                ".owner.login + \"/\" + .name",
            ],
            &self.dir,
            None,
        )?;
        let slug = out.trim();
        slug.split_once('/')
            .map(|(o, r)| (o.to_string(), r.to_string()))
            .ok_or_else(|| GithubError::Command {
                program: "gh".to_string(),
                code: 0,
                stderr: format!("unexpected repository slug from `gh repo view`: {slug:?}"),
            })
    }

    /// A [`PrSource`] for this pull request's diff.
    pub fn pr_source(&self, pr: &ResolvedPr) -> PrSource {
        PrSource::new(self.dir.clone(), pr.base_ref.clone(), pr.number)
    }

    // -- Pull ---------------------------------------------------------------

    /// Fetch every comment thread for `pr` and map it onto the core model.
    ///
    /// Three GitHub sources are folded into one flat thread list: inline review
    /// threads (with resolved state, original commit, and diff-hunk snippet),
    /// the PR conversation (issue comments), and non-empty review summaries. See
    /// [`pull`] for the mapping. Review threads are read up to a fixed cap of 100
    /// threads x 100 comments (no pagination).
    pub fn pull(&self, pr: &ResolvedPr) -> Result<Vec<Thread>, GithubError> {
        let review_threads = self.fetch_review_threads(pr)?;
        let issue_comments = self.fetch_issue_comments(pr)?;
        let reviews = self.fetch_reviews(pr)?;
        Ok(pull::build_threads(
            &review_threads,
            &issue_comments,
            &reviews,
        ))
    }

    /// Fetch the inline review threads via GraphQL.
    fn fetch_review_threads(
        &self,
        pr: &ResolvedPr,
    ) -> Result<Vec<pull::ReviewThread>, GithubError> {
        let number = pr.number.to_string();
        let out = cmd::run_ok(
            "gh",
            &[
                "api",
                "graphql",
                "-f",
                &format!("query={REVIEW_THREADS_QUERY}"),
                "-F",
                &format!("owner={}", pr.owner),
                "-F",
                &format!("repo={}", pr.repo),
                "-F",
                &format!("number={number}"),
            ],
            &self.dir,
            None,
        )?;
        pull::parse_review_threads(&out)
    }

    /// Fetch the PR conversation (issue) comments via REST.
    fn fetch_issue_comments(
        &self,
        pr: &ResolvedPr,
    ) -> Result<Vec<pull::IssueComment>, GithubError> {
        let path = format!(
            "repos/{}/{}/issues/{}/comments",
            pr.owner, pr.repo, pr.number
        );
        let out = cmd::run_ok("gh", &["api", "--paginate", &path], &self.dir, None)?;
        pull::parse_issue_comments(&out)
    }

    /// Fetch the submitted reviews via REST.
    fn fetch_reviews(&self, pr: &ResolvedPr) -> Result<Vec<pull::SubmittedReview>, GithubError> {
        let path = format!("repos/{}/{}/pulls/{}/reviews", pr.owner, pr.repo, pr.number);
        let out = cmd::run_ok("gh", &["api", "--paginate", &path], &self.dir, None)?;
        pull::parse_reviews(&out)
    }

    // -- Push ---------------------------------------------------------------

    /// Submit a review for `pr` in a single request: every new inline draft
    /// thread in `threads` becomes a `comments[]` entry, alongside the summary
    /// `body` and `event`.
    ///
    /// After the POST the created comments are read back and matched to the
    /// drafts that produced them, so the returned [`SubmitOutcome`] carries the
    /// `(thread id, remote comment id)` pairs the caller stamps onto its store —
    /// the model is updated by return value, never written here. Draft replies
    /// and resolutions are handled by [`reply`](Self::reply) and
    /// [`resolve_thread`](Self::resolve_thread) respectively.
    pub fn submit_review(
        &self,
        pr: &ResolvedPr,
        event: ReviewEvent,
        body: &str,
        threads: &[Thread],
    ) -> Result<SubmitOutcome, GithubError> {
        let planned = push::plan_inline_comments(threads);
        let inputs: Vec<ReviewCommentInput> = planned.iter().map(|p| p.input.clone()).collect();

        let payload = serde_json::to_string(&push::ReviewPayload {
            event: event.as_api(),
            body,
            comments: &inputs,
        })
        .map_err(|e| GithubError::parse("review payload", e))?;

        let path = format!("repos/{}/{}/pulls/{}/reviews", pr.owner, pr.repo, pr.number);
        let out = cmd::run_ok(
            "gh",
            &["api", "-X", "POST", &path, "--input", "-"],
            &self.dir,
            Some(&payload),
        )?;
        let review_id = parse_created_id(&out, "created review")?;

        // Reconcile the created comment ids back onto the drafts. A pending
        // review or one with no inline comments has nothing to read back.
        let published = if inputs.is_empty() {
            Vec::new()
        } else {
            let created = self.fetch_review_comments(pr, review_id)?;
            push::match_created_comments(&planned, &created)
        };

        Ok(SubmitOutcome {
            review_id,
            published,
        })
    }

    /// Read the comments a submitted review created, to match remote ids.
    fn fetch_review_comments(
        &self,
        pr: &ResolvedPr,
        review_id: u64,
    ) -> Result<Vec<push::CreatedComment>, GithubError> {
        let path = format!(
            "repos/{}/{}/pulls/{}/reviews/{}/comments",
            pr.owner, pr.repo, pr.number, review_id
        );
        let out = cmd::run_ok("gh", &["api", "--paginate", &path], &self.dir, None)?;
        serde_json::from_str(&out).map_err(|e| GithubError::parse("review comments", e))
    }

    /// Post every draft reply in `threads` and return one [`ReplyOutcome`] per
    /// posted comment.
    ///
    /// A draft reply is a comment with no `remote_id` that follows an
    /// already-published thread root; each is posted individually with
    /// `in_reply_to` (line info is not needed, so outdated and resolved threads
    /// take replies too). The model is updated by the returned values — the
    /// caller stamps each `remote_id` onto its store.
    pub fn submit_replies(
        &self,
        pr: &ResolvedPr,
        threads: &[Thread],
    ) -> Result<Vec<ReplyOutcome>, GithubError> {
        let planned = push::plan_replies(threads);
        let mut outcomes = Vec::with_capacity(planned.len());
        for reply in planned {
            let comment = self.reply(pr, reply.in_reply_to, &reply.body)?;
            outcomes.push(ReplyOutcome {
                thread_id: reply.thread_id,
                comment_id: reply.comment_id,
                comment,
            });
        }
        Ok(outcomes)
    }

    /// Post a threaded reply to an existing review thread and return the created
    /// [`Comment`], its `remote_id` set.
    ///
    /// `in_reply_to` is the remote id of the thread's root comment (the numeric
    /// `databaseId` a pulled comment carries in `remote_id`). No line info is
    /// needed, so this works on outdated and resolved threads.
    pub fn reply(
        &self,
        pr: &ResolvedPr,
        in_reply_to: u64,
        body: &str,
    ) -> Result<Comment, GithubError> {
        let payload = serde_json::to_string(&push::ReplyPayload { body, in_reply_to })
            .map_err(|e| GithubError::parse("reply payload", e))?;
        let path = format!(
            "repos/{}/{}/pulls/{}/comments",
            pr.owner, pr.repo, pr.number
        );
        let out = cmd::run_ok(
            "gh",
            &["api", "-X", "POST", &path, "--input", "-"],
            &self.dir,
            Some(&payload),
        )?;
        parse_created_comment(&out, body)
    }

    /// Mark a review thread resolved (GraphQL `resolveReviewThread`).
    ///
    /// `thread_node_id` is the GraphQL node id a pulled thread carries as its
    /// [`Thread::id`].
    pub fn resolve_thread(&self, thread_node_id: &str) -> Result<(), GithubError> {
        self.set_thread_resolution(thread_node_id, true)
    }

    /// Mark a review thread unresolved (GraphQL `unresolveReviewThread`).
    pub fn unresolve_thread(&self, thread_node_id: &str) -> Result<(), GithubError> {
        self.set_thread_resolution(thread_node_id, false)
    }

    fn set_thread_resolution(
        &self,
        thread_node_id: &str,
        resolved: bool,
    ) -> Result<(), GithubError> {
        let query = if resolved {
            RESOLVE_THREAD_MUTATION
        } else {
            UNRESOLVE_THREAD_MUTATION
        };
        cmd::run_ok(
            "gh",
            &[
                "api",
                "graphql",
                "-f",
                &format!("query={query}"),
                "-f",
                &format!("threadId={thread_node_id}"),
            ],
            &self.dir,
            None,
        )?;
        Ok(())
    }
}

/// The `--json` field set requested from `gh pr view`.
fn pr_view_fields() -> String {
    "number,title,baseRefName,headRefName,state,url".to_string()
}

/// A human description of a PR reference, for a not-found message.
fn describe_ref(pr_ref: &PrRef) -> String {
    match (&pr_ref.owner, &pr_ref.repo) {
        (Some(owner), Some(repo)) => format!("{owner}/{repo}#{}", pr_ref.number),
        _ => format!("#{}", pr_ref.number),
    }
}

/// Parse the `{ "id": N }` a create endpoint returns.
fn parse_created_id(json: &str, context: &str) -> Result<u64, GithubError> {
    #[derive(serde::Deserialize)]
    struct Created {
        id: u64,
    }
    let created: Created =
        serde_json::from_str(json).map_err(|e| GithubError::parse(context.to_string(), e))?;
    Ok(created.id)
}

/// Parse a created review comment (from a reply POST) into a core [`Comment`].
fn parse_created_comment(json: &str, fallback_body: &str) -> Result<Comment, GithubError> {
    #[derive(serde::Deserialize)]
    struct Created {
        id: u64,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        user: Option<CreatedUser>,
    }
    #[derive(serde::Deserialize)]
    struct CreatedUser {
        #[serde(default)]
        login: String,
    }
    let created: Created =
        serde_json::from_str(json).map_err(|e| GithubError::parse("reply comment", e))?;
    let remote_id = created.id.to_string();
    Ok(Comment {
        id: remote_id.clone(),
        author: created.user.map(|u| u.login).unwrap_or_default(),
        body: created.body.unwrap_or_else(|| fallback_body.to_string()),
        created_at: created
            .created_at
            .as_deref()
            .map(pull::iso8601_to_epoch)
            .unwrap_or(0),
        remote_id: Some(remote_id),
    })
}

/// The GraphQL query for a PR's inline review threads. Both the thread cap and
/// the per-thread comment cap are 100; pagination is not implemented (a very
/// large PR would truncate — see the crate docs).
const REVIEW_THREADS_QUERY: &str = r#"query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){
    pullRequest(number:$number){
      reviewThreads(first:100){
        nodes{
          id
          isResolved
          path
          line
          originalLine
          startLine
          originalStartLine
          diffSide
          startDiffSide
          subjectType
          comments(first:100){
            nodes{
              databaseId
              body
              createdAt
              diffHunk
              originalCommit{ oid }
              author{ login }
            }
          }
        }
      }
    }
  }
}"#;

/// The GraphQL mutation to resolve a review thread by node id.
const RESOLVE_THREAD_MUTATION: &str = r#"mutation($threadId:ID!){
  resolveReviewThread(input:{threadId:$threadId}){ thread{ id isResolved } }
}"#;

/// The GraphQL mutation to unresolve a review thread by node id.
const UNRESOLVE_THREAD_MUTATION: &str = r#"mutation($threadId:ID!){
  unresolveReviewThread(input:{threadId:$threadId}){ thread{ id isResolved } }
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_ref_with_and_without_slug() {
        assert_eq!(
            describe_ref(&PrRef {
                owner: Some("o".into()),
                repo: Some("r".into()),
                number: 7,
                is_issue: false,
            }),
            "o/r#7"
        );
        assert_eq!(
            describe_ref(&PrRef {
                owner: None,
                repo: None,
                number: 7,
                is_issue: false,
            }),
            "#7"
        );
    }

    #[test]
    fn parses_created_id() {
        assert_eq!(
            parse_created_id(r#"{"id":12345}"#, "review").unwrap(),
            12345
        );
        assert!(parse_created_id("not json", "review").is_err());
    }

    #[test]
    fn parses_created_reply_comment() {
        let json = r#"{
            "id": 777,
            "body": "thanks!",
            "created_at": "2026-07-21T12:00:00Z",
            "user": { "login": "octocat" }
        }"#;
        let comment = parse_created_comment(json, "fallback").unwrap();
        assert_eq!(comment.remote_id.as_deref(), Some("777"));
        assert_eq!(comment.id, "777");
        assert_eq!(comment.author, "octocat");
        assert_eq!(comment.body, "thanks!");
        assert!(comment.created_at > 0);
        assert!(!comment.is_draft());
    }

    #[test]
    fn created_reply_falls_back_to_sent_body() {
        // Some responses omit fields; the sent body is used as a fallback.
        let comment = parse_created_comment(r#"{"id":9}"#, "my text").unwrap();
        assert_eq!(comment.body, "my text");
        assert_eq!(comment.author, "");
    }

    #[test]
    fn pr_view_fields_are_stable() {
        assert_eq!(
            pr_view_fields(),
            "number,title,baseRefName,headRefName,state,url"
        );
    }
}
