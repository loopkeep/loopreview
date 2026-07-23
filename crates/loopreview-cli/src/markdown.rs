//! A small markdown renderer for comment bodies.
//!
//! Parses with `pulldown-cmark` and paints to ratatui lines: headings (a visual
//! hierarchy, no raw `#`), emphasis, inline and fenced code (syntax-highlighted
//! via [`Highlighter`]), lists, block quotes, GitHub alerts (`> [!NOTE]` …), task
//! lists, GFM tables, thematic breaks, footnotes, images, and `<details>` folds.
//! In wrap mode (the Conversation and Overview panes) paragraphs word-wrap and
//! top-level blocks are separated by a blank line, matching how GitHub renders
//! the same text; in no-wrap mode (tight inline display) each block stays on one
//! clipped line.
//!
//! [`render_rich`] also returns [`MdRegion`]s — the columns of links, images, and
//! `<details>` summaries — so the UI can open a URL or fold a section on click.
//! [`render`] is the plain lines-only wrapper (details always expanded).

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span as TextSpan};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::palette;

use crate::highlight::Highlighter;

/// The action a rendered [`MdRegion`] triggers when clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdAction {
    /// Open a URL (a link, autolink, or image) in the browser.
    Open(String),
    /// Fold or unfold the nth `<details>` in this render (0-based).
    ToggleDetails(usize),
}

/// A clickable region on a rendered line: columns `[start, end)` carry `action`.
/// Columns are display cells from the line's left edge (prefixes included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdRegion {
    /// Index into [`Rendered::lines`].
    pub line: usize,
    /// Inclusive start column.
    pub start: u16,
    /// Exclusive end column.
    pub end: u16,
    /// What a click here does.
    pub action: MdAction,
}

/// A rich render: styled lines plus the clickable regions over them.
pub struct Rendered {
    pub lines: Vec<TextLine<'static>>,
    pub regions: Vec<MdRegion>,
}

/// The sentinel marking a forced in-paragraph line break (a soft/hard break or a
/// `<br>`). A lone newline never appears inside a normal inline span.
const BREAK: &str = "\n";

/// A styled span plus an optional click action, carried through wrapping so the
/// final positions of links/images can be reported as regions.
#[derive(Clone)]
struct Piece {
    span: TextSpan<'static>,
    action: Option<MdAction>,
}

impl Piece {
    fn plain(span: TextSpan<'static>) -> Piece {
        Piece { span, action: None }
    }
    fn is_break(&self) -> bool {
        self.span.content.as_ref() == BREAK
    }
}

fn break_piece() -> Piece {
    Piece::plain(TextSpan::raw(BREAK))
}

/// A wrap run: `(text, style, action)`. A word is a run sequence with no space.
type Run = (String, Style, Option<MdAction>);
type Word = Vec<Run>;

/// A `<details>` fold context on the stack.
struct DetailsFrame {
    /// The 0-based index of this `<details>` in the render (its toggle key).
    index: usize,
    /// Whether it is expanded (its body shows).
    open: bool,
    /// Whether a `<summary>` has been seen (a summary-less details never hides).
    has_summary: bool,
}

/// Render markdown `text` to styled lines (details always expanded). `wrap` is
/// the wrap width, or `None` to keep each block on one (clipped) line.
pub fn render(
    text: &str,
    wrap: Option<usize>,
    highlighter: &Highlighter,
) -> Vec<TextLine<'static>> {
    render_rich(text, wrap, highlighter, &|_, _| true).lines
}

