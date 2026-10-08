//! The transcript renderer: finalized entries in, display rows out.
//!
//! This module used to be called `scrollback`. It is not any more, because that
//! word was doing double duty across two unrelated types and every "scrollback"
//! in a review of this area had to be resolved by which file the line was in
//! (looprs-di9). The split, so the next reader does not have to rediscover it:
//!
//! * [`crate::state::scrollback::Scrollback`] — the **store**. The bounded
//!   rows, the offset, the pin, the `N new` count, the trim marker, and the
//!   thing a selection or a re-wrap addresses by content.
//! * this module's [`Flusher`] — the **renderer**. A cursor into one
//!   [`Transcript`] that turns each newly finalized line into the
//!   [`RenderedRow`]s the store is made of.
//!
//! Only one of them could keep the word, and the store won it: the user scrolls
//! the store. The renderer keeps a name about what it *does* — it flushes
//! settled text out of the transcript and into rows, one frame at a time — so
//! the module is `line_render` and the type is still `Flusher`.
//!
//! The pairing rule is ADR-0002 Q5's and is not changed here: a `Flusher` is a
//! cursor into **one specific** [`Transcript`], so the two are created together
//! and only ever used together, under
//! [`SessionView`](crate::session::view::SessionView). This module holds the
//! rendering half only, and knows nothing of the store beyond the shape of the
//! row it hands over.
//!
//! [`Transcript`]: crate::state::transcript::Transcript
use crate::{
    components::card::card_line,
    state::scrollback::RowEnd,
    state::transcript::{Entry, MessageKind, Transcript},
    theme::styles::{content_width, restyle, style_for},
    utils::{
        md::{self, Wrapped},
        shelltext::spanned,
    },
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;

/// One finalized line, as the flusher hands it to the scrollback store.
///
/// The store needs more than the pixels: it needs to know **which entry** the row
/// came from (selection must not cross a mode boundary, and must not select a
/// card's chrome) and whether the row **ends a logical line or continues one**
/// (ADR-0004 R14: a soft-wrapped pair joins with nothing, a hard pair with
/// exactly one `\n`). The renderer is the only thing that ever knows either, so
/// this is the type that carries them out of here.
#[derive(Clone, Debug)]
pub struct RenderedRow {
    /// Index of the transcript entry this row was rendered from.
    pub entry: usize,
    /// Hard newline vs. our soft wrap.
    pub end: RowEnd,
    pub line: Line<'static>,
}

/// The row's plain text — what the screen got, minus the styling. Every caller
/// that reads a flush back (mostly tests) reads it through here.
impl std::fmt::Display for RenderedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::utils::render::plain(&self.line))
    }
}

fn render_simple(e: &Entry, w: u16) -> Vec<Wrapped> {
    let mut rows = match &e.kind {
        MessageKind::User => md::wrap(
            vec![Span::raw(e.text.clone())],
            w as usize,
            "❯ ".into(),
            "  ".into(),
        ),
        MessageKind::Error => vec![Wrapped::hard(Line::styled(
            format!("error: {}", e.text),
            Style::new().red(),
        ))],
        MessageKind::System => vec![Wrapped::hard(Line::styled(
            format!("• {}", e.text),
            Style::new().yellow(),
        ))],
        _ => unreachable!("streamed kinds use drain_stream"),
    };
    rows.push(Wrapped::hard(Line::default())); // blank line after each entry
    rows
}

pub struct Flusher {
    first: usize, // entries[..first] are fully written to scrollback
    cur: Cursor,  // progress within entries[first]
}

#[derive(Default)]
struct Cursor {
    scan: usize,  // start of the first unconsumed line
    block: usize, // start of the open prose block (<= scan)
    fence: Option<(String, HighlightLines<'static>)>,
}

impl Flusher {
    pub fn new() -> Self {
        Self {
            first: 0,
            cur: Cursor::default(),
        }
    }

    /// Move the cursor to `first` and discard its in-progress per-entry state.
    ///
    /// Mandatory whenever the index moves rather than advancing normally: `cur`
    /// (scan/block/fence) is only meaningful for the entry `first` was reading, so
    /// pointing it somewhere else without resetting would make `drain_stream` slice
    /// the wrong text — the worst kind of bug, because it only shows up as garbled
    /// scrollback long after the cause. Used by the per-view buffer cap.
    pub fn reseat(&mut self, first: usize) {
        self.first = first;
        self.cur = Cursor::default();
    }

