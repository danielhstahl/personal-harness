//! The scrollable transcript store (looprs-pdl.6).
//!
//! Until this ticket the finalized lines were pushed *out* — `insert_before`
//! wrote them into the terminal's own scrollback above the pane and the app kept
//! only what was live. In the alternate screen there is no "out" any more
//! (ADR-0004 R1), so the transcript becomes the thing that is scrolled, and the
//! thing that is scrolled needs a store under it: the rendered rows, an offset,
//! and a follow-the-tail rule.
//!
//! # The name
//!
//! This is the only thing in the crate called *scrollback*, and that is since
//! looprs-di9. It used to share the word — and the file name — with the
//! renderer that produces its rows
//! ([`crate::components::line_render::Flusher`]), which left every
//! "scrollback" in a review comment ambiguous between **the state** and
//! **the thing that draws it**, and left "do I mean the store or the
//! flusher?" as a question the reader had to resolve by which module the line
//! was in.
//!
//! The store keeps the word because the store *is* what the user scrolls: the
//! band draws out of it, the offset and the pin are its fields, the selection
//! and its anchors address it by content, and the trim marker is its row. The
//! renderer is named for what it does — it flushes finalized text out of a
//! [`Transcript`](crate::state::transcript::Transcript) into rows — and lives
//! under [`crate::components::line_render`] so that reading `scrollback`
//! anywhere in this tree resolves to exactly one type.
//!
//! # The shape
//!
//! [`Scrollback`] is a `Vec<DisplayRow>` plus scroll state. The rows come from
//! the existing [`Flusher`](crate::components::line_render::Flusher) — the
//! markdown/fence/raw rendering stays exactly where it is — and this module adds
//! the three things the renderer knows but had nowhere to put:
//!
//! * **which entry** a row came from ([`DisplayRow::entry`]), so a selection can
//!   refuse to cross a mode boundary or select a card's chrome (looprs-pdl.9);
//! * **hard newline or our soft wrap** ([`RowEnd`]), so joining rows back into
//!   text inserts nothing for a continuation and exactly one `\n` between two
//!   logical lines (ADR-0004 R14) — see [`paste_text`];
//! * **the cell→character map** of the row ([`CellMap`]), so a cell range can be
//!   snapped to whole clusters and never emits half a CJK glyph or half a
//!   joined emoji sequence.
//!
//! The row keeps its styles rather than stripping them (ADR-0005 chose *decode
//! SGR*, not *strip*), which is why a row carries a `Line` and not a `String`;
//! the copy path drops the presentation on the way out, exactly as ADR-0004 R14
//! says it may.
//!
//! # Scroll state
//!
//! Three fields and one rule, all of them user-visible:
//!
//! * `offset` — rows hanging between the bottom of the view and the tail. `0`
//!   *is* pinned; there is no second "pinned" notion to keep in step with it,
//!   only the cached boolean that [`Scrollback::set_offset`] derives from it.
//! * `pinned` — new output appends and the view follows.
//! * `pending` — rows that arrived while unpinned, so the "N new" affordance can
//!   say so out loud rather than let the user take a held view for the tail.
//!
//! # The bound, and what it is bound in
//!
//! The store is the transcript, and the transcript is now ours to bound
//! (looprs-pdl.7). Two things about that bound are load-bearing:
//!
//! * **Bytes, not rows.** `4096 rows` says nothing about size: a rendered
//!   markdown row of prose and a 4 KB row of base64 both count one, and the
//!   second carries orders of magnitude more structure — one [`CellSpan`] per
//!   cluster, a `Vec<Span>` per line, a `String` per span. The cap is therefore
//!   in **retained bytes**, and a row's retained bytes are its own text plus
//!   [`ROW_STRUCT_BYTES`], the per-row structure measured in
//!   `spikes/results/scrollback-cost.log`. Counting the structure is what makes
//!   the cap still mean something for a transcript of a million empty rows.
//! * **The trim is loud.** Content that goes off the head of the store leaves a
//!   [`RowKind::TrimMarker`] row in its place: `scrollback trimmed: N earlier
//!   lines dropped`, plus the journal path when the app has one to give
//!   ([`Scrollback::set_trim_hint`]). A scrollback that silently starts halfway
//!   through an answer is worse than a shorter one, because the user cannot tell
//!   "beginning of session" from "beginning of what was kept" — and the second
//!   one is a loss.
//!
//! The trim runs from the **oldest end and in whole entries** wherever it can, so
//! what remains starts at a boundary the source wrote rather than in the middle
//! of a paragraph. The one case it cannot is a single entry bigger than the whole
//! cap; there it falls back to whole *rows* of that entry, keeping the newest one,
//! because letting one base64 dump hold the store open past the cap is the
//! unbounded-Vec failure the cap exists to close, and the marker reports the
//! boundary either way.
//!
//! # Re-wrapping on resize
//!
//! A resize re-renders the transcript at the new width and *replaces* the rows
//! ([`Scrollback::rewrap`]). What makes that usable instead of unusable is that
//! the view is anchored to **content**: [`ContentAnchor`] addresses the row the
//! view rests on by `(entry, logical line, byte)`, which means the same thing
//! before and after the re-wrap, while a row index means nothing across it.
//! See [`Scrollback::resting_anchor`] for why the anchor is the bottom-most
//! visible row rather than the top-most.
//!
//! What gets re-rendered is not the whole transcript, though. The store is the
//! one that trims, and a rebuild that hands it everything makes it pay for the
//! whole source before it gets to say it can only keep a slice of it — 133–151
//! MiB of transient to retain 32, measured in `spikes/results/resize-transient.log`.
//! So [`SessionView::rewrap`](crate::session::view::SessionView::rewrap) cuts
//! the *source* at the newest slice that fits this store's cap first, and tells
//! this store what it cut so the marker still counts it
//! ([`Scrollback::note_source_dropped`]): content that leaves by never being
//! made again is a trim like any other, and has to be as loud as one.

use std::fmt;

use ratatui::text::Line;

use crate::components::line_render::RenderedRow;
use crate::utils::shelltext;

/// How the text after this row continues.
///
/// The one bit a wrapped display row cannot be without: it is what tells the
/// copy path (ADR-0004 R14) that two soft-wrapped rows are one line and that
/// two hard-ended rows are two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowEnd {
    /// The source had a newline here. Joining to the next row inserts exactly
    /// one `\n`.
    Hard,
    /// A continuation of **our** soft wrap. Joining to the next row inserts
    /// nothing — the space the wrap broke on is already gone, and inventing one
    /// welds two words together.
    Soft,
}

/// How many **bytes** of rendered rows a view keeps by default.
///
/// The default cap on **retained rendered content** for one view: 32 MiB.
///
/// Chosen from the measured working set of real long passes, not picked.
/// [`crate::measure`] replays twelve real beads tickets' worth of `pi` session
/// transcript (answers, thinking, and the tool results which are most of it)
/// through the real flusher at 100 columns with a live-heap-counting allocator;
/// `spikes/results/scrollback-cost.log` is that run. Two numbers decide this
/// one: such a pass renders ~30k rows and 2.6 MiB of visible text, and every
/// one of those rows costs ~5.6 KiB of live heap ([`ROW_STRUCT_BYTES`]) — so
/// unbounded, the store held 166 MiB of work nobody was looking at.
///
/// 32 MiB is about **5.8k retained rows** at the measured shape: roughly 145
/// screens of scrollback at 40 rows a screen, ~0.5 MiB of visible text, about
/// a fifth of a long pass. That is the trade the ticket asks for — enough
/// history that scrolling back reaches the answer you were reading, and a
/// ceiling that can be stated in one number and holds on the worst transcript
/// rather than the average one.
///
/// This is the **per-view** cap: one store, one 32 MiB. The app runs one
/// [`SessionView`](crate::session::view::SessionView) per mode, so the number
/// a reader actually wants — every mode alive, both halves of every view full
/// — is stated once, with the sentence around it, on
/// [`RETAINED_BYTES_WORST_CASE`](crate::session::view::RETAINED_BYTES_WORST_CASE):
/// 3 × 32 MiB of this plus 3 × 256 KiB of transcript text, ≈ 97 MiB.
///
/// `0` means unbounded, by the convention [`Scrollback::set_cap`] keeps.
pub const DEFAULT_RETAINED_BYTES: usize = 32 * 1024 * 1024;

/// What one retained row costs beyond its own text: 5,760 bytes.
///
/// Measured, not guessed, because the gap is not a small one. A row is not its
/// text — the same characters exist as owned span strings, as a vector of
/// styled cells, and as the style runs that map one onto the other — so the
/// live heap behind a rendered row is dominated by that structure and not by
/// the sentence on it. The measured slope across the working set above is
/// **5,642 B of heap per rendered row against an average of 88 B of text per
/// row** (166.6 MiB of store for 30,239 rows).
///
/// Rounded *up* to 5,760 rather than down to the nearest KiB, for the reason
/// the cap exists at all: a charge smaller than the truth is a cap larger than
/// the number on it says, and the whole point of the number is that it holds.
///
/// `DisplayRow::charged` adds this to the row's text length, which is what
/// makes the cap a bound on the heap the store actually holds rather than on
/// the text it happens to contain.
pub const ROW_STRUCT_BYTES: usize = 5_760;

/// What a row costs the cap, given the length of its rendered text.
///
/// One definition rather than two, because there are now two places that have to
/// price a row *before* it exists: the store prices the rows it holds
/// ([`DisplayRow::charged`]), and the rewrap budget prices rows it is about to
/// ask the renderer for ([`crate::session::view::SessionView`]'s source slice).
/// A row priced two ways is a cap that means one thing on the way in and another
/// on the way out — which is the failure this whole module is a reply to.
pub const fn row_charge(text_len: usize) -> usize {
    text_len + ROW_STRUCT_BYTES
}

/// What a row **is**: transcript, or the store's own bookkeeping about the
/// transcript.
///
/// The distinction is not cosmetic. A trim marker is *about* the transcript and
/// is not *of* it, so it must never be selectable (ADR-0004 R16: chrome is not
/// transcript — the status row and the card band are not in the store at all, so
/// they cannot be hit; this is the one piece of chrome that has to live in the
/// store to scroll with the content, and so has to say what it is), and it must
/// never be renumbered by an entry eviction, because it belongs to no entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A rendered line of the transcript.
    Transcript,
    /// The `scrollback trimmed: …` row, kept at the head of what was kept.
    TrimMarker,
}

