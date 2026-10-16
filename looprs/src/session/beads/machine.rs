//! The beads machine: [`BeadsLoop`] and the values it passes around.
//!
//! This is the pass boundary. Everything that decides *what the loop does next*
//! — whether a bead is claimable, whether a ticket a dead session left
//! `in_progress` has to be finished first, whether the planner's plan landed,
//! whether the worker's pass closed the ticket it was bought for — is a method on
//! this type, and every one of them returns a value (`Pick`, `Recovery`,
//! `PlanCheck`, `PassOutcome`) rather than mutating toward a conclusion, so the
//! caller in [`task`](super::task) decides what the answer costs.
//!
//! The loop is a backend, not UI state: it reports [`SessionEvent`] and never
//! reaches into the App (ADR-0002 Q1).

use super::guards::StepCause;
use super::notes::{
    Arrival, MAX_SKIPPED_LISTED, already_worked_note, children_first_note, held_elsewhere_note,
    resume_already_worked_note, resume_note, worker_prompt,
};

use super::BeadsCmd;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use crate::services::bd::{
    Bead, BeadStatus, claim_with, in_progress_with, list_status_with, ready_with, show_with,
};
use crate::services::notification::BeadDone;
use crate::services::pi::PiRpc;
use crate::services::prompts::{PLANNER, generate_prompt};
use crate::session::{ActiveBead, BeadStep, SessionConfig, SessionEvent, SessionId};

use crate::wire::{AssistantEvent, PiEvent, parse};

use serde_json::Value;

// ---------------------------------------------------------------------------------------
// The machine itself — moved out of `app.rs`, and now reporting `SessionEvent`.
// ---------------------------------------------------------------------------------------

/// The beads terminal state, as a `BeadsLoop`.
pub struct BeadsLoop {
    /// This loop's identity, kept for logs and for the events' provenance chain
    /// (ADR-0002 Q2: the pump adds the id to what reaches the UI).
    id: SessionId,
    /// The worker currently driven by the loop. `None` whenever the loop is parked.
    pub(super) pi_rx: Option<Worker>,
    /// Serial source for workers. One per spawned pass, never reused, so a record
    /// from a pass this loop retired is distinguishable from one it is still
    /// waiting on (see [`BeadsTask::is_live`]).
    next_serial: u64,
    ev_tx: mpsc::UnboundedSender<SessionEvent>,
    /// This session's own mailbox. A worker's control edges go here, **not** out to
    /// the UI: the loop advances itself rather than being told to by whichever
    /// mode happened to be on screen (looprs-msj).
    pub(super) ctl: mpsc::UnboundedSender<BeadsCmd>,
    cfg: SessionConfig,
    bead_step: BeadStep,
    /// Set for the duration of one planner pass: the board as it looked *before* the
    /// run, plus a handle on the run's own last words. `None` means the pass in
    /// flight is a worker's (or there is no pass), and there is nothing to diff.
    planning: Option<PlanPass>,
    /// The ticket this pass holds a claim on, `None` between passes and for every
    /// planner pass (looprs-w7q).
    ///
    /// Taken before the worker is spawned and published to the UI in the same step
    /// ([`BeadsLoop::set_claim`]), so the loop's own knowledge of what it is
    /// paying for and what the screen is told cannot drift apart.
    claim: Option<ActiveBead>,
    /// Every ticket this loop has put a worker on since the last human instruction.
    ///
    /// This is the re-work guard, and the reason it is a *set* rather than "the
    /// last one" is that a board can hand a worked bead back two passes later — a
    /// cycle of X, Y, X is the same runaway as X, X. It is filled the moment a
    /// claim succeeds (not when a pass ends), so a worker that died, was killed or
    /// settled without closing is all three, and is still covered.
    ///
    /// It clears only on `launch_create_tickets`, i.e. on a new instruction from a
    /// human. That is deliberate: the guard exists to stop *automatic* re-work, and
    /// a typed instruction is the only deliberate thing the beads mode accepts.
    pub(super) worked: HashSet<String>,
}

/// What one planner pass has to be checked against when it settles (looprs-k7v).
pub(super) struct PlanPass {
    /// Ids that were already open when the planner was spawned. Set membership, not
    /// a count: on a live board a plan of two plus an unrelated close elsewhere is
    /// not evidence of anything, and a count-based diff would read that as noise.
    baseline: HashSet<String>,
    /// The planner's last assistant text, tee'd off its stdout by
    /// [`BeadsLoop::forward_records`]. A zero-ticket plan is a lot more
    /// diagnosable with the model's own sentence attached: "it refused", "it asked
    /// a question back" and "it narrated a plan it never wrote down" are otherwise
    /// indistinguishable, and only the first two are the harness's problem.
    said: Arc<StdMutex<String>>,
}

