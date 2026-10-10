//! The wheel and the trackpad, turned into whole rows of transcript (looprs-pdl.8).
//!
//! A wheel notch and a trackpad flick arrive as the *same* two event kinds —
//! `ScrollUp` / `ScrollDown` — and they mean two different things. A notch is a
//! discrete unit of intended scrolling: one report, one step. A flick is a
//! **burst**: macOS emits one report per momentum tick, so one gesture arrives as
//! dozens of reports and keeps arriving after the finger leaves the glass. A
//! handler that treats the two alike is wrong in one of two directions, and the
//! ticket names both of them: one row per report turns a flick into forty rows of
//! surprise, and a multiplier big enough to tame the flick rounds a notch down to
//! one row and makes the wheel useless.
//!
//! # The rule: rate, not weight
//!
//! The report **count** in a burst carries no information about the intended
//! distance — it is a property of how the driver samples the momentum, and the
//! same flick sends a different count depending on how the glass was last
//! calibrated. What a hand *does* control is how **long** it keeps scrolling. So
//! distance is rate × duration:
//!
//! > **[`WHEEL_ROWS_PER_STEP`] rows, applied at most once every
//! > [`WHEEL_STEP_INTERVAL`].**
//!
//! That one rule makes both of the ticket's failure modes unreachable rather than
//! merely unlikely. An isolated report — a notch, a click of the wheel — is never
//! delayed and never rounded: it moves the full three rows. A flick moves the same
//! three rows per step at a bounded rate, so it lands where the *duration* of the
//! gesture says, and a 40-report flick cannot scroll 120 rows however fast the
//! reports come. The bounded rate is also what makes a momentum tail feel right
//! rather than broken: the text keeps travelling after the finger stops, at the
//! speed we chose instead of at the driver's.
//!
//! Reports that arrive inside the interval are **dropped, not queued**, and that
//! is the deliberate half. Queuing them would owe the user a scroll that lands
//! after the gesture is over, which is the one thing worse than a slightly short
//! scroll: the text moving when the hand has stopped. Dropping them costs
//! precision nobody had, because the count was never the intent.
//!
//! # The measurements behind the numbers
//!
//! Two constants, and each one is stated against what was measured rather than
//! against how it feels. From the spike (`spikes/mouse_clipboard_e2e.py`,
//! looprs-pdl.2 #4, table in `spikes/results/terminal-matrix.md`):
//!
//! * **The wire is not the limit** (measured, `burst`): 128 SGR reports in one
//!   1,536-byte write decode in **0.301 ms** — worst inter-event gap 0.037 ms,
//!   **~4.3×10⁵ reports/s**, nothing merged and nothing dropped at 10, 50 or 128
//!   reports. Whatever rate we choose, the transport will not argue.
//! * **The flick itself is NOT MEASURED** (pdl.2 #4b, verbatim: "no finger on
//!   this path"). The burst shape a real trackpad produces — how many reports,
//!   how they are spaced, how long the tail runs — is the one number this ticket
//!   was written against and nobody has taken it with a hand on the glass.
//!
//! So the numbers are set against the three things that *can* be stated, and they
//! are cited rather than asserted:
//!
//! 1. **A notch is 3 rows.** A whole number, small enough that three notches do
//!    not leave the page, large enough that a notch is legible as movement. It is
//!    the wheel convention (vim's wheel is three lines) and it is what
//!    [`WHEEL_ROWS_PER_STEP`] says. Nothing about it depends on the flick.
//! 2. **The ceiling on rate is what the flick has to fit under.**
//!    3 rows / 60 ms is **50 rows/s** — about two pages a second on a 24-row
//!    band. The measured burst ceiling is five orders of magnitude above that, so
//!    the throttle never runs out of headroom against the transport, and a
//!    hard-wired multiplier is never needed to rescue it.
//! 3. **The burst cadence we did measure** is the one driven through the running
//!    app in `spikes/mouse_scroll_e2e.py`, kept in
//!    `spikes/results/mouse-scroll-e2e.log`. On a real pty, with the app's own
//!    painted frame as the ruler:
//!
//!    * **one notch → 3 rows**, and 3 rows back toward the tail — the
//!      isolated report is neither delayed nor rounded;
//!    * **a timed burst of 40 reports over 299 ms → 15 rows**, which is five
//!      applied steps at **50.2 rows/s** against the **50 rows/s** the
//!      constants say — the rate holding in the wall clock, not just in a loop;
//!    * **a still mouse → 0 bytes** on the wire over two seconds, and 0 again
//!      after a gesture has drained, which is "no idle frame cost" read off
//!      the transport rather than off a flag.
//!
//!    The spike recomputes its bound from the duration it measured, so a change
//!    to the constants that is not a change to the numbers on the screen shows
//!    up as a failed check instead of as a surprise on a screen.
//!
//! And because the honest answer to "is a real flick like our synthetic one?"
//! is *nobody knows*, the type records every gesture it is given — report
//! count, duration, rows applied — and logs it at `debug` (see
//! [`WheelCadence::gesture`]). A real finger does not need a code change to
//! retune this: it needs one flick and one grep
//! (`RUST_LOG=debug`, then `grep 'wheel gesture closed' "$LOG"` — `$LOG` is the
//! resolved log file, `${LOOPRS_LOG_DIR:-$HOME/.local/state/looprs}/looprs.log`, see
//! `docs/guide/operator.md`), which is the gap pdl.2 #4b left open and the
//! reason the accounting exists at all. The level matters: the default run is
//! `info`, so a flick taken without raising it is a flick that was never recorded.
//!
//! # What is *not* decided here
//!
//! Ends. Whether an offset is legal, what unpinning is, what happens at the top of
//! the transcript and at the tail — all of that is [`Scrollback`]'s, reached
//! through [`crate::App::scroll_active`], which is the same door the PageUp /
//! PageDown keys go through. This module turns a pointer gesture into a row delta
//! and stops; a wheel-up that has nowhere to go moves nothing and is not an error.
//!
//! Neither is the *band* decided here: whether the cursor is over the transcript
//! at all is [`crate::state::selection::BandSnapshot`]'s answer, and a report
//! over the input box or the status row never reaches this type.