/// One row of the transcript as it is displayed.
#[derive(Clone, Debug)]
pub struct DisplayRow {
    /// Transcript, or the store's own `scrollback trimmed: …` row. See
    /// [`RowKind`] for why the type has to know.
    pub kind: RowKind,
    /// Index of the transcript entry this row was rendered from.
    ///
    /// Meaningful against the transcript the row was rendered *from*, and
    /// re-based when that transcript is compacted
    /// ([`Scrollback::entries_evicted`]) — which is the one way it can go
    /// stale, and the one way it is kept honest. `usize::MAX` for a trim marker,
    /// which belongs to no entry; nothing should read it except
    /// [`Self::is_trim_marker`], which reads the kind instead.
    pub entry: usize,
    /// Which logical line within that entry (0-based).
    ///
    /// A logical line is a run of rows ending in [`RowEnd::Hard`]: the unit the
    /// source wrote, as opposed to the rows our wrap cut it into.
    pub logical: usize,
    /// Byte offset, within that logical line's rendered text, of this row's
    /// first character. `0` for the first row of a logical line.
    pub start: usize,
    /// Hard newline vs. our soft wrap.
    ///
    /// The flag ADR-0004 R14 is stated in: joining two soft rows must insert
    /// nothing and two logical lines exactly one `\n`.
    ///
    /// Read where the rule is *applied* — [`paste_slices`] below, on the way to
    /// the clipboard — and nowhere in the draw path, which never joins rows.
    pub end: RowEnd,
    /// The rendered line, styles included.
    pub line: Line<'static>,
    /// Where this row's clusters land in cells: what the drag hit-test reads on
    /// every motion (looprs-pdl.9), and so the reason the row carries a map at
    /// all. See [`CellMap`].
    pub cells: CellMap,
}

impl DisplayRow {
    fn new(entry: usize, logical: usize, start: usize, end: RowEnd, line: Line<'static>) -> Self {
        let cells = CellMap::of(&plain(&line));
        Self {
            kind: RowKind::Transcript,
            entry,
            logical,
            start,
            end,
            line,
            cells,
        }
    }

    /// The head-of-store marker: `scrollback trimmed: N earlier lines dropped`,
    /// with the journal path appended when the app has one to give.
    ///
    /// Built here rather than passed in as a finished `Line` so the wording, the
    /// thousands separator and the style have exactly one definition — the same
    /// reason `DumpOutcome::toast` builds its own string.
    pub fn trim_marker(dropped_lines: usize, hint: Option<&str>) -> Self {
        let line = Line::styled(
            marker_text(dropped_lines, hint),
            crate::theme::styles::trim_marker_style(),
        );
        let cells = CellMap::of(&plain(&line));
        Self {
            kind: RowKind::TrimMarker,
            // Not from an entry, and never renumbered as if it were. The store
            // keeps the marker out of every entry-addressed path: `span_on`,
            // `hit`, `resting_anchor` and the eviction renumbering all test the
            // kind first.
            entry: usize::MAX,
            logical: 0,
            start: 0,
            end: RowEnd::Hard,
            line,
            cells,
        }
    }

    /// Is this the trim marker rather than content?
    ///
    /// The one test every "is this selectable / renumberable / counted" question
    /// asks, so the answer is one function rather than four opinions.
    pub fn is_trim_marker(&self) -> bool {
        self.kind == RowKind::TrimMarker
    }

    /// What this row costs the cap: its own text plus the per-row structure.
    ///
    /// The text length is read off the cell map rather than re-plain'd, because
    /// the map *partitioned* exactly that text — see [`CellMap::text_len`]. A
    /// marker returns 0: the cap bounds *content*, and the marker is what the
    /// store says about the content it dropped, so it must not be able to push
    /// content out of the store.
    fn charged(&self) -> usize {
        if self.is_trim_marker() {
            0
        } else {
            row_charge(self.cells.text_len())
        }
    }

    /// This row as content-addressed scroll state.
    pub fn anchor(&self) -> ContentAnchor {
        ContentAnchor {
            entry: self.entry,
            logical: self.logical,
            byte: self.start,
        }
    }
}

/// `Display` is the row's plain text — the string ratatui would have printed,
/// minus the styling. Callers that want the copy value use [`paste_text`], which
/// is the rule; this is the plumbing that several tests read the row through.
impl fmt::Display for DisplayRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for span in &self.line.spans {
            f.write_str(&span.content)?;
        }
        Ok(())
    }
}

/// The value of a run of rows when it is pasted (ADR-0004 R14, R15).
///
/// Soft-wrapped rows join with **nothing**; a hard end contributes **exactly one
/// `\n`**; the trailing newline is dropped, because a paste that ends in a blank
/// line is a paste that pressed Enter for you.
///
/// The wrap never reaches the paste. Neither do styles: they live on the row's
/// `Line` and are simply not read here — the "the copy path drops them on the
/// way out" half of the ADR-0005 decision — and ADR-0005 rule 6 is what makes
/// "no control bytes can reach the paste" true rather than hoped for.
/// A part of one display row: the bytes the selection actually covers.
///
/// The row is the unit the store keeps and the *slice* is the unit the user
/// selects — a drag starts and ends mid-row more often than not. Putting the
/// pair in its own type is what lets [`paste_slices`] be the one join rule for
/// both "the whole row" and "twelve characters of it", instead of two
/// functions that each remember the hard/soft rule and can each forget it
/// differently (looprs-pdl.9, ADR-0004 R14).
#[derive(Clone, Copy, Debug)]
pub struct RowSlice<'a> {
    /// The row this part of came from — styles and provenance included.
    pub row: &'a DisplayRow,
    /// First byte of the slice, relative to the row's own text.
    pub from: usize,
    /// Byte just past the slice.
    pub to: usize,
}

impl<'a> RowSlice<'a> {
    /// The whole row as a slice — what `paste_text` turns each row into.
    pub fn whole(row: &'a DisplayRow) -> Self {
        Self {
            row,
            from: 0,
            to: row.cells.text_len(),
        }
    }

    /// The slice's text, styling dropped.
    pub fn text(&self) -> String {
        let full = crate::utils::render::plain(&self.row.line);
        full[self.from.min(full.len())..self.to.min(full.len())].to_string()
    }
}

/// The value of a run of rows when it is pasted (ADR-0004 R14, R15).
///
/// Soft-wrapped rows join with **nothing**; a hard end contributes **exactly one
/// `\n`**; the trailing newline is dropped, because a paste that ends in a blank
/// line is a paste that pressed Enter for you.
///
/// The wrap never reaches the paste. Neither do styles: they live on the row's
/// `Line` and are simply not read here — the "the copy path drops them on the
/// way out" half of the ADR-0005 decision — and ADR-0005 rule 6 is what makes
/// "no control bytes can reach the paste" true rather than hoped for.
///
/// This is [`paste_slices`] over whole rows; the rule lives in one place so that
/// "what you copy" has exactly one definition in the tree, as `StyledLine::copy_text`
/// does.
#[allow(dead_code)] // consumer: select-to-copy (looprs-pdl.10); `paste_slices` is the live half today, this is its whole-row wrapper
pub fn paste_text<'a>(rows: impl IntoIterator<Item = &'a DisplayRow>) -> String {
    paste_slices(rows.into_iter().map(RowSlice::whole))
}

/// [`paste_text`], one step finer: the join rule applied to *parts* of rows.
///
/// Same rule, same trailing-newline trim. The selection path (looprs-pdl.9)
/// produces slices because a drag's endpoints are mid-row; only the rows in
/// between are whole.
///
/// The order of `slices` *is* the paste order, so the caller owns the
/// reading-order guarantee — see [`crate::state::selection::Selection::resolve`],
/// which is what produces them.
pub fn paste_slices<'a>(slices: impl IntoIterator<Item = RowSlice<'a>>) -> String {
    let mut out = String::new();
    for s in slices {
        // Chrome never pastes. A selection can never *contain* the marker (it is
        // not hittable, not selectable), so this only ever fires for a caller
        // that hands the whole store over — and pasting the store's own
        // bookkeeping into the user's clipboard is not what they meant.
        if s.row.is_trim_marker() {
            continue;
        }
        out.push_str(&s.text());
        if s.row.end == RowEnd::Hard {
            out.push('\n');
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The content the view rests on, addressed so that it survives a re-wrap.
///
/// A row *index* cannot be the anchor: the whole point of re-wrapping is that
/// the row a piece of content lands on changes. `(entry, logical line, byte)`
/// says "this character", which says the same thing at every width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentAnchor {
    pub entry: usize,
    pub logical: usize,
    pub byte: usize,
}

/// One cluster: its cell span and its byte span in the row's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSpan {
    pub cell: usize,
    pub cells: usize,
    pub start: usize,
    pub end: usize,
}

/// Where every cluster of a display row sits, in cells.
///
/// Built from the row's own text with [`shelltext::clusters`] — the resolver's
/// cluster rule, deliberately reused rather than re-implemented, because two
/// rules is how a selection ends up cutting a glyph in half: the wrap lays the
/// row out with one idea of what a character is and the hit-test uses another.
///
/// The map is total over the row's text (its clusters partition it), and every
/// lookup answers with a *whole* cluster: cell 1 of a double-width glyph answers
/// with that glyph, not with its right-hand half.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellMap {
    spans: Vec<CellSpan>,
    cells: usize,
}

impl CellMap {
    /// The cell map of one row's text.
    pub fn of(text: &str) -> Self {
        let mut spans = Vec::new();
        let mut cell = 0usize;
        for c in shelltext::clusters(text) {
            spans.push(CellSpan {
                cell,
                cells: c.cells,
                start: c.start,
                end: c.end,
            });
            cell += c.cells;
        }
        Self { spans, cells: cell }
    }

    /// How many cells the row occupies.
    ///
    /// This is the "nothing to hit" test as well as the drag's right edge: a row
    /// with no cells has nothing a pointer can land on
    /// ([`hit`](crate::state::selection::hit) clamps to it and gives up on 0).
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// How many bytes the row's text is — the length this map was built from.
    ///
    /// Not `plain(&row.line).len()` re-computed: the map *partitioned* that text,
    /// so its last cluster's end **is** that length, and reading it here is what
    /// keeps the row's two byte-based facts (its own length and the clusters
    /// inside it) from being two different measurements of the same string.
    pub fn text_len(&self) -> usize {
        self.spans.last().map(|s| s.end).unwrap_or(0)
    }

    /// The cluster that owns `cell`, or `None` past the end of the row.
    ///
    /// The one place "which character is under this cell" is answered; every
    /// other cell question on this type is a projection of it.
    pub fn at(&self, cell: usize) -> Option<&CellSpan> {
        // Clusters are contiguous in cells, so the answer is the last cluster
        // starting at or before `cell`, and it owns `cell` by construction.
        let i = self.spans.partition_point(|s| s.cell <= cell);
        let s = i.checked_sub(1).map(|i| &self.spans[i])?;
        (cell < s.cell + s.cells).then_some(s)
    }

