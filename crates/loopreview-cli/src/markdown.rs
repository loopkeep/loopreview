//! A small markdown renderer for comment bodies.
//!
//! Parses with `pulldown-cmark` and paints to ratatui lines: headings, emphasis,
//! inline and fenced code (syntax-highlighted via [`Highlighter`]), lists, and
//! block quotes. Paragraphs word-wrap to a width when one is given (the
//! Conversation pane), or render unwrapped for tight inline display.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use unicode_width::UnicodeWidthStr;

use crate::palette;

use crate::highlight::Highlighter;

/// Render markdown `text` to styled lines. `wrap` is the wrap width, or `None`
/// to keep each paragraph on one (clipped) line.
pub fn render(
    text: &str,
    wrap: Option<usize>,
    highlighter: &Highlighter,
) -> Vec<TextLine<'static>> {
    let mut renderer = Renderer {
        wrap,
        highlighter,
        out: Vec::new(),
        spans: Vec::new(),
        style: Style::default(),
        list: Vec::new(),
        quote: 0,
        code: None,
        link: None,
    };
    let options = Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// A markdown list context: an ordered counter, or a bullet (`None`).
struct List {
    next: Option<u64>,
}

struct Renderer<'a> {
    wrap: Option<usize>,
    highlighter: &'a Highlighter,
    out: Vec<TextLine<'static>>,
    /// Inline spans accumulating for the current block.
    spans: Vec<TextSpan<'static>>,
    /// The active inline style (emphasis/strong/strikethrough).
    style: Style,
    list: Vec<List>,
    quote: usize,
    /// Fenced code-block language + collected text, while inside one.
    code: Option<(String, String)>,
    /// A link's destination, while inside one.
    link: Option<String>,
}