/// The verdict of checking a planner pass against the board. Four outcomes, because
/// there are four things the user can act on and a bool cannot name them.
pub(super) enum PlanCheck {
    /// The pass that settled was a worker's, not a planner's: nothing to check.
    NotPlanning,
    /// The planner added these tickets to the board. Show them, then work them.
    Created(Vec<Bead>),
    /// The planner settled and the board gained nothing — the silent no-op this
    /// ticket was filed for, now an explicit one.
    NothingCreated { said: String },
    /// A `bd` read failed, so the plan is unverifiable. Deliberately **not**
    /// foldable into [`PlanCheck::NothingCreated`]: "the board is empty" and "the
    /// board is unreadable" are the two facts that must never be conflated, which
    /// is the exact conflation looprs-037 was about, one level up.
    Unverifiable { stage: &'static str, reason: String },
}

/// What the loop decided to do with the tickets `bd ready` offered it.
///
/// Three answers rather than an `Option`, because "there is nothing to work" and
/// "the only thing left is one I already burned a pass on" are different
/// sentences and the human's next action differs: wait for instructions, or go
/// close the ticket.
#[derive(Debug)]
pub(super) enum Pick {
    /// Work this one.
    Work(Bead),
    /// `bd ready` was empty. The board is done; the loop parks for an instruction.
    Nothing,
    /// The first workable ticket is one this loop already ran a pass on and did not
    /// close. Refused, not re-run — the runaway guard (looprs-w7q).
    AlreadyWorked(Bead),
}

/// What the board says about the ticket a worker pass just stopped talking about
/// (looprs-w7q). One arm per thing the human can act on, because a bool cannot
/// tell "keep going" from "go close it" from "fix `bd`".
pub(super) enum PassOutcome {
    /// The ticket closed. This pass earned its keep; take the next one.
    Closed(ActiveBead),
    /// The worker left it `blocked` / `deferred`: it stopped on purpose and handed
    /// it to a human. Not the loop's problem, and not a thing it will re-buy — a
    /// blocked ticket is not ready work. Moving on is right; saying nothing about it
    /// would leave the human hunting for why a ticket did not finish.
    LeftForHuman(ActiveBead, BeadStatus),
    /// The pass ended and the ticket is still `open` / `in_progress` / unreadable-
    /// as-`unknown`. `bd ready` offers this exact bead back, so carrying on here
    /// is the loop running itself up a hill. Stops the loop.
    NotClosed(ActiveBead, BeadStatus),
    /// `bd` could not answer, so nobody knows. Deliberately **not** foldable into
    /// [`PassOutcome::Closed`] nor into [`PassOutcome::NotClosed`]: the remedy is
    /// "repair `bd`", not "close the ticket", and the loop must not silently pick
    /// one of those two readings on a read that failed.
    Unverifiable(ActiveBead, String),
    /// A worker settled while the loop held no claim. Nothing can be verified
    /// against nothing; said out loud rather than assumed harmless, because a
    /// settle that cannot be attributed to a ticket is the very event that used to
    /// drive this loop blind (looprs-msj).
    NothingHeld,
}

/// The worker child, plus the serial that says *which* pass it belongs to.
pub(super) struct Worker {
    serial: u64,
    rpc: PiRpc,
    /// The last non-empty assistant text this child streamed. Only the planner pass
    /// ever reads it, but every worker carries one because the tee lives in the
    /// shared record-forwarding path and a `Worker` without it would be a worker
    /// whose last words were thrown away.
    said: Arc<StdMutex<String>>,
}

/// What a worker pass did.
pub(super) enum WorkerPass {
    /// Nothing ready: no child was spawned, the loop should park.
    Idle,
    /// A worker was spawned *and prompted*.
    Working,
}

/// What the resume check decided, before any new work was considered (looprs-hjo).
///
/// Three answers, because three things can be true of the set `bd ready` cannot
/// show, and the loop's next action differs in each: take it, ignore it, or refuse
/// it.
pub(super) enum Recovery {
    /// This ticket was already `in_progress`, this loop holds it now, and a worker
    /// is owed for it.
    Resumed(ActiveBead),
    /// Nothing to resume: no ticket in progress, or every one of them belongs to a
    /// worker `bd` will not release it from — each of those said out loud.
    Nothing,
    /// The in-progress ticket is one *this* loop already worked and did not close:
    /// the re-work guard answers before a second pass is bought.
    Refused(Bead),
}

/// How many workable children each bead in `beads` has *in that same list*.
///
/// The parent side of the looprs-lpw rule, and it can only be built from the child
/// side: `bd`'s list rows put `parent` on a child and nothing at all on the parent
/// (the epic's own row says nothing about what hangs below it, and its
/// `*_count` fields are per-verb and read 0 on a `ready` row — see
/// [`Bead::parent`]). So "does this bead still have unfinished work under it?" is
/// answered by looking for a listed bead that names it, which is why
/// [`ready_with`] must not be paginated: cut the page and the parent looks
/// childless again.
///
/// "Workable" is doing real work in here. A child that a human took out of the loop
/// (`blocked`, `deferred`) is not work this loop can do, so it must not hold its
/// parent down: an epic whose only listed children are blocked is exactly as
/// workable as an epic with no children at all, and deferring it would park the
/// loop over a ticket it is allowed to take.
fn unfinished_children(beads: &[Bead]) -> HashMap<&str, usize> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for bead in beads {
        if bead.needs_human() {
            continue;
        }
        if let Some(parent) = bead.parent() {
            *counts.entry(parent).or_insert(0) += 1;
        }
    }
    counts
}

