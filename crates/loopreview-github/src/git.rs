//! The `git` fetches that back the PR diff source.
//!
//! Reviewing a pull request never checks it out. Instead we fetch the base
//! branch and the PR head from `origin` and diff the two — see [`crate::source`].
//! These helpers are the thin `git` shims that fetch does.

use std::path::Path;

use crate::cmd;
use crate::error::GithubError;

/// The remote pull requests are fetched from. GitHub PRs live on `origin`.
pub(crate) const ORIGIN: &str = "origin";

/// Fetch `refspec` from `origin` into `FETCH_HEAD`.
///
/// Used for both the base branch (`<base_ref>`) and the PR head
/// (`pull/<n>/head`). Errors propagate so an offline fetch surfaces a clear
/// network message rather than a confusing empty diff.
pub(crate) fn fetch(dir: &Path, refspec: &str) -> Result<(), GithubError> {
    cmd::run_ok("git", &["fetch", "--quiet", ORIGIN, refspec], dir, None)?;
    Ok(())
}

/// Resolve a revision to its commit SHA.
///
/// The PR head is captured this way immediately after its fetch so a later
/// concurrent fetch cannot move `FETCH_HEAD` out from under the diff.
pub(crate) fn rev_parse(dir: &Path, rev: &str) -> Result<String, GithubError> {
    let out = cmd::run_ok("git", &["rev-parse", "--verify", rev], dir, None)?;
    Ok(out.trim().to_string())
}