    /// Byte range of the whole cluster under `cell` — [`Self::at`] with the
    /// span's identity dropped, which is exactly what a selection needs: a click
    /// selects a whole character, never half a wide glyph.
    ///
    /// [`hit`](crate::state::selection::hit) is the caller; it wants the bytes,
    /// not the `CellSpan`.
    pub fn bytes_at(&self, cell: usize) -> Option<(usize, usize)> {
        self.at(cell).map(|s| (s.start, s.end))
    }

    /// The cell the given byte offset is drawn in.
    ///
    /// The inverse of [`Self::bytes_at`]: the store needs both directions to keep
    /// a re-wrap honest — content addressing walks byte→row, and the highlight a
    /// selection paints walks byte→cell.
    pub fn cell_of_byte(&self, byte: usize) -> Option<usize> {
        let i = self.spans.partition_point(|s| s.start <= byte);
        let s = i.checked_sub(1).map(|i| &self.spans[i])?;
        (byte < s.end).then_some(s.cell)
    }
}

/// One entry's share of the store, as the last render paid for it.
///
/// The three numbers a re-render has to be priced from, measured rather than
/// modelled: how many rows the entry made, how many of the store's
/// text-bytes they carry, and how many *logical* lines sit inside them — the
/// last because a narrower band adds at most one extra row per logical line to
/// a row count that already exists, and nothing else here says how many lines
/// that is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryShape {
    /// The transcript entry these rows came from.
    pub entry: usize,
    /// Display rows rendered from it, separators included.
    pub rows: usize,
    /// **Non-empty** logical lines within those rows: one soft-wrapped paragraph
    /// is one, and a blank separator row is none.
    ///
    /// This is the count a narrower band adds rows *to* — one per line, worst
    /// case — so blank rows are deliberately left out of it. Charging a blank
    /// row for the rounding a line it is not on might introduce is how an
    /// estimate ends up costing twice what it renders.
    pub lines: usize,
    /// Rows that carry no text at all: the blank the renderer puts after each
    /// entry, and nothing else.
    ///
    /// Kept apart from `rows` because a blank row costs the same one row at any
    /// width: it must not be scaled by a width ratio that describes how *text*
    /// re-flows.
    pub blank: usize,
    /// Rendered text bytes, styling dropped.
    pub text: usize,
}

/// The store: rendered rows plus the scroll state over them.
#[derive(Debug)]
pub struct Scrollback {
    rows: Vec<DisplayRow>,
    /// The width these rows were wrapped at.
    width: u16,
    /// Rows hanging between the bottom of the view and the tail. `0` is pinned.
    offset: usize,
    /// `offset == 0`, cached. Written only by [`Self::set_offset`], so the two
    /// cannot disagree.
    pinned: bool,
    /// Rows that arrived while unpinned.
    pending: usize,
    /// The cap, in retained bytes. `0` means unbounded — the same convention the
    /// transcript buffer cap in [`crate::session::view`] uses.
    max_bytes: usize,
    /// What the rows in `rows` currently cost, in the cap's unit
    /// ([`DisplayRow::charged`]). Kept as a running sum rather than recomputed
    /// per push because the cap is checked per push and the store can be long;
    /// [`Self::recount`] is the one place it is rebuilt from scratch, and it runs
    /// wherever `rows` is replaced wholesale.
    retained: usize,
    /// The head of a line this store dropped but did not finish dropping.
    ///
    /// A row-level trim (the one-entry-bigger-than-the-cap case) cuts a logical
    /// line in half: its head goes, its tail stays on the kept side. That loss
    /// is counted when the head goes, and without this the tail would be counted
    /// again on its way out — one line reported twice, which is the kind of
    /// number that makes a reader stop trusting the marker. Cleared whenever the
    /// store's head moves off that line.
    front_line_counted: Option<(usize, usize)>,
    /// Content lines this store has thrown away, so the trim is a counted thing
    /// rather than an invisible one. Lines, not rows or bytes, because it is the
    /// unit the reader of the marker thinks in: the marker answers "how much of
    /// what I was reading is gone", and "lines" is what they were reading.
    ///
    /// Monotonic for the life of the store. A line is counted once by whichever
    /// path dropped it: [`Self::trim`] counts the rows it drops, and
    /// [`Self::entries_evicted`] is told how many lines the caller removed
    /// because the caller knows whether this store ever rendered them.
    dropped: usize,
    /// The second half of the marker: where the dropped content still is. Set by
    /// the app from the journal path; `None` renders a shorter marker rather than
    /// a marker that points at nothing.
    trim_hint: Option<String>,
}

impl Scrollback {
    pub fn new(width: u16) -> Self {
        Self::with_cap(width, DEFAULT_RETAINED_BYTES)
    }

    /// As [`Self::new`], with an explicit retained-bytes cap (`0` = unbounded —
    /// the same convention the byte cap in [`crate::session::view`] uses).
    pub fn with_cap(width: u16, max_bytes: usize) -> Self {
        Self {
            rows: Vec::new(),
            width,
            offset: 0,
            pinned: true,
            pending: 0,
            max_bytes,
            retained: 0,
            dropped: 0,
            front_line_counted: None,
            trim_hint: None,
        }
    }

    /// Change the cap, trimming immediately if the store is already over it.
    ///
    /// A measurement/test seam in the shipped shape: reaching the default cap by
    /// ordinary means means rendering [`DEFAULT_RETAINED_BYTES`] of real content,
    /// and a test that does that is a slow test nobody runs. The app never
    /// retunes a live store, so nothing outside the test and measurement build
    /// calls this.
    #[allow(dead_code)] // consumer: crate::measure and this module's cap tests
    pub fn set_cap(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes;
        self.trim();
    }

    /// What the store currently retains, in the cap's unit.
    #[allow(dead_code)] // consumer: crate::measure and this module's cap tests
    pub fn retained_bytes(&self) -> usize {
        self.retained
    }

    /// The cap itself, in retained bytes (`0` = unbounded).
    ///
    /// Read by the rewrap source budget ([`crate::session::view::SessionView`]),
    /// which has to size the work it asks the renderer for against the same
    /// number the store later trims to. A rebuild bounded by a *different*
    /// number than the one the store enforces is a rebuild that is not bounded.
    pub fn cap_bytes(&self) -> usize {
        self.max_bytes
    }

    /// What this store's rows say each entry costs to render, oldest entry first.
    ///
    /// The rewrap estimator's price list. The alternative — estimating from the
    /// entry's text — is a guess about what a markdown pass will do; these are
    /// the receipts from the last one, at a known width. That is what makes a
    /// "does this fit?" question answerable before paying for the render.
    ///
    /// Entries the store holds no rows for are simply absent, and the caller has
    /// to treat them as unknown rather than free: they left by a trim, which is
    /// why the walk over older entries stops at the first gap.
    pub fn entry_shapes(&self) -> Vec<EntryShape> {
        let mut out: Vec<EntryShape> = Vec::new();
        for r in self.rows.iter().filter(|r| !r.is_trim_marker()) {
            if !out.last().is_some_and(|s| s.entry == r.entry) {
                out.push(EntryShape {
                    entry: r.entry,
                    ..Default::default()
                });
            }
            let len = r.cells.text_len();
            if let Some(s) = out.last_mut() {
                s.rows += 1;
                s.text += len;
                // One row per logical line *starts*: `start == 0` is the row the
                // line begins on, however many soft continuations follow it —
                // and only if that line has something on it.
                if r.start == 0 && len > 0 {
                    s.lines += 1;
                }
                if len == 0 {
                    s.blank += 1;
                }
            }
        }
        out
    }

    /// Count the content a re-render is **skipping at the source** as dropped.
    ///
    /// A trim removes rows that exist; the source cut in
    /// [`crate::session::view::SessionView::rewrap`] removes rows that would
    /// otherwise have been made again, and never are. Nothing in the store ever
    /// sees that loss on its way past `trim`, so it would leave the store with
    /// less history and a marker that says nothing — the silent half-start that
    /// the marker row exists to prevent.
    ///
    /// `from_entry` is the oldest entry the rebuild covers; `lines` is what the
    /// caller counted off the transcript for the entries below it. The store's
    /// own rows are the floor of that figure, in the same `max` shape
    /// [`Self::entries_evicted`] uses: the caller knows how much content left
    /// even where this store never rendered it, and this store knows how much it
    /// actually had. Neither may under-report.
    ///
    /// Call **before** the swap that discards the rows: it is the last moment
    /// they are still here to be counted.
    pub fn note_source_dropped(&mut self, from_entry: usize, lines: usize) {
        if from_entry == 0 {
            return;
        }
        let m = usize::from(self.rows.first().is_some_and(|r| r.is_trim_marker()));
        let cut = m + self.rows[m..]
            .iter()
            .take_while(|r| r.entry < from_entry)
            .count();
        let here = self.count_dropped(m, cut);
        self.dropped += lines.max(here);
        // The rows themselves go with the swap, so nothing is drained here —
        // but the marker is refreshed now rather than left to the rebuild's own
        // `trim`, which will not run at all when the cut kept the store under
        // its cap.
        self.sync_marker();
    }

    /// Say where the trimmed-away content can still be read. Appears on the
    /// marker row; the app calls this once it knows the journal's path.
    pub fn set_trim_hint(&mut self, hint: Option<String>) {
        self.trim_hint = hint;
        self.sync_marker();
    }

