//! //! The schedule as a table, not a trajectory.
//! //!
//! //! One row per combination of the inputs [`decide`](super::super::schedule::decide)
//! //! reads — behind the head or not, sweep due or not, probe quiet or not,
//! //! detector on or off — and the read each owes. This is the half the
//! //! process-level tests cannot do: they show that the poller *can* reach a
//! //! state, and a trajectory only ever shows the one it took. A table says how
//! //! many rows say what, which is the only way to notice an extra one.

use crate::services::bd;

use crate::services::bd::BdError;
use crate::services::board_poller::config::{CATCHUP_LIMIT, PROBE_LIMIT};
use crate::services::board_poller::schedule::{ReadReason, decide};

// ─────────────────── the decision table, with no subprocess ───────────────────

/// [`decide`] as a table. The task-level tests above prove it works; these
/// prove the three rules that make it safe, in the cases a running poller is
/// awkward to put in exactly the right state for.
#[test]
fn the_decision_table() {
    type Probe = Result<bd::JournalProbe, bd::JournalError>;
    let quiet: Probe = Ok(bd::JournalProbe {
        seqs: vec![],
        disabled: false,
    });
    let one_record: Probe = Ok(bd::JournalProbe {
        seqs: vec![7],
        disabled: false,
    });
    let filled_batch: Probe = Ok(bd::JournalProbe {
        seqs: vec![8, 9],
        disabled: false,
    });
    let disabled: Probe = Ok(bd::JournalProbe {
        seqs: vec![],
        disabled: true,
    });
    let pruned: Probe = Err(bd::JournalError::Truncated {
        since: 0,
        floor: 5,
        head: 99,
    });
    let broken: Probe = Err(bd::JournalError::Unusable(BdError::Timeout {
        bin: "bd".into(),
        args: "events tail".into(),
    }));

    // Rule 1: never read ⇒ always read, however quiet the journal is.
    let d = decide(quiet.clone(), CATCHUP_LIMIT, true, false);
    assert_eq!(d.read, Some(ReadReason::First), "{d:?}");
    assert!(d.adopt.is_none(), "nothing new to adopt: {d:?}");

    // Quiet, board already read, no sweep ⇒ skip. This is the saving.
    let d = decide(quiet.clone(), PROBE_LIMIT, false, false);
    assert_eq!(d.read, None, "{d:?}");
    assert!(d.at_head);

    // Quiet but the sweep is due ⇒ read, and the journal still owns nothing.
    let d = decide(quiet, PROBE_LIMIT, false, true);
    assert_eq!(d.read, Some(ReadReason::Sweep), "{d:?}");
    assert!(d.adopt.is_none());

    // A record ⇒ read, adopt exactly what the probe saw.
    let d = decide(one_record, CATCHUP_LIMIT, false, false);
    assert_eq!(d.read, Some(ReadReason::Journal), "{d:?}");
    assert_eq!(d.adopt, Some(7));
    assert!(d.at_head, "a short batch means the drain reached the head");

    // A batch that filled its limit ⇒ behind, and not at the head yet.
    let d = decide(filled_batch, 2, false, false);
    assert_eq!(d.read, Some(ReadReason::Journal));
    assert_eq!(d.adopt, Some(9));
    assert!(!d.at_head, "{d:?}");

    // Pruned ⇒ re-baseline at the head `bd` named, and report it.
    let d = decide(pruned, PROBE_LIMIT, false, false);
    assert_eq!(d.read, Some(ReadReason::Rebaselined), "{d:?}");
    assert_eq!(d.adopt, Some(99), "resume where the refusal pointed");
    assert!(d.at_head, "there is nothing behind the floor to drain");
    assert!(d.broken.is_some(), "and it is worth saying out loud");

    // Rule 2, the one that matters most: a probe that could not answer
    // reads the board, and adopts NOTHING. Moving the watermark here is the
    // lost change.
    let d = decide(broken, PROBE_LIMIT, false, false);
    assert_eq!(d.read, Some(ReadReason::ProbeFailed), "{d:?}");
    assert_eq!(
        d.adopt, None,
        "a failed probe must never move the watermark"
    );
    assert!(
        !d.at_head,
        "a probe that failed says nothing about the head"
    );
    assert!(d.broken.is_some());

    // A disabled journal answers quiet and says so; that is not a failure,
    // but it is not a reason to stop sweeping either.
    let d = decide(disabled, PROBE_LIMIT, false, true);
    assert_eq!(d.read, Some(ReadReason::Sweep), "{d:?}");
    assert!(d.disabled);
    assert!(d.broken.is_none());
}
