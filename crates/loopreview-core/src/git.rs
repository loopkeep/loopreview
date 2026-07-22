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

/// The unified diff of the working tree against `HEAD` (staged and unstaged
/// changes to tracked files).
pub fn diff_worktree(dir: &Path) -> Result<String, DiffError> {
    let mut cmd = diff_command(dir);
    cmd.arg("HEAD");
    run(cmd)
}

/// The unified diff for an arbitrary `git diff` target, e.g. `main`,
/// `main...HEAD`, or `abc123..def456`.
pub fn diff_target(dir: &Path, target: &str) -> Result<String, DiffError> {
    let mut cmd = diff_command(dir);
    cmd.arg(target);
    run(cmd)
}
