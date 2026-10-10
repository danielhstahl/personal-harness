//! The beads machine: [`BeadsLoop`] and the values it passes around.
//!
//! This is the pass boundary. Everything that decides *what the loop does next*
//! — whether a bead is claimable, whether the planner's plan landed, whether
//! the worker's pass closed the ticket it was bought for — is a method on this
//! type, and every one of them returns a value (`Pick`, `PlanCheck`,
//! `PassOutcome`) rather than mutating toward a conclusion, so the caller in
//! [`task`](super::task) decides what the answer costs.
//!
//! The loop is a backend, not UI state: it reports [`SessionEvent`] and never
//! reaches into the App (ADR-0002 Q1).

use super::guards::StepCause;
use super::notes::{MAX_SKIPPED_LISTED, already_worked_note, worker_prompt};

use super::BeadsCmd;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use crate::services::bd::{Bead, BeadStatus, claim_with, list_status_with, ready_with, show_with};
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

    /// Tear down the current session, then work the next ready bead or park.
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

    /// Claim the next bead, then buy the worker for it — in that order.
    ///
    /// The claim comes first because everything after it is paid for: the pass is
    /// only worth reporting, verifying and guarding once the harness actually holds
    /// the ticket. Spawn-first would mean a refused claim still cost a whole `pi`
    /// session, and one pointed at a bead we do not own answers to whoever prompts
    /// it rather than to us (looprs-w7q).
    ///
    /// Also the only place a worker is both spawned *and* prompted, so callers
    /// never see a pi child that is alive but has been given nothing to do.
    async fn work_next_bead(&mut self) -> Result<WorkerPass> {
        let beads = ready_with(&self.cfg.bd_bin).await?;
        let bead = match self.pick_bead(&beads) {
            Pick::Nothing => return Ok(WorkerPass::Idle),
            Pick::AlreadyWorked(bead) => return Err(anyhow!(already_worked_note(&bead))),
            Pick::Work(bead) => bead,
        };
        tracing::info!(bead = %bead.id(), "claiming a worker pass");

        let claim = ActiveBead {
            id: bead.id().to_string(),
            title: bead.title().to_string(),
        };
        claim_with(&self.cfg.bd_bin, &claim.id)
            .await
            .map_err(|e| anyhow!("could not claim {} before buying a worker: {e}", claim.id))?;

        // Two records of one claim, both taken before anything is prompted:
        // `worked` is the guard that outlives the pass, `claim` is the live one the
        // UI and the post-settle check read. Recording the claim *before* the spawn
        // is what covers the pass that never reaches a settle at all — a spawn that
        // fails, a prompt that is refused, a worker that is killed mid-run. Each of
        // those leaves the ticket claimed on the board, and `bd ready` hands it
        // back, so the guard has to already know about it.
        self.worked.insert(claim.id.clone());
        self.set_claim(Some(claim.clone()));

        // The session is built against locals: if any step below fails, `worker`
        // drops here and kill_on_drop reaps it, so a failed pass cannot leave an
        // orphan behind.
        let worker = self.spawn_worker(&[])?;
        let disposition = worker.rpc.prompt(&worker_prompt(&claim)).await?;
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
    /// Three rules, in this order, and each one is a thing that used to go wrong:
    ///
    /// 1. **A ticket that needs a human is skipped, out loud.** `blocked` and
    ///    `deferred` mean somebody — a planner, or a worker that gave up — took it
    ///    out of the loop on purpose, and `bd ready` should not have listed it.
    ///    Spending a metered pass on one is the worst possible answer; staying
    ///    quiet about the skip would make it indistinguishable from a lost ticket.
    /// 2. **A ticket this loop already worked is refused, never re-run.** If the
    ///    first workable bead is one whose pass already ended without closing it,
    ///    starting another pass on it is the runaway this ticket was filed for:
    ///    `bd ready` will keep offering a bead for exactly as long as it stays
    ///    open, and every pass is billed. The refusal parks the loop with its two
    ///    ways out, both of which belong to a human.
    /// 3. **Otherwise the first workable bead**, in `bd`'s own priority order.
    ///
    /// What is deliberately *not* a rule: an unknown status is workable. Skipping
    /// what this build cannot classify would let a `bd` upgrade silently empty the
    /// board, which is looprs-037's conflation wearing a different hat.
    pub(super) fn pick_bead(&self, beads: &[Bead]) -> Pick {
        let mut skipped: Vec<Bead> = Vec::new();
        let mut pick = Pick::Nothing;
        for bead in beads {
            if bead.needs_human() {
                skipped.push(bead.clone());
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