    /// Everything written to scrollback: `entries[..consumed]`. The per-view buffer
    /// cap asks this so it can tell a line the terminal already has from one it has
    /// not emitted yet — the first may be dropped, the second never may.
    pub fn consumed(&self) -> usize {
        self.first
    }

    /// How far into the entry this flusher is *currently* reading it has already
    /// rendered, in bytes.
    ///
    /// Only meaningful together with [`Self::consumed`]: `scan` is state of the
    /// entry `first` points at, which is why the buffer cap asks the two of a
    /// pair before it cuts anything. The bytes past this mark have not reached
    /// the store yet.
    #[allow(dead_code)] // measurement seam: `view::tests` asserts this never runs past the text the entry still has after a cut
    pub fn emitted(&self) -> usize {
        self.cur.scan
    }

    /// Lower the read cursor by `n` bytes because the front of the entry being
    /// read no longer exists.
    ///
    /// This is **not** [`Self::reseat`], and the difference is the whole point:
    /// a reseat rewinds to the head of an entry and would re-emit lines the
    /// store already has, duplicating everything on screen. Here the lines
    /// before the cursor stay emitted and the cursor simply follows the bytes
    /// down. `block` is kept under `scan`, which is the invariant
    /// `drain_stream` relies on for the open prose block.
    ///
    /// The caller owns the precondition that `first` indexes the entry whose
    /// front was cut; this type cannot check it, which is why only the buffer
    /// cap calls it, immediately after asking `consumed()`.
    pub fn cut_front(&mut self, n: usize) {
        self.cur.scan = self.cur.scan.saturating_sub(n);
        self.cur.block = self.cur.block.saturating_sub(n);
    }

    /// Everything that became final since the last call, with provenance. Call
    /// once per frame, before drawing.
    ///
    /// The markdown/fence/raw logic is the one the inline scrollback always had:
    /// what this adds is the entry index the loop is already standing on and the
    /// hard/soft end the wrap already chose, because the store that consumes
    /// this cannot recover either from the pixels. `drain`-shaped callers use
    /// `.map(|r| r.line)`.
    pub fn drain_rows(&mut self, t: &Transcript, term_width: u16) -> Vec<RenderedRow> {
        let w = content_width(term_width);
        let mut out = Vec::new();
        while let Some(e) = t.entries.get(self.first) {
            let rows = if e.kind.is_raw() {
                // Shell output: verbatim lines, no markdown, no re-wrap
                // (ADR-0001 rule 1). Complete lines go out as they arrive; the
                // partial tail stays in the live region.
                self.cur.drain_raw(e)
            } else if e.kind.is_streamed() {
                self.cur.drain_stream(e, w)
            } else if !e.done {
                break; // an open card (tool or compaction): the viewport owns it
            } else if matches!(
                e.kind,
                MessageKind::Tool { .. } | MessageKind::Compaction { .. }
            ) {
                // A finished card: one row, drawn by the same renderer the live
                // band uses, so the scrollback copy and the live copy cannot
                // disagree about what the card said. Spinner frame is irrelevant
                // once finished.
                vec![Wrapped::hard(card_line(e, 0))]
            } else {
                render_simple(e, w) // User / Error
            };
            let entry = self.first;
            let style = style_for(&e.kind);
            out.extend(rows.into_iter().map(|r| RenderedRow {
                entry,
                end: if r.soft { RowEnd::Soft } else { RowEnd::Hard },
                line: restyle(r.line, style),
            }));
            if !e.done {
                break;
            }
            self.first += 1;
            self.cur = Cursor::default();
        }
        out
    }

