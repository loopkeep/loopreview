//! loopreview-core: the diff model and source abstraction behind loopreview, a
//! review-first diff TUI.
//!
//! This crate is deliberately free of any UI, rendering, or syntax-highlighting
//! dependency so it can be reused and, eventually, published on its own. It
//! provides three things:
//!
//! * the [`model`] — owned `File` / `Hunk` / `Line` types where every line
//!   carries a comment-addressable [`LineAnchor`](model::LineAnchor);
//! * a [`patch`] parser from unified-diff text into that model;
//! * the [`DiffSource`] trait and its built-in implementations
//!   ([`WorktreeSource`], [`RefSource`], [`StdinPatchSource`]), which is how the
//!   rest of loopreview obtains a [`Diff`](model::Diff) without caring where it
//!   came from.

pub mod error;
pub mod git;
pub mod intraline;
pub mod model;
pub mod patch;
pub mod source;

pub use error::DiffError;
pub use intraline::{Segment, word_diff};
pub use model::{
    ChangeStatus, Diff, DiffStats, FileDiff, Hunk, Line, LineAnchor, LineKind, Provenance, Side,
};
pub use source::{DiffSource, FilePatchSource, RefSource, StdinPatchSource, WorktreeSource};