use std::time::{Duration, Instant};

/// Rows one applied step moves the view.
///
/// Three, whole, in the direction of the gesture. It is not derived from the
/// burst shape (nothing about a burst says "three") — it is the wheel's own
/// convention, kept whole so that a notch is never rounded down to one row,
/// which is the failure the ticket calls out as the bad half. The burst is
/// handled by the interval, not by shrinking or inflating this.
pub const WHEEL_ROWS_PER_STEP: isize = 3;

/// The minimum wall-clock spacing between two applied steps.
///
/// 60 ms, which is three frames at the run loop's ~60 fps: every applied step
/// lands on a repaint, and no step waits for a frame that has already been
/// drawn. Together with [`WHEEL_ROWS_PER_STEP`] it is the whole rate limit —
/// **50 rows a second**, two pages of a 24-row band — and it is chosen so that:
///
/// * a wheel notch, which no hand delivers faster than one per ~100 ms, is
///   **never** throttled into a merged step; and
/// * a trackpad burst, however dense, cannot exceed 50 rows/s no matter what
///   the driver decides to emit.
///
/// It is deliberately *not* tied to the frame period: on a slower machine the
/// steps just land on later frames, and the rate the user sees is unchanged.
pub const WHEEL_STEP_INTERVAL: Duration = Duration::from_millis(60);

/// A gap this long between two reports ends the running gesture.
///
/// Only the gesture *record* is affected; the throttle keeps its own clock,
/// because the record exists to answer "what did that flick look like" and the
/// answer must stop growing when the flick did. 250 ms is longer than the
/// longest pause inside a gesture we can state (the tail of a hard flick is a
/// continuous stream, not a pause) and shorter than the wait between two
/// deliberate flicks.
pub const WHEEL_GESTURE_GAP: Duration = Duration::from_millis(250);

/// Which way the wheel went, in the store's terms rather than the pointer's.
///
/// `Up` is **toward the past** — the same direction `PageUp` and a negative
/// [`Scrollback::scroll_by`] delta go. The mapping is spelled once, here, so
/// the sign cannot be re-decided at the call site and get it wrong quietly.
///
/// [`Scrollback`]: crate::state::scrollback::Scrollback
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelDir {
    /// Wheel rolled toward the past: rows come off the tail, the view travels up.
    Up,
    /// Wheel rolled toward the tail: the view travels down.
    Down,
}

