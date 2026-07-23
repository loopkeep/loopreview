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

/// Fetch every refspec in `refspecs` from `origin` in a single invocation.
///
/// A pull-request review needs both the base branch (`<base_ref>`) and the PR
/// head (`pull/<n>/head`); fetching them together costs one network connection
/// instead of two — the dominant cost of opening a PR. Errors propagate so an
/// offline fetch surfaces a clear network message rather than a confusing empty
/// diff. The fetched SHAs are read back with [`read_fetch_head`], because with
/// several refspecs `git rev-parse FETCH_HEAD` reports only the first.
pub(crate) fn fetch_many(dir: &Path, refspecs: &[&str]) -> Result<(), GithubError> {
    let mut args = vec!["fetch", "--quiet", ORIGIN];
    args.extend_from_slice(refspecs);
    cmd::run_ok("git", &args, dir, None)?;
    Ok(())
}

/// Read the raw `FETCH_HEAD` the last fetch wrote.
///
/// A single fetch of several refspecs writes one `FETCH_HEAD` line per ref, but
/// `git rev-parse FETCH_HEAD` collapses to just the first entry — so the per-ref
/// SHAs are parsed out of the file instead (see [`crate::source`]). The path is
/// resolved with `git rev-parse --git-path`, which stays correct from a linked
/// worktree, where `FETCH_HEAD` lives under `.git/worktrees/<name>/` rather than
/// directly in `.git/`.
pub(crate) fn read_fetch_head(dir: &Path) -> Result<String, GithubError> {
    let rel = cmd::run_ok("git", &["rev-parse", "--git-path", "FETCH_HEAD"], dir, None)?;
    // `--git-path` prints relative to `dir` (or absolute); `join` honours both.
    let path = dir.join(rel.trim());
    std::fs::read_to_string(&path).map_err(|source| GithubError::Command {
        program: "git".to_string(),
        code: -1,
        stderr: format!("could not read {}: {source}", path.display()),
    })
}

/// Resolve a revision to its commit SHA.
///
/// The PR head is captured this way immediately after its fetch so a later
/// concurrent fetch cannot move `FETCH_HEAD` out from under the diff.
pub(crate) fn rev_parse(dir: &Path, rev: &str) -> Result<String, GithubError> {
    let out = cmd::run_ok("git", &["rev-parse", "--verify", rev], dir, None)?;
    Ok(out.trim().to_string())
}
