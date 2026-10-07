//! The per-session scrollback: a [`Transcript`] welded to the [`Flusher`] that was
//! born with it (ADR-0002 Q5).
//!
//! The ADR rejects the single shared transcript. The reason is not aesthetics, it is
//! the flusher's own invariant: a `Flusher` is a *cursor into one specific
//! transcript*, and the moment two transcripts share one cursor (or one
//! transcript is rendered through two cursors) lines get dropped or duplicated.
//! When the pane printed into the terminal's scrollback that was the one place a
//! bug was unrecoverable for the user; since looprs-pdl.4 the lines land in the
//! frame's transcript band instead, and the invariant is the same one for a
//! different surface — the display cache this view keeps is written only by
//! [`SessionView::flush`].
//!
//! So the pairing is the type. `SessionView` owns both halves, its `flusher` field
//! is private, and there is no way to point it at somebody else's transcript.
//!
//! Everything the App used to keep globally — "is the live region streaming", "is a
//! bead waiting on me", "what is this mode's last error" — lives here instead, per
//! session. `App::need_input` and `App::chat_state` are *derived from the active
//! view*, never from `input.mode`: a Beads pass that is running off-screen must not
//! be able to hide the Pi input box, and a Pi answer must not be able to pause the
//! beads loop.

use std::time::{Duration, Instant};

use ratatui::text::Line;

use super::{SessionId, TerminalType};
use crate::components::scrollback::{Flusher, RenderedRow};
use crate::session::ActiveBead;
use crate::session::{BeadStep, SessionStatus};
use crate::state::scrollback::Scrollback;
use crate::state::transcript::{Entry, MessageKind, Transcript};
use crate::theme::styles::{restyle, style_for};
use crate::utils::shelltext::LineResolver;

/// Default cap on how much text an *inactive* view will hold.
///
/// A view that is on screen drains every frame, so this only binds for a session
/// nobody is looking at — which is exactly the unbounded case ADR-0002 names
/// ("a `yes | sleep 1000000`-style shell … grows its transcript forever"). When a
/// hidden session overruns the cap, old lines are dropped and one visible notice is
/// inserted, so the loss is honest rather than silent.
pub const DEFAULT_VIEW_BUFFER: usize = 256 * 1024;

/// Room held back out of the cap for the eviction notice itself, so the view ends
/// *at or under* the limit rather than limit-plus-a-line. Generous for any wording
/// under 64 bytes ("… 4,294,967,295 bytes dropped (buffer cap) …" fits).
const NOTICE_BUDGET: usize = 64;

/// What the live (not-yet-final) region of one session is showing.
///
/// Per session rather than global: this decides whether the preview renders and
/// whether the spinner ticks, and those are properties of *this* session's stream,
/// not of whichever mode happens to be on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChatState {
    /// Nothing streaming.
    #[default]
    Stopped,
    /// Prose streaming into the preview.
    Chat,
    /// A tool run is live.
    Tool,
    /// A context compaction is live.
    ///
    /// Its own state rather than reusing `Tool`: it is not a tool, and the thing
    /// the state is *for* is deciding what the live region shows and whether the
    /// spinner turns. Compaction wants the prose preview held back (the card owns
    /// the row) and the spinner turning (a compaction is a real, slow, paid-for
    /// call, and a still transcript over a working child is the "is it hung?"
    /// question this card was added to answer) — which is what
    /// [`Self::is_streaming`] and `main::view`'s `Chat`-only text draw give it.
    Compacting,
}

impl ChatState {
    pub fn is_streaming(self) -> bool {
        !matches!(self, Self::Stopped)
    }
}

/// What this view's token window has spent.
///
/// The *window* is what differs by mode, because the question differs — see
/// [`SessionView::tokens`]. The numbers come straight off pi's `usage` record,
/// folded in once per assistant message.
///
/// Cache tokens are carried separately rather than folded into `input`, because on
/// Anthropic-style accounting `input` **excludes** them — and on a long run the
/// cache buckets are where most of the tokens go. Fold them in and the row reports
/// a number quietly smaller than the work that was done; leave them out and a run
/// that is 90% cache reads looks free. They get their own segment, which is also
/// the first one the width ladder gives back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    /// `cacheRead` + `cacheWrite`, summed at the door: the row shows them as one
    /// number and nothing upstream wants the split.
    pub cache: u64,
}

impl Tokens {
    /// Fold one assistant message's usage into the window.
    ///
    /// Called from the *authoritative* `message_end` only. pi also reports `usage`
    /// on every `message_update`, and that figure is **cumulative for the message
    /// still streaming** — folding it in per delta grows the total with the square
    /// of the message length, and it looks plausible right up until it doesn't.
    /// `message_end` lands once per API call, which is every bit as live as the row
    /// needs and impossible to double-count.
    pub fn add(&mut self, u: &crate::app::Usage) {
        self.input += u.input;
        self.output += u.output;
        self.cache += u.cache_read + u.cache_write;
    }

    /// Nothing reported yet — which is not the same fact as "zero spent", per
    /// [`crate::app::Usage`]'s optionality. The row shows no segment for it.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One terminal state's rendered history, plus the render cursor over it.
///
/// `App` holds `HashMap<TerminalType, SessionView>`. Events are applied to the
/// view named by `Msg::*{ session }` — **never** to the view currently on screen.
/// Rendering picks the active view. Those two selections are independent, and that
/// independence is the fix for the whole class of "I tabbed away and the other
/// session's output corrupted this one" bugs.
pub struct SessionView {
    pub session: SessionId,
    pub transcript: Transcript,
    /// Private on purpose: this cursor is only valid for `self.transcript`.
    flusher: Flusher,
    /// The scrollable store: every rendered row this view has produced, plus the
    /// offset, the pin and the "N new" count over them.
    ///
    /// Written only through [`Self::flush`] and [`Self::rewrap`], which are the
    /// only two things that have seen the flusher's output — so "a line appears
    /// in the band exactly once" still follows from the monotonic-cursor property
    /// that made "a line reaches the scrollback exactly once" true, and a resize
    /// cannot leave the store wrapped for two different widths.
    ///
    /// Bounded by [`crate::state::scrollback::DEFAULT_MAX_ROWS`] rendered rows;
    /// the text it was rendered from is separately bounded by
    /// [`DEFAULT_VIEW_BUFFER`].
    scrollback: Scrollback,
    /// Liveness mirror of the owning session, for the status row (looprs-guh).
    pub status: SessionStatus,
    /// When the **current run** started, for `run_elapsed`.
    ///
    /// Set on the way *into* a busy state and cleared on the way out, which makes
    /// it "this run", not "this session": a session that goes idle and busy again
    /// starts a new clock rather than inheriting an age hours old. The instant is
    /// handed in rather than read here so the view stays a set of functions of
    /// state that a test can drive without waiting for anything.
    pub run_started: Option<Instant>,
    /// Most recent error, kept so the status row can show it without digging
    /// through scrollback (looprs-guh).
    pub last_error: Option<String>,
    /// What the live region shows for this session (was: a single global on `App`).
    pub chat: ChatState,
    /// The ticket the beads loop holds right now (looprs-w7q). `None` for every
    /// other mode, and for beads between passes.
    ///
    /// Set from the session's own `SessionEvent::ActiveBead` — the loop publishes
    /// its claim, it does not have it inferred. Consumer: the status row
    /// (looprs-guh), which cannot name the active ticket without this.
    pub active_bead: Option<ActiveBead>,
    /// The beads machine's step, for the status row. `None` for every other mode.
    /// Rendered, never re-derived (looprs-msj).
    pub step: Option<BeadStep>,
    /// Tokens this view's window has spent, for the status row.
    ///
    /// **Pi chat: the whole session** — every turn this warm child has answered.
    /// **Beads: the current ticket** — [`App`](crate::app::App) clears it when a
    /// new claim is published, so the row answers "what is this bead costing",
    /// which is the question a loop that spends money unwatched actually wants
    /// answered.
    ///
    /// Three things the window deliberately does *not* do:
    ///
    /// * **it does not roll back on `Esc`.** A cancelled pass spent what it spent;
    ///   cancelling does not un-buy it, and a number that went down on a keystroke
    ///   would be a number nobody could trust;
    /// * **it does not clear when a claim is released** (`ActiveBead: None`), so
    ///   the row keeps showing what that ticket cost after its pass ended, until
    ///   the next claim opens a fresh window;
    /// * **it does not zero out the planner.** A planner pass holds no claim, so
    ///   its spend lands in the window of the first bead that follows it — which
    ///   is the work it planned, and beats throwing away a pass that cost money.
    pub tokens: Tokens,
    /// Dropped-line bookkeeping for the buffer cap (see [`DEFAULT_VIEW_BUFFER`]).
    dropped: usize,
    limit: usize,
    /// Trims this view has applied that the app has not yet fed to the other
    /// things that address the same store.
    ///
    /// [`Scrollback::entries_evicted`] renumbers rows by entry, and the drag
    /// selection (looprs-pdl.9) speaks the same addresses — so it has to hear
    /// about the trim in the same breath, or an eviction moves the entries out
    /// from under a standing selection and the highlight silently starts
    /// pointing at the message *after* the one the user selected. The pairs are
    /// `(removed, notice_at)`, exactly as given to the store, and
    /// [`Self::take_trims`] is how the App drains them.
    pending_trims: Vec<(usize, usize)>,
    /// The resolver for this view's Bash output (see [`Self::push_bash`]).
    ///
    /// Per-view, and per-stream: shell output is not a string, it is a *stream*
    /// with state — an escape sequence straddles two reads, an SGR style carries
    /// from one line to the next, and the cell a `\r` returns to is the cell
    /// earlier bytes of *this* shell wrote. ADR-0005 is the decision; this field
    /// is where that state lives, and there is exactly one of them per Bash
    /// session, which is what makes "who owns the resolution" a answered
    /// question rather than a rumour.
    shell: LineResolver,
}

impl SessionView {
    /// Transcript and Flusher are created together and only ever used together.
    pub fn new(session: SessionId) -> Self {
        Self::with_buffer(session, DEFAULT_VIEW_BUFFER)
    }

