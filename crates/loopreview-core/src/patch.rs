//! A parser from unified-diff text (the output of `git diff`) into the
//! [`Diff`](crate::model::Diff) model.
//!
//! This is what turns a piped patch (`git diff | lr`) into reviewable hunks, and
//! it is exercised directly against fixture strings rather than by shelling out.
//! Hunk bodies are bounded by the line counts in each `@@ … @@` header, so the
//! parser never has to guess where one hunk ends and the next file begins.

use crate::error::DiffError;
use crate::model::{ChangeStatus, Diff, FileDiff, Hunk, Line, LineKind};

/// Parse unified-diff `input` into a [`Diff`].
///
/// Accepts the multi-file output of `git diff` as well as a plain single-file
/// `diff -u`. Metadata it does not model (index lines, mode changes) is ignored.
pub fn parse(input: &str) -> Result<Diff, DiffError> {
    let raw: Vec<&str> = input.lines().collect();
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileBuilder> = None;
    let mut i = 0;

    while i < raw.len() {
        let line = raw[i];

        // A `diff --git` line always starts a new file.
        if let Some((old, new)) = parse_diff_git(line) {
            if let Some(b) = current.take() {
                files.push(b.finish());
            }
            current = Some(FileBuilder::new(old, new));
            i += 1;
            continue;
        }

        // A bare `--- ` (plain `diff -u`, no `diff --git`) also starts a file.
        if current.is_none() && line.starts_with("--- ") {
            current = Some(FileBuilder::default());
        }

        let Some(builder) = current.as_mut() else {
            // Preamble before the first file (commit message, etc.) — skip.
            i += 1;
            continue;
        };

        if line.starts_with("new file mode") {
            builder.status = Some(ChangeStatus::Added);
            builder.old_path = None;
        } else if line.starts_with("deleted file mode") {
            builder.status = Some(ChangeStatus::Deleted);
            builder.new_path = None;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            builder.old_path = Some(p.to_string());
            builder.status = Some(ChangeStatus::Renamed);
        } else if let Some(p) = line.strip_prefix("rename to ") {
            builder.new_path = Some(p.to_string());
            builder.status = Some(ChangeStatus::Renamed);
        } else if let Some(p) = line.strip_prefix("copy from ") {
            builder.old_path = Some(p.to_string());
            builder.status = Some(ChangeStatus::Copied);
        } else if let Some(p) = line.strip_prefix("copy to ") {
            builder.new_path = Some(p.to_string());
            builder.status = Some(ChangeStatus::Copied);
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            builder.binary = true;
        } else if let Some(p) = line.strip_prefix("--- ") {
            builder.set_old_marker(p);
        } else if let Some(p) = line.strip_prefix("+++ ") {
            builder.set_new_marker(p);
        } else if line.starts_with("@@") {
            let header = parse_hunk_header(line)
                .ok_or_else(|| DiffError::Parse(format!("malformed hunk header: {line}")))?;
            let (hunk, consumed) = parse_hunk_body(&raw[i + 1..], header);
            builder.hunks.push(hunk);
            i += 1 + consumed;
            continue;
        }
        // Anything else (`index …`, mode lines, similarity index) is ignored.
        i += 1;
    }

    if let Some(b) = current.take() {
        files.push(b.finish());
    }
    Ok(Diff {
        files,
        ..Diff::default()
    })
}

/// Accumulates one file's metadata and hunks while parsing.
#[derive(Default)]
struct FileBuilder {
    old_path: Option<String>,
    new_path: Option<String>,
    status: Option<ChangeStatus>,
    binary: bool,
    hunks: Vec<Hunk>,
}

impl FileBuilder {
    fn new(old: Option<String>, new: Option<String>) -> FileBuilder {
        FileBuilder {
            old_path: old,
            new_path: new,
            ..FileBuilder::default()
        }
    }

    fn set_old_marker(&mut self, marker: &str) {
        match strip_marker(marker) {
            Some(path) => self.old_path = Some(path),
            None => {
                self.old_path = None;
                self.status.get_or_insert(ChangeStatus::Added);
            }
        }
    }

    fn set_new_marker(&mut self, marker: &str) {
        match strip_marker(marker) {
            Some(path) => self.new_path = Some(path),
            None => {
                self.new_path = None;
                self.status.get_or_insert(ChangeStatus::Deleted);
            }
        }
    }

    fn finish(self) -> FileDiff {
        let status = self.status.unwrap_or_else(|| {
            match (self.old_path.is_some(), self.new_path.is_some()) {
                (false, true) => ChangeStatus::Added,
                (true, false) => ChangeStatus::Deleted,
                _ => ChangeStatus::Modified,
            }
        });
        FileDiff {
            old_path: self.old_path,
            new_path: self.new_path,
            status,
            hunks: self.hunks,
            binary: self.binary,
        }
    }
}

