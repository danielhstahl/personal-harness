//! //! The notes themselves, pinned without a subprocess in the way.
//!
//! Wording is a contract too: the plan note lists what was planned and counts
//! what it did not print, the empty-plan note quotes the planner, and the
//! unverifiable note never says the board is empty.

use crate::services::bd::Bead;

use crate::session::beads::notes::{
    MAX_PLAN_LISTED, clip, no_plan_note, plan_note, unverifiable_note,
};

use super::*;

// ---------------- the notes themselves, pinned without a subprocess in the way ---------

#[test]
fn the_plan_note_lists_every_ticket_up_to_the_cap_and_counts_the_rest() {
    let tickets: Vec<Bead> = (0..MAX_PLAN_LISTED + 3)
        .map(|i| bead(&format!("looprs-{i:03}"), &format!("ticket {i}")))
        .collect();
    let note = plan_note(&tickets);
    assert!(
        note.contains(&format!(
            "planner created {} ticket(s)",
            MAX_PLAN_LISTED + 3
        )),
        "{note}"
    );
    assert!(note.contains("looprs-000: ticket 0"), "{note}");
    assert!(note.contains("looprs-019: ticket 19"), "{note}");
    assert!(
        !note.contains("looprs-020"),
        "past the cap only the count is shown: {note}"
    );
    assert!(note.contains("and 3 more"), "{note}");
    assert_eq!(note.lines().count(), MAX_PLAN_LISTED + 2, "{note}");
}

#[test]
fn the_empty_plan_note_quotes_the_planner_s_own_words() {
    let note = no_plan_note("I could not parse that request.");
    assert!(note.contains("created no tickets"), "{note}");
    assert!(note.contains("I could not parse that request."), "{note}");
    let quiet = no_plan_note("   ");
    assert!(quiet.contains("left no message"), "{quiet}");
}

#[test]
fn the_unverifiable_note_never_says_the_board_is_empty() {
    let note = unverifiable_note("reading the board after the planner ran", "bd exited 3");
    assert!(note.contains("cannot verify"), "{note}");
    assert!(note.contains("bd exited 3"), "{note}");
    assert!(
        !note.contains("created no tickets"),
        "the two verdicts must never read alike: {note}"
    );
}

#[test]
fn clipping_a_long_quote_never_cuts_a_multibyte_char() {
    let long = "é".repeat(200); // two bytes each
    let clipped = clip(&long, 10);
    assert_eq!(clipped.chars().count(), 11, "10 kept + the ellipsis");
    assert!(std::str::from_utf8(clipped.as_bytes()).is_ok());
    assert_eq!(clip("short", 100), "short");
}
