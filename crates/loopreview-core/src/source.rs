//! The [`DiffSource`] abstraction: where a diff comes from.
//!
//! loopreview loads every diff through this trait so the rest of the program
//! never cares whether the changes came from the working tree, a ref
//! comparison, or a patch on standard input. Future sources (a pull request, an
//! agent run's output) implement the same trait and slot in unchanged.

use std::io::Read;
use std::path::PathBuf;

use crate::error::DiffError;
use crate::git;
use crate::model::Diff;
use crate::patch;

/// A provider of a [`Diff`], plus a short human-readable description of what it
/// represents (shown in the UI header).
pub trait DiffSource {
    /// Load and parse the diff.
    fn load(&self) -> Result<Diff, DiffError>;

    /// A short label describing this source, e.g. `working tree` or
    /// `git diff main...`.
    fn describe(&self) -> String;
}

/// The working tree compared against `HEAD`: staged and unstaged changes to
/// tracked files.
pub struct WorktreeSource {
    dir: PathBuf,
}

impl WorktreeSource {
    /// Compare the working tree rooted at `dir` against `HEAD`.
    pub fn new(dir: impl Into<PathBuf>) -> WorktreeSource {
        WorktreeSource { dir: dir.into() }
    }
}

impl DiffSource for WorktreeSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let text = git::diff_worktree(&self.dir)?;
        patch::parse(&text)
    }

    fn describe(&self) -> String {
        "working tree".to_string()
    }
}

/// An arbitrary `git diff` comparison, e.g. `main`, `main...HEAD`, or
/// `abc123..def456`.
pub struct RefSource {
    dir: PathBuf,
    target: String,
}

impl RefSource {
    /// Compare using `target` within the repository at `dir`. `target` is passed
    /// through to `git diff`, so any revision expression git accepts works.
    pub fn new(dir: impl Into<PathBuf>, target: impl Into<String>) -> RefSource {
        RefSource {
            dir: dir.into(),
            target: target.into(),
        }
    }
}

impl DiffSource for RefSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let text = git::diff_target(&self.dir, &self.target)?;
        patch::parse(&text)
    }

    fn describe(&self) -> String {
        format!("git diff {}", self.target)
    }
}

/// A unified-diff patch read from standard input (`git diff | lr`).
#[derive(Default)]
pub struct StdinPatchSource;

impl StdinPatchSource {
    /// Create a source that reads a patch from standard input when loaded.
    pub fn new() -> StdinPatchSource {
        StdinPatchSource
    }
}

impl DiffSource for StdinPatchSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        patch::parse(&buf)
    }

    fn describe(&self) -> String {
        "stdin patch".to_string()
    }
}
