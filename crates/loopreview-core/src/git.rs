//! A thin wrapper over the `git` CLI, used by the git-backed [`DiffSource`]
//! implementations. Everything here shells out; the parsing of the resulting
//! unified diff lives in [`crate::patch`].
//!
//! [`DiffSource`]: crate::source::DiffSource

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DiffError;

/// Flags applied to every `git diff` invocation so the output parses the same
/// way regardless of the user's git configuration:
///
/// * `-c core.quotepath=false` keeps non-ASCII paths unescaped;
/// * `--no-color` / `--no-ext-diff` force plain, machine-readable unified diff;
/// * explicit `a/` and `b/` prefixes defeat `diff.noprefix` / `mnemonicPrefix`.
fn diff_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("diff")
        .arg("--no-color")
        .arg("--no-ext-diff")
        .arg("--src-prefix=a/")
        .arg("--dst-prefix=b/");
    cmd
}

/// Run a prepared `git` command, returning captured stdout on success.
fn run(mut command: Command) -> Result<String, DiffError> {
    let output = command.output().map_err(|source| DiffError::Spawn {
        program: "git".to_string(),
        source,
    })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(DiffError::Command {
            program: "git".to_string(),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The repository root that contains `dir`.
///
/// Returns [`DiffError::NotARepository`] when `dir` is not inside a git
/// worktree.
pub fn repo_root(dir: &Path) -> Result<PathBuf, DiffError> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse", "--show-toplevel"]);
    match run(cmd) {
        Ok(out) => Ok(PathBuf::from(out.trim())),
        Err(DiffError::Command { .. }) => Err(DiffError::NotARepository {
            path: dir.display().to_string(),
        }),
        Err(other) => Err(other),
    }
}

/// The unified diff against `HEAD`. When `staged`, only the index is compared
/// (`--cached`); otherwise the full working tree. `pathspec`, when non-empty,
/// restricts the diff to matching paths.
pub fn diff_worktree(dir: &Path, staged: bool, pathspec: &[String]) -> Result<String, DiffError> {
    let mut cmd = diff_command(dir);
    if staged {
        cmd.arg("--cached");
    }
    cmd.arg("HEAD");
    append_pathspec(&mut cmd, pathspec);
    run(cmd)
}

/// The unified diff for an arbitrary `git diff` target, e.g. `main`,
/// `main...HEAD`, or `abc123..def456`, optionally restricted to `pathspec`.
pub fn diff_target(dir: &Path, target: &str, pathspec: &[String]) -> Result<String, DiffError> {
    let mut cmd = diff_command(dir);
    cmd.arg(target);
    append_pathspec(&mut cmd, pathspec);
    run(cmd)
}

/// The commit SHA at `HEAD`, or `None` when it cannot be resolved (e.g. a
/// repository with no commits yet).
pub fn head_sha(dir: &Path) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse", "HEAD"]);
    let sha = run(cmd).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Append `-- <pathspec>…` to a diff command when a pathspec is given.
fn append_pathspec(cmd: &mut Command, pathspec: &[String]) {
    if !pathspec.is_empty() {
        cmd.arg("--");
        cmd.args(pathspec);
    }
}
