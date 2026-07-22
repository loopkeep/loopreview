//! The [`DiffSource`] abstraction: where a diff comes from.
//!
//! loopreview loads every diff through this trait so the rest of the program
//! never cares whether the changes came from the working tree, a ref
//! comparison, or a patch (from stdin or a file). Future sources (a pull
//! request, an agent run's output) implement the same trait and slot in
//! unchanged.

use std::io::Read;
use std::path::PathBuf;

use crate::error::DiffError;
use crate::git;
use crate::model::{Diff, Provenance};
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

/// The working tree (or the index) compared against `HEAD`.
pub struct WorktreeSource {
    dir: PathBuf,
    staged: bool,
    pathspec: Vec<String>,
}

impl WorktreeSource {
    /// Compare the working tree rooted at `dir` against `HEAD`.
    pub fn new(dir: impl Into<PathBuf>) -> WorktreeSource {
        WorktreeSource {
            dir: dir.into(),
            staged: false,
            pathspec: Vec::new(),
        }
    }

    /// When `true`, compare only the staged index against `HEAD` (`--cached`)
    /// rather than the full working tree.
    pub fn staged(mut self, staged: bool) -> WorktreeSource {
        self.staged = staged;
        self
    }

    /// Restrict the diff to paths matching `pathspec`.
    pub fn pathspec(mut self, pathspec: Vec<String>) -> WorktreeSource {
        self.pathspec = pathspec;
        self
    }
}

impl DiffSource for WorktreeSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let text = git::diff_worktree(&self.dir, self.staged, &self.pathspec)?;
        let mut diff = patch::parse(&text)?;
        // The old side is HEAD; the new side is the working tree or index, which
        // are not commits.
        diff.provenance = Provenance {
            base: git::head_sha(&self.dir),
            head: None,
        };
        Ok(diff)
    }

    fn describe(&self) -> String {
        if self.staged {
            "staged changes".to_string()
        } else {
            "working tree".to_string()
        }
    }
}

/// An arbitrary `git diff` comparison, e.g. `main`, `main...HEAD`, or
/// `abc123..def456`.
pub struct RefSource {
    dir: PathBuf,
    target: String,
    pathspec: Vec<String>,
}

impl RefSource {
    /// Compare using `target` within the repository at `dir`. `target` is passed
    /// through to `git diff`, so any revision expression git accepts works.
    pub fn new(dir: impl Into<PathBuf>, target: impl Into<String>) -> RefSource {
        RefSource {
            dir: dir.into(),
            target: target.into(),
            pathspec: Vec::new(),
        }
    }

    /// Restrict the diff to paths matching `pathspec`.
    pub fn pathspec(mut self, pathspec: Vec<String>) -> RefSource {
        self.pathspec = pathspec;
        self
    }
}

impl DiffSource for RefSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let text = git::diff_target(&self.dir, &self.target, &self.pathspec)?;
        let mut diff = patch::parse(&text)?;
        diff.provenance = resolve_ref_provenance(&self.dir, &self.target);
        Ok(diff)
    }

    fn describe(&self) -> String {
        format!("git diff {}", self.target)
    }
}

/// Resolve the base/head commit SHAs a `git diff <target>` compares, matching
/// git's range grammar:
///
/// * `A...B` — from the merge base of `A` and `B` to `B` (empty side = `HEAD`);
/// * `A..B` — from `A` to `B` (empty side = `HEAD`);
/// * `A` — from `A` to the working tree (so the new side is not a commit).
fn resolve_ref_provenance(dir: &std::path::Path, target: &str) -> Provenance {
    if let Some((a, b)) = target.split_once("...") {
        let (a, b) = (default_head(a), default_head(b));
        Provenance {
            base: git::merge_base(dir, a, b),
            head: git::rev_parse(dir, b),
        }
    } else if let Some((a, b)) = target.split_once("..") {
        let (a, b) = (default_head(a), default_head(b));
        Provenance {
            base: git::rev_parse(dir, a),
            head: git::rev_parse(dir, b),
        }
    } else {
        Provenance {
            base: git::rev_parse(dir, target),
            head: None,
        }
    }
}

/// An empty range endpoint defaults to `HEAD` (as in `main...`).
fn default_head(rev: &str) -> &str {
    if rev.is_empty() { "HEAD" } else { rev }
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

/// A unified-diff patch read from a file (`lr patch <file>`).
pub struct FilePatchSource {
    path: PathBuf,
}

impl FilePatchSource {
    /// Create a source that reads a patch from `path` when loaded.
    pub fn new(path: impl Into<PathBuf>) -> FilePatchSource {
        FilePatchSource { path: path.into() }
    }
}

impl DiffSource for FilePatchSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let text = std::fs::read_to_string(&self.path)?;
        patch::parse(&text)
    }

    fn describe(&self) -> String {
        format!("patch {}", self.path.display())
    }
}
