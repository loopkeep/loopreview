//! Syntax highlighting: an independent layer over the core diff model.
//!
//! This module takes plain line text (from [`loopreview_core`]'s model) and
//! returns per-line runs of colored text. It knows nothing about the diff model
//! beyond the strings it is handed, and the core knows nothing about it — which
//! keeps highlighting swappable and the core publishable on its own.

use std::path::Path;

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    HighlightIterator, HighlightState, Highlighter as ThemeHighlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

/// A run of text sharing one foreground color.
#[derive(Debug, Clone)]
pub struct Span {
    /// The text of the run (without any trailing newline).
    pub text: String,
    /// The foreground color as 8-bit RGB.
    pub color: (u8, u8, u8),
}

/// The incremental highlight state for one file: the syntect parse and highlight
/// state carried from one line to the next. Owned (no borrows), so it can be
/// stored per file and advanced on demand as lines scroll into view — only the
/// lines actually shown are highlighted, not the whole file up front.
pub struct LineHighlighter {
    parse: ParseState,
    highlight: HighlightState,
}

/// Loads syntaxes and a theme once, then highlights lines on demand.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    /// Build a highlighter from the bundled syntax set (via `two-face`) and a
    /// dark default theme.
    pub fn new() -> Highlighter {
        let syntaxes = two_face::syntax::extra_newlines();
        let theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
        Highlighter { syntaxes, theme }
    }

    /// Begin incremental, line-by-line highlighting for the file at `path`. The
    /// syntax is chosen from the extension, falling back to plain text. Advance
    /// it one line at a time with [`Highlighter::highlight_next`].
    pub fn line_highlighter(&self, path: &str) -> LineHighlighter {
        let syntax = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let theme_highlighter = ThemeHighlighter::new(&self.theme);
        LineHighlighter {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&theme_highlighter, ScopeStack::new()),
        }
    }

    /// A theme highlighter to reuse across a batch of [`Highlighter::highlight_next`]
    /// calls (building it per line is wasteful; it is stateless across lines).
    pub fn theme_highlighter(&self) -> ThemeHighlighter<'_> {
        ThemeHighlighter::new(&self.theme)
    }

    /// Highlight the next line in sequence, advancing `state`. Lines must be fed
    /// in file order so multi-line constructs (strings, comments) carry state.
    /// `theme` comes from [`Highlighter::theme_highlighter`] and can be reused.
    pub fn highlight_next(
        &self,
        state: &mut LineHighlighter,
        theme: &ThemeHighlighter,
        line: &str,
    ) -> Vec<Span> {
        // `parse_line` expects a trailing newline to close the line's scope.
        let with_newline = format!("{line}\n");
        let ops = match state.parse.parse_line(&with_newline, &self.syntaxes) {
            Ok(ops) => ops,
            // On a parse error, keep the state unchanged and show the raw line.
            Err(_) => {
                return vec![Span {
                    text: line.to_string(),
                    color: (200, 200, 200),
                }];
            }
        };
        HighlightIterator::new(&mut state.highlight, &ops, &with_newline, theme)
            .map(|(style, text)| Span {
                text: text.trim_end_matches('\n').to_string(),
                color: (style.foreground.r, style.foreground.g, style.foreground.b),
            })
            .filter(|span| !span.text.is_empty())
            .collect()
    }

    /// Highlight `lines` as source in `lang` (a markdown code-block info string,
    /// e.g. `rust`), falling back to plain text when the language is unknown.
    pub fn highlight_by_lang(&self, lang: &str, lines: &[&str]) -> Vec<Vec<Span>> {
        let syntax = self
            .syntaxes
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        self.highlight_with(syntax, lines)
    }

    fn highlight_with(
        &self,
        syntax: &syntect::parsing::SyntaxReference,
        lines: &[&str],
    ) -> Vec<Vec<Span>> {
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        lines
            .iter()
            .map(|line| self.highlight_one(&mut highlighter, line))
            .collect()
    }

    fn highlight_one(&self, highlighter: &mut HighlightLines, line: &str) -> Vec<Span> {
        // `highlight_line` expects a trailing newline to close the line's scope.
        let with_newline = format!("{line}\n");
        match highlighter.highlight_line(&with_newline, &self.syntaxes) {
            Ok(runs) => runs
                .into_iter()
                .map(|(style, text)| Span {
                    text: text.trim_end_matches('\n').to_string(),
                    color: (style.foreground.r, style.foreground.g, style.foreground.b),
                })
                .filter(|span| !span.text.is_empty())
                .collect(),
            // On any highlighting error, fall back to the raw line, uncolored.
            Err(_) => vec![Span {
                text: line.to_string(),
                color: (200, 200, 200),
            }],
        }
    }
}

impl Default for Highlighter {
    fn default() -> Highlighter {
        Highlighter::new()
    }
}
