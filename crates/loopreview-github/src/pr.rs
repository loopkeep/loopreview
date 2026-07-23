//! Resolving a pull request from what the user typed.
//!
//! A `lr pr` invocation can name a PR three ways — a bare number, a URL or
//! `owner/repo#N` reference, or `--detect` (the PR for the current branch) — and
//! this module turns any of them into a fully [`ResolvedPr`] (owner, repo,
//! number, and the base/head branch names the diff source needs). The parsing of
//! a query string is pure and unit-tested; the resolution that fills in the rest
//! shells out to `gh`.

use serde::Deserialize;

/// A parsed pull-request reference, before it is resolved against `gh`.
///
/// `owner`/`repo` are present only when the query carried them (a URL or
/// `owner/repo#N`); a bare `#N` or `N` leaves them `None` so the current
/// repository is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    /// Repository owner, when the query named one.
    pub owner: Option<String>,
    /// Repository name, when the query named one.
    pub repo: Option<String>,
    /// The pull-request (or issue) number.
    pub number: u64,
    /// True when the source looked like an issue URL rather than a pull URL.
    pub is_issue: bool,
}

/// How the caller asked for a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrQuery {
    /// An explicit reference parsed from a number, URL, or `owner/repo#N`.
    Ref(PrRef),
    /// Detect the pull request for the branch currently checked out.
    Detect,
}

impl PrQuery {
    /// Parse the positional argument to `lr pr` into a query.
    ///
    /// Returns [`None`] when the text does not look like a PR reference; the
    /// caller turns that into [`GithubError::InvalidPrQuery`].
    ///
    /// [`GithubError::InvalidPrQuery`]: crate::GithubError::InvalidPrQuery
    pub fn parse(input: &str) -> Option<PrQuery> {
        parse_pr_query(input).map(PrQuery::Ref)
    }
}

/// A pull request resolved to everything the rest of the crate needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResolvedPr {
    /// Repository owner.
    #[serde(skip)]
    pub owner: String,
    /// Repository name.
    #[serde(skip)]
    pub repo: String,
    /// Pull-request number.
    pub number: u64,
    /// Pull-request title.
    #[serde(default)]
    pub title: String,
    /// The base (target) branch name, e.g. `main` — the diff source fetches
    /// `origin/<base_ref>` to compare against.
    #[serde(default, rename = "baseRefName")]
    pub base_ref: String,
    /// The head (source) branch name.
    #[serde(default, rename = "headRefName")]
    pub head_ref: String,
    /// The PR state, e.g. `OPEN`, `MERGED`, `CLOSED`.
    #[serde(default)]
    pub state: String,
    /// Whether the PR is a draft (`isDraft`) — an open PR not yet ready for review.
    #[serde(default, rename = "isDraft")]
    pub is_draft: bool,
    /// The merge timestamp (`mergedAt`), present only once the PR is merged.
    #[serde(default, rename = "mergedAt")]
    pub merged_at: Option<String>,
    /// The merge commit (`mergeCommit`), present only once the PR is merged — its
    /// first parent is the base the diff should compare against.
    #[serde(default, rename = "mergeCommit")]
    pub merge_commit: Option<MergeCommit>,
    /// The creation timestamp (`createdAt`).
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<String>,
    /// The PR author — the person who opened it (distinct from the viewer). Its
    /// `login` is what the Overview shows.
    #[serde(default)]
    pub author: Option<PrAuthor>,
    /// The PR description (`body`), as markdown.
    #[serde(default)]
    pub body: String,
    /// The canonical PR URL.
    #[serde(default)]
    pub url: String,
}

/// The `author` object `gh pr view` returns — only its `login` is used.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct PrAuthor {
    /// The author's GitHub login.
    #[serde(default)]
    pub login: String,
}

/// The `mergeCommit` object `gh pr view` returns — only its `oid` is used.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct MergeCommit {
    /// The merge commit SHA.
    #[serde(default)]
    pub oid: String,
}

impl ResolvedPr {
    /// The `owner/repo` slug for this pull request.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// The merge commit SHA, once the PR is merged (its first parent is the diff
    /// base). `None` for an open or closed-unmerged PR.
    pub fn merge_commit_sha(&self) -> Option<&str> {
        self.merge_commit
            .as_ref()
            .map(|m| m.oid.as_str())
            .filter(|oid| !oid.is_empty())
    }

