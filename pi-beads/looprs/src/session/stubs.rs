//! Stub implementations of [`Session`] — the two backends that are not built yet.
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
//! corresponding stub when that ticket lands. The Beads backend is *not* here: it
//! moved to [`super::beads`] when looprs-05j put the Router in charge of it.

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use super::{Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned};

/// `Err` with the name of the missing implementation and the ticket that owns it.
pub(crate) fn todo_method(what: &str, ticket: &str) -> Result<()> {
    Err(anyhow!("{what}: not implemented yet ({ticket})"))
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
    use crate::session::{TerminalType, spawn};

    /// Every backend is swappable behind the trait object, which is the whole
    /// claim of the abstraction: the router holds `Box<dyn Session>` and could not
    /// tell them apart if it wanted to. `spawn` covers the real Beads session too,
    /// so this exercises all three modes through the one constructor.
    #[tokio::test]
    async fn every_backend_fits_the_same_handle() {
        let cfg = SessionConfig::default();
        let mut handles: Vec<Box<dyn Session>> = Vec::new();
        let mut receivers = Vec::new();
        for mode in TerminalType::ALL {
            let spawned = spawn(mode, &cfg, 7).unwrap();
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

    /// Stubs must refuse loudly, never quietly succeed: an unimplemented backend
    /// that returns `Ok` is how a wiring ticket ends up testing nothing.
    #[tokio::test]
    async fn stubs_refuse_rather_than_pretend() {
        let cfg = SessionConfig::default();
        let mut pi = PiChatSession::start(SessionId::new(TerminalType::Pi, 0), &cfg)
            .unwrap()
            .session;
        let mut bash = BashSession::start(SessionId::new(TerminalType::Bash, 0), &cfg)
            .unwrap()
            .session;

        for s in [&mut pi, &mut bash] {
            let text = s.send_text("hello".into()).unwrap_err().to_string();
            assert!(text.contains("not implemented yet"), "{text}");
            let abort = s.abort().unwrap_err().to_string();
            assert!(abort.contains("looprs-5g7"), "{abort}");
        }
        // set_active is the KeepRunning default: a no-op, not a refusal, because
        // there is genuinely nothing for a warm session to do when it is shown.
        assert!(pi.set_active(true).is_ok());
        assert!(bash.set_active(false).is_ok());
    }
}