impl WheelDir {
    /// The row delta this direction asks for, in [`Scrollback::scroll_by`]'s
    /// sign convention (positive toward the tail).
    pub fn rows(self) -> isize {
        match self {
            WheelDir::Up => -WHEEL_ROWS_PER_STEP,
            WheelDir::Down => WHEEL_ROWS_PER_STEP,
        }
    }
}

/// One gesture's shape, as the type saw it.
///
/// The record looprs-pdl.2 #4b never got: how many reports a gesture was, how
/// long it ran, how many rows it moved. Read it from a real finger off
/// `looprs.log` and the constants above can be retuned against measurement
/// instead of against the synthetic flick in
/// `spikes/mouse_scroll_e2e.py`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gesture {
    /// Reports received since the gesture started, including throttled ones.
    pub reports: u32,
    /// Wall clock between the first report and the last.
    pub duration: Duration,
    /// Rows actually applied — what the store was asked for, before it clamped.
    pub rows: isize,
}

impl Gesture {
    /// Reports per second over the gesture: the burst density.
    pub fn reports_per_sec(&self) -> f64 {
        if self.duration.is_zero() {
            return self.reports as f64;
        }
        self.reports as f64 / self.duration.as_secs_f64()
    }

    /// Rows per report: how much of the burst became movement. `WHEEL_ROWS_PER_STEP`
    /// for a notch, and well under one for a long burst, which is the throttle
    /// doing its job.
    pub fn rows_per_report(&self) -> f64 {
        if self.reports == 0 {
            return 0.0;
        }
        self.rows as f64 / self.reports as f64
    }
}

/// The wheel's clock: what it will accept, and what the last gesture looked like.
///
/// Stateful on purpose, and stateful about *time* rather than about the view: the
/// throttle's whole job is to compare this report's arrival against the last one,
/// which is a fact no frame carries. The clock arrives as an argument
/// ([`Self::step`]'s `now`) rather than being read here, which is what lets the
/// tests drive a whole burst without sleeping — the same seam
/// [`Selection::auto_scroll`](crate::state::selection::Selection::auto_scroll)
/// uses for the same reason.
#[derive(Clone, Debug, Default)]
pub struct WheelCadence {
    /// When the last step was *applied*. `None` until the first one.
    last_step: Option<Instant>,
    /// The running gesture, and when its first and last reports landed.
    started: Option<Instant>,
    last_report: Option<Instant>,
    reports: u32,
    applied: isize,
    /// The last gesture that closed, so a caller can read the shape of the flick
    /// that just ended without having been present for it.
    last: Option<Gesture>,
}

impl WheelCadence {
    /// Turn one report into the row delta to apply **now**.
    ///
    /// `0` means "this report changes nothing": it arrived inside
    /// [`WHEEL_STEP_INTERVAL`] of the step before it, which is the burst being
    /// throttled and is the normal case for a trackpad, not a rejection.
    ///
    /// The gesture record is updated on every report, throttled or not, so the
    /// count is the count the wire delivered rather than the count the app
    /// honoured — which is the distinction the measurement needs.
    pub fn step(&mut self, dir: WheelDir, now: Instant) -> isize {
        self.close_if_stale(now);
        // The gesture opens on its first report and is extended by the rest; the
        // `get_or_insert` is the whole of "if no record is open, open one".
        self.started.get_or_insert(now);
        self.last_report = Some(now);
        self.reports += 1;

        let rows = match self.last_step {
            Some(at) if now.saturating_duration_since(at) < WHEEL_STEP_INTERVAL => 0,
            _ => {
                self.last_step = Some(now);
                dir.rows()
            }
        };
        self.applied += rows;
        rows
    }

    /// The gesture in flight, if one is.
    #[allow(dead_code)] // test seam: the burst-shape tests read the record through this rather than by sleeping on a real gesture; the log line in `close_if_stale` is the production half
    pub fn gesture(&self) -> Option<Gesture> {
        let started = self.started?;
        let last = self.last_report.unwrap_or(started);
        Some(Gesture {
            reports: self.reports,
            duration: last.saturating_duration_since(started),
            rows: self.applied,
        })
    }

    /// The most recently closed gesture, if any has closed.
    #[allow(dead_code)] // reached in this crate through `App::wheel_gesture`, which is itself a diagnostic seam; see the note there
    pub fn last_gesture(&self) -> Option<Gesture> {
        self.last
    }

