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
//!       │   │   pump(id, rx) ── SessionEvent ──wrap(id, .)──> Msg ──┼──> app_tx
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

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::app::{Msg, UiCommand};
use crate::session::{
    ExitReason, Session, SessionConfig, SessionEvent, SessionFactory, SessionId, SessionStatus,
    TerminalType, default_factory,
};

/// How long [`Router::shutdown_all`] waits for a session to say goodbye before it
/// stops waiting. Exact numbers are looprs-ecr's; this is the placeholder that keeps
/// quitting possible, which is the property.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
        SessionEvent::ActiveBead { bead } => Msg::ActiveBead { session: id, bead },
        SessionEvent::System(text) => Msg::System {
            session: Some(id),
            text,
        },
        SessionEvent::RestoreInput { text } => Msg::RestoreInput { session: id, text },
        SessionEvent::Error(text) => Msg::Error {
            session: Some(id),
            text,
        },
        SessionEvent::ScreenHeld { active } => Msg::ScreenHeld { session: id, active },
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
    factory: SessionFactory,
    /// Every `Msg` produced by a session goes here, into the UI loop.
    app_tx: mpsc::UnboundedSender<Msg>,
    active: TerminalType,
    sessions: HashMap<TerminalType, Managed>,
    /// The last size we were told, kept so that a session created *after* the last
    /// resize still gets it. Without this the size is shouted once into an empty
    /// room: Bash is spawned lazily, so the startup resize and every resize before
    /// the first command land nowhere, and the shell opens at the library default
    /// and wraps its output at 80 columns inside a 132-column window forever
    /// (measured: `stty size` said `24 80` in a 40x132 pty).
    last_size: Option<(u16, u16)>,
    /// Generation source. Every spawn takes one; nothing else may invent ids.
    next_gen: u64,
}

impl Router {
    /// The production router: real backends from [`default_factory`].
    pub fn new(
        active: TerminalType,
        cfg: SessionConfig,
        app_tx: mpsc::UnboundedSender<Msg>,
    ) -> Self {
        Self::with_factory(active, app_tx, default_factory(cfg))
    }

    /// As [`Router::new`], with the session constructor injected. Every lifecycle
    /// test uses this to drive real policy with sessions whose behavior it controls.
    pub fn with_factory(
        active: TerminalType,
        app_tx: mpsc::UnboundedSender<Msg>,
        factory: SessionFactory,
    ) -> Self {
        Self {
            factory,
            app_tx,
            active,
            sessions: HashMap::new(),
            last_size: None,
            next_gen: 1,
        }
    }

    pub fn active_mode(&self) -> TerminalType {
        self.active
    }

    pub fn session(&self, mode: TerminalType) -> Option<&Managed> {
        self.sessions.get(&mode)
    }

    /// The id of the session currently standing for `mode`, if any. The App needs
    /// this to seed a view with the identity the Router issued rather than a
    /// placeholder of its own.
    pub fn id_of(&self, mode: TerminalType) -> Option<SessionId> {
        self.sessions.get(&mode).map(|m| m.id)
    }

    /// For the status row: which modes have a live child right now (looprs-guh).
    pub fn live_modes(&self) -> Vec<TerminalType> {
        let mut live: Vec<TerminalType> = self
            .sessions
            .iter()
            .filter(|(_, m)| m.session.status().is_alive())
            .map(|(mode, _)| *mode)
            .collect();
        live.sort_by_key(|m| {
            TerminalType::ALL
                .iter()
                .position(|x| x == m)
                .unwrap_or(usize::MAX)
        });
        live
    }

    pub fn status_of(&self, mode: TerminalType) -> SessionStatus {
        self.sessions
            .get(&mode)
            .map(|m| m.session.status())
            .unwrap_or(SessionStatus::NotStarted)
    }

    /// Bring up the session that is already active. App startup calls this before
    /// [`Router::run`], so the beads loop self-starts exactly the way it always
    /// did — just owned now, instead of hand-wired in `main`.
    pub async fn boot(&mut self) -> Result<()> {
        let mode = self.active;
        self.ensure(mode)?;
        if let Some(m) = self.sessions.get_mut(&mode) {
            m.session.set_active(true)?;
        }
        Ok(())
    }

