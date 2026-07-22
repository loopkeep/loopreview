//! End-to-end coverage of the git-backed sources: drive real `git` in a
//! throwaway repository and check that its output parses into the expected
//! model. This exercises `git.rs` (which unit tests cannot reach) and guards
//! against drift between real `git diff` output and the parser.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use loopreview_core::{ChangeStatus, DiffSource, LineKind, RefSource, WorktreeSource};

/// Run `git` in `dir` with `args`, panicking on failure.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Run `git` in `dir` and return its trimmed stdout.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Create an isolated repository under the system temp dir with one committed
/// file, returning its path.
fn init_repo(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("loopreview-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo dir");

    git(&dir, &["init", "--quiet"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);

    std::fs::write(dir.join("file.txt"), "one\ntwo\nthree\n").expect("write file");
    git(&dir, &["add", "file.txt"]);
    git(&dir, &["commit", "--quiet", "-m", "initial"]);
    dir
}

#[test]
fn worktree_source_parses_a_real_modification() {
    let repo = init_repo("worktree");
    // Edit the middle line in the working tree.
    std::fs::write(repo.join("file.txt"), "one\nTWO\nthree\n").expect("edit file");

    let diff = WorktreeSource::new(&repo)
        .load()
        .expect("load worktree diff");
    assert_eq!(diff.files.len(), 1, "one file changed");
    let file = &diff.files[0];
    assert_eq!(file.status, ChangeStatus::Modified);
    assert_eq!(file.new_path.as_deref(), Some("file.txt"));

    let (added, removed) = file.line_stats();
    assert_eq!((added, removed), (1, 1));

    // The added and removed lines carry the right anchors.
    let hunk = &file.hunks[0];
    let deletion = hunk
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Deletion)
        .expect("a deletion");
    assert_eq!(deletion.content, "two");
    let addition = hunk
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Addition)
        .expect("an addition");
    assert_eq!(addition.content, "TWO");
    assert_eq!(addition.anchor("file.txt").line, 2);

    // Provenance: the old side is HEAD; the new side is the working tree.
    assert_eq!(
        diff.provenance.base.as_deref(),
        Some(git_out(&repo, &["rev-parse", "HEAD"]).as_str())
    );
    assert_eq!(diff.provenance.head, None);

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn ref_source_parses_a_commit_range() {
    let repo = init_repo("ref");
    // Second commit adds a new file.
    std::fs::write(repo.join("added.txt"), "hello\n").expect("write added file");
    git(&repo, &["add", "added.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "add file"]);

    let diff = RefSource::new(&repo, "HEAD~1..HEAD")
        .load()
        .expect("load ref diff");
    assert_eq!(diff.files.len(), 1);
    let file = &diff.files[0];
    assert_eq!(file.status, ChangeStatus::Added);
    assert_eq!(file.new_path.as_deref(), Some("added.txt"));
    assert_eq!(file.old_path, None);

    // Provenance: both endpoints of the range resolve to their commit SHAs.
    assert_eq!(
        diff.provenance.base.as_deref(),
        Some(git_out(&repo, &["rev-parse", "HEAD~1"]).as_str())
    );
    assert_eq!(
        diff.provenance.head.as_deref(),
        Some(git_out(&repo, &["rev-parse", "HEAD"]).as_str())
    );

    let _ = std::fs::remove_dir_all(&repo);
}
