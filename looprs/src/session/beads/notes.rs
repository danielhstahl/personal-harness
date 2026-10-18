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

/// How the ticket reached the worker being prompted (looprs-hjo, workspace-9th).
///
/// Three ways a ticket arrives, and the worker needs to know which: a fresh ticket is
/// worked from nothing, an in-progress one may already be half done — by the session
/// that was killed, crashed or cancelled while holding it — and a retried one was
/// half done by this harness's own previous pass. A worker told neither redoes the
/// part that already shipped, or worse, throws it away.
pub(super) enum Arrival {
    /// Ready work, and this pass is the first pass on it.
    Fresh,
    /// The board already had it `in_progress` when this loop claimed it.
    Resumed,
    /// The board offered it as ready work, but a pass *this loop* already bought
    /// ended without closing it, and the loop spent an automatic re-buy on it
    /// (workspace-9th). Neither of the two cases above: `bd` does not have it
    /// `in_progress`, and no foreign session was ever holding it.
    Retried,
}

/// The worker prompt: the standing instructions, plus the one ticket this worker
/// owns and the fact that the harness has already claimed it.
///
/// The claim is stated rather than assumed, because the alternative — telling the
/// agent to go find work — is a race with every other hand on the board, and the
/// worker would end up doing (and billing) a different ticket than the one the
/// loop reports, guards and verifies. Hence `bd ready` is gone from the worker's
/// command list in `prompts::WORKER`: a prompt that advertises it invites the
/// agent to shop.
///
/// A resumed ticket adds one paragraph — [`resumed_note_for_worker`] — and a retried
/// one adds [`retried_note_for_worker`]; nothing else changes. That is deliberate: a
/// resumed pass is not a lesser pass, and it gets the same claim, the same guard and
/// the same close-check as a fresh one.
pub(super) fn worker_prompt(claim: &ActiveBead, arrival: Arrival) -> String {
    let mut instructions = format!(
        "Your assigned ticket: {id} — {title}\n\n\
         The harness has already run `bd update {id} --claim`, so this ticket is yours. \
         Work it and close it when the work is done. Do not look for other work and do not \
         pick a different ticket: {id} is the one being reported, guarded and paid for.\n",
        id = claim.id,
        title = claim.title,
    );
    if let Arrival::Resumed = arrival {
        instructions.push_str(&resumed_note_for_worker(&claim.id));
    } else if let Arrival::Retried = arrival {
        instructions.push_str(&retried_note_for_worker(&claim.id));
    }
    generate_prompt(WORKER, &instructions)
}

/// The paragraph a worker gets when its own harness already bought a pass on this
/// ticket and that pass did not close it (workspace-9th).
///
/// The same order as [`resumed_note_for_worker`] — look before writing — for a
/// different reason: nobody else was ever holding this ticket, so saying so would
/// point the agent at the wrong story. What it has to know is that the diff it is
/// about to find in `git status` may be *this ticket's* unfinished work rather than
/// somebody else's, and that starting over would throw it away.
fn retried_note_for_worker(id: &str) -> String {
    format!(
        "\nThis ticket is being worked a second time by THIS harness: an earlier pass on it \
         stopped talking before closing it, so part of the work may already exist in the \
         working tree or in a recent commit. Do not assume nothing has been done. Before \
         writing anything, find out what state it is in: `git status` and `git diff` for \
         uncommitted work, `git log` for work that is already committed, and `bd show {id}` \
         for notes that pass left. Then continue from there: do not start over, and do not \
         redo what is already done.\n"
    )
}

/// The paragraph a worker gets when the ticket handed to it was already
/// `in_progress` (looprs-hjo).
///
/// Written as an order to *look before writing*, because "continue" is ambiguous
/// about a half-finished state: the work may be committed, staged, unstaged, or only
/// ever have existed in the previous session's head.
fn resumed_note_for_worker(id: &str) -> String {
    format!(
        "\nThis ticket was ALREADY `in_progress` on the board when this harness claimed it: a \
         previous session — most likely one that crashed, was killed or was cancelled partway \
         through — was working it. Do not assume nothing has been done. Before writing \
         anything, find out what state it is in: `bd show {id}` for the ticket and any notes \
         an earlier session left on it, `git status` and `git diff` for uncommitted work, and \
         `git log` for work that is already committed. Then continue from there: do not start \
         over, and do not redo what is already done.\n"
    )
}

/// "This ticket was mid-flight when I got here, and it outranks new work."
///
/// Said out loud because the silent version is the confusing half of the bug: a loop
/// that quietly works a ticket nobody remembers asking for reads exactly like a loop
/// that picked the wrong ticket.
pub(super) fn resume_note(bead: &Bead) -> String {
    format!(
        "beads: resuming {} — `bd` already holds it `in_progress`, so a session before this one claimed it and never closed it. `bd ready` cannot show an in-progress ticket, so finishing what is already started comes before taking anything new.",
        bead.id()
    )
}

