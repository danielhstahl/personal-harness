//! The sentences the beads loop can say.
//!
//! The worker's prompt, and every note the loop puts on the transcript about a
//! plan, a claim, a refusal or a pass that needs a human. These are wording, not
//! policy — the policy is the verdict each one is attached to, in
//! [`machine`](super::machine) — which is why they are functions of the facts
//! they quote and can be pinned without a subprocess in the way (see
//! `notes_tests` in [`tests`](super::tests)).

use crate::services::bd::{Bead, BeadStatus};

use crate::services::prompts::{WORKER, generate_prompt};
use crate::session::ActiveBead;

/// The worker prompt: the standing instructions, plus the one ticket this worker
/// owns and the fact that the harness has already claimed it.
///
/// The claim is stated rather than assumed, because the alternative — telling the
/// agent to go find work — is a race with every other hand on the board, and the
/// worker would end up doing (and billing) a different ticket than the one the
/// loop reports, guards and verifies. Hence `bd ready` is gone from the worker's
/// command list in `prompts::WORKER`: a prompt that advertises it invites the
/// agent to shop.
pub(super) fn worker_prompt(claim: &ActiveBead) -> String {
    generate_prompt(
        WORKER,
        &format!(
            "Your assigned ticket: {id} — {title}\n\n\
             The harness has already run `bd update {id} --claim`, so this ticket is yours. \
             Work it and close it when the work is done. Do not look for other work and do not \
             pick a different ticket: {id} is the one being reported, guarded and paid for.\n",
            id = claim.id,
            title = claim.title,
        ),
    )
}

/// How many of a plan's tickets get listed before the note truncates.
///
/// Bounded because the transcript is the human's screen, not a dump: past a couple
/// of dozen lines the list is no longer readable anyway, and the count is the
/// number they actually wanted.
pub(super) const MAX_PLAN_LISTED: usize = 20;

/// How many skipped tickets get listed before the note truncates.
pub(super) const MAX_SKIPPED_LISTED: usize = 5;

/// "Here is the plan, before anybody spends money on it."
pub(super) fn plan_note(tickets: &[Bead]) -> String {
    let mut out = format!("beads: planner created {} ticket(s):", tickets.len());
    for t in tickets.iter().take(MAX_PLAN_LISTED) {
        out.push_str(&format!("\n  {}: {}", t.id(), t.title()));
    }
    if let Some(rest) = tickets.len().checked_sub(MAX_PLAN_LISTED) {
        out.push_str(&format!("\n  … and {rest} more"));
    }
    out
}

/// The planner settled with nothing on the board to show for it.
///
/// The planner's own last words are quoted rather than paraphrased: the model
/// usually states its reason ("I need more information about X", "already
/// covered by looprs-1"), and that sentence is the thing the user needs in order
/// to re-instruct it. Without it, "no tickets" is a dead end.
pub(super) fn no_plan_note(said: &str) -> String {
    let said = clip(said, 400);
    let reason = if said.is_empty() {
        "The planner left no message to explain itself.".to_string()
    } else {
        format!("The planner said: “{said}”")
    };
    format!(
        "beads: the planner finished and created no tickets — nothing was queued, and no workers were started. {reason}"
    )
}

/// The plan could not be checked, which is not the same sentence as "the plan is
/// empty" and must never be shortened into it.
pub(super) fn unverifiable_note(stage: &str, reason: &str) -> String {
    format!(
        "beads: cannot verify the plan ({stage}): {}. Nothing was queued and no workers started — this is a board read failure, not an empty plan.",
        clip(reason, 300)
    )
}

/// "Worked it, did not close it, and I am not paying for that a second time."
///
/// Names both exits, because the point of stopping the loop is that a human now
/// decides — and a stop that does not say what would unblock it just moves the
/// confusion from the transcript to the terminal.
pub(super) fn not_closed_note(claim: &ActiveBead, status: BeadStatus) -> String {
    format!(
        "beads: worked {} but `bd` says it is `{}`, not closed. The loop is stopped: `bd ready` hands this same ticket back, so another automatic pass would be a retry nobody asked for. Close it (`bd close {}`) or type a new instruction to let this loop work it again.",
        claim.id, status, claim.id
    )
}

/// A worker pass that could not be checked is not a worker pass that succeeded, and
/// the sentence must not be able to collapse into either of its neighbours.
pub(super) fn unverified_pass_note(claim: &ActiveBead, reason: &str) -> String {
    format!(
        "beads: cannot verify whether {} was closed: {}. Nothing is queued and the loop is stopped — this is a board read failure, not a finished ticket. Type a new instruction to continue.",
        claim.id,
        clip(reason, 300)
    )
}

/// The worker pushed this one to a human; the rest of the board is still the
/// loop's business.
pub(super) fn left_for_human_note(claim: &ActiveBead, status: BeadStatus) -> String {
    format!(
        "beads: {} was left `{}` by its worker, so a human has to move that one. The loop is moving on to the rest of the board and will not pick this ticket up again by itself.",
        claim.id, status
    )
}

/// A settle with no ticket behind it: nothing to verify, and something to report.
pub(super) fn nothing_held_note() -> &'static str {
    "beads: a worker settled while this loop held no claimed ticket, so there is nothing to verify. The loop is stopped; type a new instruction to continue."
}

/// The guard speaking for itself, at the pass boundary: `bd ready` offered a ticket
/// this loop already burned a pass on, and it is still open. A refusal has to say
/// what it refused *and* what would unblock it, or the human reads the stop as a
/// crash.
pub(super) fn already_worked_note(bead: &Bead) -> String {
    format!(
        "`bd ready` offered {} again, but this loop already worked it and `bd` says it is `{}`, not closed. It will not be started a second time by itself: `bd close {}`, or type a new instruction to continue.",
        bead.id(),
        bead.status(),
        bead.id()
    )
}

/// Bound a quoted child message so a rambling refusal cannot bury the transcript.
/// Char-based, not byte-based, so a multi-byte codepoint cannot be cut in half.
pub(super) fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\u{2026}")
}
