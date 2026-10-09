//! The flusher half of the view: the door every finalized line goes through.
//!
//! [`SessionView::flush`] is the only path from a [`Transcript`] entry to a row
//! in the [`Scrollback`] store, and everything that has to be arranged around
//! it lives in this file: the record-push verbs the sessions call
//! ([`SessionView::push_note`], [`SessionView::push_delta`], the tool and
//! compaction cards, [`SessionView::finish_stream`]), the seal that closes what
//! is still open when a session ends, and the write-side accounting
//! ([`SessionView::after_write`], [`SessionView::journal_finalised`]) that keeps
//! the journal and the store telling the same story about the same lines.
//!
//! The invariant the ADR is picky about — one cursor, one transcript, each
//! finalized line exactly once — is a property of this pairing rather than of
//! the callers, and the width the rows are wrapped for is decided here instead
//! of by whoever happens to be drawing.
use crate::session::view::SessionView;

use crate::state::transcript::MessageKind;

impl SessionView {
    /// Rows that became final since the last call. Call once per frame, for the
    /// active view only; each one is also appended to
    /// [`Self::scrollback`](SessionView::scrollback), which is what the frame's
    /// transcript band draws.
    ///
    /// Invariant preserved here: monotonic. Every finalized line of this transcript
    /// is returned exactly once, ever — and, for the same reason, appears in the
    /// store exactly once. The flusher is the only door to either, which is what
    /// makes "the band shows each line exactly once" a property of the type rather
    /// than of the caller.
    ///
    /// The width is not an incidental argument: it is the geometry the stored rows
    /// are wrapped for. A call at a new width re-wraps the whole store first
    /// ([`Self::rewrap`]) before anything new goes in, because a store that is
    /// half one width and half another renders as text that stops making sense at
    /// the seam — which is why this is one door and not two.
    ///
    /// How many rows **became final** on this call and went into the store.
    ///
    /// It used to hand back the rows themselves, cloned. That clone was the whole
    /// batch, every frame, at the store's own measured 5,760 bytes of heap per
    /// row — and nothing on the display ever read it: the band draws from
    /// [`App::transcript_window`], which is the store's own window, and the run
    /// loop calls this for the side effect and drops the value. A count carries
    /// the same fact ("N rows arrived") at no cost, and a caller that wants the
    /// text reads the store that already owns it.
    ///
    /// On a width change the store is *rebuilt* rather than extended; the count
    /// stays "what the flusher finalized this call", which is the question the
    /// caller asks, not "how big is the store now".
    pub fn flush(&mut self, width: u16) -> usize {
        if self.scrollback.width() != width {
            // Every stored row is wrapped for a window that no longer exists. The
            // rows that would have been pushed here are already part of what the
            // rebuild makes again, so the rebuild is the one drain — it used to
            // be two, one of them for a return value nobody read.
            return self.rewrap(width);
        }
        let rows = self.flusher.drain_rows(&self.transcript, width);
        let added = rows.len();
        self.scrollback.push(rows);
        added
    }

    /// Close whatever streamed entry is still open.
    ///
    /// MUST be called when the owning session dies ([`super::SessionEvent::Exited`])
    /// or is torn down. Without it the transcript keeps an entry that is `!done`
    /// forever: the flusher stalls on it, nothing that session already produced
    /// ever reaches the transcript band again, and the live preview shows a
    /// spinner on dead text forever. This is the seam that breaks the one-door
    /// invariant, so it is part of the contract rather than a detail.
    pub fn seal(&mut self) {
        // The line the resolver still had open is part of this tail, and it goes
        // in *before* anything is closed: pushed into the still-open Bash entry,
        // then closed with it. After `finish_last` the entry is done, and the tail
        // would start a second entry — same words, wrong shape.
        self.flush_shell_pending();
        self.transcript.finish_last();
        // ...and so are any cards still open in it. `finish_last` deliberately
        // leaves running tools alone (parallel tools must survive a delta
        // arriving), so without this a tool or compaction that was in flight when
        // the child died holds the flush cursor forever: everything the session
        // said *after* that point — which is most of what the exit drain exists
        // to collect — never reaches the scrollback.
        self.transcript.abandon_open_cards();
        // A half-parsed escape sequence belongs to a stream that is never going to
        // send the rest of it. Left as it is, the next Bash generation's first
        // bytes get eaten by the previous one's dangling `\x1b[`, which shows up
        // as the first line of a brand new shell silently missing its head.
        self.shell.reset();
        // The last chance this session gets to say everything it said: sealing
        // closes the entries the journal's prefix walk was waiting on, so the
        // tail of a dead or torn-down session reaches the file rather than staying
        // behind an open card. This is not the journal's mechanism — R2 is
        // append-as-you-finalise, and by the time we get here the normal case has
        // been flushed entry by entry — it is the drain of work the walk could
        // not finish on its own.
        self.after_write();
    }

    /// A finished, one-shot line (status notices, the mode-switch separator).
    pub fn push_note(&mut self, kind: MessageKind, text: String) {
        self.flush_shell_pending();
        self.transcript.push_done(kind, text);
        self.after_write();
    }