impl BeadsLoop {
    /// Builds the loop without touching any process: it starts parked in
    /// `AwaitInput`. Work only begins when [`BeadsLoop::next`] is called, so a
    /// constructed-but-unstarted loop can never be holding an idle, unprompted
    /// pi child (the bug this replaces: `new()` used to spawn a session nobody
    /// ever prompted, and nothing else would ever prompt it).
    ///
    /// Private to the module on purpose: `BeadsCmd` is private, and a public
    /// constructor taking one would advertise the loop's own mailbox.
    /// [`BeadsSession::start`] is the way in.
    pub(super) fn new(
        id: SessionId,
        ev_tx: mpsc::UnboundedSender<SessionEvent>,
        ctl: mpsc::UnboundedSender<BeadsCmd>,
        cfg: SessionConfig,
    ) -> Self {
        Self {
            id,
            pi_rx: None,
            next_serial: 1,
            ev_tx,
            ctl,
            cfg,
            bead_step: BeadStep::AwaitInput,
            planning: None,
            claim: None,
            worked: HashSet::new(),
        }
    }

    /// Move the machine, naming why.
    ///
    /// There is deliberately no `set_step(BeadStep)`: a caller cannot put the loop
    /// somewhere its own story does not justify, and the step is looked up in one
    /// place ([`StepCause::step`]) rather than decided six times. Every emitted
    /// [`SessionEvent::BeadStep`] therefore arrived through the same table the
    /// tests enumerate.
    pub fn set_step(&mut self, cause: StepCause) {
        let next = cause.step();
        self.bead_step = next;
        let _ = self.ev_tx.send(SessionEvent::BeadStep(next));
    }

    /// The step the machine is on, as the machine sees it.
    ///
    /// The UI never calls this: it renders the [`SessionEvent::BeadStep`] it was
    /// handed and must not re-derive the step (looprs-msj). This is the loop's own
    /// state, exposed so a test can assert a transition happened instead of
    /// inferring one from a transcript.
    #[allow(dead_code)] // consumers: beads::tests; the UI renders the event, never this
    pub fn get_step(&self) -> &BeadStep {
        &self.bead_step
    }

    /// As [`Self::get_step`], in the one question the loop is actually asked:
    /// "is a human's turn now?" — and through [`BeadStep::awaits_user`], so the
    /// loop and the view cannot disagree about what "my turn" means.
    #[allow(dead_code)] // consumers: beads::tests; the UI asks its own view
    pub fn is_awaiting_input(&self) -> bool {
        self.bead_step.awaits_user()
    }

    /// Is there a worker process behind this loop? This is the check that makes
    /// "one pass at a time" a property of the machine rather than a hope.
    pub fn has_live_worker(&self) -> bool {
        self.pi_rx.is_some()
    }

    /// The serial of the pass currently in flight, `None` when nothing is running.
    /// A settle means "advance" only when it carries this serial.
    pub fn worker_serial(&self) -> Option<u64> {
        self.pi_rx.as_ref().map(|w| w.serial)
    }

    /// Tear down the current session, then resume what is in progress or work the
    /// next ready bead, or park.
    ///
    /// Infallible by design: a failing `bd`, a `pi` that will not start, or a prompt
    /// that never gets answered is reported to the transcript and the loop parks in
    /// `AwaitInput` for a human. Nothing here retries on a timer, so a broken board
    /// cannot turn into a respawn storm.
    pub(super) async fn next(&mut self) {
        self.close().await;
        match self.work_next_bead().await {
            Ok(WorkerPass::Working) => {}
            Ok(WorkerPass::Idle) => {
                tracing::debug!("board empty, awaiting input");
                self.report_system("beads: board empty, awaiting input".to_string());
                self.set_step(StepCause::Awaiting);
            }
            Err(e) => {
                tracing::error!("beads worker pass failed: {e:#}");
                self.report_error(format!("beads: {e:#}"));
                self.set_step(StepCause::Awaiting);
            }
        }
    }

