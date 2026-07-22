//! The review UI's colors, in one place.
//!
//! Every fixed non-generic color the TUI uses is named here so the look is
//! defined and tuned centrally, not scattered across the renderer. Two kinds of
//! color stay inline at the call site: truly generic ANSI colors (a plain
//! `DarkGray` for dim text, `White` for body text), and colors computed at
//! runtime from data — the syntax-highlight theme (`syntect`) yields per-token
//! RGB that is converted, not chosen. A color earns a name here when it is a
//! fixed choice carrying a specific role, or a custom RGB tuned for a dark
//! terminal.

use ratatui::style::Color;

// -- diff line tints --------------------------------------------------------

/// Subtle row tints for changed lines, and the stronger tint for the exact
/// words that changed within them (readable on a dark terminal).
pub const ADD_BG: Color = Color::Rgb(18, 44, 26);
pub const DEL_BG: Color = Color::Rgb(52, 24, 27);
pub const ADD_EMPH_BG: Color = Color::Rgb(30, 84, 44);
pub const DEL_EMPH_BG: Color = Color::Rgb(96, 40, 46);
/// Background of a side-by-side cell with no line (the other side changed).
pub const ABSENT_BG: Color = Color::Rgb(22, 24, 30);

// -- cursor and headers -----------------------------------------------------

/// Background of the line the cursor is on (when it has no diff tint).
pub const CURSOR_BG: Color = Color::Rgb(38, 43, 56);
/// The line cursor when the body is the inactive pane (sidebar focused): a
/// faint fill so it reads as "not the active target".
pub const CURSOR_DIM_BG: Color = Color::Rgb(30, 33, 42);
/// Faint full-width band behind every file header (even without the cursor), so
/// headers read as list dividers and inter-file blank lines can be dropped.
pub const HEADER_BG: Color = Color::Rgb(28, 31, 40);
/// Background of the file header the cursor rests on — brighter than a content
/// line's cursor, since headers are the diff's few anchors and must stand out.
pub const HEADER_CURSOR_BG: Color = Color::Rgb(54, 64, 92);
/// The header cursor when the body is the inactive pane (sidebar focused).
pub const HEADER_CURSOR_DIM_BG: Color = Color::Rgb(40, 45, 62);

// -- selection and sidebar --------------------------------------------------

/// Background of a selected sidebar / finder row — a clear blue, distinct at a
/// glance from the current-file and cursor tints.
pub const SEL_BG: Color = Color::Rgb(48, 66, 106);
/// Background of lines in a range selection (for a multi-line comment).
pub const SELECTION_BG: Color = Color::Rgb(38, 48, 74);
/// Background marking the sidebar file currently shown in the body (a subtle
/// blue tint under a cyan bar), distinct from the stronger selection color.
pub const SIDEBAR_CURRENT_BG: Color = Color::Rgb(33, 43, 62);
/// The sidebar's resting selection when the sidebar is not the focused pane —
/// a faint fill, dimmer than the focused selection and the current-file tint.
pub const SIDEBAR_SEL_DIM_BG: Color = Color::Rgb(31, 35, 47);
/// Background of the selected comment in the Conversation view.
pub const CONV_SELECT_BG: Color = Color::Rgb(40, 46, 60);

// -- chrome -----------------------------------------------------------------

/// Accent used on the focused pane's divider (dim when it is not focused), and
/// the accent for pull-request affordances in the footer.
pub const FOCUS_ACCENT: Color = Color::Cyan;
/// The bar background used for the header and footer.
pub const BAR_BG: Color = Color::Rgb(30, 33, 40);
/// The muted blue used for pull-request-only affordances (submit/refresh hints,
/// the horizontal-scroll indicator).
pub const PR_ACCENT: Color = Color::Rgb(120, 160, 220);

// -- comment threads --------------------------------------------------------

/// The left gutter bar drawn beside an inline comment thread.
pub const COMMENT_BAR: Color = Color::Rgb(90, 130, 200);
/// Subtle background on the anchored line(s) within a thread's code excerpt.
pub const EXCERPT_ANCHOR_BG: Color = Color::Rgb(46, 42, 30);
/// The disposition badges: a draft (queued to submit) draws attention; a local
/// note (never sent) stays subdued.
pub const BADGE_DRAFT: Color = Color::Yellow;
pub const BADGE_LOCAL: Color = Color::DarkGray;
/// The muted line-number/context text in a placed thread's code excerpt.
pub const EXCERPT_CONTEXT_FG: Color = Color::Rgb(120, 120, 130);
/// The saved-snippet text shown for an outdated thread with no reconstruction.
pub const SNIPPET_FG: Color = Color::Rgb(90, 90, 100);

// -- finder and markdown ----------------------------------------------------

/// Background of the fuzzy file-finder's rows.
pub const FINDER_BG: Color = Color::Rgb(20, 22, 28);
/// Inline `code`: a warm foreground on a faint fill.
pub const CODE_FG: Color = Color::Rgb(220, 180, 120);
pub const CODE_INLINE_BG: Color = Color::Rgb(40, 40, 48);
/// A markdown link.
pub const LINK_FG: Color = Color::Rgb(110, 140, 200);
/// A fenced code block's background.
pub const CODE_BLOCK_BG: Color = Color::Rgb(30, 32, 40);