    /// Handle one UI command. Called from the Router task, one at a time, in order.
    ///
    /// The in-flight-command policy (looprs-05j, explicit rather than accidental):
    ///
    /// * `Submit` for a mode the user has already left is **dropped, visibly**. Not
    ///   routed to the mode it was typed into — the user cannot see that mode any
    ///   more, so a silent queue there is worse than a dropped one — and never by
    ///   resurrecting a session to honor it.
    /// * `SwitchMode` is never blocked behind a submit; the router serializes, so
    ///   "Tab then Submit" and "Submit then Tab" each have exactly one answer.
    /// * `Cancel` touches the **active** session only. Fanning one keypress out to
    ///   every live session is the worst possible reading of Esc.
    /// * Nothing here awaits a child process. `send_text`/`set_active`/`abort` are
    ///   queue-and-return by contract, which is why Esc cannot queue behind a
    ///   30-second model call.
    pub async fn handle(&mut self, cmd: UiCommand) -> Result<()> {
        match cmd {
            UiCommand::Submit { mode, text } => {
                if mode != self.active {
                    tracing::debug!(?mode, active = ?self.active, "submit raced a switch; dropped");
                    let _ = self.app_tx.send(Msg::System {
                        session: None,
                        text: format!("switched modes; the {} message was not sent", mode.label()),
                    });
                    return Ok(());
                }
                let id = self.ensure(mode)?;
                let session = &mut self.sessions.get_mut(&mode).expect("just ensured").session;
                if let Err(e) = session.send_text(text) {
                    // A stub backend refusing (`ctn`/`553`) or a child that will
                    // not take it: the user pressed Enter, so the answer goes on
                    // their screen, attributed.
                    let _ = self.app_tx.send(Msg::Error {
                        session: Some(id),
                        text: format!("{}: {e:#}", mode.label()),
                    });
                }
                Ok(())
            }
            UiCommand::SwitchMode { from, to } => {
                if from != self.active {
                    // The input box is authoritative about where the *keystroke*
                    // came from, but the router is authoritative about what it is
                    // showing. Ours wins; the difference is logged, not guessed at.
                    tracing::debug!(?from, active = ?self.active, "stale switch `from`");
                }
                self.switch_to(to).await
            }
            UiCommand::Cancel => {
                let mode = self.active;
                match self.sessions.get_mut(&mode) {
                    // Nothing to cancel. Silently: Esc means "stop the thing", and
                    // there is no thing. An error here would train the user to
                    // ignore errors, which is a worse outcome than a no-op.
                    None => Ok(()),
                    Some(m) => {
                        let id = m.id;
                        let session = &mut m.session;
                        if let Err(e) = session.abort() {
                            let _ = self.app_tx.send(Msg::Error {
                                session: Some(id),
                                text: format!("cancel: {e:#}"),
                            });
                        }
                        Ok(())
                    }
                }
            }
            UiCommand::Keys { mode, bytes } => {
                // Keystrokes go to the mode they were typed into, and only while
                // that mode is the one on screen — the same rule as `Submit`. A
                // raw byte must never reach a session the user is not looking at.
                if mode != self.active {
                    tracing::debug!(?mode, active = ?self.active, "keys raced a switch; dropped");
                    return Ok(());
                }
                match self.sessions.get_mut(&mode) {
                    None => Ok(()),
                    Some(m) => {
                        let id = m.id;
                        if let Err(e) = m.session.send_bytes(bytes) {
                            let _ = self.app_tx.send(Msg::Error {
                                session: Some(id),
                                text: format!("keys: {e:#}"),
                            });
                        }
                        Ok(())
                    }
                }
            }
            UiCommand::Resize { rows, cols } => {
                // Recorded first, so a session born later still gets it.
                if rows > 0 && cols > 0 {
                    self.last_size = Some((rows, cols));
                }
                // Every live session gets the real size, not just the active one:
                // a shell running while the user looks at another mode wraps its
                // output once, wrongly, and that wrong wrap is what lands in the
                // transcript permanently (ADR-0001 rule 6).
                //
                // `values_mut` over what exists, never `ensure` — dragging a
                // window must not spawn a child. A failed delivery is a session on
                // its way out; the log is the right weight for that.
                for m in self.sessions.values_mut() {
                    let id = m.id;
                    if let Err(e) = m.session.resize(rows, cols) {
                        tracing::warn!("{id} resize to {rows}x{cols} not delivered: {e:#}");
                    }
                }
                Ok(())
            }
        }
    }

