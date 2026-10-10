//! The Bash terminal state: **one persistent shell on its own pty** (ADR-0001).
//!
//! Same handle/task split as the other two backends: [`BashSession`] is a cheap
//! handle whose every method queues a [`BashCmd`] and returns; the task owning the
//! pty is the only thing that touches the child. A dedicated OS *thread* does the
//! blocking `read()` on the master and pushes bytes into that same mailbox, which
//! is what `portable-pty` forces (its reader is synchronous) and what keeps the
//! tokio runtime out of it — exactly the shape ADR-0001's "Blocking reader"
//! consequence calls for.
//!
//! Five decisions this file makes, all of them ADR-0001's:
//!
//! 1. **A real pty, not pipes.** The child believes it owns a terminal: `isatty()`
//!    is true, job control works, `stty size` answers, and `0x03` actually
//!    interrupts a running command instead of sitting unreadable in a pipe.
//! 2. **One byte stream.** stdout and stderr are already merged by the line
//!    discipline, so interleaving is the *program's* order and needs no merging
//!    logic here ([`ByteStream::Merged`]).
//! 3. **No re-framing.** Bytes are forwarded as the read-sized chunks they are.
//!    Re-splitting on `\n` would rewrite the child's own framing.
//! 4. **Exit codes come from the shell, not from hope.** An interactive shell does
//!    not report a per-command status to its parent, so the generated rcfile sets
//!    a `PROMPT_COMMAND` that prints a `DC1 looprs:exit:<code> DC2` marker after
//!    every command. The marker is stripped from the byte stream here, at the
//!    boundary, so it never reaches the transcript or the screen.
//! 5. **A dead shell is a notice, not a wedged mode.** `exit`, Ctrl-D, a crash:
//!    the child going away is reported, the status goes `Dead`, and the next
//!    command starts a fresh shell with the restart already explained.
//! ## What is where (looprs-00u.18)
//!
//! This was one 3,039-line file. What is left here is the Bash **session half**
//! — the mailbox and the handle the Router holds — with the five
//! responsibilities it carried next door:
//!
//! * [`pty`](pty) — the pty itself: spawn, shell integration, the blocking
//!   reader thread and its byte lane;
//! * [`task`](task) — the owning task: the command queue, `ensure_shell`, the
//!   output pump, the exit marker, stream end and shutdown;
//! * [`interrupt`](interrupt) — `Esc`, `0x03`, and the stalled-cancel report;
//! * [`screen`](screen) — resize and the full-screen handover (ADR-0001 Q2);
//! * [`reap`](reap) — the never-reap-alone rule and the wording around a dead
//!   shell;
//! * [`tests`](tests) — this suite, split along the banners already in it.
//!
//! Behaviour-preserving: whole items moved, and `bash::BashSession` still
//! resolves from the old path.

use crate::session::bash::pty::{BYTE_LANE_DEPTH, ByteLane};
use crate::session::bash::reap::{drain_lane_now, handle_chunk};
use crate::session::bash::task::BashTask;

use std::collections::VecDeque;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use portable_pty::PtySize;
use tokio::sync::{mpsc, oneshot};

use crate::screen::ScreenWatch;
use crate::session::{
    ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned,
    publish_liveness,
};

mod interrupt;
mod pty;
mod reap;
mod screen;
mod task;

#[cfg(test)]
mod tests;

/// Commands to the task that owns the pty.
enum BashCmd {
    /// A command line typed into the Bash box.
    Submit(String),
    /// `Esc` / Ctrl-C: `0x03` to the master, which the line discipline turns into
    /// SIGINT for the child's foreground process group. The command dies; the
    /// shell survives.
    Interrupt,
    /// The `Interrupt` posted `attempt` attempts ago has not produced an exit
    /// marker within [`cancel::GRACE`](crate::session::cancel). The command is
    /// most likely trapping or ignoring SIGINT; the session says so instead of
    /// leaving the user staring at a spinner (ADR-0003).
    InterruptStalled { attempt: u32 },
    /// Raw keystrokes for a child that currently holds the screen
    /// ([`ScreenWatch`]): written to the master exactly as typed, with no newline
    /// and no interpretation. This is what makes `Esc` be `Esc` inside vim
    /// instead of `0x03`.
    Keys(Vec<u8>),
    /// The real terminal changed shape; ADR-0001 rule 6: the child gets the real
    /// size, not a virtual 80x24.
    Resize { rows: u16, cols: u16 },
    /// Test seam: ack once every command queued before this one is handled.
    ///
    /// Only [`BashSession::quiesce`] ever sends one, which makes it unreachable in
    /// the binary and load-bearing in the test suite: without it every lifecycle
    /// assertion in `bash::tests` would be a sleep, and a sleep is a test that
    /// passes on a fast machine for the wrong reason.
    #[allow(dead_code)] // consumer: BashSession::quiesce (test seam)
    Sync(oneshot::Sender<()>),
    /// The app is exiting.
    Shutdown,
}

