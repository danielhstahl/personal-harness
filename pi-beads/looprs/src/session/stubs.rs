//! Stub implementations of [`Session`].
//!
//! They exist so the contract type-checks: each one proves that a `Box<dyn Session>`
//! can be built from a `(SessionId, &SessionConfig)` and a
//! `mpsc::UnboundedReceiver<SessionEvent>` can be handed back out of `start`.
//! **Nothing here does any work.** Every operation returns `Err` rather than a
//! plausible no-op, because a stub that silently succeeds is worse than one that
//! says what it is: it would let a wiring ticket pass a test that never exercised
//! anything.
//!
//! Each method names the ticket that owns its real implementation. Delete the
//! corresponding stub when that ticket lands.

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use super::{Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned};

/// `Err` with the name of the missing implementation and the ticket that owns it.
pub(crate) fn todo_method(what: &str, ticket: &str) -> Result<()> {
    Err(anyhow!("{what}: not implemented yet ({ticket})"))
}

/// The Beads terminal state: planner passes, worker passes, one fresh pi child per
/// pass. Today that machine is `BeadsLoop` in `src/app.rs`; `looprs-msj` moves it
/// behind this type so the loop is driven by its own step instead of by the UI's
/// input mode.
pub struct BeadsSession {
    id: SessionId,
    cfg: SessionConfig,
    /// Kept so the shape is real: this is the sender the real implementation hands
    /// its pi-event pump. Dropped when the struct drops, which is what closes the
    /// router's stream.
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl BeadsSession {
    pub fn start(id: SessionId, cfg: &SessionConfig) -> Result<Spawned> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Spawned {
            session: Box::new(Self {
                id,
                cfg: cfg.clone(),
                events: tx,
            }),
            events: rx,
        })
    }
}

impl Session for BeadsSession {
    fn id(&self) -> SessionId {
        self.id
    }
    fn send_text(&mut self, _text: String) -> Result<()> {
        // looprs-msj: a submit here is a planner instruction ("make tickets for
        // this"), and the resulting pass must advance BeadStep by itself.
        let _ = &self.cfg;
        todo_method("BeadsSession::send_text", "looprs-msj")
    }
    fn abort(&mut self) -> Result<()> {
        // looprs-5g7: abort the pi run AND park; an aborted worker must not be
        // mistaken for agent_settled -> next bead.
        todo_method("BeadsSession::abort", "looprs-5g7")
    }
    fn shutdown(&mut self) -> Result<()> {
        // looprs-ecr: close pi's stdin, then bound-wait, then kill.
        todo_method("BeadsSession::shutdown", "looprs-ecr")
    }
    fn set_active(&mut self, _active: bool) -> Result<()> {
        // The only mode with real work to do here: stop starting passes while hidden.
        todo_method("BeadsSession::set_active", "looprs-05j")
    }
    fn status(&self) -> SessionStatus {
        SessionStatus::NotStarted
    }
}

/// The Pi terminal state: one persistent, stateful `pi --mode rpc` chat session.
/// Owned by looprs-ctn.
pub struct PiChatSession {
    id: SessionId,
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl PiChatSession {
    pub fn start(id: SessionId, cfg: &SessionConfig) -> Result<Spawned> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Spawned {
            session: Box::new(Self {
                id,
                cfg: cfg.clone(),
                events: tx,
            }),
            events: rx,
        })
    }
}

impl Session for PiChatSession {
    fn id(&self) -> SessionId {
        self.id
    }
    fn send_text(&mut self, _text: String) -> Result<()> {
        // looprs-ctn: spawn-once-reuse; steer/follow_up while a run is in flight.
        let _ = &self.cfg;
        todo_method("PiChatSession::send_text", "looprs-ctn")
    }
    fn abort(&mut self) -> Result<()> {
        // looprs-5g7: clear_queue, then {"type":"abort"}, and hand the returned
        // queued text back for the input buffer.
        todo_method("PiChatSession::abort", "looprs-5g7")
    }
    fn shutdown(&mut self) -> Result<()> {
        todo_method("PiChatSession::shutdown", "looprs-ecr")
    }
    fn status(&self) -> SessionStatus {
        SessionStatus::NotStarted
    }
}

