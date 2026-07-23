//! [`PrSource`] — a [`DiffSource`] that shows a pull request's diff.
//!
//! A PR is reviewed **without a checkout**. The source fetches the base branch
//! and the PR head from `origin`, resolves both to commit SHAs, and then
//! delegates to loopreview-core's [`RefSource`] to produce the three-dot
//! `base...head` diff — the same comparison GitHub's "Files changed" tab shows.
//! Fetching the base from `origin` (rather than reusing a possibly-stale local
//! branch) is what keeps unrelated changes from leaking into the diff.

use std::path::PathBuf;

use loopreview_core::{Diff, DiffError, DiffSource, RefSource};

use crate::git;

/// How a PR's base endpoint is chosen.
#[derive(Debug, PartialEq, Eq)]
enum BasePlan {
    /// A merged PR: compare against the merge commit's first parent (the base as
    /// it was at merge). The head is an ancestor of the *current* base tip, so a
    /// tip comparison would be empty.
    MergeParent(String),
    /// An open or closed-unmerged PR: compare against the current base tip.
    BaseTip,
}

/// Decide the base endpoint plan from the merge commit SHA (present only for a
/// merged PR). Pure, so the choice is testable without git.
fn base_plan(merged_base: Option<&str>) -> BasePlan {
    match merged_base {
        Some(sha) if !sha.is_empty() => BasePlan::MergeParent(sha.to_string()),
        _ => BasePlan::BaseTip,
    }
}

/// A diff source for a pull request, comparing `origin/<base_ref>` against the
/// fetched PR head.
pub struct PrSource {
    dir: PathBuf,
    base_ref: String,
    number: u64,
    /// The merge commit SHA when the PR is merged, else `None`.
    merged_base: Option<String>,
}

impl PrSource {
    /// Create a PR diff source for pull request `number` in the repository at
    /// `dir`, comparing against `base_ref` (the PR's target branch, e.g.
    /// `main`). `merged_base` is the merge commit SHA for a merged PR (its first
    /// parent becomes the base), or `None` for an open/closed-unmerged PR.
    pub fn new(
        dir: impl Into<PathBuf>,
        base_ref: impl Into<String>,
        number: u64,
        merged_base: Option<String>,
    ) -> PrSource {
        PrSource {
            dir: dir.into(),
            base_ref: base_ref.into(),
            number,
            merged_base,
        }
    }

    /// Fetch the head and base and resolve both to commit SHAs. The head is the
    /// PR's head commit (it survives the merge); the base follows [`base_plan`].
    fn fetch_endpoints(&self) -> Result<(String, String), DiffError> {
        let head_refspec = format!("pull/{}/head", self.number);
        git::fetch(&self.dir, &head_refspec).map_err(to_diff_error)?;
        let head_sha = git::rev_parse(&self.dir, "FETCH_HEAD").map_err(to_diff_error)?;
        let base_sha = self.base_endpoint()?;
        Ok((base_sha, head_sha))
    }

    /// Resolve the base endpoint. For a merged PR, fetch the merge commit and take
    /// its first parent; if that SHA is missing or unfetchable, fall back to the
    /// base-branch tip (rather than silently emitting an empty diff).
    fn base_endpoint(&self) -> Result<String, DiffError> {
        if let BasePlan::MergeParent(sha) = base_plan(self.merged_base.as_deref())
            && git::fetch(&self.dir, &sha).is_ok()
            && let Ok(parent) = git::rev_parse(&self.dir, &format!("{sha}^1"))
        {
            return Ok(parent);
        }
        git::fetch(&self.dir, &self.base_ref).map_err(to_diff_error)?;
        git::rev_parse(&self.dir, "FETCH_HEAD").map_err(to_diff_error)
    }
}

impl DiffSource for PrSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let (base_sha, head_sha) = self.fetch_endpoints()?;
        // Three-dot: changes on the PR head since it diverged from the base,
        // which is exactly GitHub's "Files changed" comparison. RefSource fills
        // in provenance (merge-base as base, head SHA as head).
        let target = format!("{base_sha}...{head_sha}");
        RefSource::new(&self.dir, target).load()
    }

    fn describe(&self) -> String {
        format!("PR #{}", self.number)
    }
}

/// Map a GitHub fetch error into the diff pipeline's error type, so `PrSource`
/// can satisfy [`DiffSource`] while still surfacing network/auth context.
fn to_diff_error(err: crate::error::GithubError) -> DiffError {
    match err {
        crate::error::GithubError::Diff(e) => e,
        crate::error::GithubError::NotInstalled { program } => DiffError::Spawn {
            program,
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        },
        other => DiffError::Command {
            program: "git".to_string(),
            code: -1,
            stderr: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_names_the_pr() {
        let source = PrSource::new("/tmp/repo", "main", 42, None);
        assert_eq!(source.describe(), "PR #42");
    }

    #[test]
    fn base_plan_uses_the_merge_parent_only_when_merged() {
        // A merged PR compares against the merge commit's first parent…
        assert_eq!(
            base_plan(Some("abc123")),
            BasePlan::MergeParent("abc123".to_string())
        );
        // …an open/closed-unmerged PR (no merge SHA) uses the base tip…
        assert_eq!(base_plan(None), BasePlan::BaseTip);
        // …and an empty SHA falls back to the tip rather than a bad `^1`.
        assert_eq!(base_plan(Some("")), BasePlan::BaseTip);
    }
}
