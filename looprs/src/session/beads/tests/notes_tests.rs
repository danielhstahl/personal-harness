//! //! The notes themselves, pinned without a subprocess in the way.
//!
//! Wording is a contract too: the plan note lists what was planned and counts
//! what it did not print, the empty-plan note quotes the planner, and the
//! unverifiable note never says the board is empty.

use crate::services::bd::{Bead, BeadStatus};
use crate::services::bd_tools::{DEFAULT_WORKER_TOOLS, Source, Surface};
use crate::services::prompts::worker;
use crate::session::ActiveBead;

use crate::session::beads::notes::{
    Arrival, MAX_PLAN_LISTED, clip, held_elsewhere_note, no_plan_note, plan_note,
    resume_already_worked_note, resume_note, unverifiable_note, worker_prompt,
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

// ---------------- the worker prompt, and the tool surface it has to match ------------

/// The prompt is built from the same [`Surface`] the spawn argv is, so a verb
/// appears in the prose exactly when the pass can call it. Both directions are
/// asserted, because both have their own failure: promise a tool the child lacks and
/// the worker reports a verb that fails; hide one it has and it shells out for it
/// anyway.
#[test]
fn the_prompt_names_the_verbs_the_pass_holds() {
    let claim = ActiveBead {
        id: "looprs-26r".into(),
        title: "Beads loop never self-starts".into(),
    };

    let typed = worker_prompt(
        &claim,
        Arrival::Fresh,
        &Surface::new(DEFAULT_WORKER_TOOLS, Source::Embedded),
    );
    for tool in ["bd_show", "bd_close", "bd_remember"] {
        assert!(typed.contains(tool), "the pass has {tool}: {typed}");
    }
    assert!(
        !typed.contains("bd show <id>") && !typed.contains("bd close <id>"),
        "the CLI forms have no business in a prompt whose pass holds the tools: {typed}"
    );
    assert!(
        !typed.contains("bd_ready") && !typed.contains("bd_claim"),
        "work discovery is the loop's job; the prompt must not advertise it back: {typed}"
    );

    let shell = worker_prompt(&claim, Arrival::Fresh, &Surface::off());
    assert!(
        shell.contains("bd show <id>") && shell.contains("bd close <id>"),
        "{shell}"
    );
    assert!(
        !shell.contains("bd_close"),
        "the off surface is the prompt this loop shipped before, not a hybrid: {shell}"
    );
}

/// A resumed pass gets the same rule, because it gets the same tools: the order to
/// *look before writing* has to name a door the worker is holding.
#[test]
fn a_resumed_pass_is_told_to_read_the_ticket_in_the_shape_it_can() {
    let claim = ActiveBead {
        id: "looprs-mid".into(),
        title: "the ticket that was mid-flight".into(),
    };
    let typed = worker_prompt(
        &claim,
        Arrival::Resumed,
        &Surface::new("read,bash,bd_show", Source::Embedded),
    );
    assert!(typed.contains("`bd_show` with id looprs-mid"), "{typed}");
    let shell = worker_prompt(&claim, Arrival::Resumed, &Surface::off());
    assert!(shell.contains("`bd show looprs-mid`"), "{shell}");
}

/// A narrower allowlist than the default is a coherent pass, not one that lies to
/// the model: what is left out of `--tools` is left out of the prose too, and the
/// bullet that loses its tool falls back to the CLI rather than vanishing.
#[test]
fn a_narrow_surface_promises_narrowly() {
    let prompt = worker(&Surface::new("read,bash,bd_close", Source::Embedded));
    assert!(prompt.contains("bd_close"), "{prompt}");
    assert!(!prompt.contains("bd_show"), "{prompt}");
    assert!(!prompt.contains("bd_remember"), "{prompt}");
    assert!(prompt.contains("`bd remember`"), "{prompt}");
}