    /// Resume what is already started, else claim the next bead, then buy the
    /// worker for it — in that order.
    ///
    /// Two questions get answered here, in this order, and the second one only gets
    /// asked when the first says there is nothing in front of it:
    ///
    /// 1. **Is there a ticket a dead session left behind?** `bd ready` excludes
    ///    `in_progress` work, so a ticket whose worker died is not merely unclaimed
    ///    — it is *invisible*, and a loop that reads only `ready` takes the next
    ///    ticket and abandons that one (looprs-hjo). [`recover_in_progress`] is what
    ///    makes the unfinished one outrank the untouched ones.
    /// 2. **Otherwise, what is ready?** [`pick_bead`], with its skip/defer/refuse
    ///    rules — including the one that works a child before the epic that merely
    ///    contains it (looprs-lpw).
    ///
    /// The claim comes before the spawn in both branches because everything after a
    /// claim is paid for: the pass is only worth reporting, verifying and guarding
    /// once the harness actually holds the ticket. Spawn-first would mean a refused
    /// claim still cost a whole `pi` session, and one pointed at a bead we do not
    /// own answers to whoever prompts it rather than to us (looprs-w7q).
    ///
    /// Also the only place a worker is both spawned *and* prompted, so callers never
    /// see a pi child that is alive but has been given nothing to do.
    async fn work_next_bead(&mut self) -> Result<WorkerPass> {
        match self.recover_in_progress().await? {
            Recovery::Resumed(claim) => return self.buy_worker(&claim, Arrival::Resumed).await,
            Recovery::Refused(bead) => return Err(anyhow!(resume_already_worked_note(&bead))),
            Recovery::Nothing => {}
        }

        let beads = ready_with(&self.cfg.bd_bin).await?;
        let bead = match self.pick_bead(&beads) {
            Pick::Nothing => return Ok(WorkerPass::Idle),
            Pick::AlreadyWorked(bead) => return Err(anyhow!(already_worked_note(&bead))),
            Pick::Work(bead) => bead,
        };
        let claim = self.take_claim(&bead).await?;
        self.buy_worker(&claim, Arrival::Fresh).await
    }

    /// Take the claim on a fresh bead and record it, before anything is bought.
    ///
    /// Split out of [`Self::work_next_bead`] so that the resume path and the ready
    /// path cannot drift apart on the one thing that matters: a claim the board
    /// granted is recorded in both registers or the pass is not counted as worked.
    async fn take_claim(&mut self, bead: &Bead) -> Result<ActiveBead> {
        tracing::info!(bead = %bead.id(), "claiming a worker pass");
        let claim = ActiveBead {
            id: bead.id().to_string(),
            title: bead.title().to_string(),
        };
        claim_with(&self.cfg.bd_bin, &claim.id)
            .await
            .map_err(|e| anyhow!("could not claim {} before buying a worker: {e}", claim.id))?;
        self.record_claim(claim.clone());
        Ok(claim)
    }

    /// Write a granted claim into both registers.
    ///
    /// Two records of one claim, both taken before anything is prompted: `worked` is
    /// the guard that outlives the pass, `claim` is the live one the UI and the
    /// post-settle check read. Recording the claim *before* the spawn is what covers
    /// the pass that never reaches a settle at all — a spawn that fails, a prompt
    /// that is refused, a worker that is killed mid-run, and (looprs-hjo) the
    /// resumed pass whose ticket is *already* claimed on the board. Each of those
    /// leaves the ticket claimed, and the next read hands it back, so the guard has
    /// to already know about it.
    fn record_claim(&mut self, claim: ActiveBead) {
        self.worked.insert(claim.id.clone());
        self.set_claim(Some(claim));
    }

    /// Find the ticket a dead session left behind, and take it (looprs-hjo).
    ///
    /// This is the fix for "a crashed session abandons its ticket". `bd ready`
    /// excludes `in_progress` work — that is what *in progress* means — so the
    /// abandoned ticket never appears in the ready list, and the next pass silently
    /// took whatever was on top of that list instead. Three rules, one per thing that
    /// could otherwise go wrong:
    ///
    /// * **The re-work guard applies here too.** An in-progress ticket *this* loop
    ///   already spent a pass on is refused with the guard's own sentence, not
    ///   resumed: an `Esc`, a dead worker or a settle that closed nothing all leave
    ///   the ticket in exactly this state, and re-buying it is still a human call.
    /// * **`bd` is the arbiter of whose ticket it is.** A claim it *refuses* means
    ///   another worker holds it — a human in another terminal, a second loop — so
    ///   this loop walks past it, out loud, and takes fresh work. A claim it cannot
    ///   answer at all is not a refusal and does not get walked past: that is a board
    ///   outage, and the two must not read alike.
    /// * **A resumed pass is an ordinary pass.** Same claim, same `worked` entry, same
    ///   `ActiveBead` published, same post-settle check. Only the prompt differs, and
    ///   only by telling the worker some of the work may already exist.
    ///
    /// A `bd reclaim`-style reaper is deliberately *not* used here: it reverts every
    /// stale lease on the replica, including another actor's live-looking work, and
    /// this loop only has a claim on continuing its own.
    async fn recover_in_progress(&mut self) -> Result<Recovery> {
        let unfinished = in_progress_with(&self.cfg.bd_bin).await.map_err(|e| {
            anyhow!("could not read which tickets are in progress before taking new work: {e}")
        })?;
        for bead in unfinished {
            if self.worked.contains(bead.id()) {
                return Ok(Recovery::Refused(bead));
            }
            match claim_with(&self.cfg.bd_bin, bead.id()).await {
                Ok(()) => {
                    tracing::info!(bead = %bead.id(), "resuming a ticket left in progress");
                    let claim = ActiveBead {
                        id: bead.id().to_string(),
                        title: bead.title().to_string(),
                    };
                    self.record_claim(claim.clone());
                    self.report_system(resume_note(&bead));
                    return Ok(Recovery::Resumed(claim));
                }
                // `bd` answered "no", so the ticket is somebody else's live claim.
                // It is left alone — and the reason is said, because a resumable
                // ticket that silently stops being resumable reads as a lost ticket.
                Err(e) if e.is_refusal() => {
                    self.report_system(held_elsewhere_note(&bead, &e.to_string()));
                }
                // No answer at all. That is not "not ours", it is "unknown", and
                // unknown never buys a pass on something else.
                Err(e) => {
                    return Err(anyhow!(
                        "could not read the board to resume {}: {e}",
                        bead.id()
                    ));
                }
            }
        }
        Ok(Recovery::Nothing)
    }

