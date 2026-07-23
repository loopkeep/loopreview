//! [`PrSource`] — a [`DiffSource`] that shows a pull request's diff.
//!
//! A PR is reviewed **without a checkout**. The source fetches the base branch
//! and the PR head from `origin` in a **single** `git fetch`, resolves both to
//! commit SHAs, and then delegates to loopreview-core's [`RefSource`] to produce
//! the three-dot `base...head` diff — the same comparison GitHub's "Files
//! changed" tab shows. Fetching the base from `origin` (rather than reusing a
//! possibly-stale local branch) is what keeps unrelated changes from leaking into
//! the diff; fetching both endpoints at once is what keeps the review fast to
//! open. The combined fetch's SHAs are read out of `FETCH_HEAD` by matching each
//! ref — see [`base_and_head_shas`].

use std::path::PathBuf;

use loopreview_core::{Diff, DiffError, DiffSource, RefSource};

use crate::git;

/// How a PR's base endpoint is chosen.
#[derive(Debug, PartialEq, Eq)]
enum BasePlan {
    /// A merged PR: compare against the merge commit's first parent (the base as
    /// it was at merge). The head is an ancestor of the *current* base tip, so a
    /// tip comparison would be empty.
    MergeParent(String),
    /// An open or closed-unmerged PR: compare against the current base tip.
    BaseTip,
}

/// Decide the base endpoint plan from the merge commit SHA (present only for a
/// merged PR). Pure, so the choice is testable without git.
fn base_plan(merged_base: Option<&str>) -> BasePlan {
    match merged_base {
        Some(sha) if !sha.is_empty() => BasePlan::MergeParent(sha.to_string()),
        _ => BasePlan::BaseTip,
    }
}

/// A diff source for a pull request, comparing `origin/<base_ref>` against the
/// fetched PR head.
pub struct PrSource {
    dir: PathBuf,
    base_ref: String,
    number: u64,
    /// The merge commit SHA when the PR is merged, else `None`.
    merged_base: Option<String>,
}

impl PrSource {
    /// Create a PR diff source for pull request `number` in the repository at
    /// `dir`, comparing against `base_ref` (the PR's target branch, e.g.
    /// `main`). `merged_base` is the merge commit SHA for a merged PR (its first
    /// parent becomes the base), or `None` for an open/closed-unmerged PR.
    pub fn new(
        dir: impl Into<PathBuf>,
        base_ref: impl Into<String>,
        number: u64,
        merged_base: Option<String>,
    ) -> PrSource {
        PrSource {
            dir: dir.into(),
            base_ref: base_ref.into(),
            number,
            merged_base,
        }
    }

    /// Fetch the head and base and resolve both to commit SHAs. The head is the
    /// PR's head commit (it survives the merge); the base follows [`base_plan`].
    ///
    /// Both endpoints are fetched in a single `git fetch` invocation, so a review
    /// opens on one network round-trip rather than two. Because a combined fetch
    /// writes several `FETCH_HEAD` lines, the head SHA is picked out by matching
    /// the `pull/<n>/head` ref (not by line position — `git fetch a b` makes no
    /// promise about `FETCH_HEAD` line order across versions).
    fn fetch_endpoints(&self) -> Result<(String, String), DiffError> {
        let head_refspec = format!("pull/{}/head", self.number);
        match base_plan(self.merged_base.as_deref()) {
            // A merged PR: one fetch brings down the PR head and the merge commit,
            // and the base is the merge commit's first parent (the base as it was
            // at merge). On any failure — the merge commit was garbage-collected
            // or the branch force-pushed away — fall back to the base tip, which
            // the open-PR path fetches robustly, rather than emit an empty diff.
            BasePlan::MergeParent(merge_sha) => {
                if git::fetch_many(&self.dir, &[head_refspec.as_str(), merge_sha.as_str()]).is_ok()
                    && let Ok(fetch_head) = git::read_fetch_head(&self.dir)
                    && let Some(head_sha) = head_sha_from_fetch_head(&fetch_head, self.number)
                    && let Ok(base_sha) = git::rev_parse(&self.dir, &format!("{merge_sha}^1"))
                {
                    return Ok((base_sha, head_sha));
                }
                self.fetch_head_and_base_tip(&head_refspec)
            }
            BasePlan::BaseTip => self.fetch_head_and_base_tip(&head_refspec),
        }
    }

