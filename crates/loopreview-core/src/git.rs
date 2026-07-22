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
///
/// With no commits yet (an unborn `HEAD`), `git diff HEAD` fails with exit 128;
/// the comparison base is the empty tree instead, so staged files show as added.
/// (Untracked files are handled separately by the caller.)
pub fn diff_worktree(dir: &Path, staged: bool, pathspec: &[String]) -> Result<String, DiffError> {
    let base = match head_sha(dir) {
        Some(_) => "HEAD".to_string(),
        None => empty_tree(dir).ok_or_else(|| DiffError::Command {
            program: "git".to_string(),
            code: -1,
            stderr: "no commits yet, and the empty tree could not be resolved".to_string(),
        })?,
    };
    let mut cmd = diff_command(dir);
    if staged {
        cmd.arg("--cached");
    }
    cmd.arg(base);
    append_pathspec(&mut cmd, pathspec);
    run(cmd)
}

/// The SHA of this repository's empty tree object. Derived rather than
/// hard-coded because a SHA-256 repository's empty tree is not the familiar
/// SHA-1 `4b825dc…`. Empty stdin is the empty tree's content.
pub fn empty_tree(dir: &Path) -> Option<String> {
    use std::process::Stdio;
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["hash-object", "-t", "tree", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    drop(child.stdin.take()); // EOF on empty stdin
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// The unified diff for an arbitrary `git diff` target, e.g. `main`,
/// `main...HEAD`, or `abc123..def456`, optionally restricted to `pathspec`.
pub fn diff_target(dir: &Path, target: &str, pathspec: &[String]) -> Result<String, DiffError> {
    let mut cmd = diff_command(dir);
    cmd.arg(target);
    append_pathspec(&mut cmd, pathspec);
    match run(cmd) {
        // Git's raw "fatal: ambiguous argument …" is opaque; name the target.
        Err(DiffError::Command {
            code: 128, stderr, ..
        }) if stderr.contains("unknown revision") || stderr.contains("ambiguous argument") => {
            Err(DiffError::UnknownRevision {
                target: target.to_string(),
            })
        }
        other => other,
    }
}

/// The commit SHA at `HEAD`, or `None` when it cannot be resolved (e.g. a
/// repository with no commits yet).
pub fn head_sha(dir: &Path) -> Option<String> {
    rev_parse(dir, "HEAD")
}

/// The repository's untracked (but not ignored) files, relative to `dir`,
/// optionally restricted to `pathspec`. Empty on failure.
pub fn untracked(dir: &Path, pathspec: &[String]) -> Vec<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args([
        "-c",
        "core.quotepath=false",
        "ls-files",
        "--others",
        "--exclude-standard",
    ]);
    append_pathspec(&mut cmd, pathspec);
    match run(cmd) {
        Ok(out) => out.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Resolve a revision to its commit SHA, or `None` when it cannot be resolved.
pub fn rev_parse(dir: &Path, rev: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse", "--verify", rev]);
    let sha = run(cmd).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// The best common ancestor commit of `a` and `b`, or `None` when there is none.
pub fn merge_base(dir: &Path, a: &str, b: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["merge-base", a, b]);
    let sha = run(cmd).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// The contents of `path` at commit `commit` (`git show <commit>:<path>`), or
/// `None` when it cannot be read (e.g. the file did not exist there).
pub fn show_file(dir: &Path, commit: &str, path: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(["show", &format!("{commit}:{path}")]);
    run(cmd).ok()
}

/// Read a git config value (e.g. `user.name`), or `None` when it is unset.
pub fn config(dir: &Path, key: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["config", "--get", key]);
    let value = run(cmd).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// The shared git directory for the repository containing `dir`.
///
/// This is the same for every worktree of a repository, so it is a stable
/// identity for keying per-repo state (like a review store) that should be
/// shared across a repo's worktrees.
pub fn common_dir(dir: &Path) -> Option<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse", "--git-common-dir"]);
    let raw = run(cmd).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    Some(absolute.canonicalize().unwrap_or(absolute))
}

/// Append `-- <pathspec>…` to a diff command when a pathspec is given.
fn append_pathspec(cmd: &mut Command, pathspec: &[String]) {
    if !pathspec.is_empty() {
        cmd.arg("--");
        cmd.args(pathspec);
    }
}
