//! The error type returned by loopreview-core.

use std::io;

use thiserror::Error;

/// Anything that can go wrong while loading a diff from a source.
#[derive(Debug, Error)]
pub enum DiffError {
    /// A child process (`git`) could not be started at all.
    #[error("failed to run `{program}` (is it installed and on PATH?): {source}")]
    Spawn {
        /// The program that could not be spawned.
        program: String,
        /// The underlying OS error.
        source: io::Error,
    },

    /// A child process ran but exited with a non-zero status.
    #[error("`{program}` exited with status {code}{}", format_stderr(.stderr))]
    Command {
        /// The program that failed.
        program: String,
        /// Its exit code (`-1` when terminated by a signal).
        code: i32,
        /// Captured standard error, for context.
        stderr: String,
    },

    /// The requested directory is not inside a git repository.
    #[error("not a git repository: {path}")]
    NotARepository {
        /// The path that was checked.
        path: String,
    },

    /// A `git diff <target>` names a revision git cannot resolve.
    #[error("could not resolve `{target}` — no such branch, tag, or commit")]
    UnknownRevision {
        /// The unresolvable revision expression.
        target: String,
    },

    /// A patch could not be parsed into the diff model.
    #[error("could not parse patch: {0}")]
    Parse(String),

    /// An I/O error, e.g. while reading a patch from standard input.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Format captured stderr as a trailing `: …` clause, or nothing when empty.
fn format_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}
