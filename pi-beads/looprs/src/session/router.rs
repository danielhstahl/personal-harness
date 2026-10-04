//! Who owns the sessions, and how their events reach the screen (ADR-0002 Q4).
//!
//! **Decision: a Router actor owns every live session. `App` only renders.**
//!
//! The alternative — `App` holding `HashMap<TerminalType, Box<dyn Session>>` — fails
//! on one observation: `App` is driven by the draw loop and must stay cheap enough to
//! hit 60fps, while session control is full of awaits (spawn a child, await a prompt
//! response, close a stdin). Those two must not share a task, or a slow `prompt()`
//! stalls the screen and `Esc` queues behind it.
//!
//! Channel topology (one process; every arrow is a channel, never a shared lock):
//!
//! ```text
//!   crossterm EventStream ──> run() loop ──keys──> App.input ──UiCommand──> cmd_tx
//!                                                     │                      (mpsc, 16)
//!   app_tx (unbounded Msg) <───────────────────────────┘                        │
//!       │                                                                       │
//!       │   ┌───────────────────────── Router task (the only owner) ────────────┘
//!       │   │  sessions: HashMap<TerminalType, Managed{ id, Box<dyn Session>, pump }>
//!       │   │
//!       │   │   pump(id, rx) ── SessionEvent ──wrap(id, .)──> Msg ──┐
//!       │   │   pump(id, rx) ── SessionEvent ──wrap(id, .)──> Msg ──┤──> app_tx
//!       │   │   pump(id, rx) ── SessionEvent ──wrap(id, .)──> Msg ──┘
//!       │   └───────────────────────────────────────────────────────┘
//!       ▼
//!   run() loop: Msg -> App.update(view state) -> flush -> insert_before -> draw
//! ```
//!
//! Three rules fall out of it:
//!
//! * Only the Router task touches a `dyn Session`. There is exactly one owner at a
//!   time, so no mutex, and no torn lifecycle.
//! * Only the run() loop touches `App`, and only in response to a `Msg`.
//! * Sessions never learn about the UI: they emit [`SessionEvent`]. [`wrap`] is the
//!   one place that stamps provenance onto a [`Msg`].

#![allow(dead_code)] // skeleton: not wired into main.rs until looprs-05j deletes this line

use std::collections::HashMap;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{Msg, UiCommand};
use crate::session::{
    ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus, TerminalType,
};

/// The single seam where a session's event becomes a UI message.
///
/// Pure and total by construction: there is no way to produce a `Msg::Agent` /
/// `Msg::BashOutput` / `Msg::BeadStep` without a `SessionId` attached, because
/// `wrap` is the only constructor of those variants that anything calls.
pub fn wrap(id: SessionId, ev: SessionEvent) -> Msg {
    match ev {
        SessionEvent::Agent(event) => Msg::Agent { session: id, event },
        SessionEvent::BashOutput { stream, chunk } => Msg::BashOutput {
            session: id,
            stream,
            chunk,
        },
        SessionEvent::BeadStep(step) => Msg::BeadStep { session: id, step },
        SessionEvent::System(text) => Msg::System {
            session: Some(id),
            text,
        },
        SessionEvent::Error(text) => Msg::Error {
            session: Some(id),
            text,
        },
        SessionEvent::Exited { reason } => Msg::SessionDown {
            session: id,
            reason,
        },
    }
}

/// A live session plus the task that forwards its events.
pub(crate) struct Managed {
    id: SessionId,
    session: Box<dyn Session>,
    /// The pump is abortable. Killing a session means `shutdown()` **and**
    /// `pump.abort()`: the orderly path for the child, and a hard structural
    /// guarantee that no event produced after the kill can reach the UI — not
    /// "filtered out downstream", but "there is no path for it". That is what
    /// looprs-05j's "no further events from it are rendered" needs.
    pump: JoinHandle<()>,
}

/// Owns every live session. One entry per [`TerminalType`], which *is* the
/// "at most one child per terminal state" invariant — the map cannot hold two.
///
/// It is not the total number of children that is bounded (up to three may be
/// alive; see the lifecycle table in ADR-0002 Q3), it is the number per state.
pub struct Router {
    cfg: SessionConfig,
    /// Every `Msg` produced by a session goes here, into the UI loop.
    app_tx: mpsc::UnboundedSender<Msg>,
    active: TerminalType,
    sessions: HashMap<TerminalType, Managed>,
    /// Generation source. Every spawn takes one; nothing else may invent ids.
    next_gen: u64,
}

impl Router {
    pub fn new(
        active: TerminalType,
        cfg: SessionConfig,
        app_tx: mpsc::UnboundedSender<Msg>,
    ) -> Self {
        Self {
            cfg,
            app_tx,
            active,
            sessions: HashMap::new(),
            next_gen: 1,
        }
    }

    pub fn active_mode(&self) -> TerminalType {
        self.active
    }