    /// A short label such as `PR #42` for UI headers.
    pub fn label(&self) -> String {
        format!("PR #{}", self.number)
    }

    /// The author's GitHub login, or an empty string when unknown.
    pub fn author_login(&self) -> &str {
        self.author.as_ref().map(|a| a.login.as_str()).unwrap_or("")
    }

    /// The PR's lifecycle status, for the header badge.
    pub fn status(&self) -> PrStatus {
        PrStatus::derive(&self.state, self.is_draft, self.merged_at.is_some())
    }
}

/// A pull request's lifecycle status — the four states GitHub shows distinctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrStatus {
    /// Open but not yet ready for review.
    Draft,
    /// Open and ready.
    Open,
    /// Merged (a merged PR is also closed — this wins).
    Merged,
    /// Closed without merging.
    Closed,
}

impl PrStatus {
    /// Derive the status from the raw PR fields, in GitHub's precedence: merged
    /// wins over closed (a merged PR is technically closed too), then a draft flag
    /// distinguishes an open PR, else it is plain open. `merged` is the merge fact
    /// (a `mergedAt` timestamp); a `MERGED` state string counts as merged too.
    pub fn derive(state: &str, is_draft: bool, merged: bool) -> PrStatus {
        if merged || state.eq_ignore_ascii_case("merged") {
            PrStatus::Merged
        } else if state.eq_ignore_ascii_case("closed") {
            PrStatus::Closed
        } else if is_draft {
            PrStatus::Draft
        } else {
            PrStatus::Open
        }
    }

    /// The badge text.
    pub fn label(self) -> &'static str {
        match self {
            PrStatus::Draft => "Draft",
            PrStatus::Open => "Open",
            PrStatus::Merged => "Merged",
            PrStatus::Closed => "Closed",
        }
    }
}

/// An issue's lifecycle status — the three states GitHub shows distinctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStatus {
    /// Open.
    Open,
    /// Closed as completed.
    Closed,
    /// Closed as not planned.
    NotPlanned,
}

impl IssueStatus {
    /// Derive the status from the raw issue fields. GitHub issue `state` is
    /// `"open"`/`"closed"`; `state_reason` is `"completed"`/`"not_planned"`/
    /// `"reopened"`/null. An open state wins regardless of reason; a closed
    /// issue is [`NotPlanned`](IssueStatus::NotPlanned) only when explicitly
    /// closed as not-planned, otherwise it defaults to
    /// [`Closed`](IssueStatus::Closed) (completed).
    pub fn derive(state: &str, state_reason: Option<&str>) -> IssueStatus {
        if state.eq_ignore_ascii_case("open") {
            IssueStatus::Open
        } else if state_reason == Some("not_planned") {
            IssueStatus::NotPlanned
        } else {
            IssueStatus::Closed
        }
    }

    /// The badge text.
    pub fn label(self) -> &'static str {
        match self {
            IssueStatus::Open => "Open",
            IssueStatus::Closed => "Closed",
            IssueStatus::NotPlanned => "Not planned",
        }
    }
}

/// Whether a `{owner}/{repo}/issues/{n}` reference resolves to a pull request or
/// a plain issue. GitHub's issues API numbers PRs and issues in one sequence and
/// serves a PR through the issues endpoint too, so the number alone is
/// ambiguous — see [`subject_kind_from_issue_json`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    /// The number is a pull request.
    Pr,
    /// The number is a plain issue.
    Issue,
}

/// Classify the JSON that `GET /repos/{owner}/{repo}/issues/{n}` returns as a PR
/// or a plain issue.
///
/// GitHub includes a top-level `"pull_request"` object only when the number is a
/// pull request, so its presence is the discriminator.
pub fn subject_kind_from_issue_json(json: &str) -> Result<SubjectKind, serde_json::Error> {
    #[derive(Deserialize)]
    struct IssuePayload {
        #[serde(default)]
        pull_request: Option<serde_json::Value>,
    }
    let payload: IssuePayload = serde_json::from_str(json)?;
    Ok(if payload.pull_request.is_some() {
        SubjectKind::Pr
    } else {
        SubjectKind::Issue
    })
}

