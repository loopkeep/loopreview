//! The diff model: the file / hunk / line types loopreview renders and reviews.
//!
//! These types are owned by loopreview-core and deliberately independent of any
//! diff engine (`similar`) or rendering crate, so the public API stays stable as
//! the internals evolve. Every line carries its old/new line numbers, and
//! [`Line::anchor`] turns a line into a [`LineAnchor`] — the stable
//! `(file, side, line)` position that future review comments attach to.

/// Which side of a diff a position is measured on.
///
/// A line removed from the original is addressed on the [`Side::Old`] side; a
/// line added, or an unchanged context line, is addressed on [`Side::New`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The original ("before") version of the file.
    Old,
    /// The changed ("after") version of the file.
    New,
}

/// A stable, comment-addressable position for one line of a diff: the file it
/// belongs to, which [`Side`] it is measured on, and the 1-based line number on
/// that side.
///
/// This is the foundation of loopreview's review model — a comment left while
/// reviewing is pinned to a `LineAnchor` so it can be relocated as the diff is
/// recomputed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LineAnchor {
    /// Path of the file the anchor refers to (see [`FileDiff::anchor_path`]).
    pub path: String,
    /// Which version of the file `line` is counted against.
    pub side: Side,
    /// 1-based line number on `side`.
    pub line: u32,
}

/// How a single line participates in the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged line present on both sides (shown for context).
    Context,
    /// A line added on the new side.
    Addition,
    /// A line removed from the old side.
    Deletion,
}

/// One line of a hunk, tagged with its role and its position on each side.
///
/// A [`LineKind::Context`] line carries both `old_lineno` and `new_lineno`; an
/// [`LineKind::Addition`] carries only `new_lineno`; a [`LineKind::Deletion`]
/// carries only `old_lineno`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Whether the line is context, an addition, or a deletion.
    pub kind: LineKind,
    /// The line's text, without its trailing newline.
    pub content: String,
    /// 1-based line number on the old side, when the line exists there.
    pub old_lineno: Option<u32>,
    /// 1-based line number on the new side, when the line exists there.
    pub new_lineno: Option<u32>,
}

impl Line {
    /// The comment anchor for this line within `path`.
    ///
    /// Deletions anchor on the [`Side::Old`] line they removed; additions and
    /// context lines anchor on their [`Side::New`] line (the current state of
    /// the file), which is what a reviewer points at.
    pub fn anchor(&self, path: impl Into<String>) -> LineAnchor {
        let (side, line) = match self.kind {
            LineKind::Deletion => (Side::Old, self.old_lineno.unwrap_or(0)),
            LineKind::Context | LineKind::Addition => (Side::New, self.new_lineno.unwrap_or(0)),
        };
        LineAnchor {
            path: path.into(),
            side,
            line,
        }
    }
}

/// How a file changed between the two sides of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    /// The file is new on the changed side.
    Added,
    /// The file was removed on the changed side.
    Deleted,
    /// The file exists on both sides with edited contents.
    Modified,
    /// The file was moved (and possibly edited).
    Renamed,
    /// The file was copied from another path (and possibly edited).
    Copied,
}

impl ChangeStatus {
    /// A short, single-word label for the status (`added`, `deleted`, …).
    pub fn label(self) -> &'static str {
        match self {
            ChangeStatus::Added => "added",
            ChangeStatus::Deleted => "deleted",
            ChangeStatus::Modified => "modified",
            ChangeStatus::Renamed => "renamed",
            ChangeStatus::Copied => "copied",
        }
    }
}

/// A contiguous run of changed and context lines within a file, corresponding to
/// one `@@ … @@` group in a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based first line number on the old side (0 when the hunk adds only).
    pub old_start: u32,
    /// Number of old-side lines the hunk spans.
    pub old_lines: u32,
    /// 1-based first line number on the new side (0 when the hunk removes only).
    pub new_start: u32,
    /// Number of new-side lines the hunk spans.
    pub new_lines: u32,
    /// The text after the `@@ … @@` marker (often the enclosing item), if any.
    pub section: Option<String>,
    /// The hunk's lines, in display order.
    pub lines: Vec<Line>,
}