impl Renderer<'_> {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, buf)) = &mut self.code {
                    buf.push_str(&text);
                } else {
                    self.spans
                        .push(TextSpan::styled(text.into_string(), self.style));
                }
            }
            Event::Code(code) => {
                self.spans.push(TextSpan::styled(
                    code.into_string(),
                    self.style.fg(palette::CODE_FG).bg(palette::CODE_INLINE_BG),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                self.spans.push(TextSpan::styled(" ", self.style));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.into_string()),
            Tag::List(start) => self.list.push(List { next: start }),
            Tag::BlockQuote(_) => self.quote += 1,
            Tag::CodeBlock(kind) => {
                self.flush_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .into_string()
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Emphasis => self.style = self.style.remove_modifier(Modifier::ITALIC),
            TagEnd::Strong => self.style = self.style.remove_modifier(Modifier::BOLD),
            TagEnd::Strikethrough => self.style = self.style.remove_modifier(Modifier::CROSSED_OUT),
            TagEnd::Link => {
                if let Some(url) = self.link.take() {
                    self.spans.push(TextSpan::styled(
                        format!(" ({url})"),
                        self.style.fg(palette::LINK_FG),
                    ));
                }
            }
            TagEnd::Paragraph => self.flush_block(),
            TagEnd::Heading(level) => {
                // Prefix with `#`s and render bold cyan.
                let hashes = "#".repeat(heading_depth(level));
                self.spans.insert(
                    0,
                    TextSpan::styled(format!("{hashes} "), Style::default().fg(Color::DarkGray)),
                );
                for span in &mut self.spans {
                    span.style = span.style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                }
                self.flush_block();
            }
            TagEnd::List(_) => {
                self.list.pop();
            }
            TagEnd::BlockQuote(_) => self.quote = self.quote.saturating_sub(1),
            TagEnd::Item => self.flush_block(),
            TagEnd::CodeBlock => self.flush_code(),
            _ => {}
        }
    }

    /// The prefix strings for the first and continuation lines of the current
    /// block, from the list and blockquote nesting.
    fn prefixes(&mut self) -> (String, String) {
        let quote = "▏ ".repeat(self.quote);
        let indent = "  ".repeat(self.list.len().saturating_sub(1));
        match self.list.last_mut() {
            Some(list) => {
                let bullet = match &mut list.next {
                    Some(n) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    None => "• ".to_string(),
                };
                let pad = " ".repeat(bullet.chars().count());
                (
                    format!("{quote}{indent}{bullet}"),
                    format!("{quote}{indent}{pad}"),
                )
            }
            None => (quote.clone(), quote),
        }
    }

    /// Emit the accumulated inline spans as one wrapped block, then clear them.
    fn flush_block(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let (first, cont) = self.prefixes();
        let spans = std::mem::take(&mut self.spans);
        let lines = wrap_spans(&spans, self.wrap, &first, &cont);
        self.out.extend(lines);
    }

    /// Emit the collected fenced code block, syntax-highlighted.
    fn flush_code(&mut self) {
        let Some((lang, text)) = self.code.take() else {
            return;
        };
        let bg = palette::CODE_BLOCK_BG;
        let raw: Vec<&str> = text.trim_end_matches('\n').lines().collect();
        for line in self.highlighter.highlight_by_lang(&lang, &raw) {
            let mut spans = vec![TextSpan::styled("  ", Style::default().bg(bg))];
            for run in line {
                spans.push(TextSpan::styled(
                    run.text,
                    Style::default()
                        .fg(Color::Rgb(run.color.0, run.color.1, run.color.2))
                        .bg(bg),
                ));
            }
            self.out.push(TextLine::from(spans));
        }
    }

    fn finish(mut self) -> Vec<TextLine<'static>> {
        self.flush_block();
        if self.out.is_empty() {
            self.out.push(TextLine::from(""));
        }
        self.out
    }
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Word-wrap styled spans to `wrap` display columns (when set), prefixing the
/// first line with `first` and the rest with `cont`.
fn wrap_spans(
    spans: &[TextSpan<'static>],
    wrap: Option<usize>,
    first: &str,
    cont: &str,
) -> Vec<TextLine<'static>> {
    let prefix_style = Style::default().fg(Color::DarkGray);

    let Some(width) = wrap else {
        // No wrapping: one line, prefixed.
        let mut line = vec![TextSpan::styled(first.to_string(), prefix_style)];
        line.extend(spans.iter().cloned());
        return vec![TextLine::from(line)];
    };

    // Split spans into words, keeping each word's style.
    let mut words: Vec<(String, Style)> = Vec::new();
    for span in spans {
        for word in span.content.split(' ') {
            if !word.is_empty() {
                words.push((word.to_string(), span.style));
            }
        }
    }

    let mut lines = Vec::new();
    let mut current: Vec<TextSpan<'static>> = Vec::new();
    let mut used = 0usize;
    let mut on_first = true;
    for (word, style) in words {
        let budget = width.saturating_sub(prefix_width(if on_first { first } else { cont }));
        let word_w = UnicodeWidthStr::width(word.as_str());
        let sep = usize::from(!current.is_empty());
        if !current.is_empty() && used + sep + word_w > budget {
            lines.push(build_line(
                if on_first { first } else { cont },
                prefix_style,
                current,
            ));
            current = Vec::new();
            used = 0;
            on_first = false;
        }
        if !current.is_empty() {
            current.push(TextSpan::styled(" ", style));
            used += 1;
        }
        current.push(TextSpan::styled(word, style));
        used += word_w;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(build_line(
            if on_first { first } else { cont },
            prefix_style,
            current,
        ));
    }
    lines
}

fn prefix_width(prefix: &str) -> usize {
    UnicodeWidthStr::width(prefix)
}

fn build_line(
    prefix: &str,
    prefix_style: Style,
    mut spans: Vec<TextSpan<'static>>,
) -> TextLine<'static> {
    let mut line = vec![TextSpan::styled(prefix.to_string(), prefix_style)];
    line.append(&mut spans);
    TextLine::from(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The concatenated plain text of a rendered line.
    fn text_of(line: &TextLine) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wraps_a_paragraph_to_width() {
        let hl = Highlighter::new();
        let lines = render("one two three four five", Some(9), &hl);
        assert!(lines.len() > 1, "should wrap into multiple lines");
        for line in &lines {
            assert!(text_of(line).chars().count() <= 9);
        }
    }

    #[test]
    fn renders_a_bullet_list() {
        let hl = Highlighter::new();
        let lines = render("- alpha\n- beta", None, &hl);
        assert_eq!(text_of(&lines[0]), "• alpha");
        assert_eq!(text_of(&lines[1]), "• beta");
    }

    #[test]
    fn inline_code_and_emphasis_keep_their_text() {
        let hl = Highlighter::new();
        let lines = render("use `x` and **y**", None, &hl);
        let joined: String = lines.iter().map(text_of).collect();
        assert!(joined.contains('x'));
        assert!(joined.contains('y'));
    }
}