/// Render markdown with interactivity. `is_open(index, default_open)` decides
/// whether the nth `<details>` is expanded — `default_open` is its `open`
/// attribute, which the caller may override with session fold state.
pub fn render_rich(
    text: &str,
    wrap: Option<usize>,
    highlighter: &Highlighter,
    is_open: &dyn Fn(usize, bool) -> bool,
) -> Rendered {
    let mut renderer = Renderer {
        wrap,
        highlighter,
        is_open,
        out: Vec::new(),
        regions: Vec::new(),
        spans: Vec::new(),
        style: Style::default(),
        list: Vec::new(),
        quotes: Vec::new(),
        task: None,
        code: None,
        link: None,
        image: None,
        table: None,
        details: Vec::new(),
        details_count: 0,
        started: false,
        fresh: false,
    };
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
    is_open: &'a dyn Fn(usize, bool) -> bool,
    out: Vec<TextLine<'static>>,
    regions: Vec<MdRegion>,
    /// Inline pieces accumulating for the current block (or table cell).
    spans: Vec<Piece>,
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
    /// An image's `(destination, alt-text-so-far)`, while inside one.
    image: Option<(String, String)>,
    /// A table being built, while inside one.
    table: Option<TableBuild>,
    /// The open `<details>` frames.
    details: Vec<DetailsFrame>,
    /// How many `<details>` have been seen (the next one's index).
    details_count: usize,
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
                if let Some((_, alt)) = &mut self.image {
                    alt.push_str(&text); // an image's alt text, buffered for its label
                } else if let Some((_, buf)) = &mut self.code {
                    buf.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                let style = self.style.fg(palette::CODE_FG).bg(palette::CODE_INLINE_BG);
                self.spans
                    .push(Piece::plain(TextSpan::styled(code.into_string(), style)));
            }
            // A `- [ ]` / `- [x]` marker: remembered so the item's bullet becomes
            // a checkbox (rather than adding a second glyph beside the bullet).
            Event::TaskListMarker(checked) => self.task = Some(checked),
            // GitHub renders a single newline in a comment/issue body as a line
            // break, so both soft and hard breaks become real breaks here.
            Event::SoftBreak | Event::HardBreak => self.spans.push(break_piece()),
            // Raw HTML: `<br>` (a break), `<img>` (an image placeholder),
            // `<details>`/`<summary>` (a fold); everything else is dropped — its
            // inner text still arrives as Text events, so content is kept.
            Event::Html(html) | Event::InlineHtml(html) => self.html(&html),
            Event::Rule => self.rule(),
            Event::FootnoteReference(label) => {
                let style = self.style.fg(palette::LINK_FG);
                self.spans
                    .push(Piece::plain(TextSpan::styled(format!("[{label}]"), style)));
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
            Tag::Image { dest_url, .. } => {
                self.image = Some((dest_url.into_string(), String::new()))
            }
            Tag::List(start) => {
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
            Tag::FootnoteDefinition(label) => {
                self.open_block();
                let style = Style::default()
                    .fg(palette::LINK_FG)
                    .add_modifier(Modifier::BOLD);
                self.spans
                    .push(Piece::plain(TextSpan::styled(format!("[{label}] "), style)));
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
            // The label text carried the link style + action; nothing to append.
            TagEnd::Link => {
                self.link = None;
            }
            TagEnd::Image => {
                if let Some((url, alt)) = self.image.take() {
                    self.push_image(&url, &alt);
                }
            }
            TagEnd::Paragraph => self.flush_block(),
            TagEnd::Heading(level) => {
                let (color, bold) = heading_style(level);
                for piece in &mut self.spans {
                    if bold {
                        piece.span.style = piece.span.style.add_modifier(Modifier::BOLD);
                    }
                    if let Some(color) = color {
                        piece.span.style = piece.span.style.fg(color);
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
                // Tables render without click regions; drop the pieces' actions.
                let cell = std::mem::take(&mut self.spans)
                    .into_iter()
                    .map(|p| p.span)
                    .collect();
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

    /// Push a run of text. Inside a markdown link label it becomes underlined
    /// link text carrying the destination (clickable, no separate `(url)`);
    /// elsewhere bare `http(s)://` URLs autolink (GitHub does).
    fn push_text(&mut self, text: &str) {
        if let Some(url) = self.link.clone() {
            self.spans.push(Piece {
                span: TextSpan::styled(
                    text.to_string(),
                    self.style
                        .fg(palette::LINK_FG)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                action: Some(MdAction::Open(url)),
            });
            return;
        }
        let mut rest = text;
        while let Some((pre, url, tail)) = next_url(rest) {
            if !pre.is_empty() {
                self.spans
                    .push(Piece::plain(TextSpan::styled(pre.to_string(), self.style)));
            }
            self.spans.push(Piece {
                span: TextSpan::styled(
                    url.to_string(),
                    self.style
                        .fg(palette::LINK_FG)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                action: Some(MdAction::Open(url.to_string())),
            });
            rest = tail;
        }
        if !rest.is_empty() {
            self.spans
                .push(Piece::plain(TextSpan::styled(rest.to_string(), self.style)));
        }
    }

    /// Emit an image as an inline `[Image]` / `[Image: alt]` link placeholder.
    fn push_image(&mut self, url: &str, alt: &str) {
        let alt = alt.trim();
        let label = if alt.is_empty() {
            "[Image]".to_string()
        } else {
            format!("[Image: {alt}]")
        };
        self.spans.push(Piece {
            span: TextSpan::styled(
                label,
                self.style
                    .fg(palette::LINK_FG)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            action: (!url.is_empty()).then(|| MdAction::Open(url.to_string())),
        });
    }

    /// Handle a raw-HTML fragment: `<br>`, `<img>`, `<details>` open/close, or a
    /// `<summary>`. Anything else is dropped (inner text kept via Text events).
    fn html(&mut self, html: &str) {
        let lower = html.trim_start().to_ascii_lowercase();
        if is_br(html) {
            self.spans.push(break_piece());
        } else if lower.starts_with("<details") {
            let default_open = details_open_attr(&lower);
            let index = self.details_count;
            self.details_count += 1;
            let open = (self.is_open)(index, default_open);
            self.details.push(DetailsFrame {
                index,
                open,
                has_summary: false,
            });
        } else if lower.starts_with("</details") {
            self.details.pop();
        } else if let Some((src, alt)) = parse_img(html) {
            if !src.is_empty() {
                self.push_image(&src, &alt);
            }
        } else if let Some(summary) = parse_summary(html) {
            self.emit_summary(&summary);
        }
    }

    /// Emit a `<details>` title as a `▸`/`▾ summary` header line and register its
    /// toggle region. Hidden when an ancestor `<details>` is folded.
    fn emit_summary(&mut self, summary: &str) {
        let depth = self.details.len();
        let ancestors_hidden = self.details[..depth.saturating_sub(1)]
            .iter()
            .any(|f| !f.open && f.has_summary);
        if let Some(top) = self.details.last_mut() {
            top.has_summary = true;
        }
        if ancestors_hidden {
            return; // folded away under a closed ancestor
        }
        self.flush_block();
        self.block_gap(false);
        self.fresh = false;

        let open = self.details.last().map(|f| f.open).unwrap_or(true);
        let marker = if open { "▾ " } else { "▸ " };
        let summary = summary.trim();
        let label = if summary.is_empty() {
            "Details"
        } else {
            summary
        };
        let prefix = "▏ ".repeat(self.quotes.len());
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(TextSpan::styled(
                prefix.clone(),
                Style::default().fg(self.alert_color().unwrap_or(Color::DarkGray)),
            ));
        }
        spans.push(TextSpan::styled(
            marker,
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(TextSpan::styled(
            label.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let width = span_width(&spans) as u16;
        let line = self.out.len();
        self.out.push(TextLine::from(spans));
        if let Some(frame) = self.details.last() {
            self.regions.push(MdRegion {
                line,
                start: 0,
                end: width,
                action: MdAction::ToggleDetails(frame.index),
            });
        }
        self.mark_emitted();
    }

    /// Whether body content is currently hidden by a folded `<details>` (one that
    /// has a summary to reopen it).
    fn body_hidden(&self) -> bool {
        self.details.iter().any(|f| !f.open && f.has_summary)
    }

    /// Place the inter-block separator before the next block, when wrapping.
    fn open_block(&mut self) {
        let hidden = self.body_hidden();
        self.block_gap(hidden);
    }

    /// The gap logic, with an explicit hide flag (a summary uses ancestor-only
    /// hiding). Top level gets a blank; a blockquote a bar-blank; lists stay tight.
    fn block_gap(&mut self, hidden: bool) {
        if self.wrap.is_none() || !self.started || hidden {
            return;
        }
        if self.fresh {
            self.fresh = false;
            return;
        }
        if !self.list.is_empty() {
            return;
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

    /// Emit a thematic break (`---`) as a dim rule.
    fn rule(&mut self) {
        self.flush_block();
        if self.body_hidden() {
            return;
        }
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

    /// Emit a GitHub alert's coloured header line at the top of its blockquote.
    fn alert_header(&mut self, kind: BlockQuoteKind) {
        self.flush_block();
        if self.body_hidden() {
            return;
        }
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

    /// Emit the accumulated inline pieces as one wrapped block, recording regions.
    fn flush_block(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let pieces = std::mem::take(&mut self.spans);
        if self.body_hidden() {
            return; // content under a folded <details>: dropped, no regions
        }
        let color = self.alert_color();
        let (first, cont) = self.prefixes();
        let base = self.out.len();
        let (lines, regions) = wrap_spans(&pieces, self.wrap, &first, &cont, color);
        for (ln, start, end, action) in regions {
            self.regions.push(MdRegion {
                line: base + ln,
                start,
                end,
                action,
            });
        }
        self.out.extend(lines);
        self.mark_emitted();
    }

    /// Emit the collected fenced code block, syntax-highlighted.
    fn flush_code(&mut self) {
        let Some((lang, text)) = self.code.take() else {
            return;
        };
        if self.body_hidden() {
            return;
        }
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
        if self.body_hidden() {
            return;
        }
        let prefix = "▏ ".repeat(self.quotes.len());
        let color = self.alert_color();
        let lines = render_table(&table, self.wrap, &prefix, color);
        self.out.extend(lines);
        self.mark_emitted();
    }

    fn finish(mut self) -> Rendered {
        self.flush_block();
        if self.out.is_empty() {
            self.out.push(TextLine::from(""));
        }
        Rendered {
            lines: self.out,
            regions: self.regions,
        }
    }
}

/// Whether a raw inline-HTML fragment is a `<br>` (in any of its spellings).
fn is_br(html: &str) -> bool {
    let t = html.trim().trim_start_matches("</").trim_start_matches('<');
    let t = t.trim_end_matches('>').trim_end_matches('/').trim();
    t.eq_ignore_ascii_case("br")
}

/// Whether a lowercased `<details …>` opening tag carries the `open` attribute.
fn details_open_attr(lower_tag: &str) -> bool {
    lower_tag
        .strip_prefix("<details")
        .and_then(|r| r.split('>').next())
        .is_some_and(|attrs| {
            attrs
                .split_whitespace()
                .any(|a| a == "open" || a == "open=\"\"")
        })
}

/// Parse an `<img …>` tag into `(src, alt)`; `None` when it is not an img.
fn parse_img(html: &str) -> Option<(String, String)> {
    if !html.trim_start().to_ascii_lowercase().starts_with("<img") {
        return None;
    }
    let src = html_attr(html, "src").unwrap_or_default();
    let alt = html_attr(html, "alt").unwrap_or_default();
    Some((src, alt))
}

/// The value of an HTML attribute `name=` (quoted or bare), if present.
fn html_attr(html: &str, name: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let key = format!("{name}=");
    let at = lower.find(&key)? + key.len();
    let rest = &html[at..];
    match rest.chars().next()? {
        q @ ('"' | '\'') => {
            let body = &rest[1..];
            let end = body.find(q)?;
            Some(body[..end].to_string())
        }
        _ => {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        }
    }
}

/// Extract the text of a `<summary>…</summary>`, tags stripped; `None` when the
/// fragment has no summary.
fn parse_summary(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<summary")?;
    let gt = html[open..].find('>')? + open + 1;
    let rest = &html[gt..];
    let end = rest
        .to_ascii_lowercase()
        .find("</summary>")
        .unwrap_or(rest.len());
    Some(strip_tags(rest[..end].trim()))
}

/// Remove `<…>` tags from a fragment, keeping the text between them.
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Find the first bare `http(s)://` URL in `s`, returning `(before, url, after)`.
/// Conservative: the URL ends at whitespace or an enclosing character, and
/// trailing sentence punctuation is left outside it.
fn next_url(s: &str) -> Option<(&str, &str, &str)> {
    let start = [s.find("https://"), s.find("http://")]
        .into_iter()
        .flatten()
        .min()?;
    let after = &s[start..];
    let mut end = after.len();
    for (i, ch) in after.char_indices() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '|' | '[' | ']') {
            end = i;
            break;
        }
    }
    let mut url = &after[..end];
    url = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    if url.ends_with(')') && !url.contains('(') {
        url = &url[..url.len() - 1];
    }
    if url.len() <= "https://".len() {
        return None;
    }
    let real_end = start + url.len();
    Some((&s[..start], &s[start..real_end], &s[real_end..]))
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

/// A local region within a wrapped block: `(line_in_block, start, end, action)`.
type LocalRegion = (usize, u16, u16, MdAction);

/// Word-wrap styled pieces to `wrap` display columns (when set), prefixing the
/// first line with `first` and the rest with `cont`. Returns the lines and their
/// click regions (columns include the prefix). A break sentinel forces a new
/// line; in no-wrap mode it collapses to a space.
fn wrap_spans(
    pieces: &[Piece],
    wrap: Option<usize>,
    first: &str,
    cont: &str,
    prefix_color: Option<Color>,
) -> (Vec<TextLine<'static>>, Vec<LocalRegion>) {
    let prefix_style = Style::default().fg(prefix_color.unwrap_or(Color::DarkGray));

    let Some(width) = wrap else {
        // No wrapping: one line; breaks become spaces.
        let mut spans = vec![TextSpan::styled(first.to_string(), prefix_style)];
        let mut regions = Vec::new();
        let mut col = prefix_width(first) as u16;
        for piece in pieces {
            if piece.is_break() {
                spans.push(TextSpan::styled(" ", piece.span.style));
                col += 1;
                continue;
            }
            let w = UnicodeWidthStr::width(piece.span.content.as_ref()) as u16;
            if let Some(action) = &piece.action {
                regions.push((0, col, col + w, action.clone()));
            }
            spans.push(piece.span.clone());
            col += w;
        }
        return (vec![TextLine::from(spans)], regions);
    };

    let poff = prefix_width(first) as u16; // first and cont share a width
    let budget = width.saturating_sub(poff as usize).max(1);
    let (rows, local) = wrap_pieces(pieces, budget);
    let lines = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| build_line(if i == 0 { first } else { cont }, prefix_style, row))
        .collect();
    let regions = local
        .into_iter()
        .map(|(ln, s, e, a)| (ln, s + poff, e + poff, a))
        .collect();
    (lines, regions)
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

/// Break pieces into lines fitting `width` columns, returning the lines' spans
/// and content-relative click regions. Words split only at real spaces; a
/// non-space run crossing styles stays one unit; an over-wide word hard-breaks.
fn wrap_pieces(pieces: &[Piece], width: usize) -> (Vec<Vec<TextSpan<'static>>>, Vec<LocalRegion>) {
    let width = width.max(1);
    let mut lines: Vec<Vec<TextSpan<'static>>> = Vec::new();
    let mut regions: Vec<LocalRegion> = Vec::new();
    for segment in split_breaks(pieces) {
        let words = to_words(&segment);
        place_words(&words, width, &mut lines, &mut regions);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    (lines, regions)
}

/// Split a piece list at break sentinels, dropping the sentinels.
fn split_breaks(pieces: &[Piece]) -> Vec<Vec<Piece>> {
    let mut segments: Vec<Vec<Piece>> = vec![Vec::new()];
    for piece in pieces {
        if piece.is_break() {
            segments.push(Vec::new());
        } else {
            segments.last_mut().unwrap().push(piece.clone());
        }
    }
    segments
}

/// Group a break-free piece list into words (runs of non-space text split only
/// at real spaces), each run keeping its style and action.
fn to_words(pieces: &[Piece]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Word = Vec::new();
    for piece in pieces {
        let style = piece.span.style;
        let action = &piece.action;
        let mut frag = String::new();
        for ch in piece.span.content.chars() {
            if ch == ' ' || ch == '\t' {
                if !frag.is_empty() {
                    cur.push((std::mem::take(&mut frag), style, action.clone()));
                }
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            } else {
                frag.push(ch);
            }
        }
        // A piece boundary ends the run (styles/actions may change) but not the
        // word — adjacent pieces stay flush, no fabricated space.
        if !frag.is_empty() {
            cur.push((frag, style, action.clone()));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn word_width(word: &Word) -> usize {
    word.iter()
        .map(|(t, _, _)| UnicodeWidthStr::width(t.as_str()))
        .sum()
}

/// Append a word's runs to `line` at column `col`, recording a region per run
/// that carries an action.
fn emit_runs(
    word: &Word,
    line: &mut Vec<TextSpan<'static>>,
    col: &mut u16,
    line_idx: usize,
    regions: &mut Vec<LocalRegion>,
) {
    for (text, style, action) in word {
        let w = UnicodeWidthStr::width(text.as_str()) as u16;
        if let Some(action) = action {
            regions.push((line_idx, *col, *col + w, action.clone()));
        }
        line.push(TextSpan::styled(text.clone(), *style));
        *col += w;
    }
}

/// Greedy word-wrap, hard-breaking any word wider than `width`.
fn place_words(
    words: &[Word],
    width: usize,
    lines: &mut Vec<Vec<TextSpan<'static>>>,
    regions: &mut Vec<LocalRegion>,
) {
    let mut cur: Vec<TextSpan<'static>> = Vec::new();
    let mut col: u16 = 0;
    for word in words {
        let ww = word_width(word);
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                col = 0;
            }
            let mut rest = word.clone();
            while word_width(&rest) > width {
                let (head, tail) = split_word(&rest, width);
                let mut hline = Vec::new();
                let mut hcol = 0u16;
                emit_runs(&head, &mut hline, &mut hcol, lines.len(), regions);
                lines.push(hline);
                rest = tail;
            }
            emit_runs(&rest, &mut cur, &mut col, lines.len(), regions);
            continue;
        }
        let sep = usize::from(!cur.is_empty());
        if !cur.is_empty() && col as usize + sep + ww > width {
            lines.push(std::mem::take(&mut cur));
            col = 0;
        }
        if !cur.is_empty() {
            cur.push(TextSpan::raw(" "));
            col += 1;
        }
        emit_runs(word, &mut cur, &mut col, lines.len(), regions);
    }
    lines.push(cur);
}

/// Split a word at `width` display columns, preserving style/action runs.
fn split_word(word: &Word, width: usize) -> (Word, Word) {
    let mut head: Word = Vec::new();
    let mut tail: Word = Vec::new();
    let mut used = 0usize;
    let mut cutting = false;
    for (text, style, action) in word {
        if cutting {
            tail.push((text.clone(), *style, action.clone()));
            continue;
        }
        let tw = UnicodeWidthStr::width(text.as_str());
        if used + tw <= width {
            head.push((text.clone(), *style, action.clone()));
            used += tw;
        } else {
            let (h, t) = split_at_width(text, (width - used).max(1));
            if !h.is_empty() {
                head.push((h, *style, action.clone()));
            }
            if !t.is_empty() {
                tail.push((t, *style, action.clone()));
            }
            cutting = true;
        }
    }
    (head, tail)
}

fn span_width(spans: &[TextSpan<'static>]) -> usize {
    spans
        .iter()
        .filter(|s| s.content.as_ref() != BREAK)
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

    // No wrapping: one line per row, cells joined by the gutter.
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
                if s.content.as_ref() == BREAK {
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

/// Word-wrap one cell's styled spans to `width` columns (no click regions). A
/// break sentinel splits lines; a too-wide word hard-breaks. Header cells bold.
fn wrap_cell(spans: &[TextSpan<'static>], width: usize, bold: bool) -> Vec<Vec<TextSpan<'static>>> {
    let width = width.max(1);
    let pieces: Vec<Piece> = spans
        .iter()
        .map(|s| {
            if s.content.as_ref() == BREAK || !bold {
                Piece::plain(s.clone())
            } else {
                Piece::plain(TextSpan::styled(
                    s.content.to_string(),
                    s.style.add_modifier(Modifier::BOLD),
                ))
            }
        })
        .collect();
    wrap_pieces(&pieces, width).0
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

    fn text_of(line: &TextLine) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[TextLine]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    fn color_of(lines: &[TextLine], needle: &str) -> Option<Color> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .map(|s| s.style.fg.unwrap_or(Color::Reset))
    }

    fn style_of(lines: &[TextLine], needle: &str) -> Option<Style> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .map(|s| s.style)
    }

    /// Render with all details expanded, exposing regions.
    fn rich(src: &str, wrap: Option<usize>) -> Rendered {
        let hl = Highlighter::new();
        render_rich(src, wrap, &hl, &|_, default| default)
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
        assert_eq!(
            texts(&lines),
            vec![
                "para one".to_string(),
                String::new(),
                "Heading".to_string(),
                String::new(),
                "para two".to_string(),
            ]
        );
    }

    #[test]
    fn no_block_spacing_when_not_wrapping() {
        let hl = Highlighter::new();
        let lines = render("para one\n\npara two", None, &hl);
        assert!(!texts(&lines).iter().any(|l| l.is_empty()));
    }

    #[test]
    fn soft_and_hard_breaks_become_real_line_breaks() {
        let hl = Highlighter::new();
        let lines = render("alpha\nbeta", Some(40), &hl);
        assert_eq!(texts(&lines), vec!["alpha".to_string(), "beta".to_string()]);
        let hard = render("alpha  \nbeta", Some(40), &hl);
        assert_eq!(texts(&hard), vec!["alpha".to_string(), "beta".to_string()]);
        let tight = render("alpha\nbeta", None, &hl);
        assert_eq!(texts(&tight), vec!["alpha beta".to_string()]);
    }

    #[test]
    fn adjacent_styles_keep_no_fabricated_space() {
        let hl = Highlighter::new();
        let lines = render("see `config.toml`, and **bold**. done", Some(40), &hl);
        let joined: String = texts(&lines).join(" ");
        assert!(joined.contains("config.toml,"), "{joined:?}");
        assert!(joined.contains("bold."), "{joined:?}");
    }

    #[test]
    fn a_thematic_break_renders_a_dim_rule() {
        let hl = Highlighter::new();
        let lines = render("above\n\n---\n\nbelow", Some(20), &hl);
        let t = texts(&lines);
        let rule = t.iter().find(|l| l.contains('─')).expect("a rule line");
        assert!(UnicodeWidthStr::width(rule.as_str()) <= 20);
        assert!(rule.chars().all(|c| c == '─'));
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
        assert!(t.iter().any(|l| l.starts_with("  ") && l.contains("inner")));
    }

    #[test]
    fn a_footnote_reference_and_definition_render() {
        let hl = Highlighter::new();
        let lines = render("text[^1]\n\n[^1]: the note", Some(40), &hl);
        let joined: String = texts(&lines).join("\n");
        assert!(joined.contains("[1]"));
        assert!(joined.contains("the note"));
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
            assert!(joined.contains(label), "{label}: {joined:?}");
            assert!(!joined.contains("[!"), "{joined:?}");
            assert_eq!(color_of(&lines, label), Some(color));
        }
    }

    #[test]
    fn a_plain_blockquote_is_unchanged() {
        let hl = Highlighter::new();
        let lines = render("> just a quote", Some(40), &hl);
        let joined: String = lines.iter().map(text_of).collect();
        assert!(joined.contains("just a quote"));
        assert!(!joined.contains("Note") && !joined.contains("[!"));
        assert!(joined.contains('▏'));
    }

    #[test]
    fn blockquote_paragraphs_are_separated_by_a_bar_blank() {
        let hl = Highlighter::new();
        let lines = render("> one\n>\n> two", Some(40), &hl);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.trim() == "▏"));
        assert!(t.iter().any(|l| l.contains("one")) && t.iter().any(|l| l.contains("two")));
    }

    #[test]
    fn task_list_items_render_checkboxes() {
        let hl = Highlighter::new();
        let lines = render("- [ ] todo\n- [x] done", None, &hl);
        assert_eq!(text_of(&lines[0]), "☐ todo");
        assert_eq!(text_of(&lines[1]), "☑ done");
    }

    // --- regions (clickable links / images) ---

    #[test]
    fn a_bare_url_is_autolinked_and_clickable() {
        let r = rich("see https://example.com/path, ok", Some(60));
        let url = r
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "https://example.com/path")
            .expect("the url is its own span");
        assert!(
            url.style.add_modifier.contains(Modifier::UNDERLINED)
                && url.style.fg == Some(palette::LINK_FG)
        );
        // A region opens exactly that url, and covers the url's columns.
        let region = r
            .regions
            .iter()
            .find(|reg| reg.action == MdAction::Open("https://example.com/path".to_string()))
            .expect("a click region for the url");
        let line = text_of(&r.lines[region.line]);
        let slice: String = line
            .chars()
            .skip(region.start as usize)
            .take((region.end - region.start) as usize)
            .collect();
        assert_eq!(
            slice, "https://example.com/path",
            "the region covers the url"
        );
    }

    #[test]
    fn a_markdown_image_renders_a_clickable_placeholder() {
        let r = rich("see ![a shot](https://ex.com/a.png) here", Some(60));
        let joined = texts(&r.lines).join(" ");
        assert!(joined.contains("[Image: a shot]"), "{joined:?}");
        assert!(
            !joined.contains("ex.com"),
            "url not shown inline: {joined:?}"
        );
        assert!(
            r.regions
                .iter()
                .any(|reg| reg.action == MdAction::Open("https://ex.com/a.png".to_string())),
            "the image opens its url"
        );
    }

    #[test]
    fn a_markdown_link_shows_underlined_text_only_and_is_clickable() {
        let r = rich("read [the docs](https://docs.example.com)", Some(60));
        let joined = texts(&r.lines).join(" ");
        assert!(joined.contains("the docs"), "the label shows: {joined:?}");
        assert!(
            !joined.contains("(https://"),
            "no `(url)` suffix: {joined:?}"
        );
        // The label text is underlined link text (wrapping splits it into words).
        let label = style_of(&r.lines, "docs").unwrap();
        assert!(
            label.add_modifier.contains(Modifier::UNDERLINED) && label.fg == Some(palette::LINK_FG)
        );
        // A click on the label opens the destination.
        assert!(
            r.regions
                .iter()
                .any(|reg| reg.action == MdAction::Open("https://docs.example.com".to_string())),
            "regions: {:?}",
            r.regions
        );
    }

    #[test]
    fn an_html_img_becomes_a_placeholder_and_other_html_is_stripped() {
        let hl = Highlighter::new();
        let lines = render(
            "<img src=\"https://e.com/x.png\" alt=\"pic\"> tail",
            Some(60),
            &hl,
        );
        let joined = texts(&lines).join(" ");
        assert!(joined.contains("[Image: pic]"), "{joined:?}");
        assert!(joined.contains("tail"));
        let none = render("<img alt=\"x\"> body", Some(60), &hl);
        assert!(!texts(&none).join(" ").contains("[Image"));
    }

    // --- details fold ---

    #[test]
    fn a_details_defaults_closed_showing_only_its_summary() {
        // Default closed: the summary (▸) shows with a toggle region; body hidden.
        let r = rich(
            "<details>\n<summary>Click me</summary>\n\nHidden body.\n\n</details>",
            Some(60),
        );
        let joined = texts(&r.lines).join("\n");
        assert!(joined.contains("▸ Click me"), "closed marker: {joined:?}");
        assert!(!joined.contains("Hidden body"), "body hidden: {joined:?}");
        assert!(
            r.regions
                .iter()
                .any(|reg| reg.action == MdAction::ToggleDetails(0)),
            "a toggle region on the summary"
        );
    }

    #[test]
    fn an_open_details_shows_its_body() {
        // Fold state opens index 0 → ▾ and the body renders.
        let hl = Highlighter::new();
        let r = render_rich(
            "<details>\n<summary>Click me</summary>\n\nHidden body.\n\n</details>",
            Some(60),
            &hl,
            &|_, _| true,
        );
        let joined = texts(&r.lines).join("\n");
        assert!(joined.contains("▾ Click me"), "open marker: {joined:?}");
        assert!(joined.contains("Hidden body."), "body shows: {joined:?}");
    }

    #[test]
    fn details_open_attribute_defaults_open() {
        let r = rich(
            "<details open>\n<summary>Shown</summary>\n\nVisible body.\n\n</details>",
            Some(60),
        );
        let joined = texts(&r.lines).join("\n");
        assert!(joined.contains("▾ Shown"));
        assert!(joined.contains("Visible body."));
    }

    #[test]
    fn a_folded_details_registers_no_inner_regions() {
        // A link inside a closed details is not hit-testable.
        let r = rich(
            "<details>\n<summary>Hidden</summary>\n\nhttps://inside.example.com\n\n</details>",
            Some(60),
        );
        assert!(
            !r.regions
                .iter()
                .any(|reg| matches!(&reg.action, MdAction::Open(u) if u.contains("inside"))),
            "no region for a link inside a folded details"
        );
    }

    #[test]
    fn a_summaryless_details_shows_its_body() {
        // Robust degrade: no summary → nothing to reopen with, so show the body.
        let r = rich("<details>\n\nplain body\n\n</details>", Some(60));
        assert!(texts(&r.lines).join("\n").contains("plain body"));
    }

    // --- tables ---

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
        assert!(texts[0].contains("Left") && texts[0].contains("Right"));
        assert!(texts[1].contains('─'), "{:?}", texts[1]);
        assert!(texts.iter().any(|t| t.contains("fff")));
        let left = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("Left"))
            .unwrap();
        assert!(left.style.add_modifier.contains(Modifier::BOLD));
        let right_row = &texts[2];
        assert!(right_row.trim_end().ends_with('c'), "{right_row:?}");
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
                "{:?}",
                text_of(line)
            );
        }
        assert!(lines.len() > 3);
    }

    #[test]
    fn a_mean_document_renders_every_construct_within_the_pane() {
        let hl = Highlighter::new();
        let src = "\
# Title

Intro with `code.rs`, https://example.com/x, and ![shot](https://e/i.png).

> [!WARNING]
> Watch out.
>
> > nested quote

| A | B |
| --- | --- |
| one | two<br>three |

<details open>
<summary>More detail</summary>

Inside the fold.

</details>

Text[^1] with a footnote.

[^1]: the note.

---

日本語のテキストも混ぜてみる。";
        let width = 40;
        let lines = render(src, Some(width), &hl);
        for line in &lines {
            assert!(
                UnicodeWidthStr::width(text_of(line).as_str()) <= width,
                "no line overruns the pane: {:?}",
                text_of(line)
            );
        }
        let flat = texts(&lines).join(" ");
        for needle in [
            "Title",
            "https://example.com/x",
            "[Image: shot]",
            "Watch out",
            "nested quote",
            "▾ More detail",
            "Inside the fold.",
            "[1]",
            "the note",
            "日本語",
        ] {
            assert!(flat.contains(needle), "missing {needle:?} in:\n{flat}");
        }
        assert!(!flat.contains('#'));
        assert!(!flat.contains("<summary"));
        assert!(flat.contains('─'));
    }

    #[test]
    fn table_cells_keep_inline_styles_and_cjk_width() {
        let hl = Highlighter::new();
        let src = "\
| Name | Note |
| --- | --- |
| `code` | 日本語 |";
        let lines = render(src, Some(40), &hl);
        let code = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("code"));
        assert!(code.is_some_and(|s| s.style.bg == Some(palette::CODE_INLINE_BG)));
        for line in &lines {
            assert!(UnicodeWidthStr::width(text_of(line).as_str()) <= 40);
        }
    }
}