    /// The width the current rows were wrapped at. Differing from the window's
    /// is the signal that a [`Self::rewrap`] is owed.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Every row in the store, oldest first — the slice a selection is resolved
    /// against (looprs-pdl.9), because the extent of a selection is a fact
    /// about the content and not about the visible window.
    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
    }

    #[allow(dead_code)] // companion to `is_empty`; see `rows`
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code)] // companion to `len`; a view with no rows cannot be scrolled, only followed
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Is the view following the tail?
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Rows between the bottom of the view and the tail — the number a wheel
    /// step is a delta against (looprs-pdl.8) and the thing the run loop
    /// compares before and after a scroll to decide whether anything moved.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Rows that arrived since the view last looked at the tail.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Is there new content the user is not looking at? The "N new" affordance
    /// reads exactly this.
    pub fn shows_new(&self) -> bool {
        !self.pinned && self.pending > 0
    }

    /// Rows the trim has dropped, counted as the content lines they were.
    ///
    /// The marker's number, and the answer to "how much did I lose". Zero for a
    /// store that has never trimmed, which is the same thing as "the head of this
    /// scrollback is the head of the session".
    #[allow(dead_code)] // consumer: crate::measure, the marker's tests, and the app's marker hint
    pub fn dropped_lines(&self) -> usize {
        self.dropped
    }

    /// Append rows that just became final.
    ///
    /// Pinned: the view follows, because the tail moved and `offset` did not.
    /// Unpinned: the view holds, for the same reason — `offset` is measured
    /// from the tail, so holding it holds the *content* — and the rows are
    /// counted, so the affordance can tell the user a held view is not up to
    /// date.
    pub fn push(&mut self, rows: Vec<RenderedRow>) {
        if rows.is_empty() {
            return;
        }
        if !self.pinned {
            self.pending += rows.len();
            // The new rows hang *below* the view, so holding the view means the
            // offset grows with them. Hold the offset instead and the view rides
            // the tail, which is exactly the thing the user said they did not
            // want when they scrolled up.
            self.offset += rows.len();
        }
        let made = rows_from_rendered(rows);
        self.retained += made.iter().map(DisplayRow::charged).sum::<usize>();
        self.rows.extend(made);
        self.trim();
    }

    /// Replace every row with the same content re-wrapped at `width`.
    ///
    /// The view stays where the *content* was, not where the row index was:
    /// [`Self::resting_anchor`] is taken before the swap and put back after it.
    /// A pinned view has nothing to anchor — it is pinned to the tail, and the
    /// tail is wherever the content ends.
    pub fn rewrap(&mut self, width: u16, rendered: Vec<RenderedRow>) {
        let anchor = self.resting_anchor();
        self.width = width;
        self.rows = rows_from_rendered(rendered);
        self.recount();
        // The rows were replaced wholesale, which threw the marker out with the
        // old ones; `trim` puts it back along with whatever the new wrap costs
        // over the cap. Doing it here rather than in the caller is what keeps
        // "a trimmed store has a marker" true across a resize rather than true
        // until the window moves.
        self.trim();
        if self.pinned {
            self.offset = 0;
            return;
        }
        match anchor.and_then(|a| self.index_of(a)) {
            Some(i) => self.set_offset(self.rows.len().saturating_sub(i + 1)),
            // The anchored content is gone — its entry was evicted while the
            // user was looking away. There is no honest way to "keep" a position
            // whose content left, so hold as close as the new list allows rather
            // than teleport to the tail, which is the one thing not asked for.
            None => self.set_offset(self.offset),
        }
    }

    /// The content the view is resting on: the **bottom-most visible row**.
    ///
    /// Bottom-most rather than top-most because it is the anchor that needs
    /// nothing else remembered. `offset` counts rows hanging *below* the view,
    /// so the row the view ends on is `len - offset - 1`; anchoring the top row
    /// would need the band height, which lives in the frame and not here.
    ///
    /// It is also the anchor that behaves while a window is being dragged: the
    /// rows a narrowing re-wrap inserts appear *above* the anchor, so the
    /// content slides up out of the top of the band exactly as it would have
    /// anyway, and nothing moves out from under the bottom edge — the edge the
    /// "N new" affordance and the chrome are measured from.
    pub fn resting_anchor(&self) -> Option<ContentAnchor> {
        if self.pinned || self.rows.is_empty() {
            return None;
        }
        let end = self.rows.len().saturating_sub(self.offset);
        let idx = end.saturating_sub(1).min(self.rows.len() - 1);
        // A view resting on the trim marker has no content under it to rest on:
        // the marker is chrome, and chrome cannot be an anchor any more than the
        // status row can. `rewrap` takes the "content is gone" branch, which
        // holds the offset rather than inventing a position for it.
        if self.rows[idx].is_trim_marker() {
            return None;
        }
        Some(self.rows[idx].anchor())
    }

    /// The row index a content anchor points at now, at this width.
    ///
    /// Also the way the drag hit-test turns a *screen* row back into a store row:
    /// the frame publishes the content address of the first row it drew
    /// ([`crate::state::selection::BandSnapshot`]), and counting down from
    /// *that* index keeps the mapping on content rather than on a row number that
    /// a trim or a re-wrap has already invalidated.
    pub fn index_of(&self, a: ContentAnchor) -> Option<usize> {
        // The last row of that logical line that starts at or before the
        // anchored byte: that is the row the anchored character is drawn on.
        let mut first = None;
        let mut last = None;
        for (i, r) in self.rows.iter().enumerate() {
            if r.entry != a.entry || r.logical != a.logical {
                continue;
            }
            if first.is_none() {
                first = Some(i);
            }
            if r.start <= a.byte {
                last = Some(i);
            }
        }
        last.or(first)
    }

    /// Move the view by `rows`: **positive toward the tail** (down), **negative
    /// into history** (up).
    ///
    /// The sign is the direction the *view* travels, not the direction `offset`
    /// moves — a page down is `+page`, a wheel-toward-the-past is negative.
    /// `visible` is the height of the band being scrolled; it sets how far up
    /// there is anything to see.
    pub fn scroll_by(&mut self, rows: isize, visible: usize) {
        let max = self.max_scroll(visible);
        let cur = self.offset.min(max) as isize;
        self.set_offset((cur - rows).clamp(0, max as isize) as usize);
    }

    /// The very bottom. Always legal, always pins.
    pub fn scroll_to_tail(&mut self) {
        self.set_offset(0);
    }

    /// As far up as there is content.
    pub fn scroll_to_top(&mut self, visible: usize) {
        let max = self.max_scroll(visible);
        self.set_offset(max);
    }

    /// The largest offset that still shows something.
    pub fn max_scroll(&self, visible: usize) -> usize {
        self.rows.len().saturating_sub(visible.max(1))
    }

    fn set_offset(&mut self, offset: usize) {
        // Never past the head of the content: an offset of `len` shows nothing at
        // all, and "nothing at all" is not a place a view can be.
        self.offset = offset.min(self.rows.len().saturating_sub(1));
        // One rule for both halves of the interaction: being at the very bottom
        // *is* being pinned, and being off it *is* not. Scrolling up one row
        // unpins, and a keystroke at the bottom re-pins, because both come
        // through here — not because two places each remembered their half.
        self.pinned = self.offset == 0;
        if self.pinned {
            self.pending = 0;
        }
    }

    /// The rows to draw for a band `visible` rows tall.
    ///
    /// The slice is `visible` rows long whenever there are that many above the
    /// offset, and shorter only because the transcript ran out — the frame pads
    /// the rest; it does not scroll past the top of the content.
    pub fn window(&self, visible: usize) -> &[DisplayRow] {
        if visible == 0 || self.rows.is_empty() {
            return &[];
        }
        let end = self.rows.len().saturating_sub(self.offset);
        if end == 0 {
            return &[];
        }
        let start = end.saturating_sub(visible);
        &self.rows[start..end]
    }

    /// The transcript was compacted: `removed` entries came off the front, and
    /// `lines` says how many content lines went with them. Follow the indices.
    ///
    /// A row whose entry is gone cannot be re-rendered, and a store that cannot
    /// re-render a row must not keep showing it: the next resize would drop it
    /// silently, and a scrollback that loses content only when the window moves
    /// is the worst kind of scrollback. So those rows go here, visibly, at the
    /// same moment their entries do — and the rows that survive are renumbered,
    /// so `entry` still means *this* entry and not whatever now occupies the
    /// number it used to have.
    ///
    /// `lines` is passed in rather than read off the rows for one reason: a view
    /// that is not on screen is not being flushed, so its store may never have
    /// held rows for the entries the caller dropped, while the *content* was
    /// still lost just the same. The caller knows how many lines of transcript
    /// went; the store knows whether it ever had them, and reports the loss
    /// either way.
    pub fn entries_evicted(&mut self, removed: usize, lines: usize) {
        if removed == 0 {
            return;
        }
        // Rows are in entry order, so the rows to drop are exactly the leading
        // run whose entries are gone — starting *past* the marker, which belongs
        // to no entry and must not be eaten by an eviction, miscounted as
        // content, or treated as the head of the entry run.
        let start = usize::from(self.rows.first().is_some_and(|r| r.is_trim_marker()));
        let cut = start
            + self.rows[start..]
                .iter()
                .take_while(|r| r.entry < removed)
                .count();
        let dropped_here = self.count_dropped(start, cut);
        let freed: usize = self.rows[..cut].iter().map(DisplayRow::charged).sum();
        self.rows.drain(..cut);
        for r in self.rows.iter_mut() {
            if !r.is_trim_marker() {
                // Every surviving entry shifted down by exactly what came off the
                // front. No notice entry is inserted any more (the marker is the
                // notice, and it is the store's rather than the transcript's),
                // so there is nothing else to adjust for.
                r.entry -= removed;
            }
        }
        self.retained = self.retained.saturating_sub(freed);
        // The caller's count is the one that stands, but never a smaller one
        // than the rows this store actually had: under-reporting a loss is the
        // exact thing the marker exists to prevent.
        self.dropped += lines.max(dropped_here);
        self.sync_marker();
    }

    /// Rebuild [`Self::retained`] from the rows themselves.
    ///
    /// The one place the running sum is made rather than maintained: correct by
    /// construction, and only used where `rows` was replaced wholesale, so the
    /// common path (a push under the cap) never pays the walk.
    fn recount(&mut self) {
        self.retained = self.rows.iter().map(DisplayRow::charged).sum();
    }

    /// Bring the store back under its cap, oldest end first, in whole entries
    /// where that is possible.
    ///
    /// The walk starts at the tail rather than at the head because the question
    /// "how much of the newest content fits?" is answerable in one pass from that
    /// end, and it is the only question worth asking: the rows that fit are kept,
    /// and everything older goes.
    ///
    /// Three cases, and they are worth naming because they are the three ways a
    /// trim can be wrong:
    ///
    /// 1. **the aligned case** — the walk's cut is moved *back* to the start of
    ///    the entry it landed in, so the store keeps that entry whole and the head
    ///    of what remains is a boundary the source wrote, not a paragraph cut in
    ///    half. This costs over the cap by up to one entry, which is what a bound
    ///    on a projection means.
    /// 2. **one entry bigger than the cap** — aligning cannot help, so the cut
    ///    stays where the walk put it and the boundary is mid-entry. That is the
    ///    "where possible" of the ticket, and the marker reports the lines dropped
    ///    all the same; a base64 dump must not be able to hold the store open past
    ///    the cap.
    /// 3. **not even the newest row fits** — the newest row is kept anyway.
    ///    A scrollback that throws away the line that just arrived is worse than
    ///    one that is momentarily over its own bound, and there is nothing else
    ///    on offer that is not a lie.
    fn trim(&mut self) {
        if self.max_bytes == 0 || self.retained <= self.max_bytes {
            // Even with nothing over the cap, the marker has to be re-checked:
            // `rewrap` replaces every row it stands on, and a store that lost
            // content before the resize still lost it after.
            self.sync_marker();
            return;
        }
        // The marker, if present, is at index 0 and is charged nothing: the cap
        // bounds content, and the marker must never be able to push content out
        // by taking room for itself.
        let m = match self.rows.first() {
            Some(r) if r.is_trim_marker() => 1,
            _ => 0,
        };
        // Walk back from the tail, keeping as much as fits.
        let mut kept = 0usize;
        let mut first_fit = self.rows.len();
        let mut i = self.rows.len();
        while i > m {
            let charged = self.rows[i - 1].charged();
            if kept + charged > self.max_bytes {
                break;
            }
            kept += charged;
            first_fit = i - 1;
            i -= 1;
        }
        // Case 3: nothing fits. Keep the newest row and let the marker say the
        // rest went.
        if first_fit == self.rows.len() {
            first_fit = self.rows.len().saturating_sub(1);
        }
        // Case 1: align the cut back to the head of the entry the walk landed in,
        // so a trimmed store starts at a boundary the source wrote.
        let head_entry = self.rows[first_fit].entry;
        let mut cut = first_fit;
        while cut > m
            && !self.rows[cut - 1].is_trim_marker()
            && self.rows[cut - 1].entry == head_entry
        {
            cut -= 1;
        }
        // Case 2: if the aligned cut does not fit, fall back on the walk's own
        // cut — whole rows of that entry, boundary mid-entry, cap intact.
        let aligned_frees: usize = self.rows[m..cut].iter().map(DisplayRow::charged).sum();
        if self.retained - aligned_frees > self.max_bytes {
            cut = first_fit;
        }
        if cut <= m {
            return;
        }
        let lines = self.count_dropped(m, cut);
        self.dropped += lines;
        self.rows.drain(m..cut);
        self.recount();
        self.sync_marker();
    }

    /// How many content lines removing `rows[from..to)` costs, adjusted for the
    /// one line that may already have been reported.
    ///
    /// The bookkeeping runs both ways, which is why it is one function rather
    /// than a subtraction at each call site: this pass subtracts the count it
    /// would be double-charging for a line whose head went earlier, and records
    /// whether *this* cut is the one that leaves a line split so the next pass
    /// can do the same. Only one line can straddle a boundary, so one `Option`
    /// is the whole ledger.
    fn count_dropped(&mut self, from: usize, to: usize) -> usize {
        let dropped = &self.rows[from..to];
        let last = dropped.last().map(|r| (r.entry, r.logical));
        let mut lines = count_lines(dropped);
        if last.is_some() && self.front_line_counted == last {
            // This line's head was already reported; the rest of it going is not
            // a second line lost.
            lines = lines.saturating_sub(1);
        }
        self.front_line_counted = match (dropped.last(), self.rows.get(to)) {
            (Some(d), Some(kept)) if (d.entry, d.logical) == (kept.entry, kept.logical) => {
                Some((kept.entry, kept.logical))
            }
            _ => None,
        };
        lines
    }

    /// Keep the marker honest: present exactly when something has been dropped,
    /// at index 0, worded for the current count and the current hint.
    ///
    /// The one writer of the marker row, so "a trimmed store shows the marker"
    /// has a single owner that every trim path goes through — `trim`,
    /// `entries_evicted` and `set_trim_hint` all call it, and none of them
    /// words the row themselves.
    fn sync_marker(&mut self) {
        if self.dropped == 0 {
            if self.rows.first().is_some_and(|r| r.is_trim_marker()) {
                self.rows.remove(0);
            }
            return;
        }
        let text = marker_text(self.dropped, self.trim_hint.as_deref());
        if let Some(first) = self.rows.first()
            && first.is_trim_marker()
            && first.to_string() == text
        {
            // Same sentence it already says: rebuilding it would allocate a
            // `Line`, a `CellMap` and two `String`s per frame to change nothing.
            return;
        }
        let marker = DisplayRow::trim_marker(self.dropped, self.trim_hint.as_deref());
        if self.rows.first().is_some_and(|r| r.is_trim_marker()) {
            self.rows[0] = marker;
        } else {
            // Inserting at the head moves no view: `offset` is measured from the
            // tail, so the content on screen does not move and the user simply
            // finds one more row above when they get there — which is the point.
            self.rows.insert(0, marker);
        }
    }
}

