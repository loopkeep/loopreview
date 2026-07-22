//! Word-level (intra-line) diffing of a changed line pair, built on `similar`.
//!
//! A line-level diff shows that a line changed; an intra-line diff shows *what*
//! within it changed. Given the old and new text of a modified line, this splits
//! each side into [`Segment`]s, marking the runs that actually differ so a
//! renderer can emphasize them (GitHub-style). It is a pure, standalone layer:
//! the diff model does not depend on it, and it does not depend on the model.

use similar::{ChangeTag, TextDiff};

/// A run of one side of a changed line, flagged by whether it differs from the
/// other side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The run's text.
    pub text: String,
    /// True when this run is part of the change (added on the new side, or
    /// removed from the old side); false for text common to both sides.
    pub changed: bool,
}

/// Compute word-level segments for a changed line pair.
///
/// Returns `(old_segments, new_segments)`: the old line split into removed vs.
/// common runs, and the new line split into added vs. common runs. Adjacent
/// runs with the same flag are merged.
pub fn word_diff(old: &str, new: &str) -> (Vec<Segment>, Vec<Segment>) {
    let diff = TextDiff::from_words(old, new);
    let mut old_segments = SegmentBuilder::default();
    let mut new_segments = SegmentBuilder::default();

    for change in diff.iter_all_changes() {
        let value = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                old_segments.push(value, false);
                new_segments.push(value, false);
            }
            ChangeTag::Delete => old_segments.push(value, true),
            ChangeTag::Insert => new_segments.push(value, true),
        }
    }

    (old_segments.finish(), new_segments.finish())
}

/// Accumulates runs, merging consecutive runs that share a `changed` flag.
#[derive(Default)]
struct SegmentBuilder {
    segments: Vec<Segment>,
}

impl SegmentBuilder {
    fn push(&mut self, text: &str, changed: bool) {
        if let Some(last) = self.segments.last_mut()
            && last.changed == changed
        {
            last.text.push_str(text);
        } else {
            self.segments.push(Segment {
                text: text.to_string(),
                changed,
            });
        }
    }

    fn finish(self) -> Vec<Segment> {
        self.segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect just the text of the changed runs on one side.
    fn changed(segments: &[Segment]) -> Vec<&str> {
        segments
            .iter()
            .filter(|s| s.changed)
            .map(|s| s.text.as_str())
            .collect()
    }

    #[test]
    fn isolates_the_changed_word() {
        let (old, new) = word_diff("foo bar baz", "foo qux baz");
        assert_eq!(changed(&old), vec!["bar"]);
        assert_eq!(changed(&new), vec!["qux"]);
        // The unchanged runs still reconstruct each full line.
        let old_text: String = old.iter().map(|s| s.text.as_str()).collect();
        let new_text: String = new.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(old_text, "foo bar baz");
        assert_eq!(new_text, "foo qux baz");
    }

    #[test]
    fn identical_lines_have_no_changed_runs() {
        let (old, new) = word_diff("same text", "same text");
        assert!(changed(&old).is_empty());
        assert!(changed(&new).is_empty());
    }

    #[test]
    fn a_pure_insertion_marks_only_the_new_side() {
        let (old, new) = word_diff("hello world", "hello brave world");
        assert!(changed(&old).is_empty());
        // The inserted run carries its adjacent whitespace so the common words
        // still line up; the old side has nothing changed.
        assert_eq!(changed(&new), vec!["brave "]);
        let new_text: String = new.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(new_text, "hello brave world");
    }
}
