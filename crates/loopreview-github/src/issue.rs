//! GitHub issues — resolution and facts, mirroring [`pr`](crate::pr) for the
//! no-diff issue reader.
//!
//! An issue has no diff and no review threads; its conversation is the same
//! flat issue-comment timeline a pull request's conversation uses. So the issue
//! reader reuses the comment machinery and adds only a [`ResolvedIssue`] (facts +
//! [`IssueStatus`]) and the [`Subject`] a bare reference resolves to.

use serde::Deserialize;

use crate::pr::{IssueStatus, ResolvedPr};

/// A pull request or an issue — what a bare `lr <ref>` resolves to once its true
/// type is known (GitHub redirects `/pull/N` ⇆ `/issues/N`, so the reference's
/// look is not trusted; the type comes from the API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A pull request — opens the diff reader.
    Pr(ResolvedPr),
    /// An issue — opens the no-diff reader.
    Issue(ResolvedIssue),
}

/// An issue resolved to its facts, from `GET /repos/{owner}/{repo}/issues/{n}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIssue {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// The issue number.
    pub number: u64,
    /// The title.
    pub title: String,
    /// The state — `open` / `closed`.
    pub state: String,
    /// The close reason — `completed` / `not_planned` / `reopened` / absent.
    pub state_reason: Option<String>,
    /// The author's login (empty when unknown).
    pub author: String,
    /// The creation timestamp.
    pub created_at: Option<String>,
    /// The close timestamp, present once closed.
    pub closed_at: Option<String>,
    /// The description body (markdown).
    pub body: String,
    /// The canonical URL (`html_url`).
    pub url: String,
}

impl ResolvedIssue {
    /// The issue's lifecycle status (open / closed / not planned).
    pub fn status(&self) -> IssueStatus {
        IssueStatus::derive(&self.state, self.state_reason.as_deref())
    }

    /// The `owner/repo` slug.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// A short label such as `issue #42` for UI headers.
    pub fn label(&self) -> String {
        format!("issue #{}", self.number)
    }

    /// Parse the REST issue response, filling `owner`/`repo` (not in the body).
    /// The body is `null` for an empty description — decoded as an empty string.
    pub(crate) fn from_json(
        json: &str,
        owner: &str,
        repo: &str,
    ) -> Result<ResolvedIssue, serde_json::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            number: u64,
            #[serde(default)]
            title: String,
            #[serde(default)]
            state: String,
            #[serde(default)]
            state_reason: Option<String>,
            #[serde(default)]
            user: Option<User>,
            #[serde(default)]
            created_at: Option<String>,
            #[serde(default)]
            closed_at: Option<String>,
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            html_url: String,
        }
        #[derive(Deserialize)]
        struct User {
            #[serde(default)]
            login: String,
        }
        let raw: Raw = serde_json::from_str(json)?;
        Ok(ResolvedIssue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: raw.number,
            title: raw.title,
            state: raw.state,
            state_reason: raw.state_reason,
            author: raw.user.map(|u| u.login).unwrap_or_default(),
            created_at: raw.created_at,
            closed_at: raw.closed_at,
            body: raw.body.unwrap_or_default(),
            url: raw.html_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_issue_response() {
        let json = r#"{
            "number": 42,
            "title": "Flaky retry",
            "state": "closed",
            "state_reason": "not_planned",
            "user": {"login": "octocat"},
            "created_at": "2026-07-20T10:00:00Z",
            "closed_at": "2026-07-21T09:00:00Z",
            "body": "It flakes under load.",
            "html_url": "https://github.com/o/r/issues/42"
        }"#;
        let issue = ResolvedIssue::from_json(json, "o", "r").unwrap();
        assert_eq!(issue.number, 42);
        assert_eq!(issue.title, "Flaky retry");
        assert_eq!(issue.author, "octocat");
        assert_eq!(issue.url, "https://github.com/o/r/issues/42");
        assert_eq!(issue.status(), IssueStatus::NotPlanned);
        assert_eq!(issue.slug(), "o/r");
        assert_eq!(issue.label(), "issue #42");
    }

    #[test]
    fn a_null_body_decodes_as_empty() {
        let json = r#"{"number":1,"title":"t","state":"open","body":null,"user":null}"#;
        let issue = ResolvedIssue::from_json(json, "o", "r").unwrap();
        assert_eq!(issue.body, "");
        assert_eq!(issue.author, "");
        assert_eq!(issue.status(), IssueStatus::Open);
    }
}