    /// A streaming delta from this session's backend.
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        self.flush_shell_pending();
        self.transcript.push_delta(kind, delta);
        self.after_write();
    }

    /// A session-level error: recorded for the status row and shown.
    pub fn push_error(&mut self, text: String) {
        self.last_error = Some(text.clone());
        self.flush_shell_pending();
        self.transcript.push_done(MessageKind::Error, text);
        self.after_write();
    }

    // ─────────────── the card kinds, through the same door ───────────────
    //
    // A tool card and a compaction card used to be written straight into
    // `view.transcript` by the App, which meant the two things `after_write`
    // exists to do did not happen for them: no cap, and no journal. Tool
    // results are the **biggest content in a beads pass** — a `bd show`, a
    // file read, a `git diff` — so the cap that was supposed to bound a
    // session's memory did not bound the part of it that actually costs
    // anything, and the journal that was supposed to survive a crash never saw
    // the part worth reading. Both are fixed by the same move: the App asks the
    // view to write the card, and the view runs the edge.

    /// A tool started: opens the card entry.
    pub fn start_tool(&mut self, id: String, name: String, input: String) {
        self.flush_shell_pending();
        self.transcript.start_tool(id, name, input);
        self.after_write();
    }

    /// A tool came back: fills the card in and finalises it.
    ///
    /// Note this finalises an entry that is *not* the last one — a tool that
    /// reported back behind an open sibling, or behind streamed prose, lands in
    /// the middle of the transcript. That is exactly what the journal's prefix
    /// walk is built for: the entry is journalled when the walk reaches it, and
    /// the walk cannot pass an entry that is still open, so file order is
    /// transcript order.
    pub fn finish_tool(&mut self, id: String, summary: String, is_error: bool) {
        self.transcript.finish_tool(id, summary, is_error);
        self.after_write();
    }

    /// pi is pausing the run to compact the context.
    pub fn start_compaction(&mut self, reason: String) {
        self.flush_shell_pending();
        self.transcript.start_compaction(reason);
        self.after_write();
    }

    /// The compaction finished, one way or another.
    ///
    /// Returns whether there was a card to close; `false` is the caller's cue to
    /// record the event itself (see the App's arm), and this records nothing.
    pub fn finish_compaction(
        &mut self,
        state: crate::components::compaction::CompactionState,
        detail: String,
    ) -> bool {
        let closed = self.transcript.finish_compaction(state, detail);
        self.after_write();
        closed
    }

    /// Close the streamed entry that is open, if any: the point a turn's prose
    /// stops being live and starts being transcript.
    ///
    /// The `message_end` arm of the App's event handling, with the edge that goes
    /// with it — an answer that ended is an answer the journal should have.
    pub fn finish_stream(&mut self) {
        self.transcript.finish_last();
        self.after_write();
    }

    /// Every write to this view's transcript ends here, and only here.
    ///
    /// Two things happen on this edge, in this order:
    ///
    /// 1. [`Self::journal_finalised`] hands whatever just became final to the
    ///    journal, and
    /// 2. [`Self::enforce_buffer`] trims what has grown past the cap.
    ///
    /// The order is load-bearing and it is the reason the two are one function
    /// rather than two calls at every call site: journal **before** trim, so
    /// content that is about to leave memory has already been handed to the
    /// file. Done the other way round, the cap eats the history the journal
    /// exists to keep, which is the exact failure ADR-0004 R2 was written to
    /// close — and it is silent, because by the time anyone looks, the bytes are
    /// neither in memory nor on disk.
    pub(super) fn after_write(&mut self) {
        self.journal_finalised();
        self.enforce_buffer();
        // The buffer counter is the one number in this view that no single
        // structure owns: the transcript keeps it, the cap reads it, eviction
        // debits it. Checked here, on the one edge every write passes, so a site
        // that moves bytes without moving the counter is caught in a debug build
        // by whatever test streams anything (looprs-7m5).
        self.transcript.debug_assert_bytes();
    }

    /// Push every entry that has become final, and only those, to the journal.
    ///
    /// The chunk is built to the same join as [`Transcript::plain_text`] — entry
    /// text with trailing newlines off, entries separated by a blank line — so
    /// the journal of a whole run is byte-for-byte the same document the
    /// `Ctrl-S a` dump of that run writes (ADR-0004 R2). The rule lives in both
    /// places on purpose: this one appends it as it finalises, that one answers
    /// for a transcript in one go, and the claim that they agree is what
    /// `the_journal_is_what_select_all_and_copy_would_have_given` checks. If
    /// `plain_text`'s join ever changes, that test fails and this one follows it
    /// rather than the journal quietly drifting to a different document.
    ///
    /// Entries with no text are skipped without being journalled, and the cursor
    /// still moves past them: an empty tool summary is not a missing paragraph.
    fn journal_finalised(&mut self) {
        let mut chunk = String::new();
        while self.journalled < self.transcript.entries.len() {
            let e = &self.transcript.entries[self.journalled];
            // The prefix walk stops at the first still-open entry. Ordering in
            // the file is transcript order, not finalisation order; a card that
            // has not come back holds up what arrived behind it for as long as it
            // takes, and `seal` empties the pipe when the stream ends.
            if !e.done {
                break;
            }
            let body = e.text.trim_end_matches('\n');
            self.journalled += 1;
            // The entry is in the file now, whole or finish-it-off: whatever comes
            // next starts a new entry and gets the ordinary between-entries join.
            self.journal_continued = false;
            if body.is_empty() {
                continue;
            }
            // No separator on a *continuation*: this entry already began in the
            // file as the head of a cut that `trim_open_entry` made, and putting
            // a blank line in the middle of it would make the journal a
            // different document from the transcript it is meant to be.
            if self.journalled_any && !self.journal_continued {
                // Two newlines, because that is the join `plain_text` uses: the
                // entry's own line ending plus a blank line between entries.
                // Matching it exactly is the point — the journal of a whole run
                // and the `Ctrl-S a` dump of that run are then the same bytes,
                // which is what makes either one safe to hand to someone.
                chunk.push_str("\n\n");
            }
            chunk.push_str(body);
            chunk.push('\n');
            self.journalled_any = true;
        }
        if chunk.is_empty() {
            return;
        }
        self.journal.append(self.session.mode.label(), chunk);
    }
}