/// Handle onto the Bash session.
pub struct BashSession {
    id: SessionId,
    cmd: mpsc::UnboundedSender<BashCmd>,
    status: Arc<StdMutex<SessionStatus>>,
    /// See [`BashTask::readers_live`] — the same counter, reachable without the
    /// task (looprs-2ck).
    readers_live: Arc<AtomicUsize>,
}

impl BashSession {
    /// Start the Bash session. Builds **no shell**: it comes up on the first
    /// command, and then lives until the app does.
    pub fn start(id: SessionId, cfg: &SessionConfig) -> Result<Spawned> {
        let (session, events) = Self::build(id, cfg)?;
        Ok(Spawned {
            session: Box::new(session),
            events,
        })
    }

    /// The un-boxed form, so a test can hold a concrete handle next to the event
    /// stream the Router would otherwise pump.
    fn build(
        id: SessionId,
        cfg: &SessionConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SessionEvent>)> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BashCmd>();
        // The pty byte lane, bounded (looprs-6cj). Depth is deliberately small:
        // it is one of the two places a runaway `while true; do echo; done` can
        // be standing when the UI stops draining, and the other one is the UI's
        // own bus. Eight read buffers is 64 KiB — about half a frame of output
        // at the rate the app can actually draw, which is the right amount of
        // rope: enough that a producer that keeps up never notices the bound.
        let (bytes_tx, mut bytes_rx) = mpsc::channel::<ByteLane>(BYTE_LANE_DEPTH);
        let status = Arc::new(StdMutex::new(SessionStatus::NotStarted));
        let task_status = status.clone();
        // Shared with the task so the handle can answer "is a reader thread of
        // mine still parked?" without owning the thread (looprs-2ck).
        let readers_live = Arc::new(AtomicUsize::new(0));

        let mut task = BashTask {
            id,
            cfg: cfg.clone(),
            bytes_tx: bytes_tx.clone(),
            ev_tx: ev_tx.clone(),
            cmd: cmd_tx.clone(),
            shell: None,
            had_shell: false,
            ready: false,
            queue: VecDeque::new(),
            outstanding: VecDeque::new(),
            aborting: false,
            interrupt_attempt: 0,
            stall_reported: false,
            pending: Vec::new(),
            output_emitted: 0,
            readers_live: readers_live.clone(),
            // The one place the session learns who owns the alternate screen. With
            // the app in the alternate screen, the child's own enter/leave pair is
            // cut out of the stream here rather than teed (ADR-0001 amendment 4),
            // which is what keeps the user inside our screen for the whole run and
            // leaves exactly one `?1049l` for the ledger to write at the exit.
            screen: {
                let mut w = ScreenWatch::new();
                w.hosting_alt_screen(cfg.alt_screen_hosted);
                w
            },
            utf8: Vec::new(),
            // An honest starting size; the app sends the real one on its first
            // resize (and `Shell::spawn` uses this for the initial pty).
            size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            last_exit: None,
        };

        // The liveness this session has already told the UI about, so
        // `publish_liveness` fires on changes rather than on every turn of the loop.
        let mut published = SessionStatus::NotStarted;

