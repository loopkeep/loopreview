//! The error type returned by loopreview-github.
//!
//! Every failure that reaches the caller is one of these variants, each with a
//! message written for a human staring at a terminal: a missing `gh`, an
//! unauthenticated session, an offline network, and a pull request that does not
//! exist are told apart so the CLI can print the right next step instead of a
//! raw subprocess dump.

use std::io;

use thiserror::Error;

/// Anything that can go wrong while talking to GitHub through the `gh` CLI (or
/// the `git` fetches that back the PR diff source).
#[derive(Debug, Error)]
pub enum GithubError {
    /// The `gh` (or `git`) executable is not installed or not on `PATH`.
    #[error(
        "`{program}` was not found on PATH — install it to use GitHub features \
         (see https://cli.github.com)"
    )]
    NotInstalled {
        /// The program that could not be found.
        program: String,
    },

    /// `gh` is installed but the user is not authenticated.
    #[error("GitHub authentication is required — run `gh auth login` first{}", detail(.stderr))]
    NotAuthenticated {
        /// Captured standard error, for context.
        stderr: String,
    },

    /// The request could not reach GitHub (offline, DNS failure, timeout).
    #[error("could not reach GitHub — check your network connection{}", detail(.stderr))]
    Network {
        /// Captured standard error, for context.
        stderr: String,
    },

    /// A child process could not be spawned for a reason other than a missing
    /// executable.
    #[error("failed to run `{program}`: {source}")]
    Spawn {
        /// The program that could not be spawned.
        program: String,
        /// The underlying OS error.
        source: io::Error,
    },

    /// A child process ran but exited non-zero (and was not recognised as an
    /// auth or network failure).
    #[error("`{program}` exited with status {code}{}", detail(.stderr))]
    Command {
        /// The program that failed.
        program: String,
        /// Its exit code (`-1` when terminated by a signal).
        code: i32,
        /// Captured standard error, for context.
        stderr: String,
    },

    /// The text the user gave could not be read as a PR reference.
    #[error("`{input}` is not a pull request number, URL, or owner/repo#N reference")]
    InvalidPrQuery {
        /// The offending input.
        input: String,
    },

    /// A pull request matching the request could not be found.
    #[error("no pull request found for {query}")]
    PrNotFound {
        /// A human description of what was looked up.
        query: String,
    },

    /// `--detect` was used but the current branch has no open pull request.
    #[error("the current branch has no associated pull request")]
    NoPrForBranch,

    /// A JSON response from `gh` could not be parsed into the expected shape.
    #[error("could not parse the response from `gh` ({context}): {source}")]
    Parse {
        /// What was being parsed, for context.
        context: String,
        /// The underlying deserialization error.
        source: serde_json::Error,
    },

    /// An error surfaced by the loopreview-core diff pipeline (used by the PR
    /// diff source).
    #[error(transparent)]
    Diff(#[from] loopreview_core::DiffError),
}

impl GithubError {
    /// Build a [`GithubError::Parse`] with the given context label.
    pub(crate) fn parse(context: impl Into<String>, source: serde_json::Error) -> GithubError {
        GithubError::Parse {
            context: context.into(),
            source,
        }
    }
}

/// Classify a non-zero `gh`/`git` exit into the most specific error variant.
///
/// The `gh` CLI does not use distinct exit codes for auth versus network
/// versus "not found", so we read its stderr. Matching is deliberately loose
/// (lower-cased substrings) to survive small wording changes across `gh`
/// versions.
pub(crate) fn classify(program: &str, code: i32, stderr: String) -> GithubError {
    let lower = stderr.to_lowercase();

    let looks_unauthenticated = lower.contains("gh auth login")
        || lower.contains("requires authentication")
        || lower.contains("not logged into")
        || lower.contains("authentication failed")
        || lower.contains("bad credentials")
        || lower.contains("http 401");
    if program == "gh" && looks_unauthenticated {
        return GithubError::NotAuthenticated { stderr };
    }

    let looks_offline = lower.contains("could not resolve host")
        || lower.contains("network is unreachable")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("dial tcp");
    if looks_offline {
        return GithubError::Network { stderr };
    }

    GithubError::Command {
        program: program.to_string(),
        code,
        stderr,
    }
}

/// Render captured stderr as a trailing ` — …` clause, or nothing when empty.
fn detail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(" — {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_detects_auth_from_stderr() {
        let err = classify(
            "gh",
            1,
            "gh: To get started with GitHub CLI, please run: gh auth login".into(),
        );
        assert!(matches!(err, GithubError::NotAuthenticated { .. }));
    }

    #[test]
    fn classify_detects_network_from_stderr() {
        let err = classify(
            "gh",
            1,
            "error connecting: could not resolve host: api.github.com".into(),
        );
        assert!(matches!(err, GithubError::Network { .. }));
    }

    #[test]
    fn classify_falls_back_to_command() {
        let err = classify("git", 128, "fatal: some other failure".into());
        assert!(matches!(err, GithubError::Command { code: 128, .. }));
    }

    #[test]
    fn auth_message_only_applies_to_gh() {
        // The same stderr from git is not treated as a gh auth prompt.
        let err = classify("git", 1, "please run: gh auth login".into());
        assert!(matches!(err, GithubError::Command { .. }));
    }
}