    /// Perform a mode switch per the per-mode policy (ADR-0002 Q3).
    ///
    /// Order matters:
    /// 1. tell the session being left that it is no longer on screen — and **only**
    ///    that. Both `KeepRunning` and `DrainThenPark` mean "do not kill it"; the
    ///    difference is what the session does with the call. Dropping the `Box` is
    ///    what kills a child, and a Tab must not do that.
    /// 2. move the active pointer.
    /// 3. bring the target up (creating it costs nothing: Bash and Pi spawn no
    ///    child until the first submit, and Beads spawns one only if the board has
    ///    something ready).
    /// 4. tell the target it is now on screen — which is what resumes a parked
    ///    beads loop, from its parked state and nowhere else.
    /// 5. write one separator line into the target's transcript, so a scrollback
    ///    that later mixes modes is readable. This is the only cross-session write
    ///    the router ever does.
    pub async fn switch_to(&mut self, to: TerminalType) -> Result<()> {
        let from = self.active;
        if from == to {
            return Ok(());
        }
        let policy = from.switch_away_policy();
        if let Some(m) = self.sessions.get_mut(&from) {
            if let Err(e) = m.session.set_active(false) {
                tracing::warn!(mode = ?from, ?policy, "set_active(false) failed: {e:#}");
            }
        }
        self.active = to;
        if let Err(e) = self.ensure(to) {
            let _ = self.app_tx.send(Msg::Error {
                session: None,
                text: format!("could not open {}: {e:#}", to.label()),
            });
            return Err(e);
        }
        if let Some(m) = self.sessions.get_mut(&to) {
            if let Err(e) = m.session.set_active(true) {
                tracing::warn!(mode = ?to, "set_active(true) failed: {e:#}");
            }
        }
        let id = self.sessions[&to].id;
        let _ = self.app_tx.send(Msg::System {
            session: Some(id),
            text: format!("── switched to {} ──", to.label()),
        });
        Ok(())
    }

    /// Teardown for app exit (ADR-0002 Q3, "on quit").
    ///
    /// Bounded and ordered: `shutdown()` each session (close stdin first so pi can
    /// dispose its runtime), then bound-wait for the `Exited` each one owes us, then
    /// stop waiting past the grace period rather than hanging on a wedged child.
    /// The sessions stay alive for the duration of the wait — dropping one early is
    /// how a child gets orphaned.
    pub async fn shutdown_all(&mut self, grace: Duration) -> Result<()> {
        let mut leaving: Vec<(SessionId, Box<dyn Session>, JoinHandle<()>)> = Vec::new();
        for (_, m) in self.sessions.drain() {
            let Managed {
                id,
                mut session,
                pump,
            } = m;
            if let Err(e) = session.shutdown() {
                tracing::warn!("{id} shutdown failed: {e:#}");
            }
            leaving.push((id, session, pump));
        }
        for (id, _session, pump) in leaving {
            if timeout(grace, pump).await.is_err() {
                // Past the grace period we stop waiting. The pump's session has
                // been told to die; turning that into a SIGKILL is looprs-ecr's
                // job, hanging the exit on it is not.
                tracing::warn!(
                    "{id} did not report its exit within {grace:?}; abandoning its pump"
                );
            }
        }
        Ok(())
    }

    /// The Router task body: serve commands until the UI closes the channel, then
    /// take every session with us.
    pub async fn run(mut self, mut cmd_rx: mpsc::Receiver<UiCommand>) -> Result<()> {
        while let Some(cmd) = cmd_rx.recv().await {
            if let Err(e) = self.handle(cmd).await {
                let _ = self.app_tx.send(Msg::Error {
                    session: None,
                    text: format!("router: {e:#}"),
                });
            }
        }
        self.shutdown_all(SHUTDOWN_GRACE).await
    }