    pub fn session(&self, mode: TerminalType) -> Option<&Managed> {
        self.sessions.get(&mode)
    }

    /// For the status row: which modes have a live child right now (looprs-guh).
    pub fn live_modes(&self) -> Vec<TerminalType> {
        self.sessions
            .iter()
            .filter(|(_, m)| m.session.status().is_alive())
            .map(|(mode, _)| *mode)
            .collect()
    }

    pub fn status_of(&self, mode: TerminalType) -> SessionStatus {
        self.sessions
            .get(&mode)
            .map(|m| m.session.status())
            .unwrap_or(SessionStatus::NotStarted)
    }

    /// Handle one UI command. Called from the Router task, one at a time, in order.
    ///
    /// Contract for the implementation (looprs-05j), none of which may block on a
    /// child process:
    ///
    /// * `Submit { mode, text }` — route by `mode`, because that is where the user
    ///   *typed it*, i.e. declared intent. If `mode != self.active`, the switch
    ///   raced the submit: **drop it** with a visible `Msg::System` ("switched
    ///   modes; message not sent") rather than guessing. Never resurrect a session
    ///   to satisfy a submit into a mode the user has since left.
    /// * `SwitchMode { to }` — [`Router::switch_to`].
    /// * `Cancel` (Esc) — `abort()` the **active** session only. Never fan out.
    /// * `BeadsNext` — legacy shim for the App-driven beads advance; it goes away
    ///   with looprs-msj, which moves the transition inside `BeadsSession`.
    pub async fn handle(&mut self, cmd: UiCommand) -> Result<()> {
        let _ = cmd;
        Err(anyhow::anyhow!(
            "Router::handle: not implemented yet (looprs-05j)"
        ))
    }

    /// Perform a mode switch per the per-mode policy (ADR-0002 Q3).
    ///
    /// Order matters, and the *implementation* steps are:
    ///
    /// 1. `from.switch_away_policy()`:
    ///    * `KeepRunning` (Pi, Bash) — do nothing to the child; `set_active(false)`
    ///      only. Note the boxed session must **not** be dropped here: dropping is
    ///      what kills the child, and keeping it warm is the whole point.
    ///    * `DrainThenPark` (Beads) — `set_active(false)`; the pass already
    ///      running finishes, and no *new* pass starts while hidden. No kill:
    ///      a Tab must not throw away a run that is already paid for.
    /// 2. `self.active = to`.
    /// 3. Bring the target up lazily per its own rule: Bash and Pi create no child
    ///    until the first submit; Beads owns a loop driver that spawns a worker
    ///    only when the board has something ready.
    /// 4. `to_session.set_active(true)`, and if it had parked because of the switch,
    ///    resume it — but only from a parked state, so a re-entry can never
    ///    double-spawn a worker.
    /// 5. Emit one `Msg::System { session: Some(target) }` separator
    ///      ("── switched to Pi ──") so a scrollback that later mixes modes is
    ///      readable. This is the only cross-session write the router ever does.
    pub async fn switch_to(&mut self, to: TerminalType) -> Result<()> {
        let _ = to;
        Err(anyhow::anyhow!(
            "Router::switch_to: not implemented yet (looprs-05j)"
        ))
    }

    /// Teardown for app exit (ADR-0002 Q3, "on quit").
    ///
    /// Bounded and ordered: `shutdown()` each session (close stdin first so pi can
    /// dispose its runtime), then bound-wait for the `Exited` each one owes us, then
    /// `pump.abort()` past the grace period. Never hangs on a wedged child — the
    /// caller passes the deadline (looprs-ecr owns the exact numbers).
    pub async fn shutdown_all(&mut self) -> Result<()> {
        Err(anyhow::anyhow!(
            "Router::shutdown_all: not implemented yet (looprs-ecr)"
        ))
    }