    /// As [`SessionView::new`], with an explicit buffer cap (0 = unbounded). Tests
    /// use the small-cap form to exercise eviction without streaming 256 KiB.
    pub fn with_buffer(session: SessionId, limit: usize) -> Self {
        Self {
            session,
            transcript: Transcript::new(),
            flusher: Flusher::new(),
            scrollback: Scrollback::new(0),
            status: SessionStatus::NotStarted,
            run_started: None,
            last_error: None,
            chat: ChatState::Stopped,
            step: None,
            active_bead: None,
            tokens: Tokens::default(),
            dropped: 0,
            limit,
            pending_trims: Vec::new(),
            shell: LineResolver::new(),
        }
    }

    /// Shell output into the transcript (ADR-0005, superseding the "strip the
    /// presentation" reading of ADR-0001 rule 5).
    ///
    /// The bytes are *resolved* — SGR becomes a style, `\r`/`\b`/`\t`/`EL` are
    /// applied inside the line, clusters keep their cell count — and the finished
    /// lines go to [`Transcript::push_shell_lines`] with their styles attached.
    /// What is still guaranteed by ADR-0001 rule 1 is unchanged: no markdown, and
    /// no re-wrap by us at any point between the child and the scrollback.
    ///
    /// `term_width` is the width the child is writing for, in cells, and it must
    /// be the **same value that was forwarded to the pty**. That is what makes a
    /// `\r` here land on the row the child believed it was on rather than on the
    /// head of the whole logical line (ADR-0005 Q2).
    pub fn push_bash(&mut self, chunk: &str, term_width: u16) {
        self.shell.set_wrap_width(term_width as usize);
        let lines = self.shell.feed(chunk);
        if lines.is_empty() {
            return;
        }
        self.transcript.push_shell_lines(&lines);
        self.enforce_buffer();
    }

    /// Move the line the resolver still has open into the transcript, ended.
    ///
    /// The open line lives in the resolver rather than in the entry so that it can
    /// still be overwritten by the next `\r` — which is the whole reason the
    /// store can stay an append-only list of finished lines. That only works if
    /// "the stream ended" is answered by *somebody*, and every path that ends one
    /// (a different kind of entry opening, the seal, teardown) calls this first.
    /// Nothing typed into a shell is allowed to evaporate because a newline
    /// never arrived before the prompt changed hands.
    fn flush_shell_pending(&mut self) {
        if let Some(line) = self.shell.take_pending() {
            self.transcript
                .push_shell_lines(std::slice::from_ref(&line));
        }
    }

    /// Close off the shell output gathered so far, so the command about to run
    /// starts a fresh transcript entry (looprs-pdl.13).
    ///
    /// This is the *making* of the command boundary that
    /// [`Transcript::last_command_output`] reads. Without it a Bash session's
    /// whole life is one entry, because a stream of one kind is one entry by
    /// design, and "copy the last command's output" would silently mean
    /// "everything since the shell started" — a copy that widens itself is worse
    /// than one that refuses.
    ///
    /// The pending resolver line is flushed first and deliberately: it is almost
    /// always the prompt the shell came back to, which belongs to the block that
    /// just finished. Leaving it in the resolver and sealing underneath would let
    /// it land at the *front* of the next command's entry, which is the same
    /// bytes in the wrong place — the one mistake this file's whole
    /// text-and-styles-travel-together rule exists to prevent.
    ///
    /// Called at submit rather than at command completion because submit is the
    /// moment the boundary is known. The shell itself does not report where one
    /// command ends and the next begins, and inferring it from a prompt pattern
    /// would break on every shell that is not bash.
    pub fn seal_shell_output(&mut self) {
        self.flush_shell_pending();
        self.transcript.seal_command();
    }

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
    pub fn flush(&mut self, width: u16) -> Vec<RenderedRow> {
        let rows = self.flusher.drain_rows(&self.transcript, width);
        let out = rows.clone();
        if self.scrollback.width() != width {
            // Every stored row is wrapped for a window that no longer exists. The
            // rows just drained are already right for the new width, but they
            // are a tail on a body of old ones, so the whole store is made
            // again — including this tail, which is why it is not pushed here.
            self.rewrap(width);
        } else {
            self.scrollback.push(rows);
        }
        out
    }

    /// This view's scrollable store.
    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    /// Mutable access to the scroll state (offset, pin) — the scroll keys drive
    /// the store through here so nothing else can move the view.
    pub fn scrollback_mut(&mut self) -> &mut Scrollback {
        &mut self.scrollback
    }

    /// Re-render every stored row at `width`, keeping the view anchored to the
    /// content it was showing rather than to the row index it happened to be at.
    ///
    /// The rows are a *projection* of the entries, and the only honest way to
    /// re-wrap a projection is to make it again from what it projects: the
    /// flusher is reseeded to entry 0 and drained at the new width, which leaves
    /// its cursor exactly where the per-frame drains would have left it, so
    /// nothing is emitted twice and nothing is dropped.
    ///
    /// The cost is one full markdown pass over the retained transcript per width
    /// change — bounded by the view's buffer cap, and coalesced to at most one
    /// per frame by the draw path that calls it. A resize is a drag in practice,
    /// and paying one re-render per frame for a scrollback that stays where the
    /// user was looking is the trade ADR-0004 signed up for when it made the
    /// transcript ours to scroll.
    pub fn rewrap(&mut self, width: u16) {
        if self.scrollback.width() == width {
            return;
        }
        self.flusher.reseat(0);
        let rows = self.flusher.drain_rows(&self.transcript, width);
        self.scrollback.rewrap(width, rows);
    }

    /// The not-yet-final tail, for the live preview region.
    pub fn preview(&self, width: u16) -> Vec<Line<'static>> {
        // A Bash view's live tail is in the resolver, not in the entry: the entry
        // only ever holds lines the resolver *finished*, so the flusher would
        // honestly answer "nothing open" while the shell is mid-line.
        //
        // The pending line needs no guard against "but something else is live":
        // every path that writes a different kind through this view calls
        // `flush_shell_pending` first, so a pending line is, by construction, the
        // newest thing this session said.
        if let Some(line) = self.shell.pending() {
            return vec![restyle(line.to_line(), style_for(&MessageKind::Bash))];
        }
        self.flusher.preview(&self.transcript, width)
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
    }

    /// A finished, one-shot line (status notices, the mode-switch separator).
    pub fn push_note(&mut self, kind: MessageKind, text: String) {
        self.flush_shell_pending();
        self.transcript.push_done(kind, text);
        self.enforce_buffer();
    }

