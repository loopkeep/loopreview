//! End-to-end coverage of the git-backed sources: drive real `git` in a
//! throwaway repository and check that its output parses into the expected
//! model. This exercises `git.rs` (which unit tests cannot reach) and guards
//! against drift between real `git diff` output and the parser.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use loopreview_core::{
    ChangeStatus, DiffError, DiffSource, LineKind, RefSource, ShowSource, WorktreeSource,
};

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
    // Keep line endings verbatim so diffs are deterministic on Windows too.
    git(&dir, &["config", "core.autocrlf", "false"]);

    std::fs::write(dir.join("file.txt"), "one\ntwo\nthree\n").expect("write file");
    git(&dir, &["add", "file.txt"]);
    git(&dir, &["commit", "--quiet", "-m", "initial"]);
    dir
}

/// Like [`init_repo`] but with no commit — an unborn `HEAD`.
fn init_no_commit(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "loopreview-{name}-nocommit-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo dir");
    git(&dir, &["init", "--quiet"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "core.autocrlf", "false"]);
    dir
}

#[test]
fn worktree_source_on_an_unborn_repo_shows_all_files_added() {
    let repo = init_no_commit("worktree");
    std::fs::write(repo.join("a.txt"), "alpha\nbeta\n").expect("write a");
    std::fs::write(repo.join("b.txt"), "gamma\n").expect("write b");

    let source = WorktreeSource::new(&repo);
    let diff = source
        .load()
        .expect("worktree diff loads with an unborn HEAD");
    let mut names: Vec<&str> = diff
        .files
        .iter()
        .filter_map(|f| f.new_path.as_deref())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt"], "every file shows as added");
    assert!(diff.files.iter().all(|f| f.status == ChangeStatus::Added));
    assert_eq!(diff.provenance.base, None, "there is no base commit");
    assert!(
        source.describe().contains("no commits yet"),
        "the header notes the unborn state: {}",
        source.describe()
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn staged_source_on_an_unborn_repo_shows_staged_files_added() {
    let repo = init_no_commit("staged");
    std::fs::write(repo.join("s.txt"), "x\ny\n").expect("write s");
    git(&repo, &["add", "s.txt"]);

    let diff = WorktreeSource::new(&repo)
        .staged(true)
        .load()
        .expect("staged diff loads with an unborn HEAD");
    let file = diff
        .files
        .iter()
        .find(|f| f.new_path.as_deref() == Some("s.txt"))
        .expect("the staged file is present");
    assert_eq!(file.status, ChangeStatus::Added);
    assert_eq!(file.old_path, None);
    assert_eq!(file.line_stats(), (2, 0));
    assert_eq!(diff.provenance.base, None);

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn ref_source_reports_an_unknown_revision() {
    let repo = init_repo("unknown-rev");
    let err = RefSource::new(&repo, "no-such-ref-xyz")
        .load()
        .expect_err("an unresolvable target fails");
    assert!(
        matches!(err, DiffError::UnknownRevision { .. }),
        "a friendly error, not a raw git 128: {err:?}"
    );
    assert!(
        err.to_string().contains("no-such-ref-xyz"),
        "the message names the target: {err}"
    );

    let _ = std::fs::remove_dir_all(&repo);
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
fn worktree_source_includes_untracked_files() {
    let repo = init_repo("untracked");
    std::fs::write(repo.join("new.txt"), "alpha\nbeta\n").expect("write untracked");

    let diff = WorktreeSource::new(&repo).load().expect("load");
    let untracked = diff
        .files
        .iter()
        .find(|f| f.new_path.as_deref() == Some("new.txt"))
        .expect("untracked file present");
    assert_eq!(untracked.status, ChangeStatus::Added);
    assert_eq!(untracked.old_path, None);
    assert_eq!(untracked.line_stats(), (2, 0));

    // Opting out drops it.
    let without = WorktreeSource::new(&repo)
        .include_untracked(false)
        .load()
        .expect("load");
    assert!(
        without
            .files
            .iter()
            .all(|f| f.new_path.as_deref() != Some("new.txt"))
    );

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

#[test]
fn show_source_shows_a_commits_own_changes() {
    let repo = init_repo("show");
    std::fs::write(repo.join("added.txt"), "hello\n").expect("write added file");
    git(&repo, &["add", "added.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "add file"]);

    // `show HEAD` = the tip commit against its first parent.
    let diff = ShowSource::new(&repo, "HEAD")
        .load()
        .expect("load show diff");
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].status, ChangeStatus::Added);
    assert_eq!(diff.files[0].new_path.as_deref(), Some("added.txt"));
    assert_eq!(
        diff.provenance.base.as_deref(),
        Some(git_out(&repo, &["rev-parse", "HEAD^"]).as_str()),
        "base is the first parent"
    );
    assert_eq!(
        diff.provenance.head.as_deref(),
        Some(git_out(&repo, &["rev-parse", "HEAD"]).as_str()),
        "head is the commit shown"
    );

    // The session label names the commit and its short sha.
    let label = ShowSource::new(&repo, "HEAD").describe();
    let short = git_out(&repo, &["rev-parse", "HEAD"])[..7].to_string();
    assert_eq!(label, format!("show HEAD ({short})"));

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn show_source_of_a_root_commit_diffs_the_empty_tree() {
    let repo = init_repo("show-root"); // the single commit is the root
    let root = git_out(&repo, &["rev-parse", "HEAD"]);
    let diff = ShowSource::new(&repo, &root)
        .load()
        .expect("load root show");
    // The initial file is all additions against the empty tree (no parent).
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].status, ChangeStatus::Added);
    assert_eq!(diff.files[0].new_path.as_deref(), Some("file.txt"));
    assert_eq!(diff.provenance.head.as_deref(), Some(root.as_str()));
    assert_ne!(
        diff.provenance.base.as_deref(),
        Some(root.as_str()),
        "base is the empty tree, not the (absent) parent"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn show_source_of_a_merge_uses_its_first_parent() {
    let repo = init_repo("show-merge");
    let main = git_out(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    // A feature branch adds its own file.
    git(&repo, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(repo.join("feature.txt"), "f\n").expect("write");
    git(&repo, &["add", "feature.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "feature"]);
    // Back on the main branch, a different file, then a real merge commit.
    git(&repo, &["checkout", "--quiet", &main]);
    std::fs::write(repo.join("main.txt"), "m\n").expect("write");
    git(&repo, &["add", "main.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "main change"]);
    git(
        &repo,
        &["merge", "--quiet", "--no-ff", "feature", "-m", "merge"],
    );

    // `show` on the merge = merge vs its FIRST parent (the main tip), so it shows
    // only what the branch brought in — not the first parent's own change.
    let diff = ShowSource::new(&repo, "HEAD")
        .load()
        .expect("load merge show");
    assert_eq!(
        diff.provenance.base.as_deref(),
        Some(git_out(&repo, &["rev-parse", "HEAD^1"]).as_str()),
        "base is the first parent"
    );
    let paths: Vec<&str> = diff
        .files
        .iter()
        .filter_map(|f| f.new_path.as_deref())
        .collect();
    assert!(
        paths.contains(&"feature.txt"),
        "brings in the branch's file: {paths:?}"
    );
    assert!(
        !paths.contains(&"main.txt"),
        "not the first parent's own change: {paths:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn show_source_reports_an_unknown_target() {
    let repo = init_repo("show-bad");
    let err = ShowSource::new(&repo, "no-such-ref-xyz")
        .load()
        .unwrap_err();
    assert!(
        matches!(&err, DiffError::UnknownRevision { target } if target == "no-such-ref-xyz"),
        "a bad target gets the friendly error: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}