        tokio::spawn(async move {
            // Two lanes, polled in a fixed priority (looprs-6cj).
            //
            // `biased` puts the control lane first, every turn, and that ordering
            // is the reason a bounded output path is safe to have at all: while
            // the shell is pouring out more than the screen can take, `Esc`,
            // Ctrl-C, the resize and the exit still get answered immediately.
            // Before this, the same flood that filled the queue also decided when
            // the cancel would be looked at.
            'task: loop {
                tokio::select! {
                    biased;
                    cmd = cmd_rx.recv() => {
                        // `None` is the mailbox closing: the app let go of its
                        // senders. That ends this task exactly as the old
                        // `while let Some(cmd)` ended, and it has to be said
                        // here — a select arm whose pattern does not match is
                        // *removed*, and with the byte lane already at EOF
                        // there would be nothing left to poll.
                        let Some(cmd) = cmd else { break 'task };
                        match cmd {
                            BashCmd::Submit(text) => task.submit(text),
                            BashCmd::Interrupt => task.interrupt(),
                            BashCmd::InterruptStalled { attempt } => task.interrupt_stalled(attempt),
                            BashCmd::Keys(b) => task.write_keys(b),
                            BashCmd::Resize { rows, cols } => task.resize(rows, cols),
                            BashCmd::Sync(tx) => {
                                // Take everything the reader thread has already
                                // handed over before acking: the seam promises
                                // "every command queued before this one has been
                                // handled", and the output that command is
                                // waiting on now arrives on the *other* lane.
                                drain_lane_now(&mut task, &mut bytes_rx);
                                // Publish the mirror before the ack, so a test that waits
                                // on the seam then reads `status()` sees the state as of
                                // that ack rather than one command stale.
                                let s = task.status();
                                *task_status.lock().unwrap() = s;
                                publish_liveness(&mut published, s, &ev_tx);
                                let _ = tx.send(());
                            }
                            BashCmd::Shutdown => {
                                // Take the output that is already on the wire
                                // before going down. The exit drain's whole job
                                // is that a session gets its parting word in,
                                // and since looprs-6cj the bytes of that word
                                // ride the other lane, so a shutdown that went
                                // straight to `task.shutdown()` would drop the
                                // last thing the child said.
                                drain_lane_now(&mut task, &mut bytes_rx);
                                // The lane goes *with* the shutdown: the reap
                                // keeps draining it so the reader thread and
                                // the dying child can both finish instead of
                                // parking on each other (looprs-2ck).
                                task.shutdown(&mut bytes_rx);
                                *task_status.lock().unwrap() = SessionStatus::Dead;
                                let _ = ev_tx.send(SessionEvent::Exited {
                                    reason: ExitReason::Shutdown,
                                });
                                break 'task;
                            }
                        }
                    }
                    // The byte lane, always armed. A read EOF ends *this* shell's
                    // stream, not the lane: the next command respawns a shell
                    // whose reader feeds the same lane, and an arm that stayed
                    // disabled after the first EOF would leave the restarted
                    // shell running with its output going nowhere.
                    lane = bytes_rx.recv() => match lane {
                        Some(ByteLane::Chunk(b)) => handle_chunk(&mut task, b),
                        Some(ByteLane::Eof) => task.stream_end(),
                        // Only reachable once every byte-lane sender is gone,
                        // which includes this task's own copy — so the task is
                        // already on its way out.
                        None => {
                            task.stream_end();
                            break 'task;
                        }
                    },
                }
                let s = task.status();
                *task_status.lock().unwrap() = s;
                publish_liveness(&mut published, s, &ev_tx);
            }
            // The mailbox is closed. Dropping the task drops the shell, whose `Drop`
            // kills the child: no bash outlives this session.
            drop(task);
        });

        Ok((
            Self {
                id,
                cmd: cmd_tx,
                status,
                readers_live,
            },
            ev_rx,
        ))
    }

    /// Test seam: returns once every command queued before this call has been
    /// fully handled.
    #[allow(dead_code)] // consumer: bash::tests (deterministic lifecycle, no sleeps)
    pub async fn quiesce(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(BashCmd::Sync(tx)).is_err() {
            return true;
        }
        tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .is_ok()
    }
}

impl Drop for BashSession {
    fn drop(&mut self) {
        // The shell must not outlive the handle. It cannot be left to the task
        // noticing that the channel closed, because **the pty reader thread holds a
        // sender clone**: as long as it is running, `cmd_rx.recv()` never returns
        // `None`, so the task never exits, the `Shell` is never dropped, the shell
        // is never killed — and so the reader never sees the EOF that would end it.
        // A cycle with a live child at the middle of it, which is exactly how an
        // "exited" app leaves a bash behind.
        //
        // Asking politely on the way out breaks it: `Shutdown` kills the shell, the
        // reader hits EOF, the thread ends, and every sender finally dies.
        let _ = self.cmd.send(BashCmd::Shutdown);
    }
}

impl Session for BashSession {
    fn id(&self) -> SessionId {
        self.id
    }

    fn send_text(&mut self, text: String) -> Result<()> {
        self.cmd
            .send(BashCmd::Submit(text))
            .map_err(|_| anyhow!("bash session task is gone"))
    }

    fn abort(&mut self) -> Result<()> {
        // `Esc` in the Bash view: `0x03` to the master, i.e. SIGINT to the
        // child's foreground process group. The command dies, the shell does not
        // (ADR-0001 Q3). An idle shell ignores it — no noise for a no-op.
        self.cmd
            .send(BashCmd::Interrupt)
            .map_err(|_| anyhow!("bash session task is gone"))
    }

    fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        // Raw keystrokes for a full-screen child: verbatim to the master, no line.
        self.cmd
            .send(BashCmd::Keys(bytes))
            .map_err(|_| anyhow!("bash session task is gone"))
    }

    fn shutdown(&mut self) -> Result<()> {
        self.cmd
            .send(BashCmd::Shutdown)
            .map_err(|_| anyhow!("bash session task is gone"))
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.cmd
            .send(BashCmd::Resize { rows, cols })
            .map_err(|_| anyhow!("bash session task is gone"))
    }

    fn status(&self) -> SessionStatus {
        *self.status.lock().unwrap()
    }
}

impl BashSession {
    /// How many of this session's pty reader threads have not finished.
    ///
    /// `0` is the answer the exit path wants: it means no reader thread of ours
    /// is parked on a byte lane that has stopped draining. Diagnostic first, and
    /// load-bearing in `bash::tests`, where it is the only way to assert the
    /// *reader ends* half of looprs-2ck instead of assuming it.
    #[allow(dead_code)] // consumer: bash::tests ("the reader thread ends")
    pub fn readers_live(&self) -> usize {
        self.readers_live.load(Ordering::SeqCst)
    }
}