    /// Buy the worker a claim was taken for: spawn it, prompt it, publish the pass.
    async fn buy_worker(&mut self, claim: &ActiveBead, arrival: Arrival) -> Result<WorkerPass> {
        // The session is built against locals: if any step below fails, `worker`
        // drops here and kill_on_drop reaps it, so a failed pass cannot leave an
        // orphan behind.
        let worker = self.spawn_worker(&[])?;
        let disposition = worker.rpc.prompt(&worker_prompt(claim, arrival)).await?;
        if disposition == "handled" {
            // pi took the prompt but started no run, so no `agent_settled` will ever
            // arrive to advance the loop. Do not hold an idle session open.
            drop(worker);
            return Err(anyhow!(
                "worker prompt for {} was handled without starting a run",
                claim.id
            ));
        }

        self.pi_rx = Some(worker);
        self.report_system(format!("beads: working {}", claim.id));
        self.set_step(StepCause::Working);
        Ok(WorkerPass::Working)
    }

    /// Decide which bead this pass works, given what `bd ready` offered.
    ///
    /// Four rules, in this order, and each one is a thing that used to go wrong:
    ///
    /// 1. **A ticket that needs a human is skipped, out loud.** `blocked` and
    ///    `deferred` mean somebody — a planner, or a worker that gave up — took it
    ///    out of the loop on purpose, and `bd ready` should not have listed it.
    ///    Spending a metered pass on one is the worst possible answer; staying
    ///    quiet about the skip would make it indistinguishable from a lost ticket.
    /// 2. **A parent is worked after its children, never before them (looprs-lpw).**
    ///    A parent bead — the epic a planner wrote first, usually at a higher
    ///    priority — is a *container*: the work is in the children. `bd ready` does
    ///    not know that and lists the parent first, so the loop used to claim the
    ///    epic, spend a pass reading a paragraph of intent, and leave the children
    ///    untouched. A bead that is named as the parent of another workable bead in
    ///    this same list is therefore walked past, out loud, and becomes workable
    ///    again once nothing is left under it. A hierarchy resolves depth-first for
    ///    free: an epic, its child and that child's own subtask all defer to the one
    ///    below them, and the list re-sorts itself as the leaves close.
    /// 3. **A ticket this loop already worked is refused, never re-run.** If the
    ///    first bead that survives the two rules above is one whose pass already
    ///    ended without closing it, starting another pass on it is the runaway this
    ///    ticket was filed for: `bd ready` will keep offering a bead for exactly as
    ///    long as it stays open, and every pass is billed. The refusal parks the
    ///    loop with its two ways out, both of which belong to a human. It is checked
    ///    *after* rule 2 on purpose: stepping a parent aside is not a licence to walk
    ///    past the guard and re-buy a ticket this loop already paid for — and the
    ///    deferral is still said out loud, because a refusal on a child while its
    ///    epic sits on top of the list otherwise reads as a mispick.
    /// 4. **Otherwise the first workable bead**, in `bd`'s own priority order.
    ///
    /// What is deliberately *not* a rule: an unknown status is workable. Skipping
    /// what this build cannot classify would let a `bd` upgrade silently empty the
    /// board, which is looprs-037's conflation wearing a different hat. Nor is "the
    /// epic is unworkable, never touch it": a parent with nothing left under it is
    /// the one who knows the epic, so it gets the pass and closes it — and a parent
    /// whose children are all blocked, deferred or gone is workable the moment
    /// `bd` itself says it is.
    pub(super) fn pick_bead(&self, beads: &[Bead]) -> Pick {
        let children = unfinished_children(beads);
        let mut skipped: Vec<Bead> = Vec::new();
        let mut deferred: Vec<(Bead, usize)> = Vec::new();
        let mut pick = Pick::Nothing;
        for bead in beads {
            if bead.needs_human() {
                skipped.push(bead.clone());
                continue;
            }
            if let Some(n) = children.get(bead.id()) {
                deferred.push((bead.clone(), *n));
                continue;
            }
            pick = if self.worked.contains(bead.id()) {
                Pick::AlreadyWorked(bead.clone())
            } else {
                Pick::Work(bead.clone())
            };
            break;
        }
        self.report_skipped(&skipped);
        // Only a pick that *moved* is worth a sentence: a pass that would have worked
        // the epic anyway did not need this rule to say so. A refusal counts as
        // moving — a loop that parks on a re-offered child while its epic sits on top
        // of the ready list is precisely the sight that reads as a bug from outside.
        let moved = match &pick {
            Pick::Work(chosen) | Pick::AlreadyWorked(chosen) => Some(chosen),
            Pick::Nothing => None,
        };
        if let Some(chosen) = moved {
            self.report_deferred_parents(&deferred, chosen);
        }
        pick
    }