/// The marker's sentence, in one place so the row and the tests that read it
/// cannot drift apart on capitalisation, the separator, or the thousands.
///
/// `⌄` opens the line because the row is about what is *above* it, and the row
/// the user is standing on when they read it is the top of what was kept.
pub fn marker_text(dropped_lines: usize, hint: Option<&str>) -> String {
    let mut s = format!(
        "\u{2304} scrollback trimmed: {} earlier lines dropped",
        crate::services::clipboard::thousands(dropped_lines)
    );
    if let Some(h) = hint {
        s.push_str(" \u{00b7} full transcript: ");
        s.push_str(h);
    }
    s
}

/// How many content lines a run of rows covers.
///
/// Distinct `(entry, logical)` pairs, which is what "a line of content" means in
/// a store whose rows are *our* wrap of *their* lines: three soft-wrapped rows
/// of one paragraph are one line dropped, not three. Counting rows instead would
/// over-report the loss by the wrap ratio, which is the marker lying in the
/// alarming direction.
fn count_lines(rows: &[DisplayRow]) -> usize {
    let mut n = 0usize;
    let mut last: Option<(usize, usize)> = None;
    for r in rows {
        if r.is_trim_marker() {
            continue;
        }
        let key = (r.entry, r.logical);
        if last != Some(key) {
            n += 1;
            last = Some(key);
        }
    }
    n
}

/// Turn the flusher's lines into display rows.
///
/// `logical` and `start` fall straight out of the hard/soft chain the renderer
/// already set: a row that is *not* a soft continuation starts a logical line,
/// so counting hard ends across an entry counts its logical lines, and the bytes
/// in between are the offsets inside each one. Nothing extra is asked of the
/// renderer, which is the point — a second bookkeeping of the wrap would be a
/// second opinion about the wrap.
pub fn rows_from_rendered(rendered: Vec<RenderedRow>) -> Vec<DisplayRow> {
    let mut out = Vec::with_capacity(rendered.len());
    let mut entry = usize::MAX;
    let mut logical = 0usize;
    let mut start = 0usize;
    for r in rendered {
        if r.entry != entry {
            entry = r.entry;
            logical = 0;
            start = 0;
        }
        // The row's own text, measured once: the cell map is built from it and
        // the running byte offset has to agree with what the cell map saw.
        let len = plain(&r.line).len();
        let row = DisplayRow::new(entry, logical, start, r.end, r.line);
        match r.end {
            RowEnd::Hard => {
                logical += 1;
                start = 0;
            }
            RowEnd::Soft => start += len,
        }
        out.push(row);
    }
    out
}

