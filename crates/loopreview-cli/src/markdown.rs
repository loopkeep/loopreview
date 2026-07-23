//! A small markdown renderer for comment bodies.
//!
//! Parses with `pulldown-cmark` and paints to ratatui lines: headings (a visual
//! hierarchy, no raw `#`), emphasis, inline and fenced code (syntax-highlighted
//! via [`Highlighter`]), lists, block quotes, GitHub alerts (`> [!NOTE]` …), task
//! lists, GFM tables, thematic breaks, and footnotes. In wrap mode (the
//! Conversation and Overview panes) paragraphs word-wrap and top-level blocks are
//! separated by a blank line, matching how GitHub renders the same text; in
//! no-wrap mode (tight inline display) each block stays on one clipped line.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::palette;

use crate::highlight::Highlighter;

/// The sentinel span marking a forced in-paragraph line break (a soft/hard break
/// or a `<br>`). A lone newline never appears inside a normal inline span, so it
/// is safe to recognize by content.
const BREAK: &str = "\n";

fn break_span() -> TextSpan<'static> {
    TextSpan::raw(BREAK)
}

fn is_break(span: &TextSpan<'_>) -> bool {
    span.content.as_ref() == BREAK
}

/// A wrap unit: a run of non-space text that may cross several styles. Kept
/// together with no fabricated space at a style boundary.
type Word = Vec<(String, Style)>;

