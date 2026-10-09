//! //! The decision layer, as tables (looprs-6ol).
//!
//! Nothing in this file spawns a process, reads a fake binary, or waits on a
//! channel. These are the beads step machine and the claim / cancel guards
//! enumerated as functions, which is what the process-level tests cannot do:
//! those show that the loop *can* reach a state, and a trajectory only ever
//! shows the one it took. A table says how many rows say yes, which is the
//! only way to notice an extra one.

use crate::services::bd::{Bead, BeadStatus};
use crate::session::BeadStep;

use crate::session::beads::guards::{PassGate, abort_is_redundant, stall_timer_is_live};
use crate::session::beads::machine::Pick;

use super::*;

// =========================================================================
// The decision layer, as tables (looprs-6ol).
//
// Nothing below this line spawns a process, reads a fake binary, or waits on a
// channel. These are the beads step machine and the claim / cancel guards
// enumerated as functions, which is the half the process-level tests above
// cannot do: those show that the loop *can* reach a state, and a trajectory
// only ever shows the one it took. A table says how many rows say yes, which is
// the only way to notice an extra one.
// =========================================================================

/// A loop with no fixtures. The bins point at `/bin/true` and nothing here
/// ever runs them: a test in this section that spawns a child has stopped
/// being a table test and should move back above.
fn bare_loop() -> (BeadsLoop, mpsc::UnboundedReceiver<SessionEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ctl_tx, _ctl_rx) = mpsc::unbounded_channel::<BeadsCmd>();
    let cfg = SessionConfig {
        output_budget: crate::bus::Budget::unbounded(),
        pi_bin: "/bin/true".into(),
        bd_bin: "/bin/true".into(),
        shell_bin: "/bin/true".into(),
        notifier: Arc::new(crate::services::notification::Noop),
        // Not the point of any test in this file; explicit rather than
        // defaulted-from-env so a `LOOPRS_MODES` in the developer's shell
        // cannot change what these table tests assert.
        alt_screen_hosted: false,
        // Same reason as the notifier above: no test in this file copies
        // anything, and a clipboard reached through a default would make
        // that a matter of luck.
        clipboard: Arc::new(crate::services::clipboard::Noop),
    };
    (
        BeadsLoop::new(SessionId::new(TerminalType::Beads, 0), tx, ctl_tx, cfg),
        rx,
    )
}

fn bead_status_row(id: &str, status: BeadStatus) -> Bead {
    bead_status(id, "a title, for the row that names it", status)
}