    /// A streaming delta from this session's backend.
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        self.flush_shell_pending();
        self.transcript.push_delta(kind, delta);
        self.enforce_buffer();
    }

    /// A session-level error: recorded for the status row and shown.
    pub fn push_error(&mut self, text: String) {
        self.last_error = Some(text.clone());
        self.flush_shell_pending();
        self.transcript.push_done(MessageKind::Error, text);
        self.enforce_buffer();
    }

    /// Mirror the owning session's liveness, and wind the run clock.
    ///
    /// The run clock is *this run*, not *this session*: the clock starts on the way
    /// into a busy state and is cleared on the way out, so a session that goes idle
    /// and busy again starts a new clock rather than inheriting an age hours old.
    /// Writing it only when busy-ness actually *changes* is what makes both halves
    /// idempotent — a session that publishes `Running` twice (a mirror refresh, a
    /// duplicate edge) leaves the age the row is showing alone, and `Idle → Dead`
    /// has nothing left to clear.
    ///
    /// This no longer touches the input box. Input availability is *derived* in
    /// [`Self::accepts_input`] from the mode plus this same status, so a liveness
    /// edge carries no opinion about the keyboard and there is nothing here left to
    /// keep in step with anything.
    pub fn set_status(&mut self, status: SessionStatus, now: Instant) {
        if status.is_busy() != self.status.is_busy() {
            self.run_started = status.is_busy().then_some(now);
        }
        // A child that is gone cannot still be streaming into the live region, so
        // stop the preview rather than spinning a spinner over dead text.
        // (`seal()` closes the transcript itself; this closes the *preview* of it.)
        if !status.is_alive() {
            self.chat = ChatState::Stopped;
        }
        self.status = status;
    }

    /// May this mode take typed input right now?
    ///
    /// One rule, and it is the whole rule: **an agentic mode owns the keyboard
    /// while it works; Bash never owns it.** So during a pi run or a beads pass
    /// the user still has a terminal — exactly one of them, the Bash one — and no
    /// agent ever has a box competing with the run it is in the middle of.
    ///
    /// Derived rather than stored, and that is the point. This used to be a field
    /// written from three places — `set_status` on death, `set_step` on every
    /// beads transition, and the boot seed in `main.rs` — which meant "can I type
    /// here?" had three potential answers depending on which message landed last,
    /// and `set_status` had to carry the arithmetic to reconcile them. Computed
    /// here, there is one source of truth, and two useful consequences come free
    /// with no special case: a session that never started, and a session whose
    /// child died, are both "not busy", so neither can hold the keyboard hostage.
    pub fn accepts_input(&self) -> bool {
        match self.session.mode {
            // A shell is never mid-turn from our point of view. Its command may
            // have been running for an hour and the user can still type into it
            // (Ctrl-C is the shell's, not ours); this is the mode that stays open
            // while the agents are working.
            TerminalType::Bash => true,
            // The agentic modes own the keyboard for the duration of a turn —
            // including the `Aborting` window, where the turn is still unwinding
            // and is still an agentic workflow occurring.
            TerminalType::Pi | TerminalType::Beeds => !self.status.is_busy(),
        }
    }

    /// How long this session's current run has been going, measured at `now`.
    ///
    /// `None` unless a run is live: "idle for 4 minutes" is not a thing the row
    /// should imply, and an age that keeps counting after the run ended is worse
    /// than no age at all. `now` comes from the caller's tick, never from here, so
    /// the render path reads no clock.
    pub fn run_elapsed(&self, now: Instant) -> Option<Duration> {
        if !self.status.is_busy() {
            return None;
        }
        Some(
            self.run_started
                .map(|s| now.saturating_duration_since(s))
                .unwrap_or_default(),
        )
    }

    /// The beads machine moved: record the step, for the status row.
    ///
    /// This used to gate the input box as well, on [`BeadStep::awaits_user`]. That
    /// second gate is gone because it was a copy of a fact already on the wire: the
    /// beads loop publishes `Idle` for precisely the state `AwaitInput` describes
    /// — "started, no worker up, waiting for a human" (`BeadsTask::status`) — so
    /// the step and the liveness mirror are one sentence said twice, and
    /// [`Self::accepts_input`] reads the copy every mode has rather than keeping a
    /// second lock that can rust out of sync with the first.
    ///
    /// The step still lives here because the row renders it and must not re-derive
    /// it (looprs-msj). It is a label now, not a lock.
    pub fn set_step(&mut self, step: BeadStep) {
        self.step = Some(step);
    }

    /// How many bytes of this view's output the cap has dropped.
    ///
    /// The counter is what makes an eviction honest rather than invisible, and the
    /// honesty lives in the transcript itself: [`Self::enforce_buffer`] inserts a
    /// visible `… N bytes dropped (buffer cap) …` notice when it evicts. The
    /// status row's own `~N dropped` segment is gone — the row now spends that
    /// room on token counts — so nothing in the shipped binary reads this.
    #[allow(dead_code)] // test seam: `view::tests` asserts the count and the notice that quotes it
    pub fn dropped_bytes(&self) -> usize {
        self.dropped
    }

    /// Cap the buffered transcript while this view is *not* on screen.
    ///
    /// Lossy on purpose — that is what a cap is — so the two things it must get
    /// right are: say what was lost, and do not corrupt the render cursor. The
    /// notice goes in *at* the cursor rather than at the head of the transcript, so
    /// it is the next thing the terminal sees and the still-pending entries behind
    /// it keep their order. The cursor is then reseat-ed, because its per-entry
    /// state (scan/block/fence) belongs to whatever entry it was last reading.
    ///
    /// The notice's own size is reserved out of the cap: without that the view ends
    /// at `limit + notice` and the cap is a rounding error with an apology note.
    fn enforce_buffer(&mut self) {
        // What the cap has to cover is everything this view holds, which is the
        // transcript *plus* the Bash line the resolver is still resolving: the
        // store cannot see that one, and it is exactly the thing a child that
        // never ends a line can grow without bound.
        let buffered = self.transcript.byte_len() + self.shell.pending_len();
        if self.limit == 0 || buffered <= self.limit {
            return;
        }
        let target = self.limit.saturating_sub(NOTICE_BUDGET);
        let first = self.flusher.consumed();
        let mut dropped = 0usize;
        let mut removed = 0usize;
        // Always keep one entry: an empty transcript with the cursor past the end is
        // a state nothing downstream is written to expect.
        while self.transcript.byte_len() > target && self.transcript.entries.len() > 1 {
            dropped += self.transcript.entries.remove(0).text.len();
            removed += 1;
        }
        if dropped == 0 {
            return;
        }
        self.dropped += dropped;
        // The eviction shifted the transcript; the cursor follows it, and the notice
        // takes the cursor's slot so it is what gets written out next.
        let at = first.saturating_sub(removed);
        self.transcript.entries.insert(
            at,
            Entry {
                kind: MessageKind::System,
                text: format!("… {} bytes dropped (buffer cap) …", self.dropped),
                done: true,
                styles: Vec::new(),
            },
        );
        self.flusher.reseat(at);
        // The store indexes rows by entry, and the entries just moved underneath
        // it. Rows rendered from a gone entry cannot be re-rendered, so they go
        // now rather than vanishing on the next resize; the survivors get
        // renumbered so `entry` keeps naming the right thing.
        self.scrollback.entries_evicted(removed, at);
        self.pending_trims.push((removed, at));
    }

    /// Drain the trims applied since the last call, as `(removed, notice_at)`
    /// pairs in the order they happened.
    ///
    /// The consumer is the drag selection, which addresses rows by entry and so
    /// must be renumbered by the same eviction the store was renumbered by
    /// (looprs-pdl.9). Both calls take the same two numbers from the same
    /// place, which is the only thing keeping "what the row is numbered" and
    /// "what the selection thinks it is numbered" one fact rather than two.
    pub fn take_trims(&mut self) -> Vec<(usize, usize)> {
        std::mem::take(&mut self.pending_trims)
    }

    /// Set the byte cap. A test seam in the shipped shape: eviction is
    /// otherwise reachable only by streaming [`DEFAULT_VIEW_BUFFER`] bytes, and
    /// a test that does that is a slow test that nobody runs.
    #[allow(dead_code)] // test seam: `app::tests` trips the buffer cap through this to test the selection's trim re-base
    pub fn set_buffer_limit(&mut self, limit: usize) {
        self.limit = limit;
    }
}

