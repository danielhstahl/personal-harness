//! The rewrap policy: rebuilding the store's rows at a new width, out of the
//! slice of source the retained budget can still pay for.
//!
//! A resize invalidates every wrapped row in the store. Rather than hold the
//! whole transcript in memory to rebuild it exactly, [`SessionView::rewrap`]
//! picks the oldest entry a rebuild at the new width can cover
//! ([`SessionView::rewrap_source_start`]) and re-renders from there — and the
//! entries cut off the front are *counted* as dropped, not quietly forgotten,
//! because a trim the user cannot see is the failure this store's marker exists
//! to prevent.
//!
//! The arithmetic that decides how much history a resize buys, and the
//! measurement behind it (`spikes/results/resize-transient.log`), is all here,
//! because this is the one place that has to know the cap and the flusher's
//! position at the same time.
use crate::session::view::SessionView;
use crate::session::view::buffer::{MIN_BYTES_PER_ROW, UNSEEN_ROW_SAFETY, price_scaled};

use crate::theme::styles::content_width;

impl SessionView {
    /// Re-make the store at a new width, from the **newest slice of the source
    /// that can still fit the cap**, and report how many of the rows it produced
    /// were *not yet emitted* before the rebuild.
    ///
    /// The rows are a *projection* of the entries, and the only honest way to
    /// re-wrap a projection is to make it again from what it projects: the
    /// flusher is reseated and drained at the new width, which leaves its cursor
    /// exactly where the per-frame drains would have left it, so nothing is
    /// emitted twice and nothing is dropped that the store is keeping.
    ///
    /// What it does *not* do any more is re-render the whole transcript and let
    /// the store trim afterwards. That shape cost one full markdown pass over
    /// every entry the view holds to retain the newest slice of them: measured
    /// on a long pass, 30,000 rows materialised (~164 MiB at the code's own
    /// [`ROW_STRUCT_BYTES`] charge) to keep 5,787 of them (32 MiB), for one
    /// column of window drag. The source is now cut at
    /// [`Self::rewrap_source_start`] first, so the transient is O(cap) plus
    /// whatever the drain owed this frame anyway, instead of O(transcript).
    ///
    /// A resize is a drag in practice, and paying one bounded re-render per
    /// frame for a scrollback that stays where the user was looking is the trade
    /// ADR-0004 signed up for when it made the transcript ours to scroll.
    ///
    /// The entries cut off the front of the source leave the store by *never
    /// being made again* rather than by a trim, so they are counted as dropped
    /// here — before the swap, while the rows the store has for them are still
    /// here to count. A trim the user cannot see is the failure this store's
    /// whole marker exists to prevent.
    ///
    /// "Not yet emitted" is taken from the flusher's own position *before* it
    /// is reseated: everything it had already rendered stays rendered, and the
    /// rest is what this call newly finalised. Counting it after the fact is not
    /// possible — the rebuild trims as it goes, so the store's length says what
    /// was kept, not what arrived.
    pub fn rewrap(&mut self, width: u16) -> usize {
        if self.scrollback.width() == width {
            return 0;
        }
        let unseen_from = self.flusher.consumed();
        let start = self.rewrap_source_start(width);
        // Counted off the transcript before the drain, because it is the
        // entries themselves that carry the line count and the drain is where
        // memory gets tight.
        let skipped_lines = self.skipped_source_lines(start);
        self.flusher.reseat(start);
        let rows = self.flusher.drain_rows(&self.transcript, width);
        let added = rows.iter().filter(|r| r.entry >= unseen_from).count();
        self.scrollback.note_source_dropped(start, skipped_lines);
        self.scrollback.rewrap(width, rows);
        self.source_skipped = start;
        added
    }