/// Everything this loop has announced on its event stream, concatenated.
fn spoken(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> String {
    let mut out = String::new();
    while let Ok(ev) = rx.try_recv() {
        if let SessionEvent::System(text) = ev {
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

/// **The step table.** Every cause the machine accepts maps to exactly one
/// step, and the step it maps to is published, not just stored — the UI renders
/// the event and never re-derives the step (looprs-msj), so a transition that
/// updated the field without emitting would freeze the screen on the old step.
#[test]
fn every_step_cause_lands_on_one_step_and_says_so() {
    let (mut l, mut rx) = bare_loop();
    assert_eq!(*l.get_step(), BeadStep::AwaitInput, "a new loop waits");

    let table = [
        (StepCause::Planning, BeadStep::CreateTickets, "step:plan"),
        (StepCause::Working, BeadStep::WorkTickets, "step:work"),
        (StepCause::Awaiting, BeadStep::AwaitInput, "step:await"),
    ];
    for (cause, step, published) in table {
        l.set_step(cause);
        assert_eq!(
            *l.get_step(),
            step,
            "set_step({cause:?}) stored the wrong step"
        );
        let ev = rx
            .try_recv()
            .expect("a transition must be published, not only remembered");
        assert_eq!(
            describe(&ev),
            published,
            "set_step({cause:?}) published something else"
        );
    }
}

/// **The cycle the ticket names**: `AwaitInput -> CreateTickets -> WorkTickets
/// -> AwaitInput`. Walked once end to end, from a real loop, in that order.
#[test]
fn the_machine_walks_await_plan_work_and_home_again() {
    let (mut l, _rx) = bare_loop();
    let lap = [StepCause::Planning, StepCause::Working, StepCause::Awaiting];
    assert!(l.is_awaiting_input(), "one lap starts at the human");
    for cause in lap {
        l.set_step(cause);
    }
    assert!(
        l.is_awaiting_input(),
        "three causes, one lap, back where a human is needed: got {:?}",
        l.get_step()
    );
}

/// **No orphan states, no unmapped causes, no two causes claiming one state.**
/// The mapping is a bijection between the causes and the steps, which is what
/// makes "what is the loop doing" answerable in one word with no ambiguity.
#[test]
fn the_step_table_is_a_bijection_over_the_whole_enum() {
    let produced: Vec<BeadStep> = StepCause::ALL.iter().map(|c| c.step()).collect();
    assert_eq!(
        produced.len(),
        StepCause::ALL.len(),
        "every cause must produce a step"
    );
    for step in [
        BeadStep::AwaitInput,
        BeadStep::CreateTickets,
        BeadStep::WorkTickets,
    ] {
        assert!(
            produced.contains(&step),
            "{step:?} can never be reached: no cause produces it"
        );
    }
    for (i, a) in StepCause::ALL.iter().enumerate() {
        for (j, b) in StepCause::ALL.iter().enumerate() {
            if i != j {
                assert_ne!(
                    a.step(),
                    b.step(),
                    "{a:?} and {b:?} both claim {:?}; the UI could not tell them apart",
                    a.step()
                );
            }
        }
    }
}

/// **One pass at a time, as sixteen rows.** The gate in front of
/// `BeadsLoop::next` is four bools, so the whole thing fits in one loop, and
/// the assertion that matters is a *count*: exactly one row opens it. "We
/// tested parked-and-streaming" is one trajectory; sixteen is a proof there is
/// no fifth combination that sneaks a second pass out.
#[test]
fn the_pass_gate_has_exactly_one_open_row_in_sixteen() {
    let no_yes = [false, true];
    let mut rows = 0usize;
    let mut open: Vec<PassGate> = Vec::new();
    for &started in &no_yes {
        for &parked in &no_yes {
            for &streaming in &no_yes {
                for &pending in &no_yes {
                    let g = PassGate {
                        started,
                        parked,
                        streaming,
                        pending,
                    };
                    rows += 1;
                    if g.allows() {
                        open.push(g);
                    }
                }
            }
        }
    }
    assert_eq!(
        rows, 16,
        "four flags means sixteen rows: the count is the test"
    );
    assert_eq!(open.len(), 1, "exactly one row may start a pass: {open:?}");
    assert_eq!(
        open[0],
        PassGate {
            started: true,
            parked: false,
            streaming: false,
            pending: true,
        },
        "the one open row is: entered, visible, idle, and asked"
    );
}

/// The same gate read one flag at a time, so a regression names the promise it
/// broke rather than just the boolean that flipped.
#[test]
fn each_flag_closes_the_pass_gate_by_itself() {
    let open = PassGate {
        started: true,
        parked: false,
        streaming: false,
        pending: true,
    };
    assert!(open.allows(), "the reference row opens");
    assert!(
        !PassGate {
            started: false,
            ..open
        }
        .allows(),
        "a mode nobody entered must not run"
    );
    assert!(
        !PassGate {
            parked: true,
            ..open
        }
        .allows(),
        "a hidden mode must not spend (DrainThenPark)"
    );
    assert!(
        !PassGate {
            streaming: true,
            ..open
        }
        .allows(),
        "never two passes at once"
    );
    assert!(
        !PassGate {
            pending: false,
            ..open
        }
        .allows(),
        "nothing was asked for"
    );
}

/// **The cancel guard, all four rows.** `Esc` while a cancel is unwinding is
/// absorbed; `Esc` after the stall has been reported is a fresh try.
#[test]
fn esc_is_absorbed_while_a_cancel_is_unwinding_and_only_while() {
    // (aborted, stall_reported, is the new Esc redundant?)
    let table = [
        (false, false, false, "idle: nothing to absorb"),
        (true, false, true, "unwinding: swallow the second Esc"),
        (
            true,
            true,
            false,
            "stalled and said: this Esc means 'again'",
        ),
        (
            false,
            true,
            false,
            "cannot be mid-stall without being aborted",
        ),
    ];
    for (aborted, reported, redundant, why) in table {
        assert_eq!(
            abort_is_redundant(aborted, reported),
            redundant,
            "abort_is_redundant({aborted}, {reported}) — {why}"
        );
    }
}

/// **The stall timer's rows.** A timer is only ever answered for the attempt
/// that armed it: the user pressing `Esc` twice means attempt 2 is live, and a
/// late attempt-1 timer killing that pass would be the harness throwing away
/// work the user just asked for a second chance on. Enumerated rather than
/// reasoned about, because every row here is a real interleaving.
#[test]
fn a_stall_timer_only_answers_for_the_attempt_that_armed_it() {
    let flags = [(false, false), (false, true), (true, false), (true, true)];
    let timers = [(1, 1), (1, 2), (2, 1), (2, 2), (3, 1)];
    let mut rows = 0usize;
    let mut live_rows: Vec<(bool, bool, u32, u32)> = Vec::new();
    for &(aborted, reported) in &flags {
        for &(armed, timer) in &timers {
            rows += 1;
            let live = stall_timer_is_live(aborted, reported, armed, timer);
            assert_eq!(
                live,
                aborted && !reported && armed == timer,
                "stall_timer_is_live({aborted}, {reported}, armed={armed}, timer={timer})"
            );
            if timer < armed {
                assert!(!live, "an older attempt's timer must never fire");
            }
            if live {
                live_rows.push((aborted, reported, armed, timer));
            }
        }
    }
    assert_eq!(rows, 20, "4 flag pairs x 5 timer pairs");
    assert_eq!(
        live_rows,
        vec![(true, false, 1, 1), (true, false, 2, 2)],
        "live means: aborted, not yet reported, and the timer carries the current attempt"
    );
}

/// **The claim guard, without `bd`.** Same three rules the process-level tests
/// check through a fake board, read straight off the function that decides:
/// skip what needs a human (out loud), refuse what this loop already worked,
/// and treat an unknown status as workable.
#[test]
fn the_claim_guard_skips_refuses_and_works_in_that_order_pure() {
    let (mut l, mut rx) = bare_loop();

    // Nothing on the board is nothing to work — not an error, not a pass.
    assert!(matches!(l.pick_bead(&[]), Pick::Nothing), "empty board");

    // A blocked ticket is walked past and *named*: the human has to be able to
    // see that the loop saw it.
    let board = vec![
        bead_status_row("looprs-blocked", BeadStatus::Blocked),
        bead_status_row("looprs-workable", BeadStatus::Open),
    ];
    let pick = l.pick_bead(&board);
    assert!(
        matches!(pick, Pick::Work(ref b) if b.id() == "looprs-workable"),
        "a blocked ticket must be skipped: {pick:?}"
    );
    let said = spoken(&mut rx);
    assert!(
        said.contains("looprs-blocked") && said.contains("need a human"),
        "the skip has to be said out loud: {said}"
    );

    // The workable one has already had a pass spent on it: refused, never
    // re-run, because `bd ready` hands the same bead back and every pass is
    // billed (the runaway looprs-w7q was filed for).
    l.worked.insert("looprs-workable".into());
    let pick = l.pick_bead(&board);
    assert!(
        matches!(pick, Pick::AlreadyWorked(ref b) if b.id() == "looprs-workable"),
        "a worked ticket must be refused, not re-bought: {pick:?}"
    );

    // A deferred ticket is the same story as a blocked one, and a ticket in a
    // status this build cannot name is *workable*: skipping what we cannot
    // classify would let a `bd` upgrade silently empty the board
    // (looprs-037's conflation, one level up).
    let deferred = vec![bead_status_row("looprs-defer", BeadStatus::Deferred)];
    assert!(
        matches!(l.pick_bead(&deferred), Pick::Nothing),
        "a deferred-only board has nothing for a worker"
    );
    let unknown = vec![bead_status_row("looprs-newstatus", BeadStatus::Unknown)];
    assert!(
        matches!(l.pick_bead(&unknown), Pick::Work(ref b) if b.id() == "looprs-newstatus"),
        "an unknown status is workable, never silently skipped"
    );
}