    /// Forward one session's events into the UI, with its id on every message.
    ///
    /// Called once, at spawn time, by the router. Real code, not a stub: this is
    /// the envelope contract, and the guarantee below is what lets `App` seal a
    /// transcript unconditionally.
    ///
    /// Guarantee: **exactly one** [`Msg::SessionDown`] per session ever created. If
    /// a session's stream ends without it (sender dropped, task panicked, child died
    /// without an orderly exit), the pump synthesizes
    /// [`ExitReason::Unknown`] so the App's "always seal" rule never deadlocks on
    /// an entry that will never be closed.
    pub fn pump(
        &self,
        id: SessionId,
        mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    ) -> JoinHandle<()> {
        let tx = self.app_tx.clone();
        tokio::spawn(async move {
            let mut exited = false;
            while let Some(ev) = rx.recv().await {
                exited |= matches!(ev, SessionEvent::Exited { .. });
                if tx.send(wrap(id, ev)).is_err() {
                    return; // UI is gone; nothing to forward to
                }
            }
            if !exited {
                let _ = tx.send(Msg::SessionDown {
                    session: id,
                    reason: ExitReason::Unknown,
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::BeadStep;

    /// The envelope mapping, pinned variant by variant. If somebody later drops the
    /// session tag from a variant, this is the test that says why it is there.
    #[test]
    fn wrap_stamps_the_origin_on_every_session_message() {
        let id = SessionId::new(TerminalType::Beeds, 3);
        let cfg = SessionConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel::<Msg>();
        let router = Router::new(TerminalType::Beeds, cfg, tx);

        // `Agent` is the variant looprs-msj is about: it must name its producer.
        let Msg::Agent { session, event } =
            wrap(id, SessionEvent::Agent(crate::app::PiEvent::AgentSettled))
        else {
            panic!("Agent must map to Msg::Agent");
        };
        assert!(matches!(event, crate::app::PiEvent::AgentSettled));
        assert_eq!(session, id);

        let Msg::BashOutput {
            session,
            stream,
            chunk,
            ..
        } = wrap(
            id,
            SessionEvent::BashOutput {
                stream: crate::session::ByteStream::Merged,
                chunk: "$ ".into(),
            },
        )
        else {
            panic!("BashOutput must map to Msg::BashOutput");
        };
        assert_eq!(session, id);
        assert_eq!(chunk, "$ ");
        assert_eq!(stream, crate::session::ByteStream::Merged);

        let Msg::BeadStep { session: s2, step } =
            wrap(id, SessionEvent::BeadStep(BeadStep::WorkTickets))
        else {
            panic!("BeadStep must map to Msg::BeadStep");
        };
        assert_eq!(s2, id);
        assert_eq!(step, BeadStep::WorkTickets);

        let Msg::System { session, text } = wrap(id, SessionEvent::System("working".into())) else {
            panic!()
        };
        assert_eq!(session, Some(id));
        assert_eq!(text, "working");

        let Msg::SessionDown { session, reason } = wrap(
            id,
            SessionEvent::Exited {
                reason: ExitReason::Shutdown,
            },
        ) else {
            panic!()
        };
        assert_eq!(session, id);
        assert_eq!(reason, ExitReason::Shutdown);
        // `router` is only here to prove pump()/wrap() are reachable from it.
        assert_eq!(router.active_mode(), TerminalType::Beeds);
    }

    /// Exactly-one-SessionDown, including the sloppy case where the session just
    /// stops talking. Without this the App cannot trust "seal on SessionDown".
    #[tokio::test]
    async fn a_quietly_dropped_stream_still_produces_one_session_down() {
        let id = SessionId::new(TerminalType::Pi, 9);
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let router = Router::new(TerminalType::Pi, SessionConfig::default(), tx);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();

        let handle = router.pump(id, ev_rx);
        drop(ev_tx); // stream ends with no Exited at all
        // Await first: the pump sends before it exits, so a try_recv race here would
        // make this test flaky rather than wrong.
        handle.await.unwrap();

        let msgs: Vec<Msg> = {
            let mut v = Vec::new();
            while let Ok(m) = rx.try_recv() {
                v.push(m);
            }
            v
        };
        let down: Vec<&Msg> = msgs
            .iter()
            .filter(|m| matches!(m, Msg::SessionDown { session, .. } if *session == id))
            .collect();
        assert_eq!(down.len(), 1, "exactly one SessionDown expected: {msgs:?}");
        assert!(matches!(
            down[0],
            Msg::SessionDown {
                reason: ExitReason::Unknown,
                ..
            }
        ));
    }

    /// And when the session *does* say Exited, the pump must not add a second one.
    #[tokio::test]
    async fn an_orderly_exit_is_not_duplicated() {
        let id = SessionId::new(TerminalType::Bash, 1);
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let router = Router::new(TerminalType::Bash, SessionConfig::default(), tx);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();

        let handle = router.pump(id, ev_rx);
        ev_tx
            .send(SessionEvent::Exited {
                reason: ExitReason::Shutdown,
            })
            .unwrap();
        drop(ev_tx);
        handle.await.unwrap();

        let mut downs = 0;
        while let Ok(m) = rx.try_recv() {
            if matches!(m, Msg::SessionDown { .. }) {
                downs += 1;
            }
        }
        assert_eq!(downs, 1);
    }

    /// The structural invariant: `sessions` is keyed by `TerminalType`, so two
    /// live sessions of the same state cannot both be registered.
    #[test]
    fn one_slot_per_terminal_state() {
        let (tx, _rx) = mpsc::unbounded_channel::<Msg>();
        let router = Router::new(TerminalType::Beeds, SessionConfig::default(), tx);
        assert!(router.sessions.is_empty());
        assert_eq!(
            router.status_of(TerminalType::Pi),
            SessionStatus::NotStarted
        );
    }
}