    /// The oldest transcript entry a re-render at `width` has to cover.
    ///
    /// Everything older is content this store cannot hold at *any* width, so
    /// rendering it is work whose entire output is thrown away.
    ///
    /// The slice is chosen from the back with the store's own measurements as
    /// the price list ([`Scrollback::entry_shapes`]): a row that has been
    /// rendered once before has a *measured* cost at a known width, and
    /// re-rendering it at a new width costs that, scaled by the width ratio,
    /// plus at most one row per logical line for the rounding the narrower band
    /// introduces. Text that has never been rendered has no measurement, so it
    /// is priced at the density the store has already achieved times
    /// [`UNSEEN_ROW_SAFETY`] — an estimate carrying its own margin, not a
    /// number pretending to be a measurement.
    ///
    /// Three rules keep the *tail* safe, because this cuts the source and the
    /// render then runs forward from the cut:
    ///
    /// * everything the flusher has not emitted yet is inside the slice,
    ///   always. Cutting it would be dropping content the frame has never shown,
    ///   and it is what the drain this rebuild replaces was going to render
    ///   anyway;
    /// * the newest entry is inside the slice even when it alone is over the cap
    ///   — the store's own "keep the newest row anyway" rule from
    ///   [`Scrollback::trim`], applied to the source;
    /// * an entry the store holds no rows for ends the walk rather than being
    ///   guessed at. It was trimmed, and everything older with it: it could not
    ///   have come back inside the cap either, so there is nothing to price.
    ///
    /// An *under*-estimate of any entry's cost is not a correctness problem: the
    /// store trims whatever the rebuild leaves over the cap, exactly as it did
    /// when the rebuild covered everything, and counts it.
    ///
    /// What the bound costs is the mirror image: the store fills to a fraction of
    /// its cap after a resize rather than to exactly the cap, because the safe
    /// price of a re-render is above what it will actually cost. Measured on the
    /// pdl.7 corpus a rebuild holds 28–77% of its cap, depending on which way
    /// the window moved and the shape of the text
    /// (`spikes/results/resize-transient.log`) — and the direction it is worst
    /// in is *widening*, where the strict upper price is the row count the entry
    /// already has, so a wider window buys no more history than the narrow one
    /// held. Nothing the user had is lost by that; it just does not reach further
    /// back into what an earlier resize already trimmed. Bought against that is
    /// the whole point of the ticket: 9–25 MiB of transient across a sweep of
    /// widths where rendering the whole source for the same resizes cost
    /// 133–151 MiB, and a figure that stopped growing with the length of the
    /// transcript.
    pub(super) fn rewrap_source_start(&self, width: u16) -> usize {
        let cap = self.scrollback.cap_bytes();
        let n = self.transcript.entries.len();
        // Unbounded store: nothing to cut back to, and no transient to bound.
        if cap == 0 || n == 0 {
            return 0;
        }
        let unseen = self.flusher.consumed().min(n);
        let shapes = self.scrollback.entry_shapes();
        let old_cw = content_width(self.scrollback.width()).max(1) as usize;
        let new_cw = content_width(width).max(1) as usize;

        // What one byte of never-rendered text costs here, by this store's own
        // achieved density. `MIN_BYTES_PER_ROW` covers the store that has no
        // rows yet and therefore no opinion.
        let rows_total: usize = shapes.iter().map(|s| s.rows).sum();
        let text_total: usize = shapes.iter().map(|s| s.text).sum();
        let bytes_per_row = (text_total / rows_total.max(1)).max(MIN_BYTES_PER_ROW);

        // Everything below is accumulated in **bytes scaled by `new_cw`**, so
        // the width ratio is never rounded — not per entry, not per candidate.
        // Rounding a row up per entry is a row per entry of history the budget
        // refuses to buy, and over sixty entries that is a tenth of the cap
        // thrown away on arithmetic. One rounding at the end, against
        // `cap × new_cw`, is the same decision without the tax.
        let scaled_cap = cap.saturating_mul(new_cw);

        // (1) The unseen tail: kept unconditionally, and its price is spent out
        // of the cap before anything older is considered — it is going to be on
        // the store either way, and the older entries can only have what is left.
        let mut spent: usize = (unseen..n)
            .map(|e| {
                let text = self.transcript.entries[e].text.len();
                let rows = (text / bytes_per_row).max(1) * UNSEEN_ROW_SAFETY;
                price_scaled(rows.saturating_mul(new_cw), text, new_cw)
            })
            .sum();

        // (2) Back over the entries the store already paid for, while they fit.
        let mut start = unseen;
        let mut si = shapes.len();
        for e in (0..unseen).rev() {
            while si > 0 && shapes[si - 1].entry > e {
                si -= 1;
            }
            let Some(shape) = si.checked_sub(1).and_then(|i| shapes.get(i)) else {
                break;
            };
            if shape.entry != e {
                // Rule three: no rows for this entry means it was trimmed away.
                break;
            }
            // `rows × new_cw` for what the entry will make at the new width.
            let rows_scaled = if new_cw >= old_cw {
                // Widening never adds a row to a logical line: the rows it has
                // are the rows it will make again.
                shape.rows.saturating_mul(new_cw)
            } else {
                // Three parts, because an entry's rows are not one thing. The
                // text rows re-flow with the band — `content × old_cw / new_cw`,
                // carried here as `content × old_cw` so the division stays
                // exact. The blank separators do not re-flow: one row at any
                // width. And the rounding the ratio leaves behind is worth at
                // most one extra row per non-empty logical line, which is the
                // `lines` term — the worst case of that rounding, not the
                // ordinary one.
                let content = shape.rows.saturating_sub(shape.blank);
                content
                    .saturating_mul(old_cw)
                    .saturating_add((shape.lines + shape.blank).saturating_mul(new_cw))
            };
            let charge = price_scaled(rows_scaled, shape.text, new_cw);
            if spent.saturating_add(charge) > scaled_cap {
                break;
            }
            spent += charge;
            start = e;
        }
        // Rule two: the newest entry is always in the slice, even if the walk
        // above could not afford it. `start == n` would re-render nothing at all
        // and empty the store on a resize.
        start.min(n - 1)
    }

    /// Content lines in the transcript entries the next rewrap cuts off the
    /// source, counting only the ones no other path has reported.
    ///
    /// `self.source_skipped` is where the previous rewrap left off, so the same
    /// paragraph is not counted by this *and* by the buffer-cap eviction that
    /// later takes the same entries off the front of the transcript.
    fn skipped_source_lines(&self, to_entry: usize) -> usize {
        let from = self.source_skipped.min(to_entry);
        (from..to_entry)
            .filter_map(|e| self.transcript.entries.get(e))
            .map(|e| e.text.lines().count())
            .sum()
    }
}
