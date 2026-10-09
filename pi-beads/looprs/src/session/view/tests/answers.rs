//! //! The answers a view gives the frame about itself: the step label, the input
//! //! gate, the run clock.
//! //!
//! //! Small, cheap-to-read facts that the App derives its own state from — which is
//! //! exactly why they are pinned here rather than trusted: a step that behaved like
//! //! a lock, or a run clock that wound on the value rather than the edge, would
//! //! each be visible in the chrome as a lie about the session.

use crate::session::view::ChatState;
use crate::session::{BeadStep, SessionStatus, TerminalType};
use std::time::{Duration, Instant};

use super::*;

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