/// The row's text, styling dropped — see [`crate::utils::render::plain`], which
/// is the one definition both this type and `RenderedRow` print through.
fn plain(line: &Line<'_>) -> String {
    crate::utils::render::plain(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn row(entry: usize, end: RowEnd, text: &str) -> RenderedRow {
        RenderedRow {
            entry,
            end,
            line: Line::from(text.to_string()),
        }
    }

    fn hard(entry: usize, text: &str) -> RenderedRow {
        row(entry, RowEnd::Hard, text)
    }

    fn soft(entry: usize, text: &str) -> RenderedRow {
        row(entry, RowEnd::Soft, text)
    }

    fn lines(rows: &[DisplayRow]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    /// The default state, and the one that must not be surprising: you open on
    /// the tail and you stay there while the session talks.
    #[test]
    fn pinned_is_the_default_and_new_output_follows() {
        let mut s = Scrollback::new(40);
        assert!(s.is_pinned(), "the app opens pinned to the tail");
        s.push(vec![hard(0, "one"), hard(0, "two")]);
        assert!(s.is_pinned(), "and stays pinned while output appends");
        assert_eq!(s.pending(), 0, "nothing is pending while pinned");
        assert_eq!(lines(s.window(2)), vec!["one", "two"]);

        s.push(vec![hard(1, "three")]);
        assert_eq!(
            lines(s.window(2)),
            vec!["two", "three"],
            "and followed again"
        );
    }

    /// The user said "I am reading something else" by moving the view by a
    /// single row. That is the whole interaction, so it is a test.
    #[test]
    fn scrolling_up_one_row_unpins() {
        let mut s = Scrollback::new(40);
        s.push((0..10).map(|i| hard(0, &format!("line {i}"))).collect());
        assert!(s.is_pinned());
        s.scroll_by(-1, 4);
        assert!(!s.is_pinned(), "one row up is enough to unpin");
        assert_eq!(s.offset(), 1);
    }

    /// The property the unpinned half exists for: the view holds its *content*
    /// while the tail runs away from it, and says how far away.
    #[test]
    fn unpinned_holds_its_content_and_counts_what_arrived() {
        let mut s = Scrollback::new(40);
        s.push((0..6).map(|i| hard(0, &format!("old {i}"))).collect());
        s.scroll_by(-3, 2);
        let before = lines(s.window(2));
        assert_eq!(before, vec!["old 1", "old 2"]);

        s.push(vec![hard(1, "new 1"), hard(1, "new 2")]);
        assert_eq!(
            lines(s.window(2)),
            before,
            "the view held exactly where it was looking"
        );
        assert_eq!(s.pending(), 2, "and knows how much went by");
        assert!(s.shows_new(), "and is ready to say so");
    }

    /// Both halves of the tail rule, in one place: reaching the bottom re-pins,
    /// and re-pinning clears the count, because there is nothing left unseen.
    #[test]
    fn reaching_the_very_bottom_re_pins_and_clears_the_count() {
        let mut s = Scrollback::new(40);
        s.push((0..8).map(|i| hard(0, &format!("a{i}"))).collect());
        s.scroll_by(-4, 2);
        assert_eq!(lines(s.window(2)), ["a2", "a3"]);

        s.push(vec![hard(1, "b")]);
        assert_eq!(
            lines(s.window(2)),
            ["a2", "a3"],
            "a row arriving at the tail does not move what the user is reading"
        );
        assert_eq!(s.pending(), 1);

        s.scroll_by(1, 2);
        assert_eq!(lines(s.window(2)), ["a3", "a4"], "one row toward the tail");
        assert_eq!(
            s.pending(),
            1,
            "still off the tail: still holding the count"
        );

        s.scroll_to_tail();
        assert!(s.is_pinned(), "the tail re-pins");
        assert_eq!(s.pending(), 0, "and the count is answered");
        assert_eq!(lines(s.window(2)), ["a7", "b"]);
    }

    /// You cannot scroll into blank space above the transcript: the top of the
    /// content is the top of the range.
    #[test]
    fn cannot_scroll_past_the_top_of_the_content() {
        let mut s = Scrollback::new(40);
        s.push((0..10).map(|i| hard(0, &format!("l{i}"))).collect());
        s.scroll_by(-1000, 4);
        assert_eq!(s.offset(), 6, "10 rows, 4 visible, 6 rows of scrollback");
        assert_eq!(lines(s.window(4)), ["l0", "l1", "l2", "l3"]);

        // A transcript shorter than the band cannot be scrolled at all.
        let mut small = Scrollback::new(40);
        small.push(vec![hard(0, "only")]);
        small.scroll_by(-5, 10);
        assert_eq!(small.offset(), 0, "nothing above to scroll to");
        assert!(small.is_pinned(), "and so it never left the tail");
    }

    /// Provenance per row: entry, logical line within the entry, and the byte
    /// offset inside that logical line. Three later tickets ask all three
    /// questions, and none of them can reconstruct the answers afterwards.
    #[test]
    fn every_row_carries_its_provenance() {
        let mut s = Scrollback::new(40);
        s.push(vec![
            soft(0, "first logical,"),
            hard(0, "wrapped twice"),
            hard(0, "second line"),
            hard(1, "another entry"),
        ]);
        let r = s.rows();
        assert_eq!(
            r.iter()
                .map(|r| (r.entry, r.logical, r.start))
                .collect::<Vec<_>>(),
            vec![(0, 0, 0), (0, 0, 14), (0, 1, 0), (1, 0, 0)],
            "the soft continuation is the second half of logical 0; the next hard \
             row is a new logical line; the entry boundary is honoured"
        );
    }

    /// ADR-0004 R14, spelled as the trap it exists to close.
    #[test]
    fn joining_rows_follows_the_hard_soft_rule() {
        let rows = rows_from_rendered(vec![
            soft(0, "The quick brown fox "),
            hard(0, "jumps over"),
            hard(0, "the lazy dog"),
        ]);
        assert_eq!(
            paste_text(&rows),
            "The quick brown fox jumps over\nthe lazy dog",
            "soft rows join with nothing, a hard end adds exactly one \\n, and the \
             trailing newline is not pasted"
        );
        // The wrong join, which is what "glue the display rows together"
        // produces, is not what this returns.
        let welded: String = rows.iter().map(|r| r.to_string()).collect();
        assert_eq!(welded, "The quick brown fox jumps overthe lazy dog");
    }

    /// The cell-to-character mapping: a cell is never the half of something.
    #[test]
    fn the_cell_map_never_splits_a_cluster() {
        // 日本語: three clusters, two cells each.
        let m = CellMap::of("日本語");
        assert_eq!(m.cells(), 6);
        // Cell 1 is the *right half* of 日. There is no such thing to select.
        assert_eq!(m.bytes_at(1), Some((0, 3)));
        assert_eq!(m.bytes_at(0), Some((0, 3)));
        assert_eq!(m.bytes_at(2), Some((3, 6)));
        assert_eq!(m.bytes_at(6), None, "past the end of the row");

        // A ZWJ sequence is ONE cluster: 5 chars, 2 cells, 18 bytes.
        let fam = CellMap::of("\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466} tail");
        assert_eq!(fam.cells(), 7, "the family is 2 cells, ' tail' is 5");
        assert_eq!(
            fam.bytes_at(1),
            Some((0, 18)),
            "the second cell of the family is still the whole family"
        );
        assert_eq!(fam.bytes_at(2), Some((18, 19)));
    }

    /// A combining mark is part of the character it follows, not its own cell.
    #[test]
    fn a_combining_mark_belongs_to_its_base() {
        let m = CellMap::of("e\u{301}x");
        assert_eq!(m.cells(), 2);
        assert_eq!(m.bytes_at(0), Some((0, 3)));
        assert_eq!(m.bytes_at(1), Some((3, 4)));
        assert_eq!(m.cell_of_byte(2), Some(0), "the mark lives in cell 0");
        assert_eq!(m.cell_of_byte(3), Some(1));
    }

    #[test]
    fn a_row_of_nothing_has_no_cells() {
        let m = CellMap::of("");
        assert_eq!(
            m.cells(),
            0,
            "nothing to hit, and the drag's right edge is 0"
        );
        assert_eq!(m.at(0), None);
    }

    /// Styles ride on the row and never reach the paste.
    #[test]
    fn styles_render_and_are_never_copied() {
        let rows = rows_from_rendered(vec![RenderedRow {
            entry: 0,
            end: RowEnd::Hard,
            line: Line::from(vec![
                Span::styled("red".to_string(), Style::new().fg(Color::Red)),
                Span::raw(" plain".to_string()),
            ]),
        }]);
        assert_eq!(rows[0].line.spans.len(), 2, "the style is kept on the row");
        assert_eq!(paste_text(&rows), "red plain", "and dropped at the door");
        assert_eq!(rows[0].cells.cells(), 9, "cells count text, not styles");
    }

    // ───────────────────────────── re-wrap ─────────────────────────────

    /// A pinned view has nothing to anchor to: it follows the tail through a
    /// resize as it follows it through an append.
    #[test]
    fn rewrap_keeps_a_pinned_view_pinned() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "a"), hard(0, "b"), hard(0, "c")]);
        s.rewrap(
            20,
            vec![hard(0, "a1"), hard(0, "a2"), hard(0, "b"), hard(0, "c")],
        );
        assert!(s.is_pinned(), "a resize did not unpin a following view");
        assert_eq!(s.offset(), 0);
        assert_eq!(s.width(), 20);
        assert_eq!(s.window(1)[0].to_string(), "c");
    }

    /// **The** resize property: the view follows the content, not the row index.
    #[test]
    fn rewrap_anchors_on_content_not_on_row_index() {
        let mut s = Scrollback::new(20);
        s.push(vec![
            soft(0, "narrow top"),
            hard(0, "wrapped part"),
            hard(0, "ANCHOR"),
            hard(0, "below it"),
        ]);
        // A one-row band at offset 1 has exactly one row hanging below it, so it
        // shows ANCHOR and "below it" is off the bottom.
        s.scroll_by(-1, 1);
        assert_eq!(s.window(1)[0].to_string(), "ANCHOR");
        let anchor = s
            .resting_anchor()
            .expect("an unpinned view has a resting anchor");
        assert_eq!(
            anchor,
            ContentAnchor {
                entry: 0,
                logical: 1,
                byte: 0
            },
            "the resting row, addressed by content"
        );

        // A wider window merged the top logical line into one row: every index
        // below it shifted by one.
        s.rewrap(
            40,
            vec![
                hard(0, "narrow top wrapped part"),
                hard(0, "ANCHOR"),
                hard(0, "below it"),
            ],
        );

        assert_eq!(
            s.resting_anchor(),
            Some(anchor),
            "the same content is still what the view rests on"
        );
        assert_eq!(
            s.window(1)[0].to_string(),
            "ANCHOR",
            "…and that is what the band shows"
        );
        assert_eq!(
            s.offset(),
            1,
            "the view did not jump to the row its content used to occupy"
        );
        // The row an index-preserving resize would have shown:
        assert_eq!(s.rows()[2].to_string(), "below it");
    }

    /// When the anchored content has left the transcript, the view holds as near
    /// as it can rather than teleporting to the tail — the user did not ask to
    /// be at the tail.
    #[test]
    fn rewrap_with_lost_content_holds_rather_than_inventing_a_position() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "a"), hard(0, "b"), hard(0, "c"), hard(0, "d")]);
        s.scroll_by(-3, 1);
        assert_eq!(s.window(1)[0].to_string(), "a");

        // Re-wrap without entry 0 at all: it was evicted while we looked away.
        s.rewrap(20, vec![hard(1, "x"), hard(1, "y")]);
        assert_eq!(s.offset(), 1, "clamped to the head of what still exists");
        assert!(!s.is_pinned(), "and not quietly moved to the tail");
        assert_eq!(
            lines(s.window(1)),
            ["x"],
            "the top of what is left is what the view shows, not the tail"
        );
    }

    /// Re-wrapping is not new content: nothing that was pending becomes anything
    /// else, and the count is not re-armed.
    #[test]
    fn rewrap_is_not_new_content() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "one"), hard(0, "two")]);
        s.scroll_by(-2, 1);
        s.push(vec![hard(1, "three")]);
        assert_eq!(s.pending(), 1);
        s.rewrap(30, vec![hard(0, "one"), hard(0, "two"), hard(1, "three")]);
        assert_eq!(s.pending(), 1, "the same unseen rows are still unseen");
    }

    // ─────────────────────── the cap, and what it trims ───────────────────────

    /// Content rows only — the marker is not content, and every "what did the
    /// store keep" question is a question about content.
    fn content(s: &Scrollback) -> Vec<String> {
        s.rows()
            .iter()
            .filter(|r| !r.is_trim_marker())
            .map(|r| r.to_string())
            .collect()
    }

    /// What a row of this text costs the cap: its text plus the structure it
    /// drags behind it. Every cap in this suite is written with this rather than
    /// a literal byte count, so the tests say what they mean in the same units
    /// the cap does and do not rot when the measured constant moves.
    fn cost(text: &str) -> usize {
        text.len() + ROW_STRUCT_BYTES
    }

    /// The cap that holds exactly these rows and nothing more besides them.
    fn cap_of(texts: &[&str]) -> usize {
        texts.iter().map(|t| cost(t)).sum::<usize>() + 1
    }

    /// The cap is in **bytes** and it trims the **oldest whole entries**: the
    /// unit is bytes because a row of base64 is not a row of prose, and the
    /// boundary is an entry because a scrollback that starts mid-paragraph
    /// reads like the transcript was cut up.
    #[test]
    fn the_byte_cap_drops_oldest_entries_whole_and_says_so() {
        // Room for two rows of this size and nothing more: the first entry fits
        // exactly, and the second can only come in by taking the first out.
        let cap = cap_of(&["aaaa", "bbbb"]);
        let mut s = Scrollback::with_cap(20, cap);
        s.push(vec![hard(0, "aaaa"), hard(0, "bbbb")]);
        assert_eq!(
            content(&s),
            vec!["aaaa", "bbbb"],
            "under the cap, nothing went"
        );
        assert_eq!(s.dropped_lines(), 0);

        s.push(vec![hard(1, "cccc"), hard(1, "dddd")]);
        assert_eq!(
            content(&s),
            vec!["cccc", "dddd"],
            "the older entry went whole, not one row of it"
        );
        assert_eq!(s.dropped_lines(), 2, "and the store counted what it lost");
        assert!(
            s.retained_bytes() <= cap,
            "and stayed under the cap while doing it: {}",
            s.retained_bytes()
        );
    }

    /// **The marker is there, at the head, and says the number** — the
    /// assertion the ticket is really about.
    #[test]
    fn a_trim_leaves_a_visible_marker_at_the_head_of_what_was_kept() {
        let cap = cost("gone gone gone") + 1;
        let mut s = Scrollback::with_cap(20, cap);
        s.push(vec![hard(0, "gone gone gone")]);
        assert!(
            !s.rows()[0].is_trim_marker(),
            "nothing trimmed, nothing to say: {}",
            s.rows()[0]
        );
        s.push(vec![hard(1, "kept kept kept")]);

        assert!(s.rows()[0].is_trim_marker(), "the marker is row 0");
        let m = s.rows()[0].to_string();
        assert!(m.contains("scrollback trimmed"), "{m:?}");
        assert!(m.contains("1 earlier lines dropped"), "{m:?}");
        assert_eq!(content(&s), vec!["kept kept kept"]);
        // The marker is chrome, so it is not charged to the cap: a long hint on
        // the end of it must not push content out to make room for an apology.
        let charged: usize = s
            .rows()
            .iter()
            .filter(|r| !r.is_trim_marker())
            .map(|r| r.charged())
            .sum();
        assert_eq!(
            charged,
            s.retained_bytes(),
            "the marker costs the cap nothing"
        );
    }

    /// The marker's sentence, pinned by **equality** rather than by `contains`.
    ///
    /// Two pages quote this row verbatim — `docs/guide/transcript.md` shows it
    /// in a fenced block (`⌄ scrollback trimmed: 412 earlier lines dropped`)
    /// and `docs/testing.md` quotes the same sentence with the count written as
    /// `N` — and every other marker assertion in the tree asks only
    /// `contains("scrollback trimmed")`: the test above, `selection.rs:1484`,
    /// `session/view/tests/{evict,retention,rewrap}.rs`. `contains` survives a
    /// reword of either end of the sentence, so it is weaker than the claim the
    /// docs make; the page quotes a whole row, so the test pins a whole row.
    ///
    /// The glyph is written as `\u{2304}` (U+2304 DOWN ARROWHEAD) on purpose: the
    /// lookalikes a reword could reach for — carons, breve, tilde, a bare `v` —
    /// all read the same in a diff, and only this one is what the site renders.
    #[test]
    fn the_marker_says_the_exact_sentence_the_docs_quote() {
        assert_eq!(
            marker_text(1, None),
            "\u{2304} scrollback trimmed: 1 earlier lines dropped"
        );
        assert_eq!(
            marker_text(412, None),
            "\u{2304} scrollback trimmed: 412 earlier lines dropped"
        );
        // The thousands separator is part of the quoted shape, not a detail of
        // the counter: this is the row `spikes/results/scrollback-cost.log`
        // captured on a real 16k-line trim.
        assert_eq!(
            marker_text(16_834, None),
            "\u{2304} scrollback trimmed: 16,834 earlier lines dropped"
        );

        // And the *rendered head row* of a trimmed store is that string, not
        // merely what the helper returns: what the docs describe is the row.
        let cap = cost("gone gone") + 1;
        let mut s = Scrollback::with_cap(20, cap);
        s.push(vec![hard(0, "gone gone")]);
        s.push(vec![hard(1, "kept kept")]);
        assert!(
            s.rows()[0].is_trim_marker(),
            "precondition: row 0 is the marker"
        );
        assert_eq!(
            s.rows()[0].to_string(),
            "\u{2304} scrollback trimmed: 1 earlier lines dropped"
        );
    }

    /// The half `docs/testing.md` promises in prose — "*and names the journal
    /// file that kept what the store dropped*" — pinned as a whole row.
    ///
    /// The path arrives through [`Scrollback::set_trim_hint`], which
    /// `SessionView::set_journal` fills from `FileJournal::display_path` (the
    /// stable per-mode `last-<mode>` symlink). Pinning the joined row fixes the
    /// words around the path and the U+00B7 middot that binds them; asserting the
    /// path alone would leave "full transcript:" free to become "see also:".
    #[test]
    fn the_marker_names_the_journal_that_still_has_what_it_dropped() {
        // The shape `transcript_file::display_path` actually hands back: home
        // abbreviated, one stable symlink per mode.
        let journal = "~/.local/share/looprs/transcripts/last-beads";
        assert_eq!(
            marker_text(412, Some(journal)),
            "\u{2304} scrollback trimmed: 412 earlier lines dropped \
             \u{00b7} full transcript: ~/.local/share/looprs/transcripts/last-beads"
        );

        // Through the store as well as through the helper, and in the order the
        // app really does it: the journal is attached *after* content exists
        // (`set_journal` is called on a running view), so the hint has to
        // re-word the row that is already sitting at the head of the store.
        let cap = cost("gone gone") + 1;
        let mut s = Scrollback::with_cap(20, cap);
        s.push(vec![hard(0, "gone gone")]);
        s.push(vec![hard(1, "kept kept")]);
        s.set_trim_hint(Some(journal.to_string()));
        assert!(s.rows()[0].is_trim_marker());
        assert_eq!(s.rows()[0].to_string(), marker_text(1, Some(journal)));

        // Losing the journal loses the tail of the sentence with it — separator
        // included. A store that cannot name a file must not point at one.
        s.set_trim_hint(None);
        let row = s.rows()[0].to_string();
        assert_eq!(row, "\u{2304} scrollback trimmed: 1 earlier lines dropped");
        assert!(!row.contains("full transcript"), "{row:?}");
    }

    /// The same pin aimed the other way: the pages that quote the marker must
    /// quote a row this code renders.
    ///
    /// The literal-equality tests above fail when the wording changes and the
    /// docs are left behind; nothing failed when the *docs* changed and the code
    /// was left behind. `include_str!` closes that side at compile time — no
    /// filesystem at test time, and a moved or deleted page is a compile error
    /// rather than a test that quietly stopped checking anything.
    ///
    /// The count is blanked before comparing because the two pages use
    /// different numbers (`412` in the guide, a literal `N` in the testing
    /// page); the contract is the shape of the sentence, and any number written
    /// into either page is allowed to be any number. A quoted row may be either
    /// rendered form — bare, or with the journal named — since both are real.
    #[test]
    fn the_pages_that_quote_the_marker_quote_a_row_this_code_renders() {
        let pages = [
            (
                "docs/guide/transcript.md",
                include_str!("../../docs/guide/transcript.md"),
            ),
            ("docs/testing.md", include_str!("../../docs/testing.md")),
        ];
        // The two forms this row renders in: bare, and with the journal named.
        // Both go in, because a page is allowed to quote either one.
        let journal = "~/.local/share/looprs/transcripts/last-beads";
        let templates: Vec<String> = vec![
            marker_text(12_345, None).replace("12,345", "<N>"),
            marker_text(12_345, Some(journal)).replace("12,345", "<N>"),
        ];
        for template in &templates {
            assert!(
                template.contains("<N>"),
                "the marker puts its count between two fixed halves: {template:?}"
            );
        }

        for (name, page) in pages {
            let quotes = quoted_marker_rows(page);
            assert!(
                !quotes.is_empty(),
                "{name}: no U+2304 marker row found — the page was rewritten and this test went vacuous"
            );
            for quote in quotes {
                let matched = templates.iter().any(|template| {
                    let (prefix, suffix) = template.split_once("<N>").unwrap();
                    quote
                        .strip_prefix(prefix)
                        .and_then(|rest| rest.strip_suffix(suffix))
                        .is_some_and(|count| {
                            !count.is_empty()
                                && count
                                    .chars()
                                    .all(|c| c.is_ascii_digit() || c == ',' || c == 'N')
                        })
                });
                assert!(
                    matched,
                    "{name}: {quote:?} is not a marker this code renders — it matches neither {:?} nor {:?}",
                    templates[0], templates[1]
                );
            }
        }
    }

    /// Every marker row quoted by a page: from the arrow to the end of its
    /// line, stopping early at a closing backtick so an inline code span in
    /// prose yields the row alone and not the sentence wrapped around it.
    fn quoted_marker_rows(page: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = page;
        while let Some(pos) = rest.find('\u{2304}') {
            let from = &rest[pos..];
            let end = from.find(['\n', '`']).unwrap_or(from.len());
            out.push(from[..end].trim_end().to_string());
            rest = &from[end..];
        }
        out
    }

    /// The marker counts **content lines**, not the rows our wrap cut them into,
    /// and it counts a line once even when it went over two trims. Three
    /// soft-wrapped rows of one paragraph is one line lost; reporting three (or
    /// reporting the same line twice as the tail follows the head out) is the
    /// marker lying in the alarming direction.
    #[test]
    fn the_marker_counts_content_lines_not_wrapped_rows() {
        let cap = cost("ccc") + 1;
        let mut s = Scrollback::with_cap(20, cap);
        // One logical line, three display rows: soft + soft + hard.
        s.push(vec![soft(0, "aaa"), soft(0, "bbb"), hard(0, "ccc")]);
        assert_eq!(s.dropped_lines(), 1, "the head of the line went: one line");
        s.push(vec![hard(1, "ddd")]);
        assert_eq!(content(&s), vec!["ddd"], "the whole of entry 0 went");
        assert_eq!(
            s.dropped_lines(),
            1,
            "and the tail of that same line did not count as a second one"
        );
    }

    /// **One entry larger than the cap** is the case a whole-entry trim cannot
    /// handle, and "where possible" means it rather than "give up on the cap":
    /// rows go instead, newest kept, and the marker says so.
    #[test]
    fn one_entry_bigger_than_the_cap_trims_rows_rather_than_the_cap() {
        let cap = cap_of(&["x", "x", "x"]);
        let mut s = Scrollback::with_cap(20, cap);
        s.push((0..6).map(|_| hard(0, "x")).collect());
        // Aligning the cut to the head of entry 0 would drop the whole store, so
        // the trim falls back on whole rows and keeps what fits.
        assert_eq!(
            content(&s).len(),
            3,
            "three rows of the six survived (the marker is the fourth row of the store)"
        );
        assert_eq!(s.dropped_lines(), 3);
        assert!(s.retained_bytes() <= cap, "{}", s.retained_bytes());
        assert!(
            s.rows().iter().any(|r| r.entry == 0),
            "entry 0 is still there — partly, which is what the marker is for"
        );
    }

    /// A cap too small for anything keeps the newest row: a scrollback that
    /// throws away the line that just arrived is worse than one that is
    /// momentarily over its own bound.
    #[test]
    fn a_cap_too_small_for_anything_keeps_the_newest_row() {
        let mut s = Scrollback::with_cap(20, 8);
        s.push(vec![hard(0, "aaa"), hard(0, "bbb")]);
        assert_eq!(content(&s), vec!["bbb"], "the tail is never what goes");
        assert_eq!(s.dropped_lines(), 1);
    }

    /// `0` means unbounded, the same convention the transcript buffer cap uses.
    #[test]
    fn zero_is_unbounded_and_says_nothing_about_a_trim() {
        let mut open = Scrollback::with_cap(20, 0);
        open.push((0..50).map(|i| hard(i, &format!("l{i}"))).collect());
        assert_eq!(open.len(), 50);
        assert_eq!(open.dropped_lines(), 0);
        assert!(!open.rows()[0].is_trim_marker());
    }

    /// Tightening the cap trims on the spot rather than at the next push: a cap
    /// that only bites when new content arrives is a cap with a hole in it.
    #[test]
    fn setting_a_smaller_cap_trims_immediately() {
        let mut s = Scrollback::with_cap(20, 1_000_000);
        s.push(vec![
            hard(0, "one one"),
            hard(1, "two two"),
            hard(2, "three three"),
        ]);
        assert_eq!(content(&s).len(), 3);
        s.set_cap(cost("three three") + 1);
        assert_eq!(content(&s), vec!["three three"]);
        assert_eq!(s.dropped_lines(), 2);
    }

    /// **A trim never moves the view.** `offset` is measured from the tail, so
    /// a row dropped from the head leaves everything below it where it was —
    /// even when the marker is inserted at the head in the same breath, and even
    /// though the same push appended a row at the tail.
    #[test]
    fn a_trim_never_moves_the_content_the_user_is_looking_at() {
        let cap = cap_of(&["a", "b", "c"]);
        let mut s = Scrollback::with_cap(20, cap);
        s.push(vec![hard(0, "a"), hard(1, "b"), hard(2, "c")]);
        s.scroll_by(-1, 1);
        let before = lines(s.window(1));
        assert_eq!(before, vec!["b"], "looking at the middle row");
        s.push(vec![hard(3, "d")]);
        assert_eq!(
            lines(s.window(1)),
            before,
            "the head lost a row, the tail gained one, the view moved by neither"
        );
        assert!(!s.is_pinned());
        assert!(s.rows()[0].is_trim_marker(), "and the loss is marked above");
    }

    /// The marker survives the one thing that replaces every row it is standing
    /// on: a resize. A trim reported before a resize and swallowed by one would
    /// be a promise the store cannot keep.
    #[test]
    fn the_marker_survives_every_row_being_replaced_by_a_rewrap() {
        let cap = cost("gone gone") + 1;
        let mut s = Scrollback::with_cap(40, cap);
        s.push(vec![hard(0, "gone gone")]);
        s.push(vec![hard(1, "kept kept")]);
        assert!(s.rows()[0].is_trim_marker());
        assert_eq!(s.dropped_lines(), 1);

        s.rewrap(80, vec![hard(1, "kept kept")]);
        assert!(
            s.rows()[0].is_trim_marker(),
            "the re-wrap replaced every row and put the marker back: {:?}",
            lines(s.rows())
        );
        assert_eq!(s.dropped_lines(), 1, "and remembered the count");
        assert_eq!(content(&s), vec!["kept kept"]);
    }

    /// A store with nothing trimmed has no marker at all: the head of the
    /// scrollback *is* the head of the session, and a `trimmed: 0` row would
    /// train the user to read a marker that means nothing.
    #[test]
    fn an_untrimmed_store_has_no_marker_to_misread() {
        let mut s = Scrollback::with_cap(20, 100_000);
        s.push(vec![hard(0, "a"), hard(1, "b")]);
        assert!(!s.rows().iter().any(|r| r.is_trim_marker()));
        assert_eq!(s.dropped_lines(), 0);
    }

    /// After the transcript itself is compacted, `entry` must still mean the
    /// entry it meant — or the row has to go.
    #[test]
    fn eviction_drops_gone_entries_and_renumbers_the_rest() {
        let mut s = Scrollback::new(20);
        s.push(vec![
            hard(0, "from entry 0"),
            hard(1, "from entry 1"),
            hard(2, "from entry 2"),
            hard(3, "from entry 3"),
        ]);
        // Entries 0 and 1 came off the front, two content lines with them.
        s.entries_evicted(2, 2);

        let got: Vec<(usize, String)> = s
            .rows()
            .iter()
            .filter(|r| !r.is_trim_marker())
            .map(|r| (r.entry, r.to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, "from entry 2".to_string()),
                (1, "from entry 3".to_string())
            ],
            "the survivors shifted down by exactly what came off, and no more"
        );
        assert_eq!(paste_text(s.rows()), "from entry 2\nfrom entry 3");
        assert_eq!(s.dropped_lines(), 2, "and the loss is on the marker");
        assert!(s.rows()[0].is_trim_marker());
    }

    /// The eviction's line count is the *caller's*, because the caller knows
    /// whether this view ever rendered the entries it dropped: a view that was
    /// off screen has no rows for them and lost the content just the same.
    #[test]
    fn eviction_reports_the_lines_the_caller_says_even_with_no_rows_to_lose() {
        let mut s = Scrollback::new(20);
        // Nothing rendered yet: this view was never on screen.
        s.entries_evicted(4, 97);
        assert_eq!(s.dropped_lines(), 97);
        assert!(s.rows()[0].is_trim_marker());
        assert!(content(&s).is_empty());
    }

    #[test]
    fn eviction_of_nothing_changes_nothing() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "a")]);
        s.entries_evicted(0, 0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.rows()[0].entry, 0);
        assert_eq!(s.dropped_lines(), 0);
    }

    /// Every `window` call is in range for every shape of store: the frame hands
    /// this a band height taken from the window, so a store shorter than the
    /// band, a band of one row and an empty store all have to be answerable.
    #[test]
    fn the_window_is_always_a_valid_slice() {
        for n in 0..12usize {
            for visible in 0..8usize {
                for scroll in 0..=n {
                    let mut s = Scrollback::new(20);
                    s.push((0..n).map(|i| hard(0, &format!("l{i}"))).collect());
                    s.scroll_by(-(scroll as isize), visible);
                    let w = s.window(visible);
                    assert!(w.len() <= visible.max(1), "n={n} visible={visible}");
                    assert!(w.len() <= n);
                    if n > 0 && visible > 0 {
                        assert!(!w.is_empty(), "n={n} visible={visible} scroll={scroll}");
                    }
                    // And never anything but a contiguous run of the store.
                    if let Some(first) = w.first() {
                        let text = first.to_string();
                        assert!(s.rows().iter().any(|r| r.to_string() == text));
                    }
                }
            }
        }
    }

    /// A pinned store of fewer rows than the band shows what there is, and no
    /// more: the caller pads.
    #[test]
    fn a_short_transcript_shows_what_exists() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "x"), hard(0, "y")]);
        assert_eq!(lines(s.window(10)), ["x", "y"]);
        assert_eq!(s.window(0).len(), 0, "a zero-height band gets nothing");
        let empty = Scrollback::new(20);
        assert!(empty.window(10).is_empty());
    }

    // ────── what a re-render costs, priced from the last one (looprs-zie) ──────

    /// The rewrap budget is only as good as this report, so the report is a
    /// test: rows, the non-empty logical lines a narrower band adds rows *to*,
    /// the separators that do not re-flow, and the text bytes they carry.
    #[test]
    fn entry_shapes_report_what_the_rows_actually_cost() {
        let mut s = Scrollback::new(40);
        // One logical line wrapped over two rows (a `Soft` row continued by a
        // `Hard` one), the blank the renderer puts after every entry, and a
        // second entry of one row.
        s.push(vec![
            soft(0, "aaaa"),
            hard(0, "bbbb"),
            hard(0, ""),
            hard(1, "cc"),
        ]);
        let shapes = s.entry_shapes();
        assert_eq!(shapes.len(), 2, "one shape per entry, marker excluded");
        assert_eq!(shapes[0].entry, 0);
        assert_eq!(shapes[0].rows, 3, "two rows of text and the separator");
        assert_eq!(
            shapes[0].lines, 1,
            "one non-empty logical line: the soft row is a continuation, not a line"
        );
        assert_eq!(
            shapes[0].blank, 1,
            "the separator row costs one row at any width"
        );
        assert_eq!(shapes[0].text, 8);
        assert_eq!(
            shapes[1],
            EntryShape {
                entry: 1,
                rows: 1,
                lines: 1,
                blank: 0,
                text: 2
            }
        );
    }

    /// The marker must say what a source cut threw away — and the count of that
    /// is a `max` between the caller, who knows what left the transcript, and
    /// the store, who knows what it ever rendered.
    #[test]
    fn a_source_cut_reports_the_larger_of_the_two_counts_it_can_make() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "one"), hard(1, "two"), hard(2, "three")]);
        assert_eq!(s.dropped_lines(), 0, "nothing lost yet");

        // A rebuild that covers entries 2.. leaves 0 and 1 behind. The caller
        // counted 4 lines off the transcript; the store had rows for 2.
        s.note_source_dropped(2, 4);
        assert_eq!(s.dropped_lines(), 4);
        assert!(
            s.rows().first().is_some_and(|r| r.is_trim_marker()),
            "and the head of the store says so"
        );
    }

    /// The other side of the same `max`: a caller that undercounts does not get
    /// to under-report what this store actually held.
    #[test]
    fn a_source_cut_never_under_reports_what_the_store_had() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "one"), hard(1, "two"), hard(2, "three")]);
        s.note_source_dropped(2, 1);
        assert_eq!(
            s.dropped_lines(),
            2,
            "two entries of rows here beats a caller that said one"
        );
    }

    /// Nothing below the cut, nothing lost, nothing to report.
    #[test]
    fn a_source_cut_at_the_head_reports_nothing() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "one"), hard(1, "two")]);
        s.note_source_dropped(0, 99);
        assert_eq!(s.dropped_lines(), 0);
        assert!(!s.rows().first().is_some_and(|r| r.is_trim_marker()));
    }

    /// The cap the rebuild is budgeted against has to be the cap the store
    /// enforces, read rather than restated.
    #[test]
    fn cap_bytes_is_the_number_the_trim_uses() {
        assert_eq!(Scrollback::with_cap(40, 1234).cap_bytes(), 1234);
        assert_eq!(
            Scrollback::with_cap(40, 0).cap_bytes(),
            0,
            "unbounded reads as zero, the same convention the view buffer uses"
        );
        assert_eq!(Scrollback::new(40).cap_bytes(), DEFAULT_RETAINED_BYTES);
    }
}
