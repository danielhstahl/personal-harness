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
    theme::styles::{RED, content_width, restyle, style_for},
    utils::{
        md::{self, Wrapped},
        shelltext::spanned,
    },
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::cell::{Cell, RefCell};
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
        // The user's own words, unmarked. The chevron this dropped used to be
        // the only thing telling the message apart from the assistant's, and it
        // did it with one character at the head of a block that can be many
        // rows long. The distinction moved to the background
        // (`theme::styles::USER_BG`), which covers every row of the message
        // rather than just the first, and which `restyle` paints from
        // `style_for` — so there is nothing to style here, and no prefix to
        // keep in step with it.
        MessageKind::User => md::wrap(
            vec![Span::raw(e.text.clone())],
            w as usize,
            String::new(),
            String::new(),
        ),
        MessageKind::Error => vec![Wrapped::hard(Line::styled(
            format!("error: {}", e.text),
            Style::new().fg(RED),
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
    /// The last live tail this flusher rendered, with the inputs that made it.
    ///
    /// A cache, so it is behind a `RefCell` rather than a `mut`: `preview` is a
    /// read of the transcript and stays a read (the frame takes it through
    /// `App::preview_active` while holding the app immutably), and nothing else
    /// in this type behaves differently because it is here.
    ///
    /// See [`Flusher::preview`] for what invalidates it and why that list is
    /// exactly the inputs.
    tail: RefCell<Option<CachedTail>>,
    /// How many `preview` calls were answered from [`Self::tail`] and how many
    /// had to render.
    ///
    /// Not needed by anything that draws. It is there because a cache whose hit
    /// rate is invisible is a cache nobody can tell the difference between and
    /// one that silently stopped working, and "we saved N frames of parsing" is
    /// the claim this whole change rests on. Read by the tests and by
    /// [`crate::measure`].
    tail_hits: Cell<u64>,
    tail_misses: Cell<u64>,
}

/// One rendered live tail and the complete set of inputs that produced it.
///
/// The rule that makes this cache legal (ADR-0002 Q5, and looprs-00u.14's own
/// note on it): **the count that sizes the live pane and the pixels that fill it
/// must not describe different things.** So a hit is only allowed on every input
/// that can change either — which is what [`CachedTail::is_for`] spells out, byte
/// for byte against the live slice rather than against a fingerprint that could
/// collide. Nothing here is a proxy for the input: the `src` copy *is* the input,
/// and `lines` is what rendering exactly those bytes at exactly `width` gave.
struct CachedTail {
    /// Index of the entry the tail was cut from. Re-rendering identical bytes is
    /// safe in itself, but naming the entry makes "same tail" mean *the same tail*
    /// and not two paragraphs that happen to read alike.
    entry: usize,
    /// Which renderer produced the rows. Same bytes, different mode, different
    /// rows: `Raw`/`Fence` hand over the text verbatim, `Markdown` re-flows it.
    mode: Tail,
    /// Content width the rows were wrapped at. A row's length *is* its shape, so
    /// a width change is a different answer even with no new bytes.
    width: u16,
    /// The style `restyle` folded into every span (`style_for(kind)`).
    style: Style,
    /// The bytes the rows were rendered from, copied out of the entry.
    ///
    /// Copied rather than borrowed because the entry's text grows under us: a
    /// `&str` here would either have to be re-borrowed (which is what the
    /// comparison below does anyway) or would dangle. Kept as one `String` and
    /// `clear`-ed on refill, so a steady stream costs no new allocation once
    /// the largest paragraph has been seen.
    src: String,
    /// The rendered, restyled rows.
    lines: Vec<Line<'static>>,
}

impl CachedTail {
    /// Is this the rendering of exactly this tail?
    ///
    /// Every field of [`LiveTail`] appears here, in the order it is cheapest to
    /// compare: three integers first, the style, then the bytes. The byte
    /// comparison is the expensive one and it is last; on a stream where the
    /// length changed it is not even reached, because a different `src.len()`
    /// fails `String == str` immediately.
    fn is_for(&self, t: &LiveTail<'_>, width: u16) -> bool {
        self.entry == t.entry
            && self.mode == t.mode
            && self.width == width
            && self.style == t.style
            && self.src == t.text
    }
}

/// How the live tail renders. Part of the tail's identity: the same bytes take a
/// different path depending on which of these they are standing in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tail {
    /// Shell output — verbatim, never markdown, never re-wrapped
    /// (ADR-0001 rule 1).
    Raw,
    /// Inside an open fenced code block — the current line, verbatim, with the
    /// highlighter's styling from the fence's own syntax state.
    Fence,
    /// The open prose block — parsed as markdown and wrapped.
    Markdown,
}

/// What the live tail *is*, cut out of the transcript in one place.
///
/// Extracted from the body of [`Flusher::preview`] (looprs-00u.14) so that
/// "which bytes are live" is a question with one answer. The cache below keys on
/// it, the measurement in `crate::measure` buckets by it, and a preview that
/// decided what to render somewhere else would be a cache that can be lied to.
pub struct LiveTail<'a> {
    /// Index of the entry the tail was cut from — `Flusher::first`.
    pub entry: usize,
    /// Which renderer it takes.
    pub mode: Tail,
    /// The style the rows get rendered with.
    pub style: Style,
    /// The live bytes themselves: `text[scan..]` for a verbatim tail,
    /// `text[block..]` for prose. Its length is what a render of this tail
    /// costs a pass over, which is the number the measurement in
    /// [`crate::measure`] buckets by.
    pub text: &'a str,
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
            tail: RefCell::new(None),
            tail_hits: Cell::new(0),
            tail_misses: Cell::new(0),
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
        // The cached rows were rendered from the entry `cur` was reading, and
        // that entry is gone from under this cursor now. Dropping the cache is
        // not needed to keep a *hit* correct (`is_for` compares the bytes, so
        // a moved cursor cannot match) — it is here so the cache is never the
        // thing that has to be reasoned about at a reseat.
        self.tail.borrow_mut().take();
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
        // Same reason as in `reseat`, with a sharper edge: a cut changes the
        // bytes at the head of the live slice while leaving the tail's *length*
        // able to match the cached one. The `src` comparison would still catch
        // it; dropping the cache means the buffer cap and the preview never have
        // to be reasoned about together.
        self.tail.borrow_mut().take();
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
            // The entry that the cached tail belonged to is finished: its rows
            // are in the store now, and the live region is a different piece of
            // text. Free them here rather than at the next preview, which may
            // never come.
            self.tail.borrow_mut().take();
        }
        out
    }

    /// `(hits, misses)` for the live-tail cache: how many `preview` calls were
    /// answered from [`Self::tail`], how many had to render.
    ///
    /// Nothing that draws reads this. A cache whose hit rate is invisible is a
    /// cache whose failure is invisible too, and "we stopped re-parsing frames
    /// that did not need it" is the whole claim looprs-00u.14 makes — so the
    /// counter is cheap and always on, read by the tests that pin the behaviour
    /// and by the measurement that reports the rate.
    #[allow(dead_code)] // measurement + test seam: `measure.rs` reports the hit rate the live-preview cache earns
    pub fn tail_cache_stats(&self) -> (u64, u64) {
        (self.tail_hits.get(), self.tail_misses.get())
    }

    /// What the live tail is right now, cut by the same cursor rules the renderer
    /// uses. `None` when there is nothing live: no entry, or the entry this
    /// flusher stands on has finished.
    ///
    /// The one place that answers "which bytes are the preview", so that the
    /// cache in [`Flusher::preview`] cannot be handed a different definition of
    /// live than the one it was filled with.
    pub fn live_tail<'a>(&self, t: &'a Transcript) -> Option<LiveTail<'a>> {
        let e = t.entries.get(self.first).filter(|e| !e.done)?;
        let c = &self.cur;
        let (mode, start) = if e.kind.is_raw() {
            // The unfinished last line of shell output: verbatim, and never
            // re-flowed through the markdown path.
            (Tail::Raw, c.scan)
        } else if c.fence.is_some() {
            (Tail::Fence, c.scan)
        } else {
            (Tail::Markdown, c.block)
        };
        Some(LiveTail {
            entry: self.first,
            mode,
            style: style_for(&e.kind),
            text: &e.text[start..],
        })
    }

    /// The not-yet-final tail of the active entry. A pure read of the
    /// transcript: same data, same cursor, same rows.
    ///
    /// It is *cached*, and asking for it twice has to cost a clone.
    ///
    /// Before looprs-00u.14 this re-rendered the whole open prose block on
    /// every call, and the frame calls it on every draw — 60 fps while the row
    /// animation turns, whether or not a byte arrived. Two facts decided what to
    /// do about it, both measured over this repo's own `pi` corpus; the numbers
    /// are in `spikes/results/live-preview-cost.log` and they are not the ones
    /// the ticket expected:
    ///
    /// * the re-parse is **not the answer, it is the paragraph**. `Cursor::block`
    ///   restarts at every blank line, so the live slice is the open paragraph:
    ///   p50 119 B, p99 1,120 B, max 3,039 B, while the entries it is cut from
    ///   reach 56,362 B. There is no cost that grows with the answer, and the
    ///   worst block measured re-rendered in 313 µs (dev) — under the 1 ms the
    ///   ticket set as its own bar for "no code change needed";
    /// * what *was* real is the bytes, and the frames that changed nothing.
    ///   A draw of a 2–4 KiB paragraph allocated 91,496 B; pooled over the
    ///   corpus, 18,840 B per call, ~1.1 MB/s of churn from one streaming
    ///   view at 60 fps. And the redraw clock ticks faster than deltas arrive,
    ///   so a large share of those frames re-render bytes that had not changed.
    ///
    /// So the fix is the first one off the ticket's list — *skip when nothing
    /// changed* — and it lands on the floor it sets: a hit costs 3.9 µs /
    /// 1,626 B against the 26.5 µs / 18,840 B it replaced (**6.8x** the time,
    /// **11.6x** the bytes), which is exactly what cloning the rows costs.
    /// A miss costs the old work *plus* the clone handed back and the `src`
    /// copy kept for the comparison — +5.6 µs (+21%) on a frame that carries
    /// news, against −22.6 µs on one that does not.
    ///
    /// Correctness is the [`CachedTail::is_for`] list, and the invariant it
    /// protects is ADR-0002 Q5's: the count that sizes the live pane and the
    /// pixels that fill it must not describe different things. A width change
    /// is a miss (rows are their shape — measured: a width change still costs a
    /// full parse, 15.8 µs → 19.2 µs, and never an answer from the other
    /// width); a style change is a miss; the bytes themselves are compared, not
    /// hashed. `mod tests` pins each of those with the hit/miss counter rather
    /// than a stopwatch.
    pub fn preview(&self, t: &Transcript, term_width: u16) -> Vec<Line<'static>> {
        let Some(tail) = self.live_tail(t) else {
            // Nothing live. Drop what the cache was holding rather than keep a
            // rendered paragraph that no screen will ask for again — an answer's
            // last paragraph stays on the heap after the answer is over
            // otherwise, per view, forever.
            self.tail.borrow_mut().take();
            return vec![];
        };
        let width = content_width(term_width);

        if let Some(c) = self.tail.borrow().as_ref()
            && c.is_for(&tail, width)
        {
            self.tail_hits.set(self.tail_hits.get() + 1);
            return c.lines.clone();
        }
        self.tail_misses.set(self.tail_misses.get() + 1);

        let lines: Vec<Line<'static>> = match tail.mode {
            Tail::Raw | Tail::Fence => vec![Line::from(tail.text.to_string())],
            Tail::Markdown => md::render_markdown(tail.text, width),
        };
        let lines: Vec<Line<'static>> = lines.into_iter().map(|l| restyle(l, tail.style)).collect();

        let mut slot = self.tail.borrow_mut();
        let c = slot.get_or_insert_with(|| CachedTail {
            entry: 0,
            mode: Tail::Markdown,
            width,
            style: tail.style,
            src: String::new(),
            lines: Vec::new(),
        });
        c.entry = tail.entry;
        c.mode = tail.mode;
        c.width = width;
        c.style = tail.style;
        // Reuse the buffer: `clear` keeps the allocation, so a stream that only
        // ever grows the paragraph stops allocating after its longest one.
        c.src.clear();
        c.src.push_str(tail.text);
        c.lines = lines;
        c.lines.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::transcript::Transcript;
    use ratatui::style::Modifier;

    /// Stream one answer into a transcript+flusher pair and take the preview,
    /// in the order the frame does: settle, then look at what is still live.
    fn live(text: &str, width: u16) -> (Transcript, Flusher) {
        let mut t = Transcript::default();
        let mut f = Flusher::new();
        t.push_delta(MessageKind::Answer, text);
        let _ = f.drain_rows(&t, width);
        (t, f)
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| crate::utils::render::plain(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **The cache is doing its job.** Two previews of an unchanged live tail,
    /// and the second one is not allowed to render.
    ///
    /// The counter is the assertion: timing a debug-build parse against a clone
    /// says "probably faster", which is not a fact anyone can defend in six
    /// months. Hits and misses either happened or they did not.
    #[test]
    fn an_unchanged_live_tail_is_served_from_the_cache() {
        let (t, f) = live(
            "The quick brown fox jumps over the lazy dog, and then the whole \
             paragraph sits still while the redraw clock turns over.\n",
            60,
        );
        let first = f.preview(&t, 60);
        assert!(!first.is_empty(), "the tail rendered at all");
        assert_eq!(f.tail_cache_stats(), (0, 1), "first call rendered");

        let second = f.preview(&t, 60);
        assert_eq!(second, first, "the same bytes draw the same rows");
        assert_eq!(
            f.tail_cache_stats(),
            (1, 1),
            "and the second call did not render them again"
        );
    }

    /// **New bytes are never served the old answer.** The whole reason a cache
    /// of a live region is allowed to exist is that the region's own bytes are
    /// part of the key; this is the case where getting it wrong is a user-facing
    /// bug — the answer on screen silently stops being the answer.
    #[test]
    fn new_bytes_in_the_live_tail_repaint_and_never_reuse_the_stale_rows() {
        let (mut t, f) = live("The answer is going to be ", 60);
        let before = f.preview(&t, 60);
        assert_eq!(text_of(&before), "The answer is going to be");

        t.push_delta(MessageKind::Answer, "quite clearly 42, and not 7.");
        let after = f.preview(&t, 60);
        assert!(
            text_of(&after).contains("42"),
            "the new words are on screen: {:?}",
            text_of(&after)
        );
        assert_ne!(after, before, "the rows changed, as they had to");
        assert_eq!(
            f.tail_cache_stats(),
            (0, 2),
            "a changed tail is a miss, whatever the widths happen to be"
        );
    }

    /// **A width change repaints in both directions.** Rows are their shape, so
    /// a window change is a change to the answer itself — the ADR-0002 Q5 case:
    /// the count that sizes the live pane and the pixels that fill it have to
    /// agree, and they cannot if a hit is allowed across a width.
    #[test]
    fn a_width_change_rerenders_and_going_back_rerenders_again() {
        let long = "A paragraph long enough that the number of rows it makes depends \
                   on the window it is drawn in, which is the only property this \
                   test is actually about. "
            .repeat(3);
        let (t, f) = live(&long, 80);

        let wide = f.preview(&t, 80);
        let narrow = f.preview(&t, 40);
        assert!(
            narrow.len() > wide.len(),
            "the narrow window wraps more rows: {} vs {}",
            narrow.len(),
            wide.len()
        );

        let back = f.preview(&t, 80);
        assert_eq!(
            back, wide,
            "back at 80 the rows are the 80-col rows, not last frame's 40-col ones"
        );
        assert_ne!(back, narrow);
        assert_eq!(
            f.tail_cache_stats(),
            (0, 3),
            "every width in that sequence had to render — a cache that answered \
             any of them across widths would be drawing the wrong shape"
        );

        let again = f.preview(&t, 80);
        assert_eq!(again, back);
        assert_eq!(f.tail_cache_stats(), (1, 3), "and only now a hit");
    }

    /// **A finished entry has no live tail, and leaves nothing cached behind.**
    /// Otherwise a later entry whose first paragraph matched the last paragraph
    /// of the old one could be drawn with the old entry's rows — same words,
    /// wrong message, wrong styling.
    #[test]
    fn a_finished_entry_previews_empty_and_clears_what_it_had_cached() {
        let words = "The very same sentence, twice, in two different entries.\n";
        let (mut t, mut f) = live(words, 60);
        let answer = f.preview(&t, 60);
        assert!(!answer.is_empty());

        t.finish_last();
        assert!(
            f.preview(&t, 60).is_empty(),
            "a done entry is not live: nothing to preview"
        );

        let _ = f.drain_rows(&t, 60);
        t.push_delta(MessageKind::Thinking, words);
        let thought = f.preview(&t, 60);
        assert!(!thought.is_empty(), "the new entry previews");
        assert_ne!(
            thought, answer,
            "same bytes, different entry: the rows are not the old answer's"
        );
        assert!(
            thought
                .iter()
                .flat_map(|l| l.spans.iter())
                .all(|s| s.style.add_modifier.contains(Modifier::ITALIC)),
            "and they carry thinking's style, not the answer's — \
             a cached row would have brought the old one"
        );
    }

    /// **A cut under the cursor cannot leave cut bytes on screen.**
    ///
    /// The per-view buffer cap eats the head of the entry the flusher is still
    /// reading. That moves the live slice's *contents* while leaving its length
    /// able to match a cached entry's — the one shape of change a length-based
    /// cache would answer with stale text.
    #[test]
    fn cutting_the_head_off_the_open_entry_does_not_preview_the_cut_bytes() {
        let mut t = Transcript::default();
        let mut f = Flusher::new();
        t.push_delta(
            MessageKind::Answer,
            "HEAD-SENTINEL the head gets eaten here, and the tail survives the cut.\n",
        );
        let _ = f.drain_rows(&t, 60);
        let before = f.preview(&t, 60);
        assert!(text_of(&before).starts_with("HEAD-SENTINEL"));

        let cut = t.cut_entry_head(0, "HEAD-SENTINEL the head gets eaten here".len());
        f.cut_front(cut);
        let after = f.preview(&t, 60);
        assert!(
            !text_of(&after).contains("HEAD-SENTINEL"),
            "the eaten bytes are not on screen: {:?}",
            text_of(&after)
        );
        assert!(
            text_of(&after).starts_with(", and the tail survives"),
            "what is left of the tail is: {:?}",
            text_of(&after)
        );
        assert_eq!(f.tail_cache_stats(), (0, 2), "the cut was a miss");
    }

    /// **A reseat invalidates.** Same reasoning as the cut, from the other
    /// direction the buffer cap can move the cursor: `reseat` points the
    /// flusher at a different entry entirely.
    #[test]
    fn a_reseat_does_not_hand_back_the_previous_entrys_rows() {
        let (t, mut f) = live("First entry's live paragraph, cached.\n", 60);
        let _ = f.preview(&t, 60);
        f.reseat(0);
        let after = f.preview(&t, 60);
        assert_eq!(
            text_of(&after),
            "First entry's live paragraph, cached.",
            "re-reading the same entry still reads the same text"
        );
        assert_eq!(
            f.tail_cache_stats(),
            (0, 2),
            "but the reseat dropped the cache rather than being checked against it"
        );
    }

    /// **The verbatim tail is cached on the same terms.** Shell output takes the
    /// `Raw` branch — no markdown, one line — and it is redrawn on every frame
    /// of a fast-filling shell just like prose. Same rule, same cache; and the
    /// mode is in the key, so a byte-identical tail that switched branch (a
    /// fence opening) is not answered across branches.
    #[test]
    fn a_raw_tail_is_cached_and_a_branch_change_is_a_miss() {
        let mut t = Transcript::default();
        let mut f = Flusher::new();
        t.push_delta(
            MessageKind::Bash,
            "ls -l /tmp\nsome shell output, still arriving",
        );
        let _ = f.drain_rows(&t, 60);
        let a = f.preview(&t, 60);
        assert_eq!(text_of(&a), "some shell output, still arriving");
        let b = f.preview(&t, 60);
        assert_eq!(a, b);
        assert_eq!(f.tail_cache_stats(), (1, 1), "raw hits too");

        // Same text, but this time the branch is Fence rather than Raw: the
        // mode is part of the identity, so the raw render must not answer it.
        let mut t2 = Transcript::default();
        let mut f2 = Flusher::new();
        t2.push_delta(MessageKind::Answer, "some shell output, still arriving");
        let _ = f2.drain_rows(&t2, 60);
        // open a fence by hand so the branch is Fence with identical bytes
        f2.cur.fence = Some(("```".into(), md::highlighter().start("rust")));
        f2.cur.scan = 0;
        let c = f2.preview(&t2, 60);
        assert_eq!(text_of(&c), "some shell output, still arriving");
        assert_eq!(
            f2.tail_cache_stats(),
            (0, 1),
            "a fresh flusher renders; and a second call hits on the Fence key"
        );
        let d = f2.preview(&t2, 60);
        assert_eq!(c, d);
        assert_eq!(f2.tail_cache_stats(), (1, 1));
    }
}