/// `bd` would not hand an in-progress ticket over: another worker holds it.
///
/// The skip is a sentence rather than a silence for the same reason every other skip
/// here is: a ticket that quietly leaves the running looks like a ticket that was
/// worked. This one was not, and only a human can say whose it is.
pub(super) fn held_elsewhere_note(bead: &Bead, reason: &str) -> String {
    format!(
        "beads: {} is `in_progress`, but `bd` would not hand it over ({}). Another worker holds that ticket, so this loop leaves it alone and takes fresh work instead.",
        bead.id(),
        clip(reason, 200)
    )
}

/// The re-work guard answering at the resume boundary (looprs-hjo).
///
/// The same rule as [`already_worked_note`], in the words this boundary has: nothing
/// was `bd ready`-offered, because `ready` cannot see this ticket at all. It is
/// still marked in progress from a pass *this* loop already paid for, and the loop is
/// out of automatic re-buys on it — so the two ways out both belong to a human, and
/// both get named.
///
/// Reached two ways, and the wording has to be true for both: a ticket whose
/// automatic re-buys ran out ([`MAX_AUTO_RESUME`](super::guards::MAX_AUTO_RESUME)),
/// and a ticket a human stopped with `Esc`, which never earned any.
pub(super) fn resume_already_worked_note(bead: &Bead) -> String {
    format!(
        "`bd` still holds {} `in_progress` from a pass this loop already paid for, and says it is `{}`, not closed. It will not be resumed by itself any more: `bd close {}`, or type a new instruction to continue.",
        bead.id(),
        bead.status(),
        bead.id()
    )
}

/// "That pass ended without closing its ticket, so this loop is buying it another
/// one — and here is which number this one is" (workspace-9th).
///
/// Said at the boundary that spends the re-buy, so the transcript says *why* a ticket
/// is being worked twice before anybody reads it as the runaway this loop is guarded
/// against. The count is the load-bearing part: "1 of 2" reads as a bounded repair,
/// and a sentence without it reads as the loop having forgotten its own guard.
pub(super) fn auto_resume_note(id: &str, attempt: u32, max: u32) -> String {
    format!(
        "beads: automatic re-buy {attempt} of {max} on {id} — its last pass stopped without closing it, so the loop is finishing what it already paid for rather than parking on it.",
        attempt = attempt,
        max = max,
        id = id
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

/// How many deferred parents get listed before the note truncates.
pub(super) const MAX_DEFERRED_LISTED: usize = 5;

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

/// "Worked it, did not close it, and the loop is not going to park on that."
///
/// The sentence the unexpected agent complete gets: the ticket did not close, the
/// pass is over, and the loop is about to buy the same ticket another pass — with
/// the bound named, because "we will keep trying" without a number is the runaway
/// the guard exists for.
pub(super) fn not_closed_note(claim: &ActiveBead, status: BeadStatus) -> String {
    format!(
        "beads: worked {} but `bd` says it is `{}`, not closed. The loop is not stopping: the same ticket gets an automatic re-buy, and when those run out the guard will stop it.",
        claim.id, status
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

/// "An epic is not the work; what hangs under it is."
///
/// The children-before-parent rule (looprs-lpw) spoken out loud. It has to be said
/// because the silent version looks like a bug from the outside: the ready list
/// clearly had `looprs-x` on top and the loop went and worked `looprs-x.4`, which
/// is exactly what a loop that picked the wrong ticket looks like. The sentence
/// names the ticket it passed over, the one it took instead, and — per parent — how
/// many children are still holding it down, which is the number that says how much
/// longer the epic waits.
///
/// `chosen` goes first; every parent in `deferred` waits, with the count of the
/// children holding it down.
pub(super) fn children_first_note(chosen: &Bead, deferred: &[(Bead, usize)]) -> String {
    let mut out = format!(
        "beads: working {} before {} parent ticket(s) whose children are still ready — a parent is a container and its children are the work, so it goes last:",
        chosen.id(),
        deferred.len()
    );
    for (b, n) in deferred.iter().take(MAX_DEFERRED_LISTED) {
        out.push_str(&format!(
            "\n  {} ({}) — {n} child ticket(s) still ready",
            b.id(),
            clip(b.title(), 60)
        ));
    }
    if let Some(rest) = deferred.len().checked_sub(MAX_DEFERRED_LISTED) {
        out.push_str(&format!("\n  \u{2026} and {rest} more"));
    }
    out
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

/// The guard speaking for itself, at the ready boundary: `bd ready` offered a ticket
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