/// Render markdown `text` to styled lines. `wrap` is the wrap width, or `None`
/// to keep each block on one (clipped) line.
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
        started: false,
        fresh: false,
    };
    // GFM adds alert blockquotes (`> [!NOTE]`); tables and task lists are the
    // other GFM constructs a PR body leans on. Footnotes appear in issue bodies.
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
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
    /// Whether any block has been emitted yet (so the first gets no leading gap).
    started: bool,
    /// Whether the next block is the first inside a freshly-entered list or
    /// blockquote (so it gets no separator against the container's opening).
    fresh: bool,
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
            // GitHub renders a single newline in a comment/issue body as a line
            // break, so both soft and hard breaks become real breaks here — the
            // same text must not read differently on github.com and in lr.
            Event::SoftBreak | Event::HardBreak => self.spans.push(break_span()),
            // Inline/raw HTML: only `<br>` is meaningful for us (a line break).
            // Any other tag is dropped — its inner text still arrives as separate
            // Text events, so content is kept, markup stripped.
            Event::Html(html) | Event::InlineHtml(html) => {
                if is_br(&html) {
                    self.spans.push(break_span());
                }
            }
            Event::Rule => self.rule(),
            // A footnote reference renders as a compact `[label]` marker; the
            // definition is emitted as its own block (see FootnoteDefinition).
            Event::FootnoteReference(label) => {
                self.spans.push(TextSpan::styled(
                    format!("[{label}]"),
                    self.style.fg(palette::LINK_FG),
                ));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.open_block(),
            Tag::Heading { .. } => self.open_block(),
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.into_string()),
            Tag::List(start) => {
                // Flush an enclosing item's pending inline text before its nested
                // list, so the two don't run together on one line.
                self.flush_block();
                self.open_block();
                self.list.push(List { next: start });
                self.fresh = true;
            }
            Tag::BlockQuote(kind) => {
                self.flush_block();
                self.open_block();
                self.quotes.push(kind);
                if let Some(kind) = kind {
                    self.alert_header(kind);
                }
                self.fresh = true;
            }
            Tag::CodeBlock(kind) => {
                self.flush_block();
                self.open_block();
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
                self.open_block();
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
            // A footnote definition renders as a block led by its `[label]`.
            Tag::FootnoteDefinition(label) => {
                self.open_block();
                self.spans.push(TextSpan::styled(
                    format!("[{label}] "),
                    Style::default()
                        .fg(palette::LINK_FG)
                        .add_modifier(Modifier::BOLD),
                ));
                self.fresh = true;
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
                // No raw `#`: a visual hierarchy by level, like GitHub.
                let (color, bold) = heading_style(level);
                for span in &mut self.spans {
                    if bold {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                    if let Some(color) = color {
                        span.style = span.style.fg(color);
                    }
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
            TagEnd::FootnoteDefinition => self.flush_block(),
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

    /// Place the inter-block separator before the next block, when wrapping. Top
    /// level gets a blank line; inside a blockquote a bar-prefixed blank; a list's
    /// items stay tight. No leading gap (nothing emitted yet), and none against a
    /// container's own opening (the `fresh` guard).
    fn open_block(&mut self) {
        if self.wrap.is_none() || !self.started {
            return;
        }
        if self.fresh {
            self.fresh = false;
            return;
        }
        if !self.list.is_empty() {
            return; // list items/blocks stay tight
        }
        if self.quotes.is_empty() {
            self.out.push(TextLine::from(""));
        } else {
            let color = self.alert_color().unwrap_or(Color::DarkGray);
            let bar = "▏ ".repeat(self.quotes.len());
            self.out.push(TextLine::from(TextSpan::styled(
                bar,
                Style::default().fg(color),
            )));
        }
    }

    /// Emit a thematic break (`---`) as a dim rule: pane-wide when wrapping, a
    /// short fixed dash run in tight mode; inside a quote it sits after the bars.
    fn rule(&mut self) {
        self.flush_block();
        self.open_block();
        let prefix = "▏ ".repeat(self.quotes.len());
        let color = self.alert_color().unwrap_or(Color::DarkGray);
        let rule = match self.wrap {
            Some(w) => "─".repeat(w.saturating_sub(prefix_width(&prefix)).max(3)),
            None => "───".to_string(),
        };
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(TextSpan::styled(prefix, Style::default().fg(color)));
        }
        spans.push(TextSpan::styled(rule, Style::default().fg(Color::DarkGray)));
        self.out.push(TextLine::from(spans));
        self.mark_emitted();
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
        self.mark_emitted();
    }

    /// Record that a block was emitted: the next block gets a separator, and the
    /// fresh-context grace is spent.
    fn mark_emitted(&mut self) {
        self.started = true;
        self.fresh = false;
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
        self.mark_emitted();
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
        self.mark_emitted();
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
        self.mark_emitted();
    }

    fn finish(mut self) -> Vec<TextLine<'static>> {
        self.flush_block();
        if self.out.is_empty() {
            self.out.push(TextLine::from(""));
        }
        self.out
    }
}

/// Whether a raw inline-HTML fragment is a `<br>` (in any of its spellings).
fn is_br(html: &str) -> bool {
    let t = html.trim().trim_start_matches("</").trim_start_matches('<');
    let t = t.trim_end_matches('>').trim_end_matches('/').trim();
    t.eq_ignore_ascii_case("br")
}

/// A heading's `(colour, bold)` by level: H1/H2 accent (cyan), H3/H4 plain bold,
/// H5/H6 dim — a visual hierarchy with no leading `#`, as GitHub renders.
fn heading_style(level: HeadingLevel) -> (Option<Color>, bool) {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => (Some(Color::Cyan), true),
        HeadingLevel::H3 | HeadingLevel::H4 => (None, true),
        HeadingLevel::H5 | HeadingLevel::H6 => (Some(Color::DarkGray), true),
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
/// prefix (an alert's bar colour, else the default dim). A break sentinel forces
/// a new line; in no-wrap mode it collapses to a space (tight display).
fn wrap_spans(
    spans: &[TextSpan<'static>],
    wrap: Option<usize>,
    first: &str,
    cont: &str,
    prefix_color: Option<Color>,
) -> Vec<TextLine<'static>> {
    let prefix_style = Style::default().fg(prefix_color.unwrap_or(Color::DarkGray));

    let Some(width) = wrap else {
        // No wrapping: one line, prefixed; breaks become spaces.
        let mut line = vec![TextSpan::styled(first.to_string(), prefix_style)];
        for span in spans {
            if is_break(span) {
                line.push(TextSpan::styled(" ", span.style));
            } else {
                line.push(span.clone());
            }
        }
        return vec![TextLine::from(line)];
    };

    // `first` and `cont` are built to the same display width, so one budget fits
    // every line; the first line carries `first`, the rest `cont`.
    let budget = width.saturating_sub(prefix_width(first)).max(1);
    let rows = wrap_runs(spans, budget);
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| build_line(if i == 0 { first } else { cont }, prefix_style, row))
        .collect()
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

/// Break `spans` into visual lines fitting `width` columns. A break sentinel
/// starts a new line; words split only at real spaces; a run of non-space text
/// that crosses style spans stays one unit (no fabricated space at a style
/// boundary); an over-wide unit hard-breaks. Returns lines of spans (no prefix).
fn wrap_runs(spans: &[TextSpan<'static>], width: usize) -> Vec<Vec<TextSpan<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<TextSpan<'static>>> = Vec::new();
    for segment in split_breaks(spans) {
        let words = to_words(&segment);
        wrap_words_into(&words, width, &mut lines);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Split a span list at break sentinels, dropping the sentinels; each returned
/// segment renders on its own line (or wraps within its own lines).
fn split_breaks(spans: &[TextSpan<'static>]) -> Vec<Vec<TextSpan<'static>>> {
    let mut segments: Vec<Vec<TextSpan<'static>>> = vec![Vec::new()];
    for span in spans {
        if is_break(span) {
            segments.push(Vec::new());
        } else {
            segments.last_mut().unwrap().push(span.clone());
        }
    }
    segments
}

/// Group a break-free span list into words: maximal runs of non-space text,
/// split only where a real space occurs. A word keeps its (possibly several)
/// style runs, so adjacent styles never get a fabricated space between them.
fn to_words(spans: &[TextSpan<'static>]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Word = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let flush_run = |run: &mut String, run_style: Style, cur: &mut Word| {
        if !run.is_empty() {
            cur.push((std::mem::take(run), run_style));
        }
    };
    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            if ch == ' ' || ch == '\t' {
                flush_run(&mut run, run_style, &mut cur);
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            } else {
                if !run.is_empty() && style != run_style {
                    flush_run(&mut run, run_style, &mut cur);
                }
                if run.is_empty() {
                    run_style = style;
                }
                run.push(ch);
            }
        }
        // A span boundary is not a word boundary — flush the run so the next
        // span's style starts cleanly, but keep the word open (no space).
        flush_run(&mut run, run_style, &mut cur);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn word_width(word: &Word) -> usize {
    word.iter()
        .map(|(t, _)| UnicodeWidthStr::width(t.as_str()))
        .sum()
}

fn push_word(line: &mut Vec<TextSpan<'static>>, word: &Word) {
    for (text, style) in word {
        line.push(TextSpan::styled(text.clone(), *style));
    }
}

/// Greedy word-wrap into `lines`, hard-breaking any word wider than `width`.
fn wrap_words_into(words: &[Word], width: usize, lines: &mut Vec<Vec<TextSpan<'static>>>) {
    let mut cur: Vec<TextSpan<'static>> = Vec::new();
    let mut used = 0usize;
    for word in words {
        let ww = word_width(word);
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut rest = word.clone();
            while word_width(&rest) > width {
                let (head, tail) = split_word_at_width(&rest, width);
                let mut line = Vec::new();
                push_word(&mut line, &head);
                lines.push(line);
                rest = tail;
            }
            push_word(&mut cur, &rest);
            used = word_width(&rest);
            continue;
        }
        let sep = usize::from(!cur.is_empty());
        if !cur.is_empty() && used + sep + ww > width {
            lines.push(std::mem::take(&mut cur));
            used = 0;
        }
        if !cur.is_empty() {
            cur.push(TextSpan::raw(" "));
            used += 1;
        }
        push_word(&mut cur, word);
        used += ww;
    }
    lines.push(cur);
}

/// Split a word at `width` display columns, preserving style runs; the split
/// point falls inside whichever run crosses the boundary.
fn split_word_at_width(word: &Word, width: usize) -> (Word, Word) {
    let mut head: Word = Vec::new();
    let mut tail: Word = Vec::new();
    let mut used = 0usize;
    let mut cutting = false;
    for (text, style) in word {
        if cutting {
            tail.push((text.clone(), *style));
            continue;
        }
        let tw = UnicodeWidthStr::width(text.as_str());
        if used + tw <= width {
            head.push((text.clone(), *style));
            used += tw;
        } else {
            let (h, t) = split_at_width(text, (width - used).max(1));
            if !h.is_empty() {
                head.push((h, *style));
            }
            if !t.is_empty() {
                tail.push((t, *style));
            }
            cutting = true;
        }
    }
    (head, tail)
}

fn span_width(spans: &[TextSpan<'static>]) -> usize {
    spans
        .iter()
        .filter(|s| !is_break(s))
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
    // Known limitation: many columns in a very narrow pane can sum past the width
    // even with every column at MIN_COL, so the rightmost cells visually clip.
    // Accepted for now — a readable minimum beats reflowing a table into nonsense.

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
                if is_break(s) {
                    spans.push(TextSpan::raw(" "));
                    continue;
                }
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
/// that is itself too wide. Header cells render bold. Break sentinels split lines.
fn wrap_cell(spans: &[TextSpan<'static>], width: usize, bold: bool) -> Vec<Vec<TextSpan<'static>>> {
    let width = width.max(1);
    let styled: Vec<TextSpan<'static>> = spans
        .iter()
        .map(|s| {
            if is_break(s) || !bold {
                s.clone()
            } else {
                TextSpan::styled(s.content.to_string(), s.style.add_modifier(Modifier::BOLD))
            }
        })
        .collect();
    wrap_runs(&styled, width)
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

    /// All rendered lines as their plain text.
    fn texts(lines: &[TextLine]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    /// The foreground colour of the first span carrying `needle`.
    fn color_of(lines: &[TextLine], needle: &str) -> Option<Color> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .map(|s| s.style.fg.unwrap_or(Color::Reset))
    }

    /// The style of the first span carrying `needle`.
    fn style_of(lines: &[TextLine], needle: &str) -> Option<Style> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .map(|s| s.style)
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
    fn headings_render_a_hierarchy_with_no_hashes() {
        let hl = Highlighter::new();
        let lines = render("# Title\n\n### Sub\n\n##### Small", Some(40), &hl);
        let joined: String = lines.iter().map(text_of).collect();
        assert!(!joined.contains('#'), "no raw # marks: {joined:?}");
        assert!(joined.contains("Title") && joined.contains("Sub") && joined.contains("Small"));
        // H1 is bold cyan, H3 bold (default fg), H5 bold dim.
        let h1 = style_of(&lines, "Title").unwrap();
        assert!(h1.add_modifier.contains(Modifier::BOLD) && h1.fg == Some(Color::Cyan));
        let h3 = style_of(&lines, "Sub").unwrap();
        assert!(h3.add_modifier.contains(Modifier::BOLD) && h3.fg != Some(Color::Cyan));
        let h5 = style_of(&lines, "Small").unwrap();
        assert!(h5.add_modifier.contains(Modifier::BOLD) && h5.fg == Some(Color::DarkGray));
    }

    #[test]
    fn top_level_blocks_are_separated_by_one_blank_line() {
        let hl = Highlighter::new();
        let lines = render("para one\n\n# Heading\n\npara two", Some(40), &hl);
        let t = texts(&lines);
        assert_eq!(
            t,
            vec![
                "para one".to_string(),
                String::new(),
                "Heading".to_string(),
                String::new(),
                "para two".to_string(),
            ],
            "one blank between each top-level block, none leading/trailing"
        );
    }

    #[test]
    fn no_block_spacing_when_not_wrapping() {
        // The tight inline path stays dense (no inserted blank lines).
        let hl = Highlighter::new();
        let lines = render("para one\n\npara two", None, &hl);
        let t = texts(&lines);
        assert!(!t.iter().any(|l| l.is_empty()), "no blank lines: {t:?}");
    }

    #[test]
    fn soft_and_hard_breaks_become_real_line_breaks() {
        let hl = Highlighter::new();
        // A single newline (soft break) and a trailing-space hard break both wrap.
        let lines = render("alpha\nbeta", Some(40), &hl);
        assert_eq!(texts(&lines), vec!["alpha".to_string(), "beta".to_string()]);
        let hard = render("alpha  \nbeta", Some(40), &hl);
        assert_eq!(texts(&hard), vec!["alpha".to_string(), "beta".to_string()]);
        // In no-wrap mode the break collapses to a space (tight display).
        let tight = render("alpha\nbeta", None, &hl);
        assert_eq!(texts(&tight), vec!["alpha beta".to_string()]);
    }

    #[test]
    fn adjacent_styles_keep_no_fabricated_space() {
        let hl = Highlighter::new();
        // `config.toml` (code) immediately followed by a comma, and a bold word
        // followed by a period, and a link followed by a paren — all must stay
        // flush when wrapping.
        let lines = render("see `config.toml`, and **bold**. done", Some(40), &hl);
        let joined: String = texts(&lines).join(" ");
        assert!(
            joined.contains("config.toml,"),
            "code and comma stay flush: {joined:?}"
        );
        assert!(
            joined.contains("bold."),
            "bold and period stay flush: {joined:?}"
        );
    }

    #[test]
    fn a_thematic_break_renders_a_dim_rule() {
        let hl = Highlighter::new();
        let lines = render("above\n\n---\n\nbelow", Some(20), &hl);
        let t = texts(&lines);
        let rule = t.iter().find(|l| l.contains('─')).expect("a rule line");
        assert!(
            UnicodeWidthStr::width(rule.as_str()) <= 20,
            "the rule fits the pane: {rule:?}"
        );
        assert!(rule.chars().all(|c| c == '─'), "a solid rule: {rule:?}");
        // No-wrap uses a short fixed rule.
        let tight = render("---", None, &hl);
        assert!(texts(&tight).iter().any(|l| l == "───"));
    }

    #[test]
    fn br_tag_breaks_a_line() {
        let hl = Highlighter::new();
        let lines = render("alpha<br>beta", Some(40), &hl);
        assert_eq!(texts(&lines), vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn an_ordered_list_honours_its_start_number() {
        let hl = Highlighter::new();
        let lines = render("5. five\n6. six", None, &hl);
        assert_eq!(text_of(&lines[0]), "5. five");
        assert_eq!(text_of(&lines[1]), "6. six");
    }

    #[test]
    fn a_nested_list_indents_its_items() {
        let hl = Highlighter::new();
        let lines = render("- outer\n  - inner", None, &hl);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l == "• outer"));
        assert!(
            t.iter().any(|l| l.starts_with("  ") && l.contains("inner")),
            "the nested item is indented: {t:?}"
        );
    }

    #[test]
    fn a_footnote_reference_and_definition_render() {
        let hl = Highlighter::new();
        let lines = render("text[^1]\n\n[^1]: the note", Some(40), &hl);
        let joined: String = texts(&lines).join("\n");
        assert!(joined.contains("[1]"), "the reference marker: {joined:?}");
        assert!(
            joined.contains("the note"),
            "the definition body: {joined:?}"
        );
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
    fn blockquote_paragraphs_are_separated_by_a_bar_blank() {
        let hl = Highlighter::new();
        let lines = render("> one\n>\n> two", Some(40), &hl);
        let t = texts(&lines);
        // A bar-only line sits between the two quote paragraphs (no empty gap).
        let bar_blank = t.iter().position(|l| l.trim() == "▏");
        assert!(
            bar_blank.is_some(),
            "a bar-prefixed blank between paras: {t:?}"
        );
        assert!(t.iter().any(|l| l.contains("one")) && t.iter().any(|l| l.contains("two")));
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
