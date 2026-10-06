//! The per-session scrollback: a [`Transcript`] welded to the [`Flusher`] that was
//! born with it (ADR-0002 Q5).
//!
//! The ADR rejects the single shared transcript. The reason is not aesthetics, it is
//! the `insert_before` invariant in `main.rs`: a `Flusher` is a *cursor into one
//! specific transcript*, and the moment two transcripts share one cursor (or one
//! transcript is rendered through two cursors) lines get dropped or duplicated in
//! the real terminal scrollback, which is the one place a bug is unrecoverable for
//! the user.
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
use crate::components::scrollback::Flusher;
use crate::session::ActiveBead;
use crate::session::{BeadStep, SessionStatus};
use crate::state::transcript::{Entry, MessageKind, Transcript};
use crate::utils::render::ControlStripper;

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
    /// Escape-sequence stripper for this view's Bash output (see [`Self::push_bash`]).
    /// Per-view because a sequence can straddle two reads.
    bash_strip: ControlStripper,
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
            status: SessionStatus::NotStarted,
            run_started: None,
            last_error: None,
            chat: ChatState::Stopped,
            step: None,
            active_bead: None,
            tokens: Tokens::default(),
            dropped: 0,
            limit,
            bash_strip: ControlStripper::default(),
        }
    }

    /// Shell output into the transcript: verbatim content, presentation stripped,
    /// **never** markdown and **never** re-wrapped (ADR-0001 rules 1 and 5).
    ///
    /// The strip is per-view because a colour sequence split across two reads must
    /// not come out as half-dropped, half-printed-garbage.
    pub fn push_bash(&mut self, chunk: &str) {
        let text = self.bash_strip.strip(chunk);
        if text.is_empty() {
            return;
        }
        self.transcript.push_delta(MessageKind::Bash, &text);
        self.enforce_buffer();
    }

    /// Lines that became final since the last call. Call once per frame, for the
    /// active view only, immediately before `insert_before` + `draw`.
    ///
    /// Invariant preserved here: monotonic. Every finalized line of this transcript
    /// is returned exactly once, ever.
    pub fn flush(&mut self, width: u16) -> Vec<Line<'static>> {
        self.flusher.drain(&self.transcript, width)
    }

    /// The not-yet-final tail, for the live preview region.
    pub fn preview(&self, width: u16) -> Vec<Line<'static>> {
        self.flusher.preview(&self.transcript, width)
    }

    /// Close whatever streamed entry is still open.
    ///
    /// MUST be called when the owning session dies ([`super::SessionEvent::Exited`])
    /// or is torn down. Without it the transcript keeps an entry that is `!done`
    /// forever: the flusher stalls on it, nothing that session already produced
    /// ever reaches the scrollback again, and the live preview shows a spinner on
    /// dead text forever. This is the seam that breaks the `insert_before`
    /// invariant, so it is part of the contract rather than a detail.
    pub fn seal(&mut self) {
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
        self.bash_strip.reset();
    }

    /// A finished, one-shot line (status notices, the mode-switch separator).
    pub fn push_note(&mut self, kind: MessageKind, text: String) {
        self.transcript.push_done(kind, text);
        self.enforce_buffer();
    }

    /// A streaming delta from this session's backend.
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        self.transcript.push_delta(kind, delta);
        self.enforce_buffer();
    }

    /// A session-level error: recorded for the status row and shown.
    pub fn push_error(&mut self, text: String) {
        self.last_error = Some(text.clone());
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
        if self.limit == 0 || self.transcript.byte_len() <= self.limit {
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
            },
        );
        self.flusher.reseat(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: TerminalType) -> SessionView {
        SessionView::new(SessionId::new(mode, 0))
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
            after.iter().any(|l| !l.spans.is_empty()),
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
        v.push_bash("first-line\u{1b}["); // ends mid-escape-sequence
        v.seal();
        let first = v.flush(60);

        v.push_bash("second-line\n"); // the next stream, which must be unaffected
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
}