    /// Make sure `mode` has a live session, and return its id.
    ///
    /// Lazy by design: this is the only place the router creates a session, so the
    /// "spawns nothing until there is input / a reason" rule has one home. An
    /// existing session is reused *unless* it is `Dead`, in which case the corpse is
    /// retired first and the new generation gets a fresh id — which is what makes a
    /// stale event distinguishable rather than merely unlikely.
    fn ensure(&mut self, mode: TerminalType) -> Result<SessionId> {
        let mut replaced: Option<SessionId> = None;
        if let Some(m) = self.sessions.get(&mode) {
            if !matches!(m.session.status(), SessionStatus::Dead) {
                return Ok(m.id);
            }
            replaced = Some(m.id);
            // Retire the dead generation. The pump is cut first: after this point
            // that incarnation has no path to the UI at all. Any trailing
            // `SessionDown` it does not get to send is covered by the App's
            // adopt-a-new-generation rule, which seals the old view regardless.
            m.pump.abort();
            self.sessions.remove(&mode);
        }
        let generation = self.next_gen;
        self.next_gen += 1;
        let spawned = (self.factory)(mode, generation)?;
        let id = spawned.session.id();
        if id.mode != mode {
            return Err(anyhow!(
                "session factory produced {id} for mode {}",
                mode.label()
            ));
        }
        let pump = self.pump(id, spawned.events);
        self.sessions.insert(
            mode,
            Managed {
                id,
                session: spawned.session,
                pump,
            },
        );
        // A session born after the last resize would otherwise open at the default
        // size and stay there: the resize it needed was broadcast before it existed.
        if let Some((rows, cols)) = self.last_size {
            if let Err(e) = self
                .sessions
                .get_mut(&mode)
                .expect("just inserted")
                .session
                .resize(rows, cols)
            {
                tracing::warn!("{id} initial size {rows}x{cols} not applied: {e:#}");
            }
        }
        // A replacement is said out loud, and it is said here, because nobody else
        // can. The new session does not know it is a replacement (it never had a
        // child), and the old one is already off the air. Without this the user gets
        // the dying session's last words and then a stranger's banner, with nothing
        // in between to say they are the same pane — which reads as a crash rather
        // than a restart. (looprs-553: "restart it with a visible notice".)
        if let Some(old) = replaced {
            let _ = self.app_tx.send(Msg::System {
                session: Some(id),
                text: format!("{old} was gone; started {id} in its place"),
            });
        }
        Ok(id)
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
    use crate::testing::{FakeBackend, fake};

    fn router_with(active: TerminalType) -> (Router, mpsc::UnboundedReceiver<Msg>, FakeBackend) {
        let (tx, rx) = mpsc::unbounded_channel::<Msg>();
        let backend = FakeBackend::new();
        let router = Router::with_factory(active, tx, backend.factory());
        (router, rx, backend)
    }

    /// Drain whatever has arrived so far, as stable strings.
    fn drain(rx: &mut mpsc::UnboundedReceiver<Msg>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(match m {
                Msg::Agent { session, .. } => format!("agent@{session}"),
                Msg::BashOutput { session, .. } => format!("bash@{session}"),
                Msg::BeadStep { session, step } => format!("step@{session} {step:?}"),
                Msg::ActiveBead { session, bead } => format!(
                    "active_bead@{session} {}",
                    bead.map(|b| b.id).unwrap_or_else(|| "-".into())
                ),
                Msg::SessionDown { session, reason } => format!("down@{session} {reason:?}"),
                // The full `SessionId` is in the string on purpose: the staleness
                // tests look for a specific incarnation, and a bare mode label would
                // match every one of them.
                Msg::Error { session, text } => format!(
                    "error[{}]: {text}",
                    session
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "harness".into())
                ),
                Msg::System { session, text } => format!(
                    "system[{}]: {text}",
                    session
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "harness".into())
                ),
                Msg::RestoreInput { session, text } => {
                    format!("restore[{session}]: {text}")
                }
                Msg::Term(_) | Msg::Tick => "ui".into(),
                Msg::ScreenHeld { session, active } => format!("screen@{session}:{active}"),
            });
        }
        out
    }

    fn tab(from: TerminalType, to: TerminalType) -> UiCommand {
        UiCommand::SwitchMode { from, to }
    }

    // ------------------------------ the envelope ------------------------------

    /// The envelope mapping, pinned variant by variant. If somebody later drops the
    /// session tag from a variant, this is the test that says why it is there.
    #[test]
    fn wrap_stamps_the_origin_on_every_session_message() {
        let id = SessionId::new(TerminalType::Beeds, 3);
        let (tx, _rx) = mpsc::unbounded_channel::<Msg>();
        let router = Router::new(TerminalType::Beeds, SessionConfig::default(), tx);

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

        // Esc's queued-text restore is tagged too: a Pi cancel must not be able to
        // type into another mode's input box.
        let Msg::RestoreInput {
            session: s3,
            text: t3,
        } = wrap(
            id,
            SessionEvent::RestoreInput {
                text: "queued".into(),
            },
        )
        else {
            panic!("RestoreInput must map to Msg::RestoreInput");
        };
        assert_eq!(s3, id);
        assert_eq!(t3, "queued");
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

        let msgs = drain(&mut rx);
        let downs: Vec<&String> = msgs
            .iter()
            .filter(|m| m.starts_with(&format!("down@{id}")))
            .collect();
        assert_eq!(downs.len(), 1, "exactly one SessionDown expected: {msgs:?}");
        assert!(downs[0].contains("Unknown"));
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

        let downs = drain(&mut rx)
            .iter()
            .filter(|m| m.starts_with(&format!("down@{id}")))
            .count();
        assert_eq!(downs, 1);
    }

    /// The structural invariant: `sessions` is keyed by `TerminalType`, so two
    /// live sessions of one state cannot both be registered.
    #[test]
    fn one_slot_per_terminal_state() {
        let (router, _rx, _b) = router_with(TerminalType::Beeds);
        assert!(router.sessions.is_empty());
        assert_eq!(
            router.status_of(TerminalType::Pi),
            SessionStatus::NotStarted
        );
    }

    // ------------------------------- switching -------------------------------

    /// A Tab is a view change, not a Cancel: the session being left is told it is
    /// off-screen, and nothing is shut down. That holds for *both* policies —
    /// `KeepRunning` and `DrainThenPark` differ in what the session does with the
    /// call, never in whether it dies.
    #[tokio::test]
    async fn switching_away_never_shuts_a_session_down() {
        let (mut router, mut rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        assert!(!backend.was_called("shutdown"), "boot shuts nothing");

        router
            .handle(tab(TerminalType::Beeds, TerminalType::Pi))
            .await
            .unwrap();

        let log = backend.log();
        assert!(
            log.iter()
                .any(|c| c.starts_with("set_active false") && c.contains("Beeds")),
            "the beads session was told it went off-screen: {log:?}"
        );
        assert!(
            !backend.was_called("shutdown"),
            "a Tab must not shut a session down: {log:?}"
        );
        assert!(
            log.iter()
                .any(|c| c.starts_with("set_active true") && c.contains("Pi")),
            "the Pi session was brought up as the new active one: {log:?}"
        );
        assert_eq!(router.active_mode(), TerminalType::Pi);
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| m.contains("switched to Pi")),
            "the switch is visible in the transcript: {msgs:?}"
        );
    }

    /// Warm modes: switching back returns the *same* session, so nothing respawns
    /// and there is no cold-start latency on return.
    #[tokio::test]
    async fn warm_modes_survive_a_switch_and_are_not_recreated() {
        let (mut router, _rx, backend) = router_with(TerminalType::Pi);
        router.boot().await.unwrap();
        router
            .handle(UiCommand::Submit {
                mode: TerminalType::Pi,
                text: "hello".into(),
            })
            .await
            .unwrap();
        let first = router.session(TerminalType::Pi).unwrap().id;
        assert_eq!(backend.spawn_count(TerminalType::Pi), 1);

        router
            .handle(tab(TerminalType::Pi, TerminalType::Bash))
            .await
            .unwrap();
        router
            .handle(tab(TerminalType::Bash, TerminalType::Pi))
            .await
            .unwrap();

        assert_eq!(
            backend.spawn_count(TerminalType::Pi),
            1,
            "a warm Pi session is kept, not respawned"
        );
        assert_eq!(
            router.session(TerminalType::Pi).unwrap().id,
            first,
            "switching back returns the same session, not a new incarnation"
        );
    }

    /// One session per terminal type, however hard the Tab key is thrashed.
    #[tokio::test]
    async fn thrashing_the_tab_key_cannot_produce_two_sessions_of_one_mode() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        for _ in 0..6 {
            for to in TerminalType::ALL {
                let from = router.active_mode();
                router.handle(tab(from, to)).await.unwrap();
            }
        }
        assert_eq!(
            router.sessions.len(),
            3,
            "the HashMap key is the one-per-mode invariant"
        );
        for mode in TerminalType::ALL {
            assert_eq!(
                backend.spawn_count(mode),
                1,
                "{mode:?} spawned more than once: {:?}",
                backend.log()
            );
        }
    }

    /// Switching into a mode must not spawn a *process*; the session object is not
    /// the child. (The Beads side of this — no worker until `bd ready` has
    /// something — is pinned in `session::beads::tests`.)
    #[tokio::test]
    async fn switching_into_a_mode_builds_no_child() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        router
            .handle(tab(TerminalType::Beeds, TerminalType::Bash))
            .await
            .unwrap();
        router
            .handle(tab(TerminalType::Bash, TerminalType::Pi))
            .await
            .unwrap();

        // Bash/Pi sessions exist but nothing was submitted, so no backend work ran.
        assert!(router.session(TerminalType::Bash).is_some());
        assert_eq!(backend.calls("send_text"), 0);
        assert_eq!(router.status_of(TerminalType::Pi), SessionStatus::Idle);
        assert!(
            !router.status_of(TerminalType::Pi).is_busy(),
            "a mode you have only looked at is not doing anything"
        );
    }

    /// Submitting into a mode the user has since left is dropped, and says so. The
    /// session for the abandoned mode is not conjured back to honor it.
    #[tokio::test]
    async fn a_submit_that_lost_the_race_with_a_switch_is_dropped_visibly() {
        let (mut router, mut rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();

        // The user Tabbed away; this submit, queued before the Tab, arrives after.
        router
            .handle(tab(TerminalType::Beeds, TerminalType::Bash))
            .await
            .unwrap();
        drain(&mut rx);

        router
            .handle(UiCommand::Submit {
                mode: TerminalType::Beeds,
                text: "make tickets for X".into(),
            })
            .await
            .unwrap();

        let msgs = drain(&mut rx);
        assert!(
            msgs.iter()
                .any(|m| m.contains("switched modes") && m.contains("not sent")),
            "the dropped message is visible: {msgs:?}"
        );
        assert_eq!(
            backend.calls("send_text"),
            0,
            "a submit into a mode the user left must not reach it: {:?}",
            backend.log()
        );
    }

    /// A submit into the mode the user *is* in is routed to that mode's session.
    #[tokio::test]
    async fn a_submit_goes_to_the_active_session() {
        let (mut router, _rx, backend) = router_with(TerminalType::Pi);
        router.boot().await.unwrap();
        router
            .handle(UiCommand::Submit {
                mode: TerminalType::Pi,
                text: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(backend.calls("send_text"), 1);
        assert!(backend.log()[backend.log().len() - 1].contains("hello"));
    }

    /// Esc touches the active session only — never a fan-out to everything live.
    #[tokio::test]
    async fn cancel_reaches_the_active_session_only() {
        let (mut router, mut rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        // Put a Pi session alive too, so a fan-out would be visible.
        router.switch_to(TerminalType::Pi).await.unwrap();
        drain(&mut rx);

        router.handle(UiCommand::Cancel).await.unwrap();

        let log = backend.log();
        assert_eq!(
            log.iter()
                .filter(|c| c.starts_with("abort") && c.contains("Pi"))
                .count(),
            1,
            "the active (Pi) session was cancelled: {log:?}"
        );
        assert!(
            !log.iter()
                .any(|c| c.starts_with("abort") && c.contains("Beeds")),
            "the hidden beads session must not be cancelled by an Esc in Pi: {log:?}"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "a successful cancel is not an error"
        );
    }

    /// A `Cancel` with nothing live behind the active mode is a no-op, not an error.
    #[tokio::test]
    async fn cancel_with_no_session_is_a_silent_no_op() {
        let (mut router, mut rx, _backend) = router_with(TerminalType::Pi);
        // No boot: no sessions exist at all.
        router.handle(UiCommand::Cancel).await.unwrap();
        assert!(
            drain(&mut rx).is_empty(),
            "no session, no message: Esc had nothing to stop"
        );
    }

    /// The legacy `BeadsNext` is gone, and with it the whole "the UI asks the beads
    /// loop to go again" surface. Asserted against the source because the compile
    /// time version of this is just "it does not build", which is a worse error
    /// message than this one.
    #[test]
    fn the_router_has_no_beads_advance_path_left() {
        // Assembled so this test's own source cannot match what it greps for.
        let cmd = concat!("Beads", "Next");
        let call = concat!(".", "advance(");
        let src = include_str!("router.rs");
        let hits: Vec<String> = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| l.contains(cmd) || l.contains(call))
            .map(|l| l.to_string())
            .collect();
        assert!(
            hits.is_empty(),
            "the beads loop is driven from inside BeadsSession, never from here: {hits:?}"
        );
    }

    // ---------------------- generations and staleness ----------------------

    /// The invariant the ADR calls "structural, not filtered": once a generation
    /// is replaced its event stream has no route to the UI, because the pump that
    /// could carry it has been aborted.
    #[tokio::test]
    async fn a_replaced_generation_cannot_reach_the_ui() {
        let (mut router, mut rx, backend) = router_with(TerminalType::Pi);
        router.boot().await.unwrap();
        let old_id = router.session(TerminalType::Pi).unwrap().id;
        let old_events = backend.events(old_id);

        // The old generation dies; the next submit brings the mode back as a new one.
        backend.set_status(old_id, SessionStatus::Dead);
        router
            .handle(UiCommand::Submit {
                mode: TerminalType::Pi,
                text: "second".into(),
            })
            .await
            .unwrap();
        let new_id = router.session(TerminalType::Pi).unwrap().id;
        assert_ne!(old_id, new_id, "a respawn is a different thing");
        drain(&mut rx); // clear everything up to the swap

        // The corpse tries to speak.
        old_events
            .send(SessionEvent::Agent(crate::app::PiEvent::AgentSettled))
            .ok();
        old_events
            .send(SessionEvent::System("I am still here".into()))
            .ok();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let msgs = drain(&mut rx);
        assert!(
            !msgs.iter().any(|m| m.contains(&old_id.to_string())),
            "no event from the dead generation may reach the UI: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m.contains("still here")),
            "nor its text: {msgs:?}"
        );

        // Meanwhile the live generation still reaches us.
        backend
            .events(new_id)
            .send(SessionEvent::System("alive".into()))
            .unwrap();
        tokio::task::yield_now().await;
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| m.contains("alive") && m.contains(&new_id.to_string())),
            "the live generation still reaches the UI"
        );
    }

    /// Generations come from one counter and nothing else issues them.
    #[tokio::test]
    async fn generations_are_monotonic_and_owned_by_the_router() {
        let (mut router, _rx, backend) = router_with(TerminalType::Pi);
        let mut seen = Vec::new();
        for _ in 0..3 {
            let id = router.ensure(TerminalType::Pi).unwrap();
            seen.push(id.generation);
            backend.set_status(id, SessionStatus::Dead);
        }
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "no generation reused: {seen:?}"
        );
    }

    /// A live session is reused, never duplicated, by repeated `ensure`.
    #[tokio::test]
    async fn ensure_reuses_a_live_session() {
        let (mut router, _rx, backend) = router_with(TerminalType::Bash);
        let a = router.ensure(TerminalType::Bash).unwrap();
        let b = router.ensure(TerminalType::Bash).unwrap();
        assert_eq!(a, b);
        assert_eq!(backend.spawn_count(TerminalType::Bash), 1);
    }

    /// Guard rail for the factory contract: a session claiming to be a different
    /// mode than the one asked for would misroute every event it produces, so it
    /// is refused rather than filed away.
    #[tokio::test]
    async fn a_factory_that_lies_about_the_mode_is_refused() {
        let (tx, _rx) = mpsc::unbounded_channel::<Msg>();
        let mut router = Router::with_factory(TerminalType::Pi, tx, fake(TerminalType::Bash));
        let err = router.ensure(TerminalType::Pi).unwrap_err();
        assert!(err.to_string().contains("factory produced"), "{err}");
        assert!(router.sessions.is_empty(), "a liar is not registered");
    }

    /// **A dead session being replaced is visible.** A new generation spawns with no
    /// memory of being a replacement, and the old one is already off the air, so
    /// without an explicit word the user sees the old session's last line and then a
    /// stranger's banner — a crash, not a restart.
    #[tokio::test]
    async fn replacing_a_dead_generation_is_announced() {
        let (mut router, mut rx, backend) = router_with(TerminalType::Bash);
        let old = router.ensure(TerminalType::Bash).unwrap();
        drain(&mut rx);
        backend.set_status(old, SessionStatus::Dead);

        let new = router.ensure(TerminalType::Bash).unwrap();
        assert_ne!(old, new, "a replacement is a new generation");
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter()
                .any(|m| m.contains("was gone") && m.contains("in its place")),
            "the replacement was silent: {msgs:?}"
        );
    }

    // ---------------------------- resize ----------------------------

    /// **ADR-0001 rule 6: the child gets the real window.** A shell wraps its own
    /// output for the width it was given, once, at the moment it wrote it — so a
    /// resize that never reaches the pty is wrong permanently, in the transcript as
    /// well as on screen. Every live session gets it, not just the visible one.
    #[tokio::test]
    async fn a_resize_reaches_every_live_session() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        router.ensure(TerminalType::Bash).unwrap();
        backend.clear_log();

        router
            .handle(UiCommand::Resize {
                rows: 50,
                cols: 180,
            })
            .await
            .unwrap();

        let log = backend.log();
        for mode in [TerminalType::Beeds, TerminalType::Bash] {
            let want = format!("resize {} ", mode.label());
            assert!(
                log.iter()
                    .any(|c| c.starts_with(&want) && c.ends_with("50x180")),
                "{mode:?} was not resized: {log:?}"
            );
        }
    }

    /// Dragging a window is not a reason to start a child. `handle` would happily
    /// `ensure` its way to a session on every resize event; that is a process
    /// spawn per pixel-drag, in the one mode where a spawn costs a shell.
    #[tokio::test]
    async fn a_resize_never_spawns_anything() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        backend.clear_log();

        router
            .handle(UiCommand::Resize {
                rows: 40,
                cols: 120,
            })
            .await
            .unwrap();

        assert!(
            backend.log().is_empty(),
            "a resize touched something it had not already: {:?}",
            backend.log()
        );
    }

    /// But a resize must not be *lost*, either. Bash spawns lazily, so the startup
    /// broadcast happens while the map is still empty; if nothing remembers the size,
    /// the shell opens at the library default and wraps at the wrong width for the
    /// rest of its life. Measured before the fix: `stty size` reporting `24 80`
    /// inside a 40x132 pty.
    #[tokio::test]
    async fn a_resize_before_a_session_is_born_is_applied_to_it() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        backend.clear_log();

        router
            .handle(UiCommand::Resize {
                rows: 44,
                cols: 150,
            })
            .await
            .unwrap();
        assert!(
            backend.log().is_empty(),
            "remembering a size is not a reason to spawn: {:?}",
            backend.log()
        );

        router.ensure(TerminalType::Bash).unwrap();
        let log = backend.log();
        assert!(
            log.iter()
                .any(|c| c.starts_with("resize Bash ") && c.ends_with("44x150")),
            "the session born after the resize never learned the size: {log:?}"
        );
    }

    // ---------------------------- shutdown ----------------------------

    /// Quitting shuts every live session down.
    #[tokio::test]
    async fn quitting_shuts_every_live_session_down() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        router.switch_to(TerminalType::Pi).await.unwrap();
        router.switch_to(TerminalType::Bash).await.unwrap();
        assert_eq!(router.sessions.len(), 3);

        router
            .shutdown_all(Duration::from_millis(200))
            .await
            .unwrap();

        for mode in TerminalType::ALL {
            assert!(
                backend
                    .log()
                    .iter()
                    .any(|c| c.starts_with("shutdown") && c.contains(mode.label())),
                "{mode:?} was not shut down: {:?}",
                backend.log()
            );
        }
        assert!(router.sessions.is_empty());
    }

    /// A session that never reports its exit must not be able to hang the exit.
    #[tokio::test]
    async fn a_wedged_session_cannot_prevent_quit() {
        let (tx, _rx) = mpsc::unbounded_channel::<Msg>();
        let silent = FakeBackend::new().with_silent_exit();
        let mut router = Router::with_factory(TerminalType::Pi, tx, silent.factory());
        router.boot().await.unwrap();

        let started = std::time::Instant::now();
        let res = timeout(
            Duration::from_millis(900),
            router.shutdown_all(Duration::from_millis(50)),
        )
        .await;
        assert!(res.is_ok(), "shutdown hung on a silent session");
        assert!(started.elapsed() < Duration::from_millis(900));
        assert!(silent.was_called("shutdown"));
    }

    /// `run()` serves commands until the UI hangs up, then leaves cleanly — the
    /// body `main` spawns, tested as a unit.
    #[tokio::test]
    async fn run_serves_commands_and_then_leaves_cleanly() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(16);
        let (app_tx, mut app_rx) = mpsc::unbounded_channel::<Msg>();
        let backend = FakeBackend::new();
        let router = Router::with_factory(TerminalType::Pi, app_tx, backend.factory());
        let task = tokio::spawn(router.run(cmd_rx));

        cmd_tx
            .send(tab(TerminalType::Pi, TerminalType::Bash))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            backend.log().iter().any(|c| c.contains("Bash")),
            "the command was served: {:?}",
            backend.log()
        );

        drop(cmd_tx); // the UI is gone
        assert!(timeout(Duration::from_secs(5), task).await.is_ok());
        assert!(
            backend.log().iter().any(|c| c.starts_with("shutdown")),
            "and its sessions went with it: {:?}",
            backend.log()
        );
        assert!(
            drain(&mut app_rx)
                .iter()
                .all(|m| !m.starts_with("error[harness]")),
            "a clean exit produces no harness errors"
        );
    }

    /// The status row (looprs-guh) reads liveness off the router; make sure that
    /// view of the world is real.
    #[tokio::test]
    async fn live_modes_reports_which_sessions_have_a_live_child() {
        let (mut router, _rx, backend) = router_with(TerminalType::Beeds);
        router.boot().await.unwrap();
        let beads = router.id_of(TerminalType::Beeds).unwrap();
        assert_eq!(router.live_modes(), vec![TerminalType::Beeds]);

        backend.set_status(beads, SessionStatus::Idle);
        assert_eq!(router.live_modes(), vec![TerminalType::Beeds]);
        backend.set_status(beads, SessionStatus::Dead);
        assert!(
            router.live_modes().is_empty(),
            "a dead session is not 'live' for the status row"
        );
    }
}
