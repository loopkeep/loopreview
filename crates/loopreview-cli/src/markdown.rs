//! A small markdown renderer for comment bodies.
//!
//! Parses with `pulldown-cmark` and paints to ratatui lines: headings, emphasis,
//! inline and fenced code (syntax-highlighted via [`Highlighter`]), lists, block
//! quotes, GitHub alerts (`> [!NOTE]` …), task lists, and GFM tables. Paragraphs
//! word-wrap to a width when one is given (the Conversation and Overview panes),
//! or render unwrapped for tight inline display.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        quotes: Vec::new(),
        task: None,
        code: None,
        link: None,
        table: None,
    };
    // GFM adds alert blockquotes (`> [!NOTE]`); tables and task lists are the
    // other two GFM constructs a PR body leans on.
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// A markdown list context: an ordered counter, or a bullet (`None`).
struct List {
    next: Option<u64>,
}

/// A table being collected, cell by cell, until its end lays it out.
struct TableBuild {
    aligns: Vec<Alignment>,
    head: Vec<Vec<TextSpan<'static>>>,
    rows: Vec<Vec<Vec<TextSpan<'static>>>>,
    in_head: bool,
    row: Vec<Vec<TextSpan<'static>>>,
}

struct Renderer<'a> {
    wrap: Option<usize>,
    highlighter: &'a Highlighter,
    out: Vec<TextLine<'static>>,
    /// Inline spans accumulating for the current block (or table cell).
    spans: Vec<TextSpan<'static>>,
    /// The active inline style (emphasis/strong/strikethrough).
    style: Style,
    list: Vec<List>,
    /// The blockquote nesting, each `Some(kind)` for a GitHub alert.
    quotes: Vec<Option<BlockQuoteKind>>,
    /// A pending task-list checkbox for the next list item (`Some(checked)`).
    task: Option<bool>,
    /// Fenced code-block language + collected text, while inside one.
    code: Option<(String, String)>,
    /// A link's destination, while inside one.
    link: Option<String>,
    /// A table being built, while inside one.
    table: Option<TableBuild>,
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
            // A `- [ ]` / `- [x]` marker: remembered so the item's bullet becomes
            // a checkbox (rather than adding a second glyph beside the bullet).
            Event::TaskListMarker(checked) => self.task = Some(checked),
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
            Tag::BlockQuote(kind) => {
                self.quotes.push(kind);
                if let Some(kind) = kind {
                    self.alert_header(kind);
                }
            }
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
            Tag::Table(aligns) => {
                self.flush_block();
                self.table = Some(TableBuild {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    in_head: false,
                    row: Vec::new(),
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.row = Vec::new();
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.row = Vec::new();
                }
            }
            Tag::TableCell => self.spans.clear(),
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
            TagEnd::BlockQuote(_) => {
                self.quotes.pop();
            }
            TagEnd::Item => self.flush_block(),
            TagEnd::CodeBlock => self.flush_code(),
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table
                    && !t.in_head
                {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => self.flush_table(),
            _ => {}
        }
    }

    /// Emit a GitHub alert's coloured header line (`⚠ Warning`, …) at the top of
    /// its blockquote, with the quote bars in the same colour.
    fn alert_header(&mut self, kind: BlockQuoteKind) {
        self.flush_block();
        let (glyph, label, color) = alert_style(kind);
        let bars = "▏ ".repeat(self.quotes.len());
        self.out.push(TextLine::from(vec![
            TextSpan::styled(bars, Style::default().fg(color)),
            TextSpan::styled(
                format!("{glyph} {label}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    /// The colour of the nearest enclosing alert, for tinting its quote bar.
    fn alert_color(&self) -> Option<Color> {
        self.quotes
            .iter()
            .rev()
            .flatten()
            .next()
            .map(|k| alert_style(*k).2)
    }

    /// The prefix strings for the first and continuation lines of the current
    /// block, from the list and blockquote nesting.
    fn prefixes(&mut self) -> (String, String) {
        let quote = "▏ ".repeat(self.quotes.len());
        let indent = "  ".repeat(self.list.len().saturating_sub(1));
        let task = self.task.take();
        match self.list.last_mut() {
            Some(list) => {
                // A task item shows a checkbox in place of its bullet.
                let bullet = if let Some(checked) = task {
                    if checked { "☑ " } else { "☐ " }.to_string()
                } else {
                    match &mut list.next {
                        Some(n) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        None => "• ".to_string(),
                    }
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
        let color = self.alert_color();
        let (first, cont) = self.prefixes();
        let spans = std::mem::take(&mut self.spans);
        let lines = wrap_spans(&spans, self.wrap, &first, &cont, color);
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

    /// Lay out the collected table and emit its lines.
    fn flush_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let prefix = "▏ ".repeat(self.quotes.len());
        let color = self.alert_color();
        let lines = render_table(&table, self.wrap, &prefix, color);
        self.out.extend(lines);
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

/// A GitHub alert's glyph, label, and colour, following GitHub's semantics.
fn alert_style(kind: BlockQuoteKind) -> (&'static str, &'static str, Color) {
    match kind {
        BlockQuoteKind::Note => ("ⓘ", "Note", Color::Blue),
        BlockQuoteKind::Tip => ("✔", "Tip", Color::Green),
        BlockQuoteKind::Important => ("★", "Important", Color::Magenta),
        BlockQuoteKind::Warning => ("⚠", "Warning", Color::Yellow),
        BlockQuoteKind::Caution => ("⚠", "Caution", Color::Red),
    }
}

/// Word-wrap styled spans to `wrap` display columns (when set), prefixing the
/// first line with `first` and the rest with `cont`. `prefix_color` tints the
/// prefix (an alert's bar colour, else the default dim).
fn wrap_spans(
    spans: &[TextSpan<'static>],
    wrap: Option<usize>,
    first: &str,
    cont: &str,
    prefix_color: Option<Color>,
) -> Vec<TextLine<'static>> {
    let prefix_style = Style::default().fg(prefix_color.unwrap_or(Color::DarkGray));

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

fn span_width(spans: &[TextSpan<'static>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Lay out a GFM table to fit `wrap` columns: natural column widths when they
/// fit, else shrink the widest and wrap cell content to multiple lines. Never
/// wider than the pane. `prefix` (quote bars) leads every line.
fn render_table(
    table: &TableBuild,
    wrap: Option<usize>,
    prefix: &str,
    prefix_color: Option<Color>,
) -> Vec<TextLine<'static>> {
    let cols = table
        .head
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return Vec::new();
    }
    let prefix_style = Style::default().fg(prefix_color.unwrap_or(Color::DarkGray));
    let dim = Style::default().fg(Color::DarkGray);
    let gutter = " │ ";
    let gutter_w = UnicodeWidthStr::width(gutter);

    // Natural column widths (max cell width in each column, ≥ 1).
    let mut widths = vec![1usize; cols];
    for row in std::iter::once(&table.head).chain(table.rows.iter()) {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(span_width(cell));
        }
    }

    // No wrapping: one line per row, cells joined by the gutter (the caller's
    // 1-line clip bounds it).
    let Some(total) = wrap else {
        let mut out = vec![table_flat_row(
            &table.head,
            cols,
            prefix,
            prefix_style,
            dim,
            gutter,
            true,
        )];
        out.extend(
            table
                .rows
                .iter()
                .map(|r| table_flat_row(r, cols, prefix, prefix_style, dim, gutter, false)),
        );
        return out;
    };

    // Shrink the widest columns until the row fits the pane.
    let avail = total.saturating_sub(prefix_width(prefix));
    let content_avail = avail
        .saturating_sub(gutter_w * cols.saturating_sub(1))
        .max(cols);
    const MIN_COL: usize = 3;
    while widths.iter().sum::<usize>() > content_avail {
        let widest = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > MIN_COL)
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i);
        match widest {
            Some(i) => widths[i] -= 1,
            None => break, // all at the floor — accept a slight overflow
        }
    }

    let mut out = table_row(
        &table.head,
        &widths,
        &table.aligns,
        prefix,
        prefix_style,
        dim,
        gutter,
        true,
    );
    // The header separator (a dim rule, gutters crossed).
    let mut sep = vec![TextSpan::styled(prefix.to_string(), prefix_style)];
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            sep.push(TextSpan::styled("─┼─".to_string(), dim));
        }
        sep.push(TextSpan::styled("─".repeat(w), dim));
    }
    out.push(TextLine::from(sep));
    for row in &table.rows {
        out.extend(table_row(
            row,
            &widths,
            &table.aligns,
            prefix,
            prefix_style,
            dim,
            gutter,
            false,
        ));
    }
    out
}

/// One un-wrapped table row (for the no-wrap path).
fn table_flat_row(
    row: &[Vec<TextSpan<'static>>],
    cols: usize,
    prefix: &str,
    prefix_style: Style,
    dim: Style,
    gutter: &str,
    bold: bool,
) -> TextLine<'static> {
    let mut spans = vec![TextSpan::styled(prefix.to_string(), prefix_style)];
    for i in 0..cols {
        if i > 0 {
            spans.push(TextSpan::styled(gutter.to_string(), dim));
        }
        if let Some(cell) = row.get(i) {
            for s in cell {
                let style = if bold {
                    s.style.add_modifier(Modifier::BOLD)
                } else {
                    s.style
                };
                spans.push(TextSpan::styled(s.content.to_string(), style));
            }
        }
    }
    TextLine::from(spans)
}

/// A table row wrapped to `widths`, aligned per column, as one or more lines.
#[allow(clippy::too_many_arguments)]
fn table_row(
    row: &[Vec<TextSpan<'static>>],
    widths: &[usize],
    aligns: &[Alignment],
    prefix: &str,
    prefix_style: Style,
    dim: Style,
    gutter: &str,
    bold: bool,
) -> Vec<TextLine<'static>> {
    let cols = widths.len();
    let empty: Vec<TextSpan<'static>> = Vec::new();
    let wrapped: Vec<Vec<Vec<TextSpan<'static>>>> = (0..cols)
        .map(|i| wrap_cell(row.get(i).unwrap_or(&empty), widths[i], bold))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

    let mut lines = Vec::new();
    for li in 0..height {
        let mut spans = vec![TextSpan::styled(prefix.to_string(), prefix_style)];
        for i in 0..cols {
            if i > 0 {
                spans.push(TextSpan::styled(gutter.to_string(), dim));
            }
            let line = wrapped[i].get(li).cloned().unwrap_or_default();
            let content_w = span_width(&line);
            let pad = widths[i].saturating_sub(content_w);
            let (lpad, rpad) = match aligns.get(i).copied().unwrap_or(Alignment::None) {
                Alignment::Right => (pad, 0),
                Alignment::Center => (pad / 2, pad - pad / 2),
                Alignment::Left | Alignment::None => (0, pad),
            };
            if lpad > 0 {
                spans.push(TextSpan::raw(" ".repeat(lpad)));
            }
            spans.extend(line);
            if rpad > 0 {
                spans.push(TextSpan::raw(" ".repeat(rpad)));
            }
        }
        lines.push(TextLine::from(spans));
    }
    lines
}

/// Word-wrap one cell's styled spans to `width` columns, hard-breaking a word
/// that is itself too wide. Header cells render bold.
fn wrap_cell(spans: &[TextSpan<'static>], width: usize, bold: bool) -> Vec<Vec<TextSpan<'static>>> {
    let width = width.max(1);
    let mut words: Vec<(String, Style)> = Vec::new();
    for s in spans {
        let style = if bold {
            s.style.add_modifier(Modifier::BOLD)
        } else {
            s.style
        };
        for w in s.content.split(' ') {
            if !w.is_empty() {
                words.push((w.to_string(), style));
            }
        }
    }
    let mut lines: Vec<Vec<TextSpan<'static>>> = Vec::new();
    let mut cur: Vec<TextSpan<'static>> = Vec::new();
    let mut used = 0usize;
    for (word, style) in words {
        let mut word = word;
        loop {
            let ww = UnicodeWidthStr::width(word.as_str());
            let sep = usize::from(!cur.is_empty());
            if used + sep + ww <= width {
                if sep == 1 {
                    cur.push(TextSpan::styled(" ", style));
                    used += 1;
                }
                cur.push(TextSpan::styled(word, style));
                used += ww;
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                used = 0;
                continue; // retry the word on a fresh line
            }
            // The word alone exceeds the column: hard-break it.
            let (head, tail) = split_at_width(&word, width);
            cur.push(TextSpan::styled(head, style));
            lines.push(std::mem::take(&mut cur));
            used = 0;
            word = tail;
            if word.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Split `s` at `width` display columns, always taking at least one char (so a
/// wide glyph in a narrow column makes progress rather than looping).
fn split_at_width(s: &str, width: usize) -> (String, String) {
    let mut w = 0;
    let mut idx = 0;
    for (i, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if i > 0 && w + cw > width {
            break;
        }
        w += cw;
        idx = i + ch.len_utf8();
    }
    (s[..idx].to_string(), s[idx..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The concatenated plain text of a rendered line.
    fn text_of(line: &TextLine) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The foreground colour of the first span carrying `needle`.
    fn color_of(lines: &[TextLine], needle: &str) -> Option<Color> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .map(|s| s.style.fg.unwrap_or(Color::Reset))
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

    #[test]
    fn github_alerts_get_a_coloured_header_and_no_raw_marker() {
        let hl = Highlighter::new();
        for (src, label, color) in [
            ("> [!NOTE]\n> hi", "Note", Color::Blue),
            ("> [!TIP]\n> hi", "Tip", Color::Green),
            ("> [!IMPORTANT]\n> hi", "Important", Color::Magenta),
            ("> [!WARNING]\n> hi", "Warning", Color::Yellow),
            ("> [!CAUTION]\n> hi", "Caution", Color::Red),
        ] {
            let lines = render(src, Some(40), &hl);
            let joined: String = lines.iter().map(text_of).collect();
            assert!(joined.contains(label), "{label} header shown: {joined:?}");
            assert!(
                !joined.contains("[!"),
                "the raw alert marker is gone: {joined:?}"
            );
            assert_eq!(color_of(&lines, label), Some(color), "{label} is coloured");
        }
    }

    #[test]
    fn a_plain_blockquote_is_unchanged() {
        let hl = Highlighter::new();
        let lines = render("> just a quote", Some(40), &hl);
        let joined: String = lines.iter().map(text_of).collect();
        assert!(joined.contains("just a quote"));
        assert!(!joined.contains("Note") && !joined.contains("[!"));
        assert!(joined.contains('▏'), "the quote bar is kept");
    }

    #[test]
    fn task_list_items_render_checkboxes() {
        let hl = Highlighter::new();
        let lines = render("- [ ] todo\n- [x] done", None, &hl);
        assert_eq!(text_of(&lines[0]), "☐ todo");
        assert_eq!(text_of(&lines[1]), "☑ done");
    }

    #[test]
    fn a_table_aligns_columns_with_a_header_rule() {
        let hl = Highlighter::new();
        let src = "\
| Left | Mid | Right |
| :--- | :-: | ----: |
| a | bb | c |
| dd | e | fff |";
        let lines = render(src, Some(40), &hl);
        let texts: Vec<String> = lines.iter().map(text_of).collect();
        // Header, a dim rule, then two body rows.
        assert!(texts[0].contains("Left") && texts[0].contains("Right"));
        assert!(
            texts[1].contains('─'),
            "a rule under the header: {:?}",
            texts[1]
        );
        assert!(texts.iter().any(|t| t.contains("fff")), "body rendered");
        // Header cells are bold.
        let left = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("Left"))
            .unwrap();
        assert!(left.style.add_modifier.contains(Modifier::BOLD));
        // Right-aligned column pads on the left: "c" sits at the column's end.
        let right_row = &texts[2];
        assert!(
            right_row.trim_end().ends_with('c'),
            "right column is right-aligned: {right_row:?}"
        );
    }

    #[test]
    fn a_wide_table_shrinks_and_wraps_within_the_pane() {
        let hl = Highlighter::new();
        let src = "\
| Col |
| --- |
| one two three four five six seven eight |";
        let width = 16;
        let lines = render(src, Some(width), &hl);
        for line in &lines {
            assert!(
                UnicodeWidthStr::width(text_of(line).as_str()) <= width,
                "no line exceeds the pane: {:?}",
                text_of(line)
            );
        }
        // The long cell wrapped to more than the header + rule + one line.
        assert!(lines.len() > 3, "the cell wrapped across lines");
    }

    #[test]
    fn table_cells_keep_inline_styles_and_cjk_width() {
        let hl = Highlighter::new();
        let src = "\
| Name | Note |
| --- | --- |
| `code` | 日本語 |";
        let lines = render(src, Some(40), &hl);
        // The inline code cell keeps its code background.
        let code = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("code"));
        assert!(code.is_some_and(|s| s.style.bg == Some(palette::CODE_INLINE_BG)));
        // No line overruns despite the wide CJK glyphs.
        for line in &lines {
            assert!(UnicodeWidthStr::width(text_of(line).as_str()) <= 40);
        }
    }
}