// ─────────────────── the chord table (looprs-pdl.13) ───────────────────
//
/// Every key the app claims, in every mode, in every state of the screen, with
/// exactly one owner each. This is the table the ticket asks for, and it is data
/// rather than prose for one reason: the rules written against it are tests, and a
/// rule checked against a comment is a rule checked by nobody.
///
/// **The rows are the order of decision**, and [`App::on_key`] executes them in
/// that same order: the control chords first (`Ctrl-C`, `Ctrl-Q`, `Ctrl-S`, all
/// three claimed before anything else because all three have to survive every
/// other state), then the armed chord's second key, then the passthrough handover,
/// then `Esc`-clears-the-selection, then the scroll keys, then the input box.
/// A row appearing above another row is the same statement as "handles the key
/// first", which is what makes the no-shadowing audit mean something.
///
/// ```text
/// MODE        KEY              STATE          OWNER     EFFECT
/// Pi, Beads   Ctrl-C         plain          App       quit (no Cancel worth the name yet)
/// Bash        Ctrl-C         plain          Shell     0x03 → SIGINT the foreground group
/// all         Ctrl-Q         plain          App       quit
/// all         Ctrl-S         plain          App       arm the copy chord (help toast is the window)
/// all         a              armed          App       copy the last answer
/// all         o              armed          App       copy the last command's output
/// all         s              armed          App       copy the live selection
/// all         t              armed          App       write the whole transcript to a file
/// all         ?              armed          App       show the chord help
/// all         Esc            armed          App       cancel the chord, and nothing else
/// Bash        Ctrl-C         child holds    Shell     0x03 → SIGINT
/// Bash        Ctrl-Q         child holds    App       quit
/// Bash        Ctrl-S         child holds    App       swallowed: XOFF is never forwarded
/// Bash        any other      child holds    Child     forwarded as the bytes the terminal sent
/// all         Esc            selection live App       clear the selection, nothing sent
/// all         Esc            plain          session   the mode's cancel (ADR-0003)
/// all         Tab            plain          App       switch mode (clears selection, chord, throttle)
/// all         Shift-Tab      plain          Box       newline
/// all         Shift-Enter    plain          Box       newline
/// all         Enter          plain          Box       submit
/// all         PageUp         plain          App       one page up (unpins)
/// all         PageDown       plain          App       one page down (re-pins at the tail)
/// all         Home           plain          App       top of the transcript
/// all         End            plain          App       bottom, and re-pin
/// all         anything else  plain          Box       typing
/// ```
///
/// The three rules the audit below runs against it are the ticket's:
///
/// 1. **`Ctrl-C` is not copy.** In Bash mode it is SIGINT and stays SIGINT. No
///    row in this table may pair a `Ctrl-C` with a copy or a dump, in any mode.
/// 2. **Nothing added may shadow an existing binding in the mode it is added
///    to**, and the audit covers the modes' *differences*: the same key must
///    have exactly one owner per (mode, state), which is why `Ctrl-C` gets two
///    rows and why "the child holds the screen" is a state at all rather than a
///    footnote.
/// 3. **`Esc`'s branch is written down per state**, with the "was a selection
///    live?" question explicit — and, since looprs-pdl.13 added one, with the
///    pending-chord state ahead of it: `Esc` undoes the most recent thing the
///    user gave us, never something older and louder.
///
/// The state of the screen a chord is being decided in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChordState {
    /// We hold the screen, nothing is outstanding, no selection is live.
    Plain,
    /// A selection is live (made by the mouse, or by any other means).
    SelectionLive,
    /// `Ctrl-S` is outstanding and the next key is the target.
    ChordArmed,
    /// A full-screen child holds the real terminal (ADR-0001 Q2).
    ChildHolds,
}

/// Who ends up with the keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// looprs's own chord/scroll layer.
    App,
    /// The shell, by way of bytes down the pty.
    Shell,
    /// The mode's session (the cancel that ADR-0003 owns).
    Session,
    /// The input box.
    Box,
    /// A full-screen child program, forwarded verbatim.
    Child,
}

impl Effect {
    /// The noun `copy_chord_hint` pairs with this effect's sub-key: what arming
    /// the prefix and pressing this target gets you. Only the copy family has a
    /// hint; everything else in the table is not a thing the chord offers.
    #[allow(dead_code)] // read only by `copy_chord_hint`, which is not on the hot path
    fn hint(self) -> Option<&'static str> {
        match self {
            Effect::CopyAnswer => Some("answer"),
            Effect::CopyLastOutput => Some("last output"),
            Effect::CopySelection => Some("selection"),
            Effect::DumpTranscriptFile => Some("transcript to file"),
            Effect::CancelChord => Some("cancel"),
            _ => None,
        }
    }
}

/// What the app does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// `0x03` to the pty master: SIGINT to the foreground process group.
    SigInt,
    /// The mode's own cancel.
    CancelRun,
    ClearSelection,
    ArmChord,
    CancelChord,
    CopyAnswer,
    CopyLastOutput,
    CopySelection,
    DumpTranscriptFile,
    ChordHelp,
    ScrollUp,
    ScrollDown,
    Top,
    Tail,
    SwitchMode,
    Newline,
    Submit,
    Typing,
    /// Forwarded raw to a full-screen child.
    Forwarded,
    /// Deliberately thrown away. The only one in the table is XOFF: we own
    /// `Ctrl-Q`, so a `Ctrl-S` we forwarded could stop a child the user then had
    /// no chord left to restart.
    Swallowed,
}

/// A key, spelled the way the table spells it, and constructible into the real
/// keystroke the driving tests send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySym {
    CtrlC,
    CtrlQ,
    CtrlS,
    /// The chord's second keys.
    TargetAnswer,
    TargetOutput,
    TargetSelection,
    TargetTranscript,
    Help,
    Esc,
    Tab,
    ShiftTab,
    Enter,
    ShiftEnter,
    PageUp,
    PageDown,
    Home,
    End,
    /// Any key not named above.
    AnyOther,
}

impl KeySym {
    /// The real keystroke this row talks about.
    ///
    /// #[allow(dead_code)] is below: the shipped binary never needs to turn a row
    /// back into a KeyEvent — the driving tests do, which is how the table is
    /// checked against the real handler instead of against a description of it.
    #[allow(dead_code)] // test seam: driven tests replay the table's chords
    /// For [`KeySym::AnyOther`] this is a plain `x`: the row's *meaning* is
    /// "anything unnamed", and `x` is the representative the driving tests use —
    /// chosen because nothing in the table names it, so a test that sends it is
    /// really sending the fallback and not a chord that happens to match.
    pub fn event(self) -> crossterm::event::KeyEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
        match self {
            KeySym::CtrlC => KeyEvent::new(KeyCode::Char('c'), M::CONTROL),
            KeySym::CtrlQ => KeyEvent::new(KeyCode::Char('q'), M::CONTROL),
            KeySym::CtrlS => KeyEvent::new(KeyCode::Char('s'), M::CONTROL),
            KeySym::TargetAnswer => KeyEvent::new(KeyCode::Char('a'), M::NONE),
            KeySym::TargetOutput => KeyEvent::new(KeyCode::Char('o'), M::NONE),
            KeySym::TargetSelection => KeyEvent::new(KeyCode::Char('s'), M::NONE),
            KeySym::TargetTranscript => KeyEvent::new(KeyCode::Char('t'), M::NONE),
            KeySym::Help => KeyEvent::new(KeyCode::Char('?'), M::NONE),
            KeySym::Esc => KeyEvent::new(KeyCode::Esc, M::NONE),
            KeySym::Tab => KeyEvent::new(KeyCode::Tab, M::NONE),
            KeySym::ShiftTab => KeyEvent::new(KeyCode::BackTab, M::SHIFT),
            KeySym::Enter => KeyEvent::new(KeyCode::Enter, M::NONE),
            KeySym::ShiftEnter => KeyEvent::new(KeyCode::Enter, M::SHIFT),
            KeySym::PageUp => KeyEvent::new(KeyCode::PageUp, M::NONE),
            KeySym::PageDown => KeyEvent::new(KeyCode::PageDown, M::NONE),
            KeySym::Home => KeyEvent::new(KeyCode::Home, M::NONE),
            KeySym::End => KeyEvent::new(KeyCode::End, M::NONE),
            KeySym::AnyOther => KeyEvent::new(KeyCode::Char('x'), M::NONE),
        }
    }

    /// Is this a *copy* chord? Rule 1 is stated in terms of this: nothing whose
    /// effect moves text out of the app may be reachable on `Ctrl-C`.
    #[allow(dead_code)] // audit-only: rule 1 of the table audit is stated over this
    pub fn is_copy(self) -> bool {
        matches!(
            self,
            KeySym::TargetAnswer
                | KeySym::TargetOutput
                | KeySym::TargetSelection
                | KeySym::TargetTranscript
        )
    }
}

