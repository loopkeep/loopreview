//! End-to-end coverage of [`PrSource`] against a real `git`, with no network and
//! no `gh`: a local bare repository stands in for `origin`, seeded with a base
//! branch and a `refs/pull/<n>/head` ref exactly as GitHub serves them. This
//! exercises the single combined `git fetch` and the `FETCH_HEAD` SHA reading
//! that unit tests cannot reach, and guards the three-dot `base...head` semantics
//! through the fetch.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use loopreview_core::{ChangeStatus, DiffSource};
use loopreview_github::PrSource;

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

/// A unique throwaway path under the system temp dir.
fn temp_path(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "loopreview-github-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Configure a repo the way the deterministic-diff tests do.
fn configure(dir: &Path) {
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["config", "core.autocrlf", "false"]);
}

/// A bare repository that plays the part of `origin`, with by-SHA fetching
/// enabled — GitHub allows it, and the merged-PR path fetches the merge commit by
/// its SHA.
fn init_bare_remote(name: &str) -> PathBuf {
    let dir = temp_path(&format!("{name}-remote"));
    std::fs::create_dir_all(&dir).expect("create remote dir");
    git(&dir, &["init", "--bare", "--quiet"]);
    git(&dir, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    git(
        &dir,
        &["config", "uploadpack.allowReachableSHA1InWant", "true"],
    );
    dir
}

/// A work repo, seeded with one committed file on branch `main`, pointed at
/// `remote` as `origin`.
fn init_seed(name: &str, remote: &Path) -> PathBuf {
    let dir = temp_path(&format!("{name}-seed"));
    std::fs::create_dir_all(&dir).expect("create seed dir");
    git(&dir, &["init", "--quiet", "-b", "main"]);
    configure(&dir);
    git(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
    std::fs::write(dir.join("file.txt"), "one\ntwo\nthree\n").expect("write file");
    git(&dir, &["add", "file.txt"]);
    git(&dir, &["commit", "--quiet", "-m", "base"]);
    dir
}

/// A fresh consumer repo whose only knowledge of the PR is through `origin` — no
/// checkout of either endpoint, matching how `lr pr` reviews without one.
fn init_consumer(name: &str, remote: &Path) -> PathBuf {
    let dir = temp_path(&format!("{name}-consumer"));
    std::fs::create_dir_all(&dir).expect("create consumer dir");
    git(&dir, &["init", "--quiet", "-b", "main"]);
    configure(&dir);
    git(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
    dir
}

fn paths(diff: &loopreview_core::Diff) -> Vec<String> {
    diff.files
        .iter()
        .filter_map(|f| f.new_path.clone())
        .collect()
}

#[test]
fn open_pr_diff_comes_from_one_combined_fetch() {
    let remote = init_bare_remote("open");
    let seed = init_seed("open", &remote);

    // The PR head branches off the base and adds a file.
    git(&seed, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(seed.join("feature.txt"), "hello\n").expect("write feature");
    git(&seed, &["add", "feature.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "add feature"]);
    let head_sha = git_out(&seed, &["rev-parse", "HEAD"]);
    let branch_point = git_out(&seed, &["rev-parse", "HEAD~1"]);

    // The base branch then moves on with its own, unrelated change — so a naive
    // tip-to-tip diff would leak it. Three-dot must not.
    git(&seed, &["checkout", "--quiet", "main"]);
    std::fs::write(seed.join("file.txt"), "one\nTWO\nthree\n").expect("edit base file");
    git(&seed, &["add", "file.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "base moves on"]);

    // Publish both endpoints on `origin`, the PR head under refs/pull/1/head.
    git(&seed, &["push", "--quiet", "origin", "main"]);
    git(
        &seed,
        &["push", "--quiet", "origin", "feature:refs/pull/1/head"],
    );

    // A consumer that has neither endpoint locally loads the PR diff.
    let consumer = init_consumer("open", &remote);
    let diff = PrSource::new(&consumer, "main", 1, None, Some(branch_point.clone()))
        .load()
        .expect("PR diff loads from origin");

    assert_eq!(
        paths(&diff),
        ["feature.txt"],
        "only the PR's own change shows"
    );
    assert_eq!(diff.files[0].status, ChangeStatus::Added);
    assert!(
        !paths(&diff).contains(&"file.txt".to_string()),
        "the base branch's later change does not leak in"
    );
    // Provenance: base is the merge base (the branch point), head is the PR head.
    assert_eq!(diff.provenance.base.as_deref(), Some(branch_point.as_str()));
    assert_eq!(diff.provenance.head.as_deref(), Some(head_sha.as_str()));

    let _ = std::fs::remove_dir_all(&remote);
    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_dir_all(&consumer);
}

#[test]
fn merged_pr_diff_compares_against_the_merge_parent() {
    let remote = init_bare_remote("merged");
    let seed = init_seed("merged", &remote);
    let branch_point = git_out(&seed, &["rev-parse", "HEAD"]);

    // The PR head.
    git(&seed, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(seed.join("feature.txt"), "hello\n").expect("write feature");
    git(&seed, &["add", "feature.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "add feature"]);
    let head_sha = git_out(&seed, &["rev-parse", "HEAD"]);

    // The base advances, then a real merge folds the PR in. The merge commit's
    // first parent is the base as it was at merge; the head survives the merge.
    git(&seed, &["checkout", "--quiet", "main"]);
    std::fs::write(seed.join("file.txt"), "one\nTWO\nthree\n").expect("edit base file");
    git(&seed, &["add", "file.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "base moves on"]);
    let merge_first_parent = git_out(&seed, &["rev-parse", "HEAD"]);
    git(
        &seed,
        &["merge", "--quiet", "--no-ff", "feature", "-m", "merge PR"],
    );
    let merge_sha = git_out(&seed, &["rev-parse", "HEAD"]);

    git(&seed, &["push", "--quiet", "origin", "main"]);
    git(
        &seed,
        &["push", "--quiet", "origin", "feature:refs/pull/1/head"],
    );

    // The merged PR carries its merge commit SHA; the base becomes its first
    // parent (a tip-to-tip diff would be empty — the head is already merged).
    let consumer = init_consumer("merged", &remote);
    let diff = PrSource::new(
        &consumer,
        "main",
        1,
        Some(merge_sha),
        Some(branch_point.clone()),
    )
    .load()
    .expect("merged PR diff loads from origin");

    assert_eq!(
        paths(&diff),
        ["feature.txt"],
        "the merged PR still shows its own change, not an empty diff"
    );
    assert_eq!(diff.files[0].status, ChangeStatus::Added);
    // base = merge-base(first parent, head) = the branch point; head = the PR head.
    assert_eq!(diff.provenance.base.as_deref(), Some(branch_point.as_str()));
    assert_eq!(diff.provenance.head.as_deref(), Some(head_sha.as_str()));
    // Sanity: the first parent really did move past the branch point, so the
    // merge-parent base is doing work a tip comparison could not.
    assert_ne!(merge_first_parent, branch_point);

    let _ = std::fs::remove_dir_all(&remote);
    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_dir_all(&consumer);
}

#[test]
fn a_deleted_base_branch_falls_back_to_the_base_oid() {
    let remote = init_bare_remote("deleted-base");
    let seed = init_seed("deleted-base", &remote);
    let branch_point = git_out(&seed, &["rev-parse", "HEAD"]);

    // The PR head.
    git(&seed, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(seed.join("feature.txt"), "hello\n").expect("write feature");
    git(&seed, &["add", "feature.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "add feature"]);
    let head_sha = git_out(&seed, &["rev-parse", "HEAD"]);

    // The base advances (so its recorded tip is past the branch point), then both
    // endpoints are published — the head under refs/pull/1/head.
    git(&seed, &["checkout", "--quiet", "main"]);
    std::fs::write(seed.join("file.txt"), "one\nTWO\nthree\n").expect("edit base file");
    git(&seed, &["add", "file.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "base moves on"]);
    let base_oid = git_out(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    git(
        &seed,
        &["push", "--quiet", "origin", "feature:refs/pull/1/head"],
    );

    // The base branch is then deleted on the "remote" — as a closed/merged PR's
    // branch commonly is. Fetching it by name now fails; only the recorded
    // baseRefOid (still served by SHA while reachable) can anchor the base.
    git(&remote, &["update-ref", "-d", "refs/heads/main"]);
    assert!(
        git_out(&seed, &["ls-remote", "origin", "main"]).is_empty(),
        "the base branch is gone from origin"
    );

    let consumer = init_consumer("deleted-base", &remote);
    let diff = PrSource::new(&consumer, "main", 1, None, Some(base_oid.clone()))
        .load()
        .expect("a deleted base branch still loads via the base OID");

    assert_eq!(
        paths(&diff),
        ["feature.txt"],
        "the diff is built from the base OID, not lost to the deleted branch"
    );
    assert_eq!(diff.files[0].status, ChangeStatus::Added);
    // Three-dot base is still the merge base (branch point); the deleted branch's
    // own later change does not leak in.
    assert_eq!(diff.provenance.base.as_deref(), Some(branch_point.as_str()));
    assert_eq!(diff.provenance.head.as_deref(), Some(head_sha.as_str()));

    let _ = std::fs::remove_dir_all(&remote);
    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_dir_all(&consumer);
}

#[test]
fn a_deleted_base_with_no_recorded_oid_reports_an_error() {
    // Without a baseRefOid to fall back to, a deleted base branch cannot produce
    // a diff — the source errors (the CLI turns this into its no-diff reader).
    let remote = init_bare_remote("no-oid");
    let seed = init_seed("no-oid", &remote);

    git(&seed, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(seed.join("feature.txt"), "hello\n").expect("write feature");
    git(&seed, &["add", "feature.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "add feature"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    git(
        &seed,
        &["push", "--quiet", "origin", "feature:refs/pull/1/head"],
    );
    git(&remote, &["update-ref", "-d", "refs/heads/main"]);

    let consumer = init_consumer("no-oid", &remote);
    let err = PrSource::new(&consumer, "main", 1, None, None)
        .load()
        .expect_err("no base branch and no OID cannot build a diff");
    // The head still fetched (pull/1/head survives) — the failure is about the base.
    assert!(
        err.to_string().contains("base branch"),
        "the error is about the missing base, not a raw ref failure: {err}"
    );

    let _ = std::fs::remove_dir_all(&remote);
    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_dir_all(&consumer);
}