/// The Bash terminal state: one long-lived shell on its own pty (ADR-0001).
/// Owned by looprs-553.
pub struct BashSession {
    id: SessionId,
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl BashSession {
    pub fn start(id: SessionId, cfg: &SessionConfig) -> Result<Spawned> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Spawned {
            session: Box::new(Self {
                id,
                cfg: cfg.clone(),
                events: tx,
            }),
            events: rx,
        })
    }
}

impl Session for BashSession {
    fn id(&self) -> SessionId {
        self.id
    }
    fn send_text(&mut self, _text: String) -> Result<()> {
        // looprs-553: write the line to the pty master. Note the shell echoes it
        // back, so *do not* re-echo locally.
        let _ = &self.cfg;
        todo_method("BashSession::send_text", "looprs-553")
    }
    fn abort(&mut self) -> Result<()> {
        // looprs-5g7: write 0x03 to the master (SIGINT via the line discipline);
        // interrupts the command, keeps the shell.
        todo_method("BashSession::abort", "looprs-5g7")
    }
    fn shutdown(&mut self) -> Result<()> {
        // looprs-ecr: close the master, reap the child, never leak it past our exit.
        todo_method("BashSession::shutdown", "looprs-ecr")
    }
    fn status(&self) -> SessionStatus {
        SessionStatus::NotStarted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TerminalType;

    /// The three stubs are swappable behind the trait object, which is the whole
    /// claim of the abstraction: the router holds `Box<dyn Session>` and could not
    /// tell them apart if it wanted to.
    #[test]
    fn all_three_backends_fit_the_same_handle() {
        let cfg = SessionConfig::default();
        let mut handles: Vec<Box<dyn Session>> = Vec::new();
        let mut receivers = Vec::new();
        for mode in TerminalType::ALL {
            let spawned = match mode {
                TerminalType::Beeds => BeadsSession::start(SessionId::new(mode, 7), &cfg),
                TerminalType::Pi => PiChatSession::start(SessionId::new(mode, 7), &cfg),
                TerminalType::Bash => BashSession::start(SessionId::new(mode, 7), &cfg),
            }
            .unwrap();
            handles.push(spawned.session);
            receivers.push(spawned.events);
        }
        assert_eq!(handles.len(), 3);
        for (h, mode) in handles.iter().zip(TerminalType::ALL) {
            assert_eq!(h.mode(), mode);
            assert_eq!(h.id().generation, 7);
            assert!(!h.is_running());
        }
        // Receiving side is alive for each stream.
        for r in receivers.iter_mut() {
            assert!(r.try_recv().is_err());
        }
    }

    /// Stubs must refuse loudly, never quietly succeed.
    #[tokio::test]
    async fn stubs_refuse_rather_than_pretend() {
        let cfg = SessionConfig::default();
        let mut beads = BeadsSession::start(SessionId::new(TerminalType::Beeds, 0), &cfg)
            .unwrap()
            .session;
        let mut pi = PiChatSession::start(SessionId::new(TerminalType::Pi, 0), &cfg)
            .unwrap()
            .session;
        let mut bash = BashSession::start(SessionId::new(TerminalType::Bash, 0), &cfg)
            .unwrap()
            .session;

        for s in [&mut beads, &mut pi, &mut bash] {
            assert!(s.send_text("hello".into()).is_err());
            assert!(s.abort().is_err());
            assert!(s.shutdown().is_err());
        }
        // set_active has a real default (no-op) for the KeepRunning modes, and a
        // stubbed refusal for the one mode that has policy attached to it.
        assert!(pi.set_active(true).is_ok());
        assert!(bash.set_active(false).is_ok());
        assert!(beads.set_active(true).is_err());
    }
}