impl Hunk {
    /// The unified-diff header for this hunk, e.g. `@@ -1,3 +1,4 @@`.
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_lines, self.new_start, self.new_lines
        )
    }

    /// Pair the deletion and addition lines that represent the same logical
    /// edit, for intra-line (word-level) diffing.
    ///
    /// Returns `(deletion_index, addition_index)` pairs of indices into
    /// [`Self::lines`]. Within each contiguous block of changed lines (a run of
    /// deletions and additions uninterrupted by context), the k-th deletion is
    /// paired with the k-th addition; surplus deletions or additions on either
    /// side are left unpaired (a whole-line insertion or removal).
    pub fn change_pairs(&self) -> Vec<(usize, usize)> {
        let mut pairs = Vec::new();
        let mut deletions: Vec<usize> = Vec::new();
        let mut additions: Vec<usize> = Vec::new();

        let mut flush = |dels: &mut Vec<usize>, adds: &mut Vec<usize>| {
            for (&d, &a) in dels.iter().zip(adds.iter()) {
                pairs.push((d, a));
            }
            dels.clear();
            adds.clear();
        };

        for (i, line) in self.lines.iter().enumerate() {
            match line.kind {
                LineKind::Deletion => deletions.push(i),
                LineKind::Addition => additions.push(i),
                LineKind::Context => flush(&mut deletions, &mut additions),
            }
        }
        flush(&mut deletions, &mut additions);
        pairs
    }
}

/// The complete set of changes to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The old path, or `None` when the file is newly [`ChangeStatus::Added`].
    pub old_path: Option<String>,
    /// The new path, or `None` when the file was [`ChangeStatus::Deleted`].
    pub new_path: Option<String>,
    /// How the file changed.
    pub status: ChangeStatus,
    /// The file's hunks; empty for binary files or pure renames.
    pub hunks: Vec<Hunk>,
    /// True when the file is binary (its contents are not shown line-by-line).
    pub binary: bool,
}

impl FileDiff {
    /// The path to show for this file: the new path when it still exists,
    /// otherwise the old path (for deletions).
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// The path a comment anchors against: the same as [`display_path`], i.e. the
    /// file as it exists after the change, falling back to the old path when the
    /// file was deleted.
    ///
    /// [`display_path`]: FileDiff::display_path
    pub fn anchor_path(&self) -> &str {
        self.display_path()
    }

    /// Count of added and removed lines across all hunks.
    pub fn line_stats(&self) -> (u32, u32) {
        let mut added = 0;
        let mut removed = 0;
        for hunk in &self.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Addition => added += 1,
                    LineKind::Deletion => removed += 1,
                    LineKind::Context => {}
                }
            }
        }
        (added, removed)
    }
}

/// Where the two sides of a diff came from: the commit each side is anchored
/// to, when known.
///
/// This is the foundation for reconstructing the history behind an outdated
/// review comment — a comment can be re-placed against `git show <sha>:<path>`.
/// A side that is not a commit (the working tree or the index) is `None`, as is
/// everything for a patch read from stdin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// Commit SHA of the old ("before") side, when it is a commit.
    pub base: Option<String>,
    /// Commit SHA of the new ("after") side, `None` for the working tree/index.
    pub head: Option<String>,
}

/// A whole diff: an ordered list of changed files plus where they came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// The changed files, in the order they should be presented.
    pub files: Vec<FileDiff>,
    /// The commits (if any) the two sides are anchored to.
    pub provenance: Provenance,
}

/// Aggregate counts for a [`Diff`], suitable for a summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffStats {
    /// Number of files changed.
    pub files: usize,
    /// Total added lines.
    pub insertions: u32,
    /// Total removed lines.
    pub deletions: u32,
}