    /// The not-yet-final tail of the active entry. A pure read: same data, same cursor.
    pub fn preview(&self, t: &Transcript, term_width: u16) -> Vec<Line<'static>> {
        let Some(e) = t.entries.get(self.first).filter(|e| !e.done) else {
            return vec![];
        };
        let c = &self.cur;
        let lines = if e.kind.is_raw() {
            // The unfinished last line of shell output: verbatim, and never
            // re-flowed through the markdown path.
            vec![Line::from(e.text[c.scan..].to_string())]
        } else if c.fence.is_some() {
            vec![Line::from(e.text[c.scan..].to_string())]
        } else {
            md::render_markdown(&e.text[c.block..], content_width(term_width))
        };
        lines
            .into_iter()
            .map(|l| restyle(l, style_for(&e.kind)))
            .collect()
    }
}

impl Cursor {
    /// Raw, line-at-a-time rendering for shell output.
    ///
    /// Deliberately *not* `drain_stream`: that one accumulates a prose block and
    /// renders it through markdown when the block closes. Shell output must reach
    /// the scrollback as the exact lines the child wrote, as soon as each line is
    /// complete — waiting for a blank line would hold back everything a shell does
    /// between prompts, and markdown would reinterpret `# comment` as a heading.
    /// One line at a time, with the styles the resolver put on them (ADR-0005).
    ///
    /// Deliberately *not* `drain_stream`: that one accumulates a prose block and
    /// renders it through markdown when the block closes. Shell output must reach
    /// the scrollback as the exact lines the child wrote, as soon as each line is
    /// complete — waiting for a blank line would hold back everything a shell does
    /// between prompts, and markdown would reinterpret `# comment` as a heading.
    ///
    /// No re-wrap and no re-parse happens here either: the text is already
    /// resolved (control-free, tab-free, overwrites applied) and the styles are
    /// byte ranges into it, so this function's whole job is to cut both at the
    /// line boundaries it is already standing on.
    fn drain_raw(&mut self, e: &Entry) -> Vec<Wrapped> {
        let mut out = Vec::new();
        while let Some(nl) = e.text[self.scan..].find('\n') {
            let end = self.scan + nl;
            out.push(Wrapped::hard(spanned(&e.text, &e.styles, self.scan, end)));
            self.scan = end + 1;
        }
        if e.done && self.scan < e.text.len() {
            out.push(Wrapped::hard(spanned(
                &e.text,
                &e.styles,
                self.scan,
                e.text.len(),
            )));
            self.scan = e.text.len();
        }
        out
    }

    fn drain_stream(&mut self, e: &Entry, w: u16) -> Vec<Wrapped> {
        let mut out = Vec::new();
        while let Some(nl) = e.text[self.scan..].find('\n') {
            let (start, end) = (self.scan, self.scan + nl);
            self.on_line(&e.text, start, end, end + 1, w, &mut out);
            self.scan = end + 1;
        }
        if e.done {
            if self.scan < e.text.len() {
                let len = e.text.len();
                self.on_line(&e.text, self.scan, len, len, w, &mut out);
                self.scan = len;
            }
            self.flush_block(&e.text, e.text.len(), w, &mut out);
            self.fence = None;
        }
        out
    }

    fn on_line(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        next: usize,
        w: u16,
        out: &mut Vec<Wrapped>,
    ) {
        let line = text[start..end].trim_end_matches('\r');
        if let Some((marker, hl)) = self.fence.as_mut() {
            if line.trim_start().starts_with(marker.as_str()) {
                self.fence = None;
                out.push(Wrapped::hard(Line::default()));
            } else {
                out.push(Wrapped::hard(md::code_line(hl, line)));
            }
        } else if let Some(marker) = fence_marker(line) {
            self.flush_block(text, start, w, out);
            out.push(Wrapped::hard(md::code_header(lang_of(line))));
            self.fence = Some((marker.into(), md::highlighter().start(lang_of(line))));
        } else if line.trim().is_empty() {
            self.flush_block(text, start, w, out);
        } else {
            return; // stays part of the open prose block
        }
        self.block = next;
    }

    fn flush_block(&self, text: &str, end: usize, w: u16, out: &mut Vec<Wrapped>) {
        let src = &text[self.block..end];
        if !src.trim().is_empty() {
            out.extend(md::render_markdown_tracked(src, w));
            out.push(Wrapped::hard(Line::default()));
        }
    }
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    ["```", "~~~"].into_iter().find(|m| t.starts_with(m))
}
fn lang_of(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches(['`', '~'])
        .trim()
        .split([' ', ','])
        .next()
        .unwrap_or("")
}
