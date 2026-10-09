//! //! The keyboard table's own rules — the tests that read `CHORD_TABLE` as data.
//! //!
//! //! Not one of these presses a key. They walk the rows and ask the questions the
//! //! dispatch cannot answer for itself: is `Ctrl-C` ever a copy, does any key have
//! //! two owners in the same mode and state, is every mode covered, is the `Esc`
//! //! branch written down rather than implied, and is the whole thing still a table
//! //! (mode × liveness, enumerated) rather than a match with cases missing.

use crate::session::view::chord::{ChordState, Effect, KeySym, Owner};
use crate::session::view::chord_table::CHORD_TABLE;
use crate::session::{BeadStep, SessionStatus, TerminalType};
use std::time::Instant;

use super::*;

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
        let armed = CHORD_TABLE
            .iter()
            .find(|r| r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::ChordArmed);
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

/// **The whole keyboard table**: mode x liveness -> may the user type here?
///
/// Enumerated rather than spot-checked, because this is the one place the
/// "only Bash while the agents run" rule lives and a table is the only form
/// that shows a missing cell. Read the Bash column as the reason the mode
/// exists: the terminal is never taken away.
#[test]
fn the_keyboard_table_is_mode_times_liveness() {
    use crate::session::SessionStatus::*;
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