/// Resolve a `--- ` / `+++ ` marker into a path, or `None` for `/dev/null`.
///
/// Trims a trailing tab-separated timestamp (from `diff -u`) and strips the
/// conventional `a/` or `b/` prefix.
fn strip_marker(marker: &str) -> Option<String> {
    let path = marker.split('\t').next().unwrap_or(marker);
    if path == "/dev/null" {
        return None;
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    Some(path.to_string())
}

/// Parse a `diff --git a/OLD b/NEW` line into best-effort paths.
///
/// The `---`/`+++`/`rename` lines that follow are authoritative and override
/// these, so this only needs to hold for mode-only or binary changes.
fn parse_diff_git(line: &str) -> Option<(Option<String>, Option<String>)> {
    let rest = line.strip_prefix("diff --git ")?;
    // Split on the " b/" that separates the two prefixed paths.
    let (old, new) = rest.split_once(" b/")?;
    let old = old.strip_prefix("a/").unwrap_or(old);
    Some((Some(old.to_string()), Some(new.to_string())))
}

/// A parsed `@@ … @@` header.
struct HunkHeader {
    old_start: u32,
    old_lines: u32,
    new_start: u32,
    new_lines: u32,
    section: Option<String>,
}

fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let rest = line.strip_prefix("@@ ")?;
    let close = rest.find(" @@")?;
    let ranges = &rest[..close];
    let section = rest[close + 3..].trim();
    let (old, new) = ranges.split_once(' ')?;
    let (old_start, old_lines) = parse_range(old.strip_prefix('-')?)?;
    let (new_start, new_lines) = parse_range(new.strip_prefix('+')?)?;
    Some(HunkHeader {
        old_start,
        old_lines,
        new_start,
        new_lines,
        section: (!section.is_empty()).then(|| section.to_string()),
    })
}

