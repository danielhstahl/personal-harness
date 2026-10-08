//! Markdown -> pre-wrapped ratatui Lines, plus syntect glue.
//! Everything returned is `Line<'static>` so it can be cached / queued freely.

use std::sync::OnceLock;

use crate::theme::styles::BLUE;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthStr;

const DIM: Style = Style::new().fg(Color::DarkGray);

// ---------- syntect ----------

pub struct Highlighter {
    ss: SyntaxSet,
    theme: Theme,
}

/// Loaded once (slow). `'static` lets `HighlightLines<'static>` live in stream state.
pub fn highlighter() -> &'static Highlighter {
    static HL: OnceLock<Highlighter> = OnceLock::new();
    HL.get_or_init(|| {
        let mut ts = ThemeSet::load_defaults();
        Highlighter {
            ss: SyntaxSet::load_defaults_newlines(),
            theme: ts.themes.remove("base16-ocean.dark").expect("theme"),
        }
    })
}

impl Highlighter {
    pub fn start(&'static self, lang: &str) -> HighlightLines<'static> {
        let syn = self
            .ss
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| self.ss.find_syntax_plain_text());
        HighlightLines::new(syn, &self.theme)
    }

    fn spans(&'static self, hl: &mut HighlightLines<'static>, text: &str) -> Vec<Span<'static>> {
        let text = format!("{}\n", text.replace('\t', "    "));
        match hl.highlight_line(&text, &self.ss) {
            Ok(regions) => regions
                .into_iter()
                .map(|(s, t)| Span::styled(t.trim_end_matches('\n').to_string(), conv(s)))
                .collect(),
            Err(_) => vec![Span::raw(text)],
        }
    }
}

fn conv(s: syntect::highlighting::Style) -> Style {
    let mut st = Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
    if s.font_style.contains(FontStyle::BOLD) {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        st = st.add_modifier(Modifier::ITALIC);
    }
    st
}

/// One highlighted code line with a gutter. Call once per *completed* line.
pub fn code_line(hl: &mut HighlightLines<'static>, text: &str) -> Line<'static> {
    let mut spans = vec![Span::styled("│ ", DIM)];
    spans.extend(highlighter().spans(hl, text));
    Line::from(spans)
}

pub fn code_header(lang: &str) -> Line<'static> {
    let l = if lang.is_empty() { "code" } else { lang };
    Line::styled(format!("┌ {l}"), DIM)
}

// ---------- markdown ----------

#[derive(Default)]
struct R {
    width: usize,
    out: Vec<Wrapped>,
    cur: Vec<Span<'static>>,
    style: Vec<Style>,
    lists: Vec<Option<u64>>, // Some(n) = ordered, next number
    links: Vec<String>,
    quote: usize,
    bullet: Option<String>,
    code: Option<HighlightLines<'static>>,
}

/// Render a *complete* markdown fragment to wrapped lines (no trailing blank).
/// Tables/images/html are not handled in this sketch.
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    unwrap(render_markdown_tracked(src, width))
}

/// As [`render_markdown`], keeping the wrap facts the caller cannot recover from
/// the pixels: which rows ended a logical line and which are a soft wrap of ours.
///
/// The renderer is the only thing in the tree that knows this — the wrap decides
/// the break, and by the time a `Line` comes out the evidence has gone. A store
/// that has to re-wrap later (looprs-pdl.6) and a copy path that has to paste
/// without inventing newlines (ADR-0004 R14) both need it said out loud.
pub fn render_markdown_tracked(src: &str, width: u16) -> Vec<Wrapped> {
    let mut r = R {
        width: width.max(20) as usize,
        ..Default::default()
    };
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(src, opts) {
        r.event(ev);
    }
    r.flush();
    while r.out.last().is_some_and(|l| l.line.spans.is_empty()) {
        r.out.pop();
    }
    r.out
}

/// Drop the wrap metadata, keeping the lines.
pub fn unwrap(rows: Vec<Wrapped>) -> Vec<Line<'static>> {
    rows.into_iter().map(|w| w.line).collect()
}

/// One row of rendered markdown, with the way its end joins to the next row.
#[derive(Clone, Debug)]
pub struct Wrapped {
    pub line: Line<'static>,
    /// `true` = **our soft wrap**: the logical line goes on at the start of the
    /// next row, and joining the two inserts nothing. `false` = a **hard** end,
    /// the source's own line break.
    ///
    /// The distinction is the whole of ADR-0004 R14: paste the joined text of a
    /// soft-wrapped paragraph with a `\n` between the rows and every paragraph
    /// reads as if it was typed with a stuck Enter key.
    pub soft: bool,
}

impl Wrapped {
    pub fn hard(line: Line<'static>) -> Self {
        Self { line, soft: false }
    }
    pub fn soft(line: Line<'static>) -> Self {
        Self { line, soft: true }
    }
}