    /// Forget the throttle, not the record of gestures.
    ///
    /// Called when the gesture cannot be continued — a mode switch, a full-screen
    /// child taking the screen — where "the same gesture" has no meaning but the
    /// measurement of the one that ended does.
    pub fn reset_throttle(&mut self) {
        self.last_step = None;
    }

    /// Close the running gesture if the gap since its last report says it is over.
    fn close_if_stale(&mut self, now: Instant) {
        let (Some(started), Some(last)) = (self.started, self.last_report) else {
            return;
        };
        if now.saturating_duration_since(last) < WHEEL_GESTURE_GAP {
            return;
        }
        let g = Gesture {
            reports: self.reports,
            duration: last.saturating_duration_since(started),
            rows: self.applied,
        };
        tracing::debug!(
            reports = g.reports,
            reports_per_sec = %format!("{:.1}", g.reports_per_sec()),
            duration_ms = g.duration.as_millis(),
            rows = g.rows,
            rows_per_report = %format!("{:.2}", g.rows_per_report()),
            "wheel gesture closed: the shape of the last gesture, as the wire delivered it \
             (looprs-pdl.2 #4b is still unmeasured with a real finger; this line is how to \
             measure one)"
        );
        self.last = Some(g);
        self.started = None;
        self.last_report = None;
        self.reports = 0;
        self.applied = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// **A notch is a notch.** One isolated report, in from nowhere, moves the
    /// full three rows and is not delayed by anything: the wheel has to work at
    /// the speed the wheel works at.
    #[test]
    fn one_isolated_report_is_a_whole_step() {
        let t = t0();
        let mut w = WheelCadence::default();
        assert_eq!(w.step(WheelDir::Up, t), -WHEEL_ROWS_PER_STEP);
        assert_eq!(WHEEL_ROWS_PER_STEP, 3, "and three rows, whole");

        // A report long after the last is a fresh notch, not a continuation.
        let mut w = WheelCadence::default();
        w.step(WheelDir::Up, t);
        assert_eq!(
            w.step(WheelDir::Down, t + Duration::from_millis(400)),
            WHEEL_ROWS_PER_STEP,
            "…including in the other direction"
        );
    }

    /// The sign is the store's, once: `Up` is toward the past, which is
    /// negative in [`Scrollback::scroll_by`]'s convention.
    #[test]
    fn up_is_toward_the_past() {
        assert!(WheelDir::Up.rows() < 0);
        assert!(WheelDir::Down.rows() > 0);
        assert_eq!(WheelDir::Up.rows(), -WheelDir::Down.rows());
    }

    /// **The flick is bounded.** A burst of reports cannot scroll more than the
    /// rate allows, however the reports are spaced. This is the whole reason the
    /// interval exists: forty reports must not become forty rows.
    #[test]
    fn a_burst_cannot_outrun_the_rate() {
        let t = t0();
        let mut w = WheelCadence::default();
        let burst = 40usize;
        let spacing = Duration::from_millis(5);
        let mut rows = 0isize;
        for i in 0..burst {
            rows += w.step(WheelDir::Up, t + spacing * i as u32);
        }
        let elapsed = spacing * (burst as u32 - 1);
        // Over `elapsed` the interval admits one step per whole interval plus the
        // one that opened the burst. That count, times the step, is the ceiling —
        // and the burst landing exactly on it is what makes the bound a rate
        // rather than a guess.
        let steps = self_steps(elapsed);
        assert_eq!(
            rows,
            -(steps * WHEEL_ROWS_PER_STEP),
            "one flick of 40 reports at 5 ms: {rows} rows in {elapsed:?}"
        );
        assert!(
            rows.abs() < burst as isize,
            "a one-row-per-report handler would have moved {burst} rows here"
        );
    }

    /// The number of steps `WHEEL_STEP_INTERVAL` admits over `elapsed`, counting
    /// the one that opens the burst.
    fn self_steps(elapsed: Duration) -> isize {
        (elapsed.as_millis() / WHEEL_STEP_INTERVAL.as_millis()) as isize + 1
    }

    /// Reports inside the interval are **dropped, not queued**: nothing lands
    /// late, and the very next one that is out of the interval applies in full.
    #[test]
    fn a_throttled_report_leaves_nothing_behind() {
        let t = t0();
        let mut w = WheelCadence::default();
        assert_eq!(w.step(WheelDir::Up, t), -3);
        for i in 1..10 {
            assert_eq!(
                w.step(WheelDir::Up, t + Duration::from_millis(i)),
                0,
                "inside the interval: nothing applied, nothing owed"
            );
        }
        assert_eq!(
            w.step(WheelDir::Up, t + WHEEL_STEP_INTERVAL),
            -3,
            "out of the interval again: one full step, and only one"
        );
    }

    /// The gesture record: the count is what the wire sent, the rows are what
    /// the throttle applied, and the two are different on purpose — that
    /// difference *is* the measurement of the burst.
    #[test]
    fn a_gesture_records_its_own_shape() {
        let t = t0();
        let mut w = WheelCadence::default();
        for i in 0..13 {
            w.step(WheelDir::Up, t + Duration::from_millis(i * 5));
        }
        let g = w.gesture().expect("a report opened a gesture");
        assert_eq!(g.reports, 13, "every report is counted, throttled or not");
        assert_eq!(g.duration, Duration::from_millis(60));
        assert_eq!(g.rows, -6, "and only the un-throttled ones moved");
        assert!(g.rows_per_report().abs() < 0.5, "{g:?}");
    }

    /// A gap longer than [`WHEEL_GESTURE_GAP`] closes the record, so the next
    /// flick is measured as itself and not as 1.5 flicks.
    #[test]
    fn a_gap_closes_the_gesture_and_the_next_one_is_measured_alone() {
        let t = t0();
        let mut w = WheelCadence::default();
        for i in 0..5 {
            w.step(WheelDir::Up, t + Duration::from_millis(i * 5));
        }
        let next = t + Duration::from_millis(5 * 4) + WHEEL_GESTURE_GAP;
        assert_eq!(
            w.step(WheelDir::Down, next),
            WHEEL_ROWS_PER_STEP,
            "and the wheel itself was never gated by the gesture gap"
        );
        let closed = w.last_gesture().expect("the gap closed the first one");
        assert_eq!(closed.reports, 5);
        assert_eq!(closed.rows, -3);
        let fresh = w.gesture().expect("the second report opened a new one");
        assert_eq!(fresh.reports, 1);
        assert_eq!(fresh.rows, WHEEL_ROWS_PER_STEP);
    }

    /// **The property the constants are supposed to have**, over every spacing we
    /// can name: no input pattern moves more rows per second than the rate, and
    /// no isolated report moves less than the full step.
    #[test]
    fn no_spacing_pattern_breaches_the_rate_or_starves_a_notch() {
        for spacing_ms in [0u64, 1, 5, 10, 25, 59, 60, 61, 120, 500] {
            let t = t0();
            let mut w = WheelCadence::default();
            let mut rows = 0isize;
            let n: usize = 60;
            for i in 0..n {
                rows += w.step(
                    WheelDir::Up,
                    t + Duration::from_millis(spacing_ms * i as u64),
                );
            }
            let elapsed = Duration::from_millis(spacing_ms * (n as u64 - 1));
            let ceiling = self_steps(elapsed) * WHEEL_ROWS_PER_STEP;
            assert!(
                rows.unsigned_abs() <= ceiling.unsigned_abs(),
                "spacing {spacing_ms}ms: {} rows over {elapsed:?}, ceiling {ceiling}",
                rows.unsigned_abs()
            );
            if spacing_ms >= WHEEL_STEP_INTERVAL.as_millis() as u64 {
                assert_eq!(
                    rows.unsigned_abs(),
                    WHEEL_ROWS_PER_STEP.unsigned_abs() * n,
                    "at notch spacing nothing may be throttled away: {rows}"
                );
            }
        }
    }

    /// The throttle can be handed back without losing the measurement: a mode
    /// switch ends the gesture without pretending it never happened.
    #[test]
    fn resetting_the_throttle_keeps_the_record() {
        let t = t0();
        let mut w = WheelCadence::default();
        w.step(WheelDir::Up, t);
        assert_eq!(w.step(WheelDir::Up, t + Duration::from_millis(10)), 0);
        w.reset_throttle();
        assert_eq!(
            w.step(WheelDir::Up, t + Duration::from_millis(20)),
            -3,
            "a fresh gesture can move immediately"
        );
        assert!(w.gesture().is_some(), "and the record is still being kept");
    }

    #[test]
    fn nothing_seen_is_no_gesture() {
        let w = WheelCadence::default();
        assert!(w.gesture().is_none());
        assert!(w.last_gesture().is_none());
    }
}