impl Diff {
    /// True when no files changed.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Aggregate file and line counts across the diff.
    pub fn stats(&self) -> DiffStats {
        let mut stats = DiffStats {
            files: self.files.len(),
            insertions: 0,
            deletions: 0,
        };
        for file in &self.files {
            let (added, removed) = file.line_stats();
            stats.insertions += added;
            stats.deletions += removed;
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(content: &str, old: u32, new: u32) -> Line {
        Line {
            kind: LineKind::Context,
            content: content.to_string(),
            old_lineno: Some(old),
            new_lineno: Some(new),
        }
    }

    fn addition(content: &str, new: u32) -> Line {
        Line {
            kind: LineKind::Addition,
            content: content.to_string(),
            old_lineno: None,
            new_lineno: Some(new),
        }
    }

    fn deletion(content: &str, old: u32) -> Line {
        Line {
            kind: LineKind::Deletion,
            content: content.to_string(),
            old_lineno: Some(old),
            new_lineno: None,
        }
    }

    #[test]
    fn addition_anchors_on_new_side() {
        let anchor = addition("hello", 7).anchor("src/lib.rs");
        assert_eq!(
            anchor,
            LineAnchor {
                path: "src/lib.rs".to_string(),
                side: Side::New,
                line: 7,
            }
        );
    }

    #[test]
    fn deletion_anchors_on_old_side() {
        let anchor = deletion("gone", 4).anchor("src/lib.rs");
        assert_eq!(anchor.side, Side::Old);
        assert_eq!(anchor.line, 4);
    }

    #[test]
    fn context_anchors_on_new_side() {
        let anchor = context("same", 3, 5).anchor("f");
        assert_eq!(anchor.side, Side::New);
        assert_eq!(anchor.line, 5);
    }

    #[test]
    fn hunk_header_is_unified_format() {
        let hunk = Hunk {
            old_start: 1,
            old_lines: 3,
            new_start: 1,
            new_lines: 4,
            section: None,
            lines: vec![],
        };
        assert_eq!(hunk.header(), "@@ -1,3 +1,4 @@");
    }

    fn hunk_of(lines: Vec<Line>) -> Hunk {
        Hunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 0,
            section: None,
            lines,
        }
    }

    #[test]
    fn change_pairs_matches_deletions_to_additions_in_a_block() {
        // context / delete / add / context: the delete and add pair up.
        let hunk = hunk_of(vec![
            context("keep", 1, 1),
            deletion("old", 2),
            addition("new", 2),
            context("tail", 3, 3),
        ]);
        assert_eq!(hunk.change_pairs(), vec![(1, 2)]);
    }

    #[test]
    fn change_pairs_leaves_surplus_lines_unpaired() {
        // Two deletions, one addition: only the first deletion pairs.
        let hunk = hunk_of(vec![deletion("a", 1), deletion("b", 2), addition("c", 1)]);
        assert_eq!(hunk.change_pairs(), vec![(0, 2)]);
    }

    #[test]
    fn change_pairs_does_not_cross_a_context_line() {
        // A deletion and addition separated by context are different edits.
        let hunk = hunk_of(vec![
            deletion("x", 1),
            context("gap", 2, 1),
            addition("y", 2),
        ]);
        assert!(hunk.change_pairs().is_empty());
    }

    #[test]
    fn change_pairs_of_a_pure_insertion_is_empty() {
        let hunk = hunk_of(vec![addition("a", 1), addition("b", 2)]);
        assert!(hunk.change_pairs().is_empty());
    }

    #[test]
    fn display_path_prefers_new_then_old() {
        let deleted = FileDiff {
            old_path: Some("old.rs".into()),
            new_path: None,
            status: ChangeStatus::Deleted,
            hunks: vec![],
            binary: false,
        };
        assert_eq!(deleted.display_path(), "old.rs");
        assert_eq!(deleted.anchor_path(), "old.rs");

        let renamed = FileDiff {
            old_path: Some("old.rs".into()),
            new_path: Some("new.rs".into()),
            status: ChangeStatus::Renamed,
            hunks: vec![],
            binary: false,
        };
        assert_eq!(renamed.display_path(), "new.rs");
    }

    #[test]
    fn stats_aggregate_files_and_lines() {
        let file = FileDiff {
            old_path: Some("a".into()),
            new_path: Some("a".into()),
            status: ChangeStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 2,
                section: None,
                lines: vec![
                    context("keep", 1, 1),
                    deletion("drop", 2),
                    addition("add one", 2),
                    addition("add two", 3),
                ],
            }],
            binary: false,
        };
        assert_eq!(file.line_stats(), (2, 1));

        let diff = Diff {
            files: vec![file],
            ..Diff::default()
        };
        let stats = diff.stats();
        assert_eq!(stats.files, 1);
        assert_eq!(stats.insertions, 2);
        assert_eq!(stats.deletions, 1);
    }
}