impl R {
    fn cur_style(&self) -> Style {
        self.style.iter().fold(Style::default(), |a, s| a.patch(*s))
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(t) => self.start(t),
            Event::End(t) => self.end(t),
            Event::Text(t) => {
                if let Some(hl) = self.code.as_mut() {
                    for l in t.lines() {
                        self.out.push(Wrapped::hard(code_line(hl, l)));
                    }
                } else {
                    let s = self.cur_style();
                    self.cur.push(Span::styled(t.to_string(), s));
                }
            }
            Event::Code(t) => self.cur.push(Span::styled(
                t.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Rgb(45, 45, 45)),
            )),
            Event::SoftBreak => self.cur.push(Span::raw(" ")),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.out
                    .push(Wrapped::hard(Line::styled("─".repeat(self.width), DIM)));
                self.out.push(Wrapped::hard(Line::default()));
            }
            Event::TaskListMarker(done) => {
                self.cur.push(Span::raw(if done { "☑ " } else { "☐ " }))
            }
            _ => {}
        }
    }

    fn start(&mut self, t: Tag<'_>) {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        match t {
            Tag::Heading { level, .. } => {
                self.flush();
                let c = match level as usize {
                    1 => Color::Magenta,
                    2 => Color::Cyan,
                    _ => BLUE,
                };
                self.style.push(bold.fg(c));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => {
                        l.split([' ', ',']).next().unwrap_or("").to_string()
                    }
                    _ => String::new(),
                };
                self.out.push(Wrapped::hard(code_header(&lang)));
                self.code = Some(highlighter().start(&lang));
            }
            Tag::List(n) => {
                self.flush();
                self.lists.push(n);
            }
            Tag::Item => {
                self.flush();
                self.bullet = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        s
                    }
                    _ => "• ".into(),
                });
            }
            Tag::Emphasis => self
                .style
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.style.push(bold),
            Tag::Strikethrough => self
                .style
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.style
                    .push(Style::default().fg(BLUE).add_modifier(Modifier::UNDERLINED));
            }
            _ => {}
        }
    }

    fn end(&mut self, t: TagEnd) {
        match t {
            TagEnd::Paragraph => {
                self.flush();
                if self.lists.is_empty() {
                    self.out.push(Wrapped::hard(Line::default()));
                }
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.style.pop();
                self.out.push(Wrapped::hard(Line::default()));
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.code = None;
                self.out.push(Wrapped::hard(Line::default()));
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.out.push(Wrapped::hard(Line::default()));
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style.pop();
            }
            TagEnd::Link => {
                self.style.pop();
                if let Some(u) = self.links.pop() {
                    self.cur.push(Span::styled(format!(" ({u})"), DIM));
                }
            }
            _ => {}
        }
    }

    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let quote = "│ ".repeat(self.quote);
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        let (ft, rt) = match self.bullet.take() {
            Some(b) => {
                let hang = " ".repeat(b.width());
                (b, hang)
            }
            None if !self.lists.is_empty() => ("  ".into(), "  ".into()),
            None => (String::new(), String::new()),
        };
        let spans = std::mem::take(&mut self.cur);
        let lines = wrap(
            spans,
            self.width,
            format!("{quote}{indent}{ft}"),
            format!("{quote}{indent}{rt}"),
        );
        self.out.extend(lines);
    }
}

/// Greedy word-wrap over styled spans, returning *rows* rather than lines.
/// Prefixes count toward `width`. (Words longer than a full line are not
/// hard-split in this sketch.)
///
/// It returns [`Wrapped`] rather than `Line` because the wrap is the only thing
/// that knows which breaks were made by us, and the store that has to re-wrap
/// later and the copy path that must not invent newlines both need that said
/// ([`RowEnd`]/ADR-0004 R14). The rows the loop breaks on are the ones that
/// *continue* onto the next row, so they are `soft`; whatever the input ends
/// with has really ended, so it is `hard`.
///
/// [`RowEnd`]: crate::state::scrollback::RowEnd
pub fn wrap(spans: Vec<Span<'static>>, width: usize, first: String, rest: String) -> Vec<Wrapped> {
    let mk = |p: &str| -> Vec<Span<'static>> {
        if p.is_empty() {
            vec![]
        } else {
            vec![Span::styled(p.to_string(), DIM)]
        }
    };
    let mut lines = Vec::new();
    let mut cur = mk(&first);
    let mut w = first.width();
    let mut has = false;

    for span in spans {
        for piece in span.content.split_inclusive(' ') {
            let pw = piece.trim_end().width();
            if has && w + pw > width {
                trim_trailing(&mut cur);
                lines.push(Wrapped::soft(Line::from(std::mem::replace(
                    &mut cur,
                    mk(&rest),
                ))));
                w = rest.width();
                has = false;
            }
            if !has && piece.trim().is_empty() {
                continue; // no leading whitespace on wrapped lines
            }
            cur.push(Span::styled(piece.to_string(), span.style));
            w += piece.width();
            has = true;
        }
    }
    trim_trailing(&mut cur);
    if has {
        lines.push(Wrapped::hard(Line::from(cur)));
    }
    lines
}

fn trim_trailing(v: &mut [Span<'static>]) {
    if let Some(l) = v.last_mut() {
        let t = l.content.trim_end().to_string();
        l.content = t.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markdown's two blues are the palette's, not `Color::Blue`.
    ///
    /// A heading and a link are the two places a user most needs to pick the
    /// colour out at speed, and both were the worst case for the ANSI basic:
    /// rendered at roughly the background's luminance, an underlined link is a
    /// smear with a line through it. The link assertion checks the underline
    /// too, so the style — not just the colour — is pinned.
    #[test]
    fn a_small_heading_and_a_link_are_the_palettes_blue() {
        let lines = render_markdown(
            "### Heading three\n\nsee [a link](https://example.com)\n",
            40,
        );

        let heading = lines
            .iter()
            .find(|l| l.to_string().contains("Heading three"))
            .expect("the heading rendered");
        assert_eq!(
            heading
                .spans
                .iter()
                .find(|s| s.content.contains("Heading"))
                .expect("the heading span")
                .style
                .fg,
            Some(BLUE)
        );

        let link = lines
            .iter()
            .find(|l| l.to_string().contains("a link"))
            .expect("the link rendered");
        let span = link
            .spans
            .iter()
            .find(|s| s.content.contains("link"))
            .expect("the link span");
        assert_eq!(span.style.fg, Some(BLUE));
        assert!(
            span.style.add_modifier.contains(Modifier::UNDERLINED),
            "the link lost its underline along with its old colour"
        );
    }
}