    /// "Here is what I walked past, and why." Bounded like the plan note: the
    /// transcript is a screen, not a dump.
    fn report_skipped(&self, skipped: &[Bead]) {
        if skipped.is_empty() {
            return;
        }
        let mut note = format!(
            "beads: skipping {} ticket(s) that need a human, not a worker:",
            skipped.len()
        );
        for b in skipped.iter().take(MAX_SKIPPED_LISTED) {
            note.push_str(&format!("\n  {} ({}) — {}", b.id(), b.status(), b.title()));
        }
        if let Some(rest) = skipped.len().checked_sub(MAX_SKIPPED_LISTED) {
            note.push_str(&format!("\n  \u{2026} and {rest} more"));
        }
        self.report_system(note);
    }

    /// "These parents went to the back of the queue, and here is the child that
    /// jumped them." Said once per pass, and only when the rule actually changed
    /// the answer: a loop that quietly works `epic.7` when `epic` was sitting on
    /// top of the ready list reads like a loop that picked the wrong ticket, which
    /// is the same complaint [`resume_note`](super::notes::resume_note) answers.
    fn report_deferred_parents(&self, deferred: &[(Bead, usize)], chosen: &Bead) {
        if deferred.is_empty() {
            return;
        }
        self.report_system(children_first_note(chosen, deferred));
    }

    /// Spawn one pass's `pi` child and wire its records into this session.
    ///
    /// The child comes back *unassigned*: the caller decides the moment the pass is
    /// live by storing it in `self.pi_rx`, which keeps "a pass is in flight" a
    /// fact the loop controls rather than a side effect of a spawn having
    /// happened somewhere.
    fn spawn_worker(&mut self, args: &[&str]) -> Result<Worker> {
        let (rpc, records) = PiRpc::spawn_with(&self.cfg.pi_bin, args)?;
        let serial = self.next_serial;
        self.next_serial += 1;
        let said = Arc::new(StdMutex::new(String::new()));
        self.forward_records(serial, records, said.clone());
        Ok(Worker { serial, rpc, said })
    }

    /// Pipe one worker's records into this session: two copies of the same fact,
    /// deliberately.
    ///
    /// * the **render** copy goes out on `ev_tx` unchanged, and the App turns it
    ///   into transcript lines knowing nothing about beads;
    /// * the **control** copy — the edges only, `agent_settled` and the end of the
    ///   stream — goes into the task that owns this loop, tagged with the serial of
    ///   the pass that produced it.
    ///
    /// The serial is not decoration. A killed worker's last records can arrive
    /// after the loop has moved on, and an untagged late settle would read as
    /// "the pass in flight finished", killing work that is already paid for to
    /// start a pass nobody asked for. The control copy is also why no UI message is
    /// involved in advancing the loop: this used to be `UiCommand::BeadsNext`,
    /// arriving from whichever mode the input box happened to be in (looprs-msj).
    ///
    /// A third, smaller copy: the child's last completed assistant text lands in
    /// `said` on the way past, so the planner check can quote the model's own
    /// conclusion instead of the harness inventing one. `TextEnd` rather than the
    /// deltas because it is the authoritative final content of the block — the
    /// deltas are what was typed, this is what landed.
    fn forward_records(
        &self,
        serial: u64,
        mut records: mpsc::UnboundedReceiver<Value>,
        said: Arc<StdMutex<String>>,
    ) {
        let render = self.ev_tx.clone();
        let ctl = self.ctl.clone();
        tokio::spawn(async move {
            while let Some(v) = records.recv().await {
                let Some(ev) = parse(&v) else {
                    continue; // `parse` logs the offender
                };
                if let PiEvent::MessageUpdate {
                    assistant_message_event: AssistantEvent::TextEnd { content, .. },
                } = &ev
                    && !content.trim().is_empty()
                {
                    *said.lock().unwrap() = content.clone();
                }
                let settled = matches!(ev, PiEvent::AgentSettled);
                if render.send(SessionEvent::Agent(ev)).is_err() {
                    return; // the session is gone: nothing to render, nothing to advance
                }
                if settled {
                    let _ = ctl.send(BeadsCmd::WorkerSettled { serial });
                }
            }
            // stdout closed. Whether that is expected is the task's call, not this
            // task's: it needs the loop's state to know.
            let _ = ctl.send(BeadsCmd::WorkerGone { serial });
        });
    }

