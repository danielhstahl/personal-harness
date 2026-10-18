//! //! The notes themselves, pinned without a subprocess in the way.
//!
//! Wording is a contract too: the plan note lists what was planned and counts
//! what it did not print, the empty-plan note quotes the planner, and the
//! unverifiable note never says the board is empty.

use crate::services::bd::{Bead, BeadStatus};

use crate::session::beads::notes::{
    MAX_DEFERRED_LISTED, MAX_PLAN_LISTED, auto_resume_note, children_first_note, clip,
    held_elsewhere_note, no_plan_note, not_closed_note, plan_note, resume_already_worked_note,
    resume_note, unverifiable_note,
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

/// The resume sentence has to say three things in one line: which ticket, that it
/// was already in progress, and that it goes first. "Resuming" without the ticket id
/// is a status nobody can act on, and a resume that reads like a retry makes the
/// loop look like it had forgotten what it was doing.
#[test]
fn the_resume_note_names_the_ticket_and_says_why_it_is_first() {
    let note = resume_note(&bead_status(
        "looprs-mid",
        "half of it is done",
        BeadStatus::InProgress,
    ));
    assert!(note.contains("resuming"), "{note}");
    assert!(note.contains("looprs-mid"), "{note}");
    assert!(note.contains("in_progress"), "{note}");
    assert!(
        note.contains("bd ready"),
        "why ready could not show it: {note}"
    );
    assert!(
        !note.contains("retry") && !note.contains("again"),
        "a resume is not a retry: {note}"
    );
}

/// The skip note quotes `bd` because "somebody else has it" is a claim about the
/// board, and the board's own words are the evidence. And it must not read like a
/// failure: a live foreign claim is the system working.
#[test]
fn the_held_elsewhere_note_quotes_bd_and_calls_it_nothing_worse_than_it_is() {
    let note = held_elsewhere_note(
        &bead_status("looprs-mid", "taken", BeadStatus::InProgress),
        "issue already claimed by danielstahl",
    );
    assert!(note.contains("looprs-mid"), "{note}");
    assert!(
        note.contains("already claimed by danielstahl"),
        "`bd`'s own reason, not a paraphrase: {note}"
    );
    assert!(note.contains("leaves it alone"), "{note}");
    assert!(!note.contains("failed"), "{note}");
    let long = held_elsewhere_note(&bead("looprs-mid", "taken"), &"x".repeat(500));
    assert!(long.chars().count() < 400, "the reason is clipped: {long}");
}

/// The sentence a pass that did not close its ticket says (workspace-9th).
///
/// It has to promise a re-buy *and* a bound: "the loop is not stopping" without the
/// second half is what a runaway reads like from the transcript, and the bound is the
/// only reason the loop is allowed to keep going at all.
#[test]
fn the_not_closed_note_promises_a_re_buy_and_a_bound() {
    let claim = crate::session::ActiveBead {
        id: "looprs-mid".to_string(),
        title: "half done".to_string(),
    };
    let note = not_closed_note(&claim, BeadStatus::InProgress);
    assert!(note.contains("looprs-mid"), "{note}");
    assert!(
        note.contains("in_progress"),
        "`bd`'s own word for the state, not a paraphrase: {note}"
    );
    assert!(note.contains("not closed"), "{note}");
    assert!(note.contains("re-buy"), "the next pass is named: {note}");
    assert!(
        note.contains("guard") && note.contains("run out"),
        "and so is the thing that stops it: {note}"
    );
}

/// The guard at the resume boundary carries the same two exits as the one at the
/// ready boundary — close it, or speak — because a stop with no way out is just a
/// hang with a nicer message.
#[test]
fn the_resume_guard_note_names_both_ways_out() {
    let note = resume_already_worked_note(&bead_status(
        "looprs-mid",
        "paid for already",
        BeadStatus::InProgress,
    ));
    assert!(note.contains("looprs-mid"), "{note}");
    assert!(note.contains("not closed"), "{note}");
    assert!(note.contains("bd close looprs-mid"), "{note}");
    assert!(note.contains("new instruction"), "{note}");
}

/// The sentence a spent re-buy says (workspace-9th) has to carry its own number.
///
/// "Working it again" without a count is indistinguishable from the runaway the
/// re-work guard exists to stop, and the whole point of bounding the automatic
/// retries is that the transcript can say which one this is and when they run out.
#[test]
fn the_auto_resume_note_says_which_re_buy_this_is() {
    let note = auto_resume_note("looprs-mid", 1, 2);
    assert!(note.contains("looprs-mid"), "{note}");
    assert!(note.contains("1 of 2"), "the count is the point: {note}");
    assert!(
        note.contains("without closing"),
        "and why a second pass is being bought at all: {note}"
    );
    assert!(
        !note.contains("error"),
        "a repair the loop chose is not an error: {note}"
    );
    assert!(
        auto_resume_note("looprs-mid", 2, 2).contains("2 of 2"),
        "the last one is countable too"
    );
}

/// The children-before-parent note (looprs-lpw) has to make a deferral legible:
/// which parent was passed over, which child got worked instead, and how many
/// children are still holding that parent down. Said silently, the pass reads like
/// the loop picked the wrong ticket off the top of the ready list — which is exactly
/// what it looked like it did before this rule existed.
#[test]
fn the_children_first_note_names_the_parent_the_child_and_the_count() {
    let epic = bead("looprs-epic", "Epic: build the thing");
    let note = children_first_note(
        &bead_child("looprs-epic.2", "the second piece", "looprs-epic"),
        &[(epic.clone(), 2)],
    );
    assert!(note.contains("working looprs-epic.2"), "{note}");
    assert!(note.contains("looprs-epic ("), "and the parent: {note}");
    assert!(note.contains("Epic: build the thing"), "{note}");
    assert!(
        note.contains("2 child ticket(s) still ready"),
        "the number that says how long the epic waits: {note}"
    );
    assert!(
        note.contains("container") && note.contains("goes last"),
        "{note}"
    );
    assert!(!note.contains("error") && !note.contains("skip"), "{note}");
}

/// Bounded like every other list note: a board with forty open epics gets the count
/// and a readable head, not a dump.
#[test]
fn the_children_first_note_lists_up_to_the_cap_and_counts_the_rest() {
    let deferred: Vec<(Bead, usize)> = (0..MAX_DEFERRED_LISTED + 2)
        .map(|i| (bead(&format!("looprs-e{i}"), &format!("epic {i}")), i + 1))
        .collect();
    let note = children_first_note(&bead("looprs-x.1", "the work"), &deferred);
    assert!(note.contains("looprs-e0 ("), "{note}");
    assert!(
        !note.contains(&format!("looprs-e{MAX_DEFERRED_LISTED} (")),
        "past the cap only the count is shown: {note}"
    );
    assert!(note.contains("and 2 more"), "{note}");
    assert_eq!(note.lines().count(), MAX_DEFERRED_LISTED + 2, "{note}");
}