/// Parse a `start[,count]` range; `count` defaults to 1 when omitted.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// Parse a hunk body, consuming exactly the lines the header's counts describe.
///
/// Returns the hunk and the number of raw lines consumed (including any
/// `\ No newline at end of file` markers, which do not affect the counts).
fn parse_hunk_body(raw: &[&str], header: HunkHeader) -> (Hunk, usize) {
    let mut lines = Vec::new();
    let mut old_no = header.old_start;
    let mut new_no = header.new_start;
    let mut rem_old = header.old_lines;
    let mut rem_new = header.new_lines;
    let mut consumed = 0;

    for &line in raw {
        if rem_old == 0 && rem_new == 0 {
            break;
        }
        match line.chars().next() {
            Some(' ') => {
                lines.push(Line {
                    kind: LineKind::Context,
                    content: line[1..].to_string(),
                    old_lineno: Some(old_no),
                    new_lineno: Some(new_no),
                });
                old_no += 1;
                new_no += 1;
                rem_old = rem_old.saturating_sub(1);
                rem_new = rem_new.saturating_sub(1);
            }
            Some('+') => {
                lines.push(Line {
                    kind: LineKind::Addition,
                    content: line[1..].to_string(),
                    old_lineno: None,
                    new_lineno: Some(new_no),
                });
                new_no += 1;
                rem_new = rem_new.saturating_sub(1);
            }
            Some('-') => {
                lines.push(Line {
                    kind: LineKind::Deletion,
                    content: line[1..].to_string(),
                    old_lineno: Some(old_no),
                    new_lineno: None,
                });
                old_no += 1;
                rem_old = rem_old.saturating_sub(1);
            }
            // "\ No newline at end of file": consume without counting.
            Some('\\') => {}
            // A truly empty line stands in for a blank context line.
            None if rem_old > 0 && rem_new > 0 => {
                lines.push(Line {
                    kind: LineKind::Context,
                    content: String::new(),
                    old_lineno: Some(old_no),
                    new_lineno: Some(new_no),
                });
                old_no += 1;
                new_no += 1;
                rem_old -= 1;
                rem_new -= 1;
            }
            // Unexpected content — the hunk ended earlier than its counts claimed.
            _ => break,
        }
        consumed += 1;
    }

    (
        Hunk {
            old_start: header.old_start,
            old_lines: header.old_lines,
            new_start: header.new_start,
            new_lines: header.new_lines,
            section: header.section,
            lines,
        },
        consumed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Side;

    #[test]
    fn parses_a_simple_modification() {
        let patch = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@ fn main()
 keep
-old line
+new line
 tail
";
        let diff = parse(patch).unwrap();
        assert_eq!(diff.files.len(), 1);
        let f = &diff.files[0];
        assert_eq!(f.old_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(f.new_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(f.status, ChangeStatus::Modified);
        assert!(!f.binary);
        assert_eq!(f.hunks.len(), 1);

        let h = &f.hunks[0];
        assert_eq!(h.section.as_deref(), Some("fn main()"));
        assert_eq!(h.header(), "@@ -1,3 +1,3 @@");
        let kinds: Vec<_> = h.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Context,
                LineKind::Deletion,
                LineKind::Addition,
                LineKind::Context,
            ]
        );
        // Anchors: deletion on old side, addition on new side.
        assert_eq!(h.lines[1].anchor("src/lib.rs").side, Side::Old);
        assert_eq!(h.lines[1].anchor("src/lib.rs").line, 2);
        assert_eq!(h.lines[2].anchor("src/lib.rs").side, Side::New);
        assert_eq!(h.lines[2].anchor("src/lib.rs").line, 2);
        assert_eq!(h.lines[3].old_lineno, Some(3));
        assert_eq!(h.lines[3].new_lineno, Some(3));
    }

    #[test]
    fn parses_an_added_file() {
        let patch = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 000..abc
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+first
+second
";
        let diff = parse(patch).unwrap();
        let f = &diff.files[0];
        assert_eq!(f.status, ChangeStatus::Added);
        assert_eq!(f.old_path, None);
        assert_eq!(f.new_path.as_deref(), Some("new.txt"));
        assert_eq!(f.display_path(), "new.txt");
        assert_eq!(f.hunks[0].lines.len(), 2);
        assert_eq!(f.hunks[0].lines[0].new_lineno, Some(1));
        assert_eq!(f.hunks[0].lines[1].new_lineno, Some(2));
    }

    #[test]
    fn parses_a_deleted_file() {
        let patch = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index abc..000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-was here
-and here
";
        let diff = parse(patch).unwrap();
        let f = &diff.files[0];
        assert_eq!(f.status, ChangeStatus::Deleted);
        assert_eq!(f.new_path, None);
        assert_eq!(f.old_path.as_deref(), Some("gone.txt"));
        assert_eq!(f.display_path(), "gone.txt");
        assert!(
            f.hunks[0]
                .lines
                .iter()
                .all(|l| l.kind == LineKind::Deletion)
        );
    }

    #[test]
    fn parses_a_rename_with_edits() {
        let patch = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 80%
rename from old/name.rs
rename to new/name.rs
index 111..222 100644
--- a/old/name.rs
+++ b/new/name.rs
@@ -1 +1 @@
-before
+after
";
        let diff = parse(patch).unwrap();
        let f = &diff.files[0];
        assert_eq!(f.status, ChangeStatus::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(f.new_path.as_deref(), Some("new/name.rs"));
        // Omitted count defaults to 1.
        assert_eq!(f.hunks[0].header(), "@@ -1,1 +1,1 @@");
    }

    #[test]
    fn parses_a_pure_rename_without_hunks() {
        let patch = "\
diff --git a/a.txt b/b.txt
similarity index 100%
rename from a.txt
rename to b.txt
";
        let diff = parse(patch).unwrap();
        let f = &diff.files[0];
        assert_eq!(f.status, ChangeStatus::Renamed);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn flags_binary_files() {
        let patch = "\
diff --git a/logo.png b/logo.png
index 111..222 100644
Binary files a/logo.png and b/logo.png differ
";
        let diff = parse(patch).unwrap();
        let f = &diff.files[0];
        assert!(f.binary);
        assert_eq!(f.status, ChangeStatus::Modified);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parses_multiple_files_and_hunks() {
        let patch = "\
diff --git a/one.txt b/one.txt
index 1..2 100644
--- a/one.txt
+++ b/one.txt
@@ -1,2 +1,2 @@
 a
-b
+B
@@ -10,2 +10,3 @@
 x
+y
 z
diff --git a/two.txt b/two.txt
index 3..4 100644
--- a/two.txt
+++ b/two.txt
@@ -1 +1 @@
-p
+q
";
        let diff = parse(patch).unwrap();
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].hunks.len(), 2);
        // Second hunk keeps its own line numbering.
        let second = &diff.files[0].hunks[1];
        assert_eq!(second.new_start, 10);
        assert_eq!(second.lines[1].new_lineno, Some(11));
        assert_eq!(diff.files[1].hunks.len(), 1);
        let stats = diff.stats();
        assert_eq!(stats.files, 2);
    }

    #[test]
    fn empty_input_is_an_empty_diff() {
        assert!(parse("").unwrap().is_empty());
    }
}