    pub fn report_error(&self, text: String) {
        let _ = self.ev_tx.send(SessionEvent::Error(text));
    }

    pub fn report_system(&self, text: String) {
        let _ = self.ev_tx.send(SessionEvent::System(text));
    }

    /// Tell everything outside this terminal that a ticket finished
    /// ([`services::notification`](crate::services::notification)).
    ///
    /// Exactly one call site, in the `PassOutcome::Closed` arm, and the reason for
    /// parking it here rather than near the settle is the whole point: this is the
    /// only place in the harness that holds a *completion* instead of an ending.
    /// `agent_settled` says the worker stopped talking, and the settle that follows
    /// an abort, a planner's, and a pass that closed nothing are the same event on
    /// the wire — announcing from any of those would send an all-clear about a pass
    /// that has none to give.
    ///
    /// A queue `send`, never an await: this runs on the task that drives the pass
    /// machine and services `Esc`, so a notifier that hangs must not become a
    /// cancellation that hangs.
    pub(super) fn announce_closed(&self, claim: ActiveBead) {
        tracing::info!("announcing a closed ticket: {claim}");
        self.cfg.notifier.notify(BeadDone {
            id: claim.id,
            title: claim.title,
        });
    }

    /// Tear down the current worker, if any.
    ///
    /// `take()` happens before the kill on purpose: from that instant the loop has
    /// no live pass, so the dying child's own `WorkerGone` carries a serial that
    /// matches nothing and is ignored instead of being read as "the pass in flight
    /// died".
    ///
    /// A pass with no child also has no planner to verify: `planning` goes with it,
    /// because a stale `PlanPass` left across the boundary would make the *next*
    /// pass's settle get diffed against this one's baseline — which is how a
    /// worker's settle ends up being reported as a plan of zero.
    pub async fn close(&mut self) {
        self.planning = None;
        self.set_claim(None);
        if let Some(mut old) = self.pi_rx.take() {
            let _ = old.rpc.kill().await;
        }
    }

    /// Take (or release) the loop's claim, and publish it in the same step.
    ///
    /// Setting the field and sending the event are one function rather than two
    /// because the failure mode of two is the one this whole ticket is about: a
    /// screen claiming to show a ticket the loop is not holding, or a loop holding
    /// one the screen never heard about. Nothing may set `claim` without the
    /// publish, so nothing can.
    fn set_claim(&mut self, claim: Option<ActiveBead>) {
        if self.claim == claim {
            return;
        }
        let _ = self.ev_tx.send(SessionEvent::ActiveBead {
            bead: claim.clone(),
        });
        self.claim = claim;
    }

    /// The ticket this loop holds, if any.
    ///
    /// The post-settle check reads the field directly (`self.claim.clone()`), so
    /// this accessor has no caller inside the binary: it is the way a test asks the
    /// loop what it is accountable for, which is exactly the thing looprs-w7q was
    /// filed to make knowable.
    #[allow(dead_code)] // consumers: beads::tests (the claim is the assertion)
    pub fn claim(&self) -> Option<&ActiveBead> {
        self.claim.as_ref()
    }

    /// What the pass in flight is working on — a bead id, or "the planner" — for
    /// the cancel sentences that have to name a thing rather than say "a pass".
    ///
    /// Derived from `claim` / `planning` rather than kept beside them: two fields
    /// that must be set and cleared together is one field too many, and a label
    /// that outlived the claim it came from would let a cancel name a bead the
    /// loop is not holding.
    pub fn pass_label(&self) -> Option<&str> {
        if let Some(claim) = &self.claim {
            return Some(claim.id.as_str());
        }
        if self.planning.is_some() {
            return Some("the planner");
        }
        None
    }

    /// `Esc`. Tell the in-flight worker to stop, and return the serial of the pass
    /// that was told, `None` when there was nothing to stop.
    ///
    /// The serial comes back rather than a bool because the caller has to arm a
    /// deadline against *that* pass: "did my abort land?" is only answerable about
    /// a pass with a name, and an untagged timer would eventually fire against a
    /// later pass's run and kill work nobody cancelled.
    ///
    /// Fire-and-forget, exactly like Pi's own Esc: `abort` is answered only once
    /// the run has unwound, and parking this loop on that answer would put the
    /// user's next keystroke behind the very run they are trying to stop. The end
    /// of the run arrives where every other end arrives — on the stream — and the
    /// caller's `aborted` flag is what makes it read as a cancellation rather than
    /// as a pass that finished (ADR-0002 Q3).
    pub fn abort_worker(&mut self) -> Option<u64> {
        let worker = self.pi_rx.as_ref()?;
        if let Err(e) = worker.rpc.abort() {
            tracing::warn!("{}: abort did not reach the worker: {e:#}", self.id);
        }
        Some(worker.serial)
    }

