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

/// A diff source for a pull request, comparing `origin/<base_ref>` against the
/// fetched PR head.
pub struct PrSource {
    dir: PathBuf,
    base_ref: String,
    number: u64,
}

impl PrSource {
    /// Create a PR diff source for pull request `number` in the repository at
    /// `dir`, comparing against `base_ref` (the PR's target branch, e.g.
    /// `main`).
    pub fn new(dir: impl Into<PathBuf>, base_ref: impl Into<String>, number: u64) -> PrSource {
        PrSource {
            dir: dir.into(),
            base_ref: base_ref.into(),
            number,
        }
    }

    /// Fetch the base and head and resolve both to commit SHAs.
    ///
    /// The base is fetched first so `FETCH_HEAD` can be read as its tip; then the
    /// PR head is fetched and its `FETCH_HEAD` captured. Reading each SHA right
    /// after its fetch keeps the two independent of `FETCH_HEAD` being
    /// overwritten.
    fn fetch_endpoints(&self) -> Result<(String, String), DiffError> {
        git::fetch(&self.dir, &self.base_ref).map_err(to_diff_error)?;
        let base_sha = git::rev_parse(&self.dir, "FETCH_HEAD").map_err(to_diff_error)?;

        let head_refspec = format!("pull/{}/head", self.number);
        git::fetch(&self.dir, &head_refspec).map_err(to_diff_error)?;
        let head_sha = git::rev_parse(&self.dir, "FETCH_HEAD").map_err(to_diff_error)?;

        Ok((base_sha, head_sha))
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
        let source = PrSource::new("/tmp/repo", "main", 42);
        assert_eq!(source.describe(), "PR #42");
    }
}