/// Parse a direct-entry query into a PR/issue reference, if it looks like one.
///
/// Accepts:
/// * `https://github.com/owner/repo/pull/123` (and `/issues/123`);
/// * `owner/repo#123`;
/// * `#123`;
/// * `123`.
pub fn parse_pr_query(input: &str) -> Option<PrRef> {
    let q = input.trim();
    if q.is_empty() {
        return None;
    }

    // Full GitHub URL.
    if let Some(rest) = q
        .strip_prefix("https://github.com/")
        .or_else(|| q.strip_prefix("http://github.com/"))
        .or_else(|| q.strip_prefix("github.com/"))
    {
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        if parts.len() >= 4 {
            let owner = parts[0];
            let repo = parts[1];
            let kind = parts[2];
            if (kind == "pull" || kind == "issues")
                && let Ok(number) = parts[3]
                    .split(['#', '?'])
                    .next()
                    .unwrap_or("")
                    .parse::<u64>()
                && !owner.is_empty()
                && !repo.is_empty()
            {
                return Some(PrRef {
                    owner: Some(owner.to_string()),
                    repo: Some(repo.to_string()),
                    number,
                    is_issue: kind == "issues",
                });
            }
        }
        return None;
    }

    // owner/repo#123
    if let Some((repo_part, num_part)) = q.split_once('#')
        && let Some((owner, repo)) = repo_part.split_once('/')
        && !owner.is_empty()
        && !repo.is_empty()
        && let Ok(number) = num_part.parse::<u64>()
    {
        return Some(PrRef {
            owner: Some(owner.to_string()),
            repo: Some(repo.to_string()),
            number,
            is_issue: false,
        });
    }

    // #123
    if let Some(num) = q.strip_prefix('#')
        && let Ok(number) = num.parse::<u64>()
    {
        return Some(PrRef {
            owner: None,
            repo: None,
            number,
            is_issue: false,
        });
    }

    // Bare number.
    if let Ok(number) = q.parse::<u64>() {
        return Some(PrRef {
            owner: None,
            repo: None,
            number,
            is_issue: false,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_status_follows_github_precedence() {
        use PrStatus::*;
        // The plain four.
        assert_eq!(PrStatus::derive("OPEN", false, false), Open);
        assert_eq!(PrStatus::derive("OPEN", true, false), Draft);
        assert_eq!(PrStatus::derive("CLOSED", false, false), Closed);
        assert_eq!(PrStatus::derive("MERGED", false, true), Merged);

        // Precedence: merged wins over closed — a merged PR is closed too, and
        // labelling it "Closed" would mislead. This is the load-bearing case.
        assert_eq!(PrStatus::derive("CLOSED", false, true), Merged);
        // A `MERGED` state with no timestamp still reads as merged.
        assert_eq!(PrStatus::derive("MERGED", false, false), Merged);
        // A draft flag never overrides a terminal (closed/merged) state.
        assert_eq!(PrStatus::derive("CLOSED", true, false), Closed);
        assert_eq!(PrStatus::derive("MERGED", true, true), Merged);

        // The state string is matched case-insensitively (gh casing varies).
        assert_eq!(PrStatus::derive("open", false, false), Open);
        assert_eq!(PrStatus::derive("closed", false, false), Closed);
        assert_eq!(PrStatus::derive("merged", false, false), Merged);
    }

    #[test]
    fn issue_status_follows_github_precedence() {
        use IssueStatus::*;
        // Open wins regardless of any reason.
        assert_eq!(IssueStatus::derive("open", None), Open);
        // Closed as completed reads as Closed.
        assert_eq!(IssueStatus::derive("closed", Some("completed")), Closed);
        // Closed as not-planned is the one distinct closed case.
        assert_eq!(
            IssueStatus::derive("closed", Some("not_planned")),
            NotPlanned
        );
        // A closed issue with no reason defaults to Closed (completed).
        assert_eq!(IssueStatus::derive("closed", None), Closed);
        // A `reopened` reason on a closed state is not not-planned — Closed.
        assert_eq!(IssueStatus::derive("closed", Some("reopened")), Closed);
        // The state string is matched case-insensitively (API casing varies).
        assert_eq!(IssueStatus::derive("OPEN", Some("not_planned")), Open);
        assert_eq!(
            IssueStatus::derive("Closed", Some("not_planned")),
            NotPlanned
        );
    }

    #[test]
    fn subject_kind_detects_pull_request() {
        // The issues endpoint carries a `pull_request` object only for PRs.
        let pr_json = r#"{
            "number": 7,
            "title": "A pull request",
            "pull_request": { "url": "https://api.github.com/repos/o/r/pulls/7" }
        }"#;
        assert_eq!(
            subject_kind_from_issue_json(pr_json).unwrap(),
            SubjectKind::Pr
        );

        // A plain issue has no `pull_request` key.
        let issue_json = r#"{ "number": 7, "title": "A plain issue" }"#;
        assert_eq!(
            subject_kind_from_issue_json(issue_json).unwrap(),
            SubjectKind::Issue
        );

        // Other fields present, still no `pull_request` — an issue.
        let rich_issue_json = r#"{
            "number": 7,
            "title": "A plain issue",
            "state": "open",
            "state_reason": null,
            "body": "text",
            "user": { "login": "octocat" }
        }"#;
        assert_eq!(
            subject_kind_from_issue_json(rich_issue_json).unwrap(),
            SubjectKind::Issue
        );
    }

    #[test]
    fn resolved_pr_derives_its_status() {
        let mut pr = ResolvedPr {
            owner: "o".into(),
            repo: "r".into(),
            number: 1,
            title: "t".into(),
            base_ref: "main".into(),
            head_ref: "feat".into(),
            state: "OPEN".into(),
            is_draft: true,
            merged_at: None,
            merge_commit: None,
            created_at: None,
            author: None,
            body: String::new(),
            url: String::new(),
        };
        assert_eq!(pr.status(), PrStatus::Draft);
        pr.is_draft = false;
        assert_eq!(pr.status(), PrStatus::Open);
        pr.merged_at = Some("2026-07-23T00:00:00Z".into());
        pr.state = "CLOSED".into(); // merged PRs report closed on some paths
        assert_eq!(pr.status(), PrStatus::Merged, "the merge fact wins");
    }

    #[test]
    fn parses_pull_url() {
        assert_eq!(
            parse_pr_query("https://github.com/octo/hello/pull/123"),
            Some(PrRef {
                owner: Some("octo".to_string()),
                repo: Some("hello".to_string()),
                number: 123,
                is_issue: false,
            })
        );
    }

    #[test]
    fn parses_issue_url() {
        let r = parse_pr_query("https://github.com/octo/hello/issues/7").unwrap();
        assert_eq!(r.number, 7);
        assert!(r.is_issue);
    }

    #[test]
    fn parses_owner_repo_hash() {
        assert_eq!(
            parse_pr_query("octo/hello#9"),
            Some(PrRef {
                owner: Some("octo".to_string()),
                repo: Some("hello".to_string()),
                number: 9,
                is_issue: false,
            })
        );
    }

    #[test]
    fn parses_bare_and_hash_numbers() {
        assert_eq!(parse_pr_query("#55").unwrap().number, 55);
        assert_eq!(parse_pr_query("55").unwrap().number, 55);
        assert!(parse_pr_query("55").unwrap().owner.is_none());
    }

    #[test]
    fn rejects_non_references() {
        assert!(parse_pr_query("just some text").is_none());
        assert!(parse_pr_query("").is_none());
        assert!(parse_pr_query("   ").is_none());
    }

    #[test]
    fn query_parse_wraps_ref() {
        assert_eq!(
            PrQuery::parse("42"),
            Some(PrQuery::Ref(PrRef {
                owner: None,
                repo: None,
                number: 42,
                is_issue: false,
            }))
        );
        assert!(PrQuery::parse("nonsense").is_none());
    }

    #[test]
    fn resolved_pr_deserializes_from_gh_view() {
        // The exact field shape of `gh pr view --json ...`.
        let json = r#"{
            "number": 7,
            "title": "Add a thing",
            "baseRefName": "main",
            "headRefName": "feature/thing",
            "state": "OPEN",
            "url": "https://github.com/o/r/pull/7"
        }"#;
        let mut pr: ResolvedPr = serde_json::from_str(json).unwrap();
        pr.owner = "o".to_string();
        pr.repo = "r".to_string();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.base_ref, "main");
        assert_eq!(pr.head_ref, "feature/thing");
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.slug(), "o/r");
        assert_eq!(pr.label(), "PR #7");
    }
}