    /// Fetch the PR head and the base branch tip together and read both SHAs from
    /// the combined `FETCH_HEAD`. Used directly for an open/unmerged PR, and as
    /// the merged path's fallback when the merge commit is unfetchable.
    fn fetch_head_and_base_tip(&self, head_refspec: &str) -> Result<(String, String), DiffError> {
        git::fetch_many(&self.dir, &[head_refspec, self.base_ref.as_str()])
            .map_err(to_diff_error)?;
        let fetch_head = git::read_fetch_head(&self.dir).map_err(to_diff_error)?;
        base_and_head_shas(&fetch_head, self.number).ok_or_else(|| DiffError::Command {
            program: "git".to_string(),
            code: -1,
            stderr: format!(
                "could not read the fetched SHAs for {head_refspec} and {} from FETCH_HEAD",
                self.base_ref
            ),
        })
    }
}

/// Parse one `FETCH_HEAD` line — `<sha>\t<merge-flag>\t<description>` — into its
/// commit SHA and the *ref* portion of the description (the text before the
/// ` of <url>` suffix).
///
/// The three-tab layout is `FETCH_HEAD`'s long-standing, documented format
/// (`git help gitrepository-layout`). The merge-flag column (empty or
/// `not-for-merge`) is ignored on purpose: with several refspecs fetched at once,
/// which entry git marks "for merge" is not something to depend on. Matching is
/// done against the ref portion rather than the whole line so a repository URL
/// that happens to contain a ref's text cannot be mistaken for that ref.
fn parse_fetch_head_line(line: &str) -> Option<(&str, &str)> {
    let mut cols = line.splitn(3, '\t');
    let sha = cols.next()?.trim();
    let _merge_flag = cols.next()?;
    let description = cols.next()?;
    if sha.is_empty() {
        return None;
    }
    let ref_part = description.split(" of ").next().unwrap_or(description);
    Some((sha, ref_part))
}

/// The `pull/<number>/head` SHA from `FETCH_HEAD` content — the entry whose ref
/// is the PR head, located by its ref marker rather than by line position. Used
/// when `FETCH_HEAD` also holds unrelated entries (the merged path fetches the
/// head alongside the merge commit).
fn head_sha_from_fetch_head(fetch_head: &str, number: u64) -> Option<String> {
    let marker = format!("pull/{number}/head");
    fetch_head
        .lines()
        .filter_map(parse_fetch_head_line)
        .find(|(_, ref_part)| ref_part.contains(&marker))
        .map(|(sha, _)| sha.to_string())
}

/// `(base_sha, head_sha)` from the two-entry `FETCH_HEAD` of the open-PR path
/// (the base tip and the PR head, fetched together). The head is the
/// `pull/<number>/head` entry; the base is the other entry. Identifying the head
/// by its marker — and taking the base as "the entry that is not the head" —
/// keeps this independent of both `FETCH_HEAD` line order and the base branch's
/// name.
fn base_and_head_shas(fetch_head: &str, number: u64) -> Option<(String, String)> {
    let marker = format!("pull/{number}/head");
    let mut base = None;
    let mut head = None;
    for (sha, ref_part) in fetch_head.lines().filter_map(parse_fetch_head_line) {
        if ref_part.contains(&marker) {
            head = Some(sha.to_string());
        } else {
            base = Some(sha.to_string());
        }
    }
    Some((base?, head?))
}

impl DiffSource for PrSource {
    fn load(&self) -> Result<Diff, DiffError> {
        let (base_sha, head_sha) = self.fetch_endpoints()?;
        // Three-dot: changes on the PR head since it diverged from the base,
        // which is exactly GitHub's "Files changed" comparison. RefSource fills
        // in provenance (merge-base as base, head SHA as head).
        let target = format!("{base_sha}...{head_sha}");
        RefSource::new(&self.dir, target).load()
    }

    fn describe(&self) -> String {
        format!("PR #{}", self.number)
    }
}