    /// Run the planner over `instructions`, remembering what the board looked like
    /// first so the result can be checked (looprs-k7v).
    ///
    /// The snapshot is taken *before* the child is spawned, which is the only order
    /// in which it means anything: a baseline captured after the run is not a
    /// baseline. And if that snapshot cannot be read, the pass is refused rather
    /// than run — an unverifiable planner is the exact failure this ticket is
    /// about, so buying a run that cannot be checked is worse than not buying it,
    /// and the honest error costs one `bd` call instead of a whole pi session.
    pub async fn launch_create_tickets(&mut self, instructions: &str) -> Result<()> {
        tracing::debug!("Launched tickets with these instructions: {}", instructions);
        // A human spoke. In beads mode this is the *only* door a deliberate human
        // instruction comes through, which makes it the acknowledgement that
        // releases the re-work guard (looprs-w7q): every automatic pass stays
        // answerable to the guard, and nothing restarts a refused ticket on a
        // timer or on a Tab.
        self.worked.clear();
        self.close().await;
        let baseline: HashSet<String> = list_status_with(&self.cfg.bd_bin, "open")
            .await?
            .into_iter()
            .map(|b| b.id)
            .collect();

        let args = vec!["--tools", "read,bash"];
        let worker = self.spawn_worker(&args)?;
        let said = worker.said.clone();
        self.set_step(StepCause::Planning);
        let prompt = generate_prompt(PLANNER, instructions);
        let disposition = worker.rpc.prompt(&prompt).await?;
        if disposition == "handled" {
            // pi took the prompt but started no run, so there is no settle coming
            // to trigger the verification either. Dropping the child and reporting
            // is the whole response; `planning` deliberately stays unset so this
            // dead pass cannot be mistaken for one that owes a verdict.
            drop(worker);
            return Err(anyhow!(
                "planner prompt was handled without starting a run; no tickets were requested"
            ));
        }
        self.planning = Some(PlanPass { baseline, said });
        self.pi_rx = Some(worker);
        // A planner pass holds no bead — `pass_label` answers "the planner" for it
        // off `planning`, which is set just above — because there is no ticket here
        // to claim, only one to invent.

        Ok(())
    }

    /// Is the pass in flight a planner's? Lets the no-settle path (`WorkerGone`)
    /// say which half of the loop it lost instead of blaming a worker that was
    /// never running.
    pub fn is_planning(&self) -> bool {
        self.planning.is_some()
    }

    /// Did the planner that just settled actually put tickets on the board?
    ///
    /// `agent_settled` means "the planner stopped talking". It is not, and never
    /// was, evidence of a plan: a planner that misread the instructions, refused
    /// them, or whose every `bd create` failed settled just as cleanly as one that
    /// worked. The board answers the real question, and answers it before a single
    /// worker is paid to read a plan that may not exist.
    ///
    /// Takes the pass rather than borrowing it: one planner pass gets exactly one
    /// verdict, so a second settle off the same child cannot re-diff a board that
    /// has since moved under it.
    pub(super) async fn verify_plan(&mut self) -> PlanCheck {
        let Some(pass) = self.planning.take() else {
            return PlanCheck::NotPlanning;
        };
        let after = match list_status_with(&self.cfg.bd_bin, "open").await {
            Ok(beads) => beads,
            Err(e) => {
                return PlanCheck::Unverifiable {
                    stage: "reading the board after the planner ran",
                    reason: e.to_string(),
                };
            }
        };
        let created: Vec<Bead> = after
            .into_iter()
            .filter(|b| !pass.baseline.contains(b.id()))
            .collect();
        let said = pass.said.lock().unwrap().clone();
        if created.is_empty() {
            PlanCheck::NothingCreated { said }
        } else {
            PlanCheck::Created(created)
        }
    }

    /// Did the worker that just settled actually finish the ticket it was paid for?
    ///
    /// `agent_settled` means the worker stopped talking. It is not evidence that
    /// anything closed, and cannot be: the identical settle arrives whether the
    /// worker shipped the feature, hit a snag and said nothing, or ran out of
    /// steam mid-sentence with the ticket still `open` — and in the last case
    /// `bd ready` hands the *same bead* straight back to the next pass, which is
    /// how a self-advancing loop turns into an uncapped bill. Only the board can
    /// answer, so the loop asks it, one `bd show` per pass, before anything else
    /// is spawned (looprs-w7q).
    pub(super) async fn verify_worker_pass(&self) -> PassOutcome {
        let Some(claim) = self.claim.clone() else {
            return PassOutcome::NothingHeld;
        };
        match show_with(&self.cfg.bd_bin, &claim.id).await {
            Ok(Some(bead)) if bead.is_closed() => PassOutcome::Closed(claim),
            Ok(Some(bead)) if bead.needs_human() => PassOutcome::LeftForHuman(claim, bead.status()),
            Ok(Some(bead)) => PassOutcome::NotClosed(claim, bead.status()),
            // `bd` has never heard of the ticket we are holding. That is not
            // "closed", any more than an empty phone book is "nobody is sick".
            Ok(None) => PassOutcome::NotClosed(claim, BeadStatus::Unknown),
            Err(e) => PassOutcome::Unverifiable(claim, e.to_string()),
        }
    }
}
