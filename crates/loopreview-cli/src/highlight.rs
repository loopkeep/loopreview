//! Syntax highlighting: an independent layer over the core diff model.
//!
//! This module takes plain line text (from [`loopreview_core`]'s model) and
//! returns per-line runs of colored text. It knows nothing about the diff model
//! beyond the strings it is handed, and the core knows nothing about it — which
//! keeps highlighting swappable and the core publishable on its own.

use std::path::Path;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// A run of text sharing one foreground color.
#[derive(Debug, Clone)]
pub struct Span {
    /// The text of the run (without any trailing newline).
    pub text: String,
    /// The foreground color as 8-bit RGB.
    pub color: (u8, u8, u8),
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

    /// Highlight `lines`, which are the successive line texts of the file at
    /// `path`, as one unit so multi-line constructs carry state between lines.
    ///
    /// The syntax is chosen from `path`'s extension, falling back to plain text.
    /// Each returned inner vector is the colored runs for the corresponding
    /// input line.
    pub fn highlight(&self, path: &str, lines: &[&str]) -> Vec<Vec<Span>> {
        let syntax = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        self.highlight_with(syntax, lines)
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