/// One row of [`CHORD_TABLE`].
///
/// The shipped binary reads `state`, `keys` and `does` — that is what
/// [`copy_chord_hint`] is built from. `mode`, `key`, `owner` and `note` exist so
/// the audit in `tests` can state the rules over the whole row; the table is the
/// artifact that gets checked, not a structure the key path walks.
#[allow(dead_code)] // four of the seven columns are the audit's, three are production's
pub struct ChordRow {
    /// The mode this row applies to. Every row names one mode explicitly: a row
    /// that said "all modes" would hide the fact that `Ctrl-C` does not mean the
    /// same thing in all of them.
    pub mode: TerminalType,
    /// The key.
    pub key: KeySym,
    /// How it is spelled in the table above, and in a failure message.
    pub keys: &'static str,
    /// The state this row applies in.
    pub state: ChordState,
    /// Who ends up with it.
    pub owner: Owner,
    /// What happens.
    pub does: Effect,
    /// Why, in one line — the part a reader of the code needs and a reader of
    /// the enum cannot carry.
    pub note: &'static str,
}

/// The table. See the comment above it.
pub const CHORD_TABLE: &[ChordRow] = &[
    // ── the control chords, claimed before anything else ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::Plain,
        owner: Owner::Shell,
        does: Effect::SigInt,
        note: "the shell's own key: 0x03 to the pty master, the foreground group gets SIGINT, \
               the app stays up — and it is never, in any mode, a copy",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit for now: Pi mode has no Cancel worth the name until looprs-5g7, and a copy \
               chord is not what Ctrl-C becomes",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "same as Pi",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit without touching the shell; this is also why Ctrl-S cannot be forwarded \u{2014} \u{2014} \u{2018} is ours",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit in every mode",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit in every mode",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ArmChord,
        note: "the copy prefix: a prefix rather than four top-level chords, because the chord budget is the whole difficulty here",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ArmChord,
        note: "the copy prefix, armed",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ArmChord,
        note: "the copy prefix, armed",
    },
    // ── the chord's second key, armed only ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetAnswer,
        keys: "Ctrl-S a",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyAnswer,
        note: "in Bash this *refuses* (there are no answers here) and names the chord that works",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetAnswer,
        keys: "Ctrl-S a",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyAnswer,
        note: "the last answer, whole, through the looprs-pdl.10 sink and toast",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetAnswer,
        keys: "Ctrl-S a",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyAnswer,
        note: "the last answer, whole, through the looprs-pdl.10 sink and toast",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetOutput,
        keys: "Ctrl-S o",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyLastOutput,
        note: "the last sealed shell block: this command's echo, output and prompt, and nothing before it",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetOutput,
        keys: "Ctrl-S o",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyLastOutput,
        note: "the agentic modes' answer to the same question: the last finished tool card",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetOutput,
        keys: "Ctrl-S o",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyLastOutput,
        note: "the agentic modes' answer to the same question: the last finished tool card",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetSelection,
        keys: "Ctrl-S s",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopySelection,
        note: "whatever selection is live; a keyboard-driven selection was *not* one of the things that landed in this ticket, and this target is ready for it",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetSelection,
        keys: "Ctrl-S s",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopySelection,
        note: "whatever selection is live",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetSelection,
        keys: "Ctrl-S s",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopySelection,
        note: "whatever selection is live",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetTranscript,
        keys: "Ctrl-S t",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::DumpTranscriptFile,
        note: "the escape hatch: the whole transcript to a timestamped file, through an injected sink like the clipboard's",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetTranscript,
        keys: "Ctrl-S t",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::DumpTranscriptFile,
        note: "the escape hatch",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetTranscript,
        keys: "Ctrl-S t",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::DumpTranscriptFile,
        note: "the escape hatch",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Help,
        keys: "Ctrl-S ?",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::ChordHelp,
        note: "the family, listed in a toast: the chords have to be reachable from inside the app",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Help,
        keys: "Ctrl-S ?",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::ChordHelp,
        note: "the family, listed in a toast",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Help,
        keys: "Ctrl-S ?",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::ChordHelp,
        note: "the family, listed in a toast",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CancelChord,
        note: "undoes the prefix and nothing else \u{2014} it is deliberately not the mode's cancel",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CancelChord,
        note: "undoes the prefix and nothing else",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CancelChord,
        note: "undoes the prefix and nothing else",
    },
    // ── the child holds the screen ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::ChildHolds,
        owner: Owner::Shell,
        does: Effect::SigInt,
        note: "still SIGINT: a full-screen child does not take Ctrl-C away from the shell it is already in",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::ChildHolds,
        owner: Owner::App,
        does: Effect::Quit,
        note: "ours in every state; the child is killed on the way out",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::ChildHolds,
        owner: Owner::App,
        does: Effect::Swallowed,
        note: "never forwarded: XOFF into a pty whose XON (Ctrl-Q) we own is a freeze the user cannot undo",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::ChildHolds,
        owner: Owner::Child,
        does: Effect::Forwarded,
        note: "a program that owns the screen owns the keyboard (ADR-0001 Q2) \u{2014} Esc included, so vim leaves insert mode",
    },
    // ── the `Esc` branch, per mode, with the question the ticket asks shown ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::SelectionLive,
        owner: Owner::App,
        does: Effect::ClearSelection,
        note: "was a selection live? yes \u{2014} the first Esc unselects and sends nothing",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::SelectionLive,
        owner: Owner::App,
        does: Effect::ClearSelection,
        note: "was a selection live? yes \u{2014} the first Esc unselects and sends nothing",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::SelectionLive,
        owner: Owner::App,
        does: Effect::ClearSelection,
        note: "was a selection live? yes \u{2014} the first Esc unselects and sends nothing",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::Plain,
        owner: Owner::Session,
        does: Effect::CancelRun,
        note: "was a selection live? no \u{2014} the Esc is the cancel the mode table already describes",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::Plain,
        owner: Owner::Session,
        does: Effect::CancelRun,
        note: "was a selection live? no \u{2014} the cancel, and the queued text comes back to the box",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::Plain,
        owner: Owner::Session,
        does: Effect::CancelRun,
        note: "was a selection live? no \u{2014} the cancel",
    },
    // ── the rest of the table, uniform across modes ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Tab,
        keys: "Tab",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::SwitchMode,
        note: "switch mode; the selection, the chord and the wheel throttle all clear with it",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Tab,
        keys: "Tab",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::SwitchMode,
        note: "switch mode",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Tab,
        keys: "Tab",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::SwitchMode,
        note: "switch mode",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::ShiftTab,
        keys: "Shift-Tab",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "newline in the input box — Shift-Enter never sends a shell command",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::ShiftTab,
        keys: "Shift-Tab",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "newline in the input box",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::ShiftTab,
        keys: "Shift-Tab",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "newline in the input box",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::ShiftEnter,
        keys: "Shift-Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "a newline in the box, not a submit",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::ShiftEnter,
        keys: "Shift-Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "a newline in the box",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::ShiftEnter,
        keys: "Shift-Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "a newline in the box",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Enter,
        keys: "Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Submit,
        note: "submit; in Bash this is also where the command boundary is sealed for Ctrl-S o",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Enter,
        keys: "Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Submit,
        note: "submit",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Enter,
        keys: "Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Submit,
        note: "submit",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::PageUp,
        keys: "PageUp",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollUp,
        note: "one page up, unpinning the tail (looprs-pdl.8's semantics, the same store)",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::PageUp,
        keys: "PageUp",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollUp,
        note: "one page up, unpinning the tail",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::PageUp,
        keys: "PageUp",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollUp,
        note: "one page up, unpinning the tail",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::PageDown,
        keys: "PageDown",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollDown,
        note: "one page down; reaching the bottom re-pins",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::PageDown,
        keys: "PageDown",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollDown,
        note: "one page down; reaching the bottom re-pins",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::PageDown,
        keys: "PageDown",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollDown,
        note: "one page down; reaching the bottom re-pins",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Home,
        keys: "Home",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Top,
        note: "top of the transcript",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Home,
        keys: "Home",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Top,
        note: "top of the transcript",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Home,
        keys: "Home",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Top,
        note: "top of the transcript",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::End,
        keys: "End",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Tail,
        note: "bottom, and re-pin: what the \u{201c}N new\u{201d} affordance names",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::End,
        keys: "End",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Tail,
        note: "bottom, and re-pin",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::End,
        keys: "End",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Tail,
        note: "bottom, and re-pin",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Typing,
        note: "typing, to whichever mode's box is up",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Typing,
        note: "typing",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Typing,
        note: "typing",
    },
];