/// Map a GitHub fetch error into the diff pipeline's error type, so `PrSource`
/// can satisfy [`DiffSource`] while still surfacing network/auth context.
fn to_diff_error(err: crate::error::GithubError) -> DiffError {
    match err {
        crate::error::GithubError::Diff(e) => e,
        crate::error::GithubError::NotInstalled { program } => DiffError::Spawn {
            program,
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        },
        other => DiffError::Command {
            program: "git".to_string(),
            code: -1,
            stderr: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_names_the_pr() {
        let source = PrSource::new("/tmp/repo", "main", 42, None);
        assert_eq!(source.describe(), "PR #42");
    }

    /// A realistic two-line FETCH_HEAD for `git fetch origin main pull/7/head`.
    /// Note the head is listed *first* here, so the tests also prove SHA reading
    /// does not depend on line order matching the fetch's argument order.
    const FETCH_HEAD_OPEN: &str = "\
1111111111111111111111111111111111111111\tnot-for-merge\t'refs/pull/7/head' of https://github.com/owner/repo
2222222222222222222222222222222222222222\t\tbranch 'main' of https://github.com/owner/repo
";

    #[test]
    fn parses_a_fetch_head_line_into_sha_and_ref() {
        let (sha, ref_part) = parse_fetch_head_line(
            "abc123\tnot-for-merge\t'refs/pull/7/head' of https://github.com/owner/repo",
        )
        .expect("a well-formed line parses");
        assert_eq!(sha, "abc123");
        // The URL is dropped — only the ref name is kept for matching.
        assert_eq!(ref_part, "'refs/pull/7/head'");

        // A for-merge line (empty flag column) parses too.
        let (sha, ref_part) =
            parse_fetch_head_line("def456\t\tbranch 'main' of https://github.com/owner/repo")
                .expect("a for-merge line parses");
        assert_eq!(sha, "def456");
        assert_eq!(ref_part, "branch 'main'");

        // A blank or malformed line yields nothing rather than a bogus SHA.
        assert_eq!(parse_fetch_head_line(""), None);
        assert_eq!(parse_fetch_head_line("\t\t"), None);
    }

    #[test]
    fn reads_base_and_head_regardless_of_line_order() {
        let (base, head) = base_and_head_shas(FETCH_HEAD_OPEN, 7).expect("both SHAs read");
        assert_eq!(head, "1".repeat(40), "head is the pull/7/head entry");
        assert_eq!(base, "2".repeat(40), "base is the other entry");
    }

    #[test]
    fn head_sha_is_found_among_unrelated_fetch_head_entries() {
        // The merged path fetches the head alongside the merge commit, so
        // FETCH_HEAD carries an extra, unrelated entry that must be ignored.
        let fetch_head = format!(
            "{FETCH_HEAD_OPEN}3333333333333333333333333333333333333333\tnot-for-merge\t'3333333333333333333333333333333333333333' of https://github.com/owner/repo\n"
        );
        assert_eq!(
            head_sha_from_fetch_head(&fetch_head, 7),
            Some("1".repeat(40)),
            "the pull/7/head SHA is picked out by its ref marker"
        );
        // A different PR number is not present.
        assert_eq!(head_sha_from_fetch_head(&fetch_head, 8), None);
    }

    #[test]
    fn a_base_ref_named_like_the_head_ref_is_not_confused_for_it() {
        // A base branch whose name contains the head marker only in its URL must
        // still be classified as the base, not the head. Here the repo URL
        // contains "pull/7/head" but the base ref itself is `main`.
        let fetch_head = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tnot-for-merge\t'refs/pull/7/head' of https://github.com/owner/pull-7-head
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\t\tbranch 'main' of https://github.com/owner/pull-7-head
";
        let (base, head) = base_and_head_shas(fetch_head, 7).expect("both SHAs read");
        assert_eq!(head, "a".repeat(40));
        assert_eq!(base, "b".repeat(40), "the URL's marker text is ignored");
    }

    #[test]
    fn missing_entries_yield_none_rather_than_a_partial_pair() {
        // Only the base line — no head. base_and_head_shas must not invent a head.
        let only_base =
            "2222222222222222222222222222222222222222\t\tbranch 'main' of https://x/y\n";
        assert_eq!(base_and_head_shas(only_base, 7), None);
    }

    #[test]
    fn base_plan_uses_the_merge_parent_only_when_merged() {
        // A merged PR compares against the merge commit's first parent…
        assert_eq!(
            base_plan(Some("abc123")),
            BasePlan::MergeParent("abc123".to_string())
        );
        // …an open/closed-unmerged PR (no merge SHA) uses the base tip…
        assert_eq!(base_plan(None), BasePlan::BaseTip);
        // …and an empty SHA falls back to the tip rather than a bad `^1`.
        assert_eq!(base_plan(Some("")), BasePlan::BaseTip);
    }
}