/// The hint shown while the copy chord is armed, and what `Ctrl-S ?` prints.
///
/// Read out of the table rather than written again: the string the user reads and
/// the table the audit checks are then the same data. A target added to the table
/// shows up in the hint by itself; a target that is not in the table cannot be
/// advertised.
pub fn copy_chord_hint() -> String {
    let mut parts: Vec<String> = Vec::new();
    for row in CHORD_TABLE
        .iter()
        .filter(|r| r.state == ChordState::ChordArmed)
    {
        let Some(noun) = row.does.hint() else {
            continue;
        };
        // The table spells each chord out in full (`"Ctrl-S a"`); the hint wants
        // the sub-key by itself next to the noun it gets you.
        let sub = row.keys.rsplit(' ').next().unwrap_or(row.keys);
        let item = format!("{sub} {noun}");
        if !parts.contains(&item) {
            parts.push(item);
        }
    }
    format!("Copy: {}", parts.join(" \u{b7} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: TerminalType) -> SessionView {
        SessionView::new(SessionId::new(mode, 0))
    }

    // ─────────────── the chord table audit (looprs-pdl.13) ───────────────
    //
    // These are the ticket's three rules, run against `CHORD_TABLE` as data. What
    // they cannot do is prove the *code* matches the table — that is what
    // `app::tests`' driven chord tests are for, and each rule names the test that
    // drives it. What they can do is make a hole in the table a build failure
    // rather than a surprise at 1 a.m.

    /// Rule 1: `Ctrl-C` is not copy, in any mode.
    #[test]
    fn ctrl_c_is_never_a_copy_in_any_mode() {
        for row in CHORD_TABLE {
            if row.key == KeySym::CtrlC {
                assert!(
                    !row.key.is_copy(),
                    "{}/{}: a copy chord spelled as Ctrl-C in {}",
                    row.mode.label(),
                    row.keys,
                    row.keys
                );
                assert!(
                    matches!(row.does, Effect::SigInt | Effect::Quit),
                    "{}: Ctrl-C must be SIGINT or quit, not {:?}",
                    row.keys,
                    row.does
                );
            }
        }
        // And positively: Bash's Ctrl-C is SIGINT, and stays SIGINT whatever else
        // this table grows.
        let bash = CHORD_TABLE
            .iter()
            .filter(|r| r.mode == TerminalType::Bash && r.key == KeySym::CtrlC);
        assert!(bash.clone().count() >= 1, "Bash must have a Ctrl-C row");
        for row in bash {
            assert_eq!(
                row.does,
                Effect::SigInt,
                "Bash Ctrl-C in state {:?} must be SIGINT",
                row.state
            );
        }
    }

    /// Rule 2: no key has two owners in the same mode and state — which is what
    /// "shadowing" is, spelled as a table constraint.
    ///
    /// The mode *differences* are covered by the fact that the triple includes
    /// the mode: `Ctrl-C` is allowed to mean two different things because it is
    /// two rows in two modes, and is not allowed to mean two things in one mode
    /// because that would be two rows with the same triple.
    #[test]
    fn no_key_has_two_owners_in_the_same_mode_and_state() {
        let mut seen: std::collections::HashMap<(TerminalType, KeySym, ChordState), (Owner, &str)> =
            std::collections::HashMap::new();
        for row in CHORD_TABLE {
            let k = (row.mode, row.key, row.state);
            if let Some(prev) = seen.insert(k, (row.owner, row.keys)) {
                assert_eq!(
                    prev,
                    (row.owner, row.keys),
                    "{:?} in {:?} is claimed twice: {:?} and {:?}",
                    row.mode,
                    row.state,
                    prev,
                    (row.owner, row.keys)
                );
            }
        }
    }

    /// Rule 2, the other half: every mode must say something about every key in
    /// the family the audit cares about. A missing cell is the failure mode a
    /// table exists to catch — the binding nobody thought about, which is the
    /// same thing as a binding nobody can find.
    #[test]
    fn every_mode_has_a_row_for_every_key_that_matters() {
        let keys = [
            KeySym::CtrlC,
            KeySym::CtrlQ,
            KeySym::CtrlS,
            KeySym::TargetAnswer,
            KeySym::TargetOutput,
            KeySym::TargetSelection,
            KeySym::TargetTranscript,
            KeySym::Help,
            KeySym::Esc,
            KeySym::Tab,
            KeySym::Enter,
            KeySym::ShiftEnter,
            KeySym::PageUp,
            KeySym::PageDown,
            KeySym::Home,
            KeySym::End,
            KeySym::AnyOther,
        ];
        for mode in TerminalType::ALL {
            for key in keys {
                let rows: Vec<_> = CHORD_TABLE
                    .iter()
                    .filter(|r| r.mode == mode && r.key == key)
                    .collect();
                assert!(
                    !rows.is_empty(),
                    "{} mode has no row for {:?}: an undocumented key is an unaudited key",
                    mode.label(),
                    key
                );
            }
        }
    }

    /// Rule 3: `Esc`'s branch is written down per mode, with the "was a
    /// selection live?" question explicit — and with the pending chord ahead of
    /// it, because looprs-pdl.13 put a new "most recent thing" in front of both.
    #[test]
    fn the_esc_branch_is_written_down_per_mode() {
        for mode in TerminalType::ALL {
            let armed = CHORD_TABLE.iter().find(|r| {
                r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::ChordArmed
            });
            let live = CHORD_TABLE.iter().find(|r| {
                r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::SelectionLive
            });
            let plain = CHORD_TABLE
                .iter()
                .find(|r| r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::Plain);
            assert!(
                armed.is_some_and(|r| r.does == Effect::CancelChord),
                "{}: Esc with a chord armed must cancel the chord",
                mode.label()
            );
            assert!(
                live.is_some_and(|r| r.does == Effect::ClearSelection),
                "{}: Esc with a live selection must clear the selection",
                mode.label()
            );
            assert!(
                plain.is_some_and(|r| r.does == Effect::CancelRun),
                "{}: Esc with nothing live must be the mode's cancel",
                mode.label()
            );
        }
        // Bash adds the fourth state: the child holds Esc too.
        let held = CHORD_TABLE
            .iter()
            .find(|r| r.mode == TerminalType::Bash && r.state == ChordState::ChildHolds);
        assert!(
            held.is_some_and(|r| r.owner == Owner::Child
                || r.owner == Owner::Shell
                || r.owner == Owner::App),
            "Bash must say what happens to keys while a full-screen child holds the screen"
        );
    }

    /// The table's own arithmetic: three modes, and every row names one of them.
    #[test]
    fn the_table_is_a_table() {
        for row in CHORD_TABLE {
            assert!(
                TerminalType::ALL.contains(&row.mode),
                "{:?} is not a mode",
                row.mode
            );
            assert!(!row.keys.is_empty() && !row.note.is_empty());
        }
        // The copy family is exactly the four targets plus help, and each is in
        // all three modes.
        for key in [
            KeySym::TargetAnswer,
            KeySym::TargetOutput,
            KeySym::TargetSelection,
            KeySym::TargetTranscript,
        ] {
            assert!(key.is_copy(), "{:?} must be a copy key", key);
            assert_eq!(
                CHORD_TABLE.iter().filter(|r| r.key == key).count(),
                3,
                "{:?} must be in all three modes",
                key
            );
        }
    }

    /// The flush invariant, stated as a test: monotonic. Note what the flusher's
    /// own contract is — a prose block is rendered when it *closes* (blank line or
    /// `done`), not on every newline — so these assertions are about the cursor,
    /// not about line counts.
    #[test]
    fn flush_is_monotonic_per_view() {
        let mut v = view(TerminalType::Pi);
        v.transcript.push_delta(MessageKind::Answer, "line one\n\n");
        let first = v.flush(60);
        assert!(!first.is_empty(), "a closed block must flush");

        assert!(
            v.flush(60).is_empty(),
            "a second drain with no new content must emit nothing"
        );

        v.transcript
            .push_delta(MessageKind::Answer, "second para\n\n");
        let second = v.flush(60);
        assert!(!second.is_empty(), "the new block must flush");

        // And nothing is re-emitted: two closed blocks in, then silence.
        assert!(v.flush(60).is_empty());
    }

    /// The stall this ticket is about: an open entry that is never sealed blocks the
    /// cursor forever — its text can never reach the scrollback, and the live
    /// preview spins over dead text. `seal()` is what unblocks it.
    #[test]
    fn an_unsealed_entry_stalls_the_flusher_and_seal_clears_it() {
        let mut v = view(TerminalType::Beeds);
        v.transcript
            .push_delta(MessageKind::Answer, "partial answer, no newline");
        assert!(
            v.flush(60).is_empty(),
            "an unterminated, undone entry is not flushed"
        );

        v.seal();
        let after = v.flush(60);
        assert!(
            after.iter().any(|l| !l.line.spans.is_empty()),
            "sealing must release the tail: {after:?}"
        );
        assert!(v.flush(60).is_empty(), "and only once");
    }

    /// The same stall from a card rather than from prose — and a card is where it
    /// bites hardest, because a running tool or compaction is *deliberately* not
    /// closed by whatever streams next (parallel tools). So `seal` closes them
    /// itself, as aborted: nothing is coming to report them, and a frozen spinner
    /// in a transcript whose process is gone is a lie that outlives its subject.
    #[test]
    fn sealing_closes_cards_left_running_so_the_transcript_keeps_flushing() {
        let mut v = view(TerminalType::Pi);
        v.transcript
            .start_tool("t1".into(), "bash".into(), "sleep 100".into());
        v.transcript.start_compaction("threshold".into());
        v.transcript
            .push_delta(MessageKind::Answer, "said after both\n");

        assert!(
            v.flush(60).is_empty(),
            "the open cards hold the cursor, as they should while the session lives"
        );

        v.seal();
        let out: String = v.flush(60).iter().map(|l| l.to_string()).collect();
        assert!(out.contains("said after both"), "{out:?}");
        assert!(
            out.contains("compaction aborted"),
            "the compaction row says how it ended: {out:?}"
        );
        assert!(
            out.contains("sleep 100"),
            "the tool row is not lost: {out:?}"
        );
        for frame in crate::utils::render::FRAMES {
            assert!(
                !out.contains(frame),
                "a dead session's card is still spinning ({frame}): {out:?}"
            );
        }
    }

    /// Two views never share a cursor: draining one cannot consume the other's
    /// lines, and one view streaming cannot advance the other's position.
    #[test]
    fn views_do_not_interfere() {
        let mut pi = view(TerminalType::Pi);
        let mut beads = view(TerminalType::Beeds);

        pi.transcript
            .push_delta(MessageKind::Answer, "pi says hi\n\n");
        beads
            .transcript
            .push_done(MessageKind::System, "working looprs-1".into());

        let pi_lines = pi.flush(60);
        let beads_lines = beads.flush(60);
        assert!(!pi_lines.is_empty(), "pi's closed block must appear");
        assert!(!beads_lines.is_empty(), "beads' system line must appear");
        assert!(pi.flush(60).is_empty());
        assert!(beads.flush(60).is_empty());

        // Pi keeps streaming; the beads view must not budge, and must not gain pi's text.
        pi.transcript.push_delta(MessageKind::Answer, "more pi\n\n");
        assert!(!pi.flush(60).is_empty());
        assert!(
            beads.flush(60).is_empty(),
            "the beads view consumed nothing it was not given"
        );
    }

    #[test]
    fn errors_are_recorded_for_the_status_row() {
        let mut v = view(TerminalType::Bash);
        v.push_error("shell exited (code 1)".into());
        assert_eq!(v.last_error.as_deref(), Some("shell exited (code 1)"));
    }

    /// **The whole keyboard table**: mode x liveness -> may the user type here?
    ///
    /// Enumerated rather than spot-checked, because this is the one place the
    /// "only Bash while the agents run" rule lives and a table is the only form
    /// that shows a missing cell. Read the Bash column as the reason the mode
    /// exists: the terminal is never taken away.
    #[test]
    fn the_keyboard_table_is_mode_times_liveness() {
        use SessionStatus::*;
        let table = [
            //  mode,            liveness,     accepts input
            (TerminalType::Bash, NotStarted, true),
            (TerminalType::Bash, Idle, true),
            (TerminalType::Bash, Running, true),
            (TerminalType::Bash, Aborting, true),
            (TerminalType::Bash, Dead, true),
            (TerminalType::Pi, NotStarted, true),
            (TerminalType::Pi, Idle, true),
            (TerminalType::Pi, Running, false),
            (TerminalType::Pi, Aborting, false),
            (TerminalType::Pi, Dead, true),
            (TerminalType::Beeds, NotStarted, true),
            (TerminalType::Beeds, Idle, true),
            (TerminalType::Beeds, Running, false),
            (TerminalType::Beeds, Aborting, false),
            (TerminalType::Beeds, Dead, true),
        ];

        assert_eq!(
            table.len(),
            TerminalType::ALL.len() * 5,
            "one row per mode per liveness state"
        );
        for (mode, status, want) in table {
            let mut v = view(mode);
            v.set_status(status, Instant::now());
            assert_eq!(v.accepts_input(), want, "{mode:?} while {status:?}");
        }
    }

    /// The scenario the rule was written for, end to end: the pi run and the beads
    /// pass are both going, the user is not locked out — they are in Bash.
    #[test]
    fn with_pi_and_beads_both_running_bash_is_still_there() {
        let now = Instant::now();
        let mut pi = view(TerminalType::Pi);
        let mut beads = view(TerminalType::Beeds);
        let mut bash = view(TerminalType::Bash);

        pi.set_status(SessionStatus::Running, now);
        beads.set_step(BeadStep::WorkTickets);
        beads.set_status(SessionStatus::Running, now);
        bash.set_status(SessionStatus::Running, now); // `make test` in the other pane

        assert!(!pi.accepts_input() && !beads.accepts_input());
        assert!(
            bash.accepts_input(),
            "the agentic modes are locked, but the shell never is"
        );
    }

    /// The beads step no longer takes the keyboard either way. It is render-only:
    /// the loop's `Idle` *is* "waiting for a human", so gating on the step as
    /// well was a second lock on the same door — and a second lock is a second
    /// thing that can be left engaged after the door opens.
    #[test]
    fn the_step_is_a_label_not_a_lock() {
        let mut beads = view(TerminalType::Beeds);

        beads.set_step(BeadStep::WorkTickets);
        assert_eq!(
            beads.step,
            Some(BeadStep::WorkTickets),
            "the row still gets the step it renders"
        );
        assert!(
            beads.accepts_input(),
            "a step with no busy liveness behind it is not a run"
        );

        beads.set_status(SessionStatus::Idle, Instant::now());
        beads.set_step(BeadStep::AwaitInput);
        assert!(beads.accepts_input(), "and the human's turn still opens it");
    }

    /// A dead session must not leave the input box hidden behind it — the case the
    /// old `if !status.is_alive() { awaiting_user = true }` existed for. It needs
    /// no special case now: `Dead` is not `is_busy()`, so the derived rule opens
    /// the box on its own. What death *does* still have to do is stop the preview.
    #[test]
    fn a_dead_view_gives_the_input_box_back() {
        let mut v = view(TerminalType::Pi);
        v.set_status(SessionStatus::Running, Instant::now());
        v.chat = ChatState::Chat;
        assert!(!v.accepts_input(), "mid-run the box is closed");

        v.set_status(SessionStatus::Dead, Instant::now());
        assert!(
            v.accepts_input(),
            "a dead session cannot answer, so ask the human"
        );
        assert_eq!(
            v.chat,
            ChatState::Stopped,
            "and the live preview stops with the child, not after it"
        );
    }

    /// The run clock is *this run*. Entering busy starts it, leaving clears it, and
    /// a repeated edge changes neither — otherwise a duplicate `Running` publish
    /// would silently restart the age the status row is showing.
    #[test]
    fn the_run_clock_winds_on_the_edge_not_on_the_value() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(30);
        let mut v = view(TerminalType::Pi);

        v.set_status(SessionStatus::Running, t0);
        assert_eq!(v.run_elapsed(later), Some(Duration::from_secs(30)));

        // Same state again, later: the age does not restart.
        v.set_status(SessionStatus::Running, later);
        assert_eq!(v.run_elapsed(later), Some(Duration::from_secs(30)));

        // Idle is not a run at all, and leaves no clock behind for the next one.
        v.set_status(SessionStatus::Idle, later);
        assert_eq!(v.run_elapsed(later), None);

        // And the next run starts from its own instant, not from t0.
        let t1 = later + Duration::from_secs(60);
        v.set_status(SessionStatus::Running, t1);
        assert_eq!(v.run_elapsed(t1), Some(Duration::ZERO));

        // Through Aborting (busy both sides) the clock is held, not reset.
        let t2 = t1 + Duration::from_secs(5);
        v.set_status(SessionStatus::Aborting, t2);
        assert_eq!(v.run_elapsed(t2), Some(Duration::from_secs(5)));
    }

    /// ADR-0002 "Consequences": buffered output while hidden must be capped, and
    /// the loss must be visible rather than silent.
    #[test]
    fn an_inactive_view_caps_its_buffer_and_says_so() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        let mut emitted: Vec<String> = Vec::new();
        for i in 0..8 {
            v.push_note(MessageKind::System, format!("line {i} {}", "x".repeat(24)));
            // Flushed every round, i.e. these are lines the terminal already has.
            emitted.extend(v.flush(60).iter().map(|l| l.to_string()));
        }
        assert!(
            v.transcript.byte_len() <= 128,
            "buffer must stay at or under the cap: {}",
            v.transcript.byte_len()
        );
        assert!(v.dropped_bytes() > 0, "the drop must be counted");
        assert!(
            emitted
                .iter()
                .any(|l| l.contains("bytes dropped") && l.contains(&v.dropped_bytes().to_string())),
            "and the notice must reach the scrollback, with the amount: {emitted:?}"
        );
    }

    /// The eviction moves the render cursor, and a cursor moved without being reset
    /// re-emits or garbles output — which in a real terminal is unrecoverable. So:
    /// after the cap kicks in, nothing already written comes back.
    #[test]
    fn eviction_never_re_emits_what_was_already_written() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        let mut seen: Vec<String> = Vec::new();
        for i in 0..24 {
            v.push_note(MessageKind::System, format!("unique line {i}"));
            seen.extend(v.flush(60).iter().map(|l| l.to_string()));
        }
        assert!(
            v.dropped_bytes() > 0,
            "this test is about eviction happening"
        );

        let remaining: Vec<String> = v
            .transcript
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect();
        let after: Vec<String> = v.flush(60).iter().map(|l| l.to_string()).collect();
        for line in &after {
            let t = line.trim();
            if t.is_empty() || t.contains("bytes dropped") {
                continue;
            }
            assert!(
                !seen.iter().any(|s| s.contains(t)),
                "re-emitted a line the terminal already printed: {line:?}"
            );
            assert!(
                remaining.iter().any(|e| e.contains(t)),
                "emitted a line the view does not hold: {line:?}"
            );
        }
    }

    /// Sealing drops a half-parsed escape sequence along with the open entry. The
    /// bytes a dangling `\x1b[` is holding belong to a stream that will never
    /// finish, and if they stay they get charged to whatever streams next.
    #[test]
    fn sealing_drops_a_dangling_escape_sequence_too() {
        let mut v = view(TerminalType::Bash);
        v.push_bash("first-line\u{1b}[", 60); // ends mid-escape-sequence
        v.seal();
        let first = v.flush(60);

        v.push_bash("second-line\n", 60); // the next stream, which must be unaffected
        v.seal();
        let second = v.flush(60);

        let joined: String = second.iter().map(|l| l.to_string()).collect();
        assert!(
            joined.contains("second-line"),
            "the next stream arrived intact: {joined:?}"
        );
        for line in first.iter().chain(second.iter()) {
            assert!(
                !line.to_string().contains('\u{1b}'),
                "a raw escape byte reached the scrollback: {line:?}"
            );
        }
    }

    /// Losing bytes is fine; losing the ability to flush at all is not.
    #[test]
    fn the_view_keeps_flushing_after_eviction() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        for i in 0..12 {
            v.push_note(
                MessageKind::System,
                format!("filler {i} {}", "y".repeat(20)),
            );
            let _ = v.flush(60);
        }
        assert!(v.dropped_bytes() > 0);
        v.push_note(MessageKind::System, "after the eviction".into());
        let out = v.flush(60);
        assert!(
            out.iter()
                .any(|l| l.to_string().contains("after the eviction")),
            "post-eviction content still reaches the terminal: {out:?}"
        );
        assert!(v.flush(60).is_empty(), "and only once");
    }

    // ─────────── the store behind the scrollback (looprs-pdl.6) ───────────

    /// **Provenance stays truthful across a trim.** Every row on the screen has
    /// to be re-renderable from the entry it names. If the byte trim cut
    /// entries out from under the store, rows would address the wrong entry —
    /// the scrollback showing text no entry can produce, which is the sort of
    /// corruption that only appears after a long session.
    #[test]
    fn eviction_leaves_no_row_the_transcript_cannot_re_render() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 256);
        for i in 0..40 {
            v.push_note(
                MessageKind::System,
                format!("line {i} of a long transcript that will be trimmed away"),
            );
            v.flush(60);
        }
        assert!(v.dropped_bytes() > 0, "the trim ran");
        let entries = &v.transcript.entries;
        assert!(
            entries.len() < 40,
            "some entries were dropped: {}",
            entries.len()
        );
        for row in v.scrollback().rows() {
            assert!(
                row.entry < entries.len(),
                "a row outlives the entry it names: {:?}",
                row.line
            );
            let text = row.to_string();
            let body = text.trim_start_matches(['•', '●', '◌', ' ']).trim();
            if body.is_empty() || body.contains("bytes dropped") {
                continue;
            }
            assert!(
                entries[row.entry].text.contains(body),
                "row {text:?} is not text the entry it names can re-render: {:?}",
                entries[row.entry].text
            );
        }
    }

    /// **A re-wrap makes the rows again.** The dangerous reading of "the store
    /// is keyed to a width" is that a resize appends the re-wrapped copy on top
    /// of the old one; the whole transcript would then be doubled.
    #[test]
    fn a_rewrap_makes_the_rows_again_rather_than_adding_to_them() {
        let mut v = view(TerminalType::Pi);
        v.push_note(
            MessageKind::Answer,
            "MARKER the answer is long enough to wrap in a narrow window and so takes several rows at forty columns but noticeably fewer at eighty columns".into(),
        );
        let wide_rows = v.flush(80);
        assert!(!wide_rows.is_empty(), "the answer rendered");
        let at_wide = v.scrollback().len();

        // A resize with nothing new to say: the store is made again at 40.
        assert!(
            v.flush(40).is_empty(),
            "nothing was finalised since the last flush"
        );
        let at_narrow = v.scrollback().len();
        assert_eq!(v.scrollback().width(), 40, "the store is wrapped for 40");
        assert!(
            at_narrow > at_wide,
            "a narrower window takes more rows: {at_wide} -> {at_narrow}"
        );

        let all: String = v
            .scrollback()
            .rows()
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(
            all.matches("MARKER").count(),
            1,
            "the re-wrap left a second copy behind: {all:?}"
        );
    }

    /// The store lags the tail exactly as far as the user scrolled, and no
    /// further: `pending` is the count of rows that arrived since, not the
    /// distance to the bottom.
    #[test]
    fn rows_that_arrive_off_the_tail_count_themselves_and_do_not_move_the_view() {
        let mut v = view(TerminalType::Pi);
        for i in 0..30 {
            v.push_note(MessageKind::System, format!("settled {i}"));
            v.flush(60);
        }
        let band = 10usize;
        v.scrollback_mut().scroll_by(-(band as isize), band);
        let held = v
            .scrollback()
            .window(band)
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>();
        v.push_note(MessageKind::System, "late".into());
        v.flush(60);
        assert_eq!(
            v.scrollback()
                .window(band)
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
            held,
            "the view held while the tail moved away"
        );
        assert_eq!(v.scrollback().pending(), 2, "the line and its separator");
        assert_eq!(
            v.scrollback().offset(),
            band + 2,
            "and the gap grew by that"
        );
    }
}
