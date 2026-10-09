//! The Bash task: the one owner of the shell, the queue, and the byte pump.
//!
//! A [`BashTask`] runs in its own tokio task and is the only thing in the
//! process allowed to touch the [`Shell`](super::pty::Shell). The half you can
//! see from the handle is a queue with a mailbox in front of it:
//! [`BashTask::submit`] enqueues, [`BashTask::flush_queue`] starts a command
//! when the shell is idle, and [`BashTask::ensure_shell`] buys the shell on the
//! first one rather than at construction. The half that comes back is a pump:
//! [`BashTask::on_bytes`] takes what the reader thread saw,
//! [`BashTask::on_marker`] pulls the exit code out of the stream before the
//! bytes go on as output, and [`BashTask::stream_end`] /
//! [`BashTask::shutdown`] are the two ways the stream stops — one the shell
//! chose, one the app chose.
//!
//! `Esc` is in [`interrupt`](super::interrupt), the screen handover in
//! [`screen`](super::screen), and the reaping of a shell that has already died
//! in [`reap`](super::reap); this file is the ordinary round trip that those
//! three keep interrupting.

use crate::session::bash::BashCmd;
use crate::session::bash::pty::{
    ByteLane, EXIT_ASK, KILL_REAP, MARKER_BUFFER_MAX, MARKER_END, MARKER_PREFIX, MARKER_START,
    Shell,
};
use crate::session::bash::reap::{
    clip, exit_text, reap_off_task, reap_while_draining, split_complete_utf8,
};

use std::collections::VecDeque;
use std::io::Write;

use std::sync::Arc;

use std::sync::atomic::AtomicUsize;

use anyhow::{Result, anyhow};
use portable_pty::PtySize;
use tokio::sync::mpsc;

use crate::screen::{Piece, ScreenWatch};
use crate::session::{
    ByteStream, ExitReason, SessionConfig, SessionEvent, SessionId, SessionStatus,
};

/// The shell, running inside its own task.
pub(super) struct BashTask {
    pub(super) id: SessionId,
    pub(super) cfg: SessionConfig,
    /// The producing end of the pty byte lane, kept so a respawned shell's
    /// reader thread can be pointed at the same lane (looprs-6cj).
    ///
    /// It is a **bounded** lane, and that is the point: the reader thread parks
    /// when the lane is full, which is what lets a fast producer be throttled at
    /// the file descriptor instead of in memory. It is *separate* from
    /// [`BashCmd`] for the same reason: nothing the user types may ever queue
    /// behind output, and the control lane stays unbounded and polled first.
    pub(super) bytes_tx: mpsc::Sender<ByteLane>,
    pub(super) ev_tx: mpsc::UnboundedSender<SessionEvent>,
    pub(super) cmd: mpsc::UnboundedSender<BashCmd>,
    pub(super) shell: Option<Shell>,
    /// Ever had one? `NotStarted` vs `Dead` is a difference the status row shows.
    pub(super) had_shell: bool,
    /// Has this shell printed its first prompt?
    ///
    /// Readiness is not a nicety. Bash prints a `PROMPT_COMMAND` marker *before* it
    /// has read a single line of its own startup files' output ordering settled, so
    /// a command written too early has its marker answered by the *prompt* rather
    /// than by the command — which reads as "`false` exited 0". The first marker is
    /// therefore consumed as readiness, and input written only after it.
    pub(super) ready: bool,
    /// Commands typed before the shell was ready, in order, still unwritten.
    pub(super) queue: VecDeque<String>,
    /// Commands written to the pty that have not reported their exit marker yet.
    ///
    /// A queue of the command *texts* rather than a count because typed-ahead input
    /// is normal: three lines entered at a prompt produce three markers, and each
    /// one has to close the command it belongs to rather than the head of the
    /// queue. Keeping the text is what lets the death notice name the command that
    /// never reported, instead of saying "something was running".
    pub(super) outstanding: VecDeque<String>,
    /// `0x03` went out and we are waiting for the command to unwind.
    pub(super) aborting: bool,
    /// How many `0x03`s this cancel has sent. The stall watchdog carries one, so a
    /// deadline for an interrupt already resolved cannot be mistaken for a deadline
    /// on the current one — the same job `serial` does everywhere else.
    pub(super) interrupt_attempt: u32,
    /// The stall has been reported for the current attempt. Set once per attempt:
    /// the escalation is one honest sentence, not a nagging timer. The user's next
    /// `Esc` clears it and starts a fresh attempt.
    pub(super) stall_reported: bool,
    /// Unconsumed marker bytes (a marker can straddle two reads).
    pub(super) pending: Vec<u8>,
    /// How many `BashOutput` events this task has emitted, ever.
    ///
    /// Used to answer one question per pty read: did this chunk become a
    /// message or not? If it did not, no downstream stage will ever hand the
    /// reader its credit back, so this task has to (see the byte arm of the
    /// task loop).
    pub(super) output_emitted: u64,
    /// Who owns the real terminal screen, read out of the child's own bytes
    /// (ADR-0001 Q2). The watcher is the single source of truth: `is_held()` is
    /// the answer the whole app obeys.
    pub(super) screen: ScreenWatch,
    /// How many pty reader threads this session has that have not finished.
    ///
    /// Not a metric: it is the shutdown path's view of the *second* property
    /// looprs-2ck has to hold, and it has to be counted rather than inferred
    /// because "the reader thread ended" is a statement about a thread this
    /// task cannot see. Respawned shells each bring their own reader, so this
    /// counts rather than flips a flag; 0 means no reader is parked anywhere on
    /// this session's lane.
    pub(super) readers_live: Arc<AtomicUsize>,
    /// Bytes held back only because they may be the start of a multi-byte UTF-8
    /// sequence that a read boundary split in half.
    pub(super) utf8: Vec<u8>,
    pub(super) size: PtySize,
    /// The exit code of the last finished command, for the status row.
    pub(super) last_exit: Option<i32>,
}

impl BashTask {
    pub(super) fn emit(&self, ev: SessionEvent) {
        let _ = self.ev_tx.send(ev);
    }

    pub(super) fn note(&self, text: impl Into<String>) {
        self.emit(SessionEvent::System(text.into()));
    }

    pub(super) fn err(&self, text: impl Into<String>) {
        self.emit(SessionEvent::Error(text.into()));
    }

    pub(super) fn status(&self) -> SessionStatus {
        if !self.had_shell && self.shell.is_none() {
            return SessionStatus::NotStarted;
        }
        if self.shell.is_none() {
            return SessionStatus::Dead;
        }
        if self.aborting {
            SessionStatus::Aborting
        } else if !self.outstanding.is_empty() || !self.queue.is_empty() {
            SessionStatus::Running
        } else {
            SessionStatus::Idle
        }
    }

    /// Write one command line to the shell and count it as in flight.
    fn write_command(&mut self, text: &str) -> Result<()> {
        // The line discipline echoes what we write, so the user sees their own
        // command the moment the shell reads it. We must not echo it ourselves —
        // that would show it twice.
        let Some(shell) = self.shell.as_mut() else {
            return Err(anyhow!("no shell to write to"));
        };
        shell.write_line(&format!("{}\n", text))?;
        self.outstanding.push_back(text.to_string());
        Ok(())
    }

    /// Empty the readiness queue, in order, into the now-usable shell.
    fn flush_queue(&mut self) {
        // Drain into a local first: a failing write re-queues what is left rather
        // than losing the user's typing and half-sending the rest.
        let pending = std::mem::take(&mut self.queue);
        for line in pending {
            if let Err(e) = self.write_command(&line) {
                self.err(format!("{e:#}"));
                return;
            }
        }
    }

    /// Bring the shell up if it is not there.
    ///
    /// Lazily, on the first submit: opening a pty and a bash for a mode the user
    /// only Tabbed through would be paying for a shell nobody asked for.
    fn ensure_shell(&mut self) -> Result<()> {
        if self.shell.is_some() {
            return Ok(());
        }
        let respawning = self.had_shell;
        let shell = Shell::spawn(
            &self.cfg,
            self.size,
            self.bytes_tx.clone(),
            &self.readers_live,
        )?;
        self.shell = Some(shell);
        self.had_shell = true;
        // A new shell is an unread shell: no prompt has come from it yet.
        self.ready = false;
        self.outstanding.clear();
        self.aborting = false;
        self.interrupt_attempt = 0;
        self.stall_reported = false;
        if respawning {
            self.note("shell restarted (the previous one was gone)");
        }
        if !crate::session::looks_like_bash(&self.cfg.shell_bin) {
            // Loud rather than mysterious: with a non-bash the `--rcfile`
            // integration never runs, so no exit marker ever comes back and every
            // command looks like it is still running. Better to say so here than to
            // have the user debug a hung-looking pane.
            self.err(format!(
                "{} is not a bash: exit codes and readiness need bash (set LOOPRS_SHELL_BIN)",
                self.cfg.shell_bin
            ));
        }
        Ok(())
    }

    /// A command line typed into the Bash box.
    ///
    /// Never written to a shell that has not printed its prompt: it goes to
    /// `queue` and is flushed by the readiness marker. That is what makes "queue
    /// input until the shell is actually up" true rather than aspirational — and
    /// it is why `echo hi` right after a cold start cannot be answered by the
    /// shell's own first prompt.
    pub(super) fn submit(&mut self, text: String) {
        if let Err(e) = self.ensure_shell() {
            self.err(format!("could not start the shell: {e:#}"));
            return;
        }
        if self.ready {
            if let Err(e) = self.write_command(&text) {
                self.err(format!("{e:#}"));
            }
        } else {
            self.queue.push_back(text);
        }
    }

    /// Raw keystrokes for a child that holds the screen (ADR-0001 Q2).
    ///
    /// Written straight to the master with nothing added. No readiness queue: a
    /// child can only hold the screen if its shell is up and running it.
    pub(super) fn write_keys(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        if let Err(e) = shell.write_raw(&bytes) {
            self.err(format!("keystrokes did not reach the shell: {e:#}"));
        }
    }

    /// Bytes off the master: split the exit markers out, forward everything else.
    ///
    /// The marker is consumed *here*, at the edge of the child's byte stream, so
    /// nothing downstream has to know it exists — neither the transcript nor the
    /// screen ever sees `0x11 … 0x12`.
    pub(super) fn on_bytes(&mut self, bytes: Vec<u8>) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(&bytes);

        loop {
            let Some(start) = buf.iter().position(|b| *b == MARKER_START) else {
                let out = std::mem::take(&mut buf);
                self.emit_output(&out);
                return;
            };
            if start > 0 {
                self.emit_output(&buf[..start]);
                buf.drain(..start);
            }
            // buf now starts at MARKER_START.
            match buf.iter().position(|b| *b == MARKER_END) {
                Some(end) => {
                    let payload = buf[1..end].to_vec();
                    buf.drain(..=end);
                    self.on_marker(&payload);
                }
                None => {
                    if buf.len() > MARKER_BUFFER_MAX {
                        // Not a marker after all. Hand the bytes back as output
                        // rather than holding a buffer that will never complete.
                        tracing::debug!("{}: unterminated marker, treating as output", self.id);
                        let out = std::mem::take(&mut buf);
                        self.emit_output(&out);
                        return;
                    }
                    return; // wait for the rest of the marker
                }
            }
        }
    }

    fn emit_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Everything goes through the screen watcher first. The order it hands the
        // pieces back in is the contract: a takeover is announced *before* the
        // bytes that switch screens (so the terminal sees `ESC[?1049h` instead of
        // the transcript eating it) and a release *after* the bytes that switch
        // back (so the main screen actually comes back).
        let pieces = self.screen.observe(bytes);
        for piece in pieces {
            match piece {
                Piece::Out(b) => self.emit_bytes(&b),
                Piece::Change(change) => self.on_screen_change(change),
            }
        }
    }

    /// The UTF-8-safe half of forwarding output: hold a split multi-byte sequence
    /// for the next read, emit the rest.
    pub(super) fn emit_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut held = std::mem::take(&mut self.utf8);
        held.extend_from_slice(bytes);
        let (text, rest) = split_complete_utf8(held);
        self.utf8 = rest;
        if !text.is_empty() {
            self.output_emitted += 1;
            self.emit(SessionEvent::BashOutput {
                stream: ByteStream::Merged,
                chunk: text,
            });
        }
    }

    /// The shell finished a command and reported its status.
    ///
    /// Two kinds of marker are *not* a result and are swallowed rather than shown
    /// as an exit line out of nowhere:
    ///
    /// * the shell's **first** prompt — that one means "I am up", and it is what
    ///   releases the input queued during startup;
    /// * any other marker with nothing outstanding, e.g. one a nested interactive
    ///   shell printed. We cannot attribute it to a command of ours, so we do not
    ///   claim one for it.
    fn on_marker(&mut self, payload: &[u8]) {
        let payload = String::from_utf8_lossy(payload).to_string();
        let Some(code_str) = payload.strip_prefix(MARKER_PREFIX) else {
            // Something shaped like our marker that is not ours: show it rather
            // than silently swallowing bytes we cannot explain.
            self.emit_output(payload.as_bytes());
            return;
        };
        let code: i32 = match code_str.trim().parse() {
            Ok(c) => c,
            Err(_) => {
                self.emit_output(payload.as_bytes());
                return;
            }
        };
        if !self.ready {
            self.ready = true;
            self.flush_queue();
            return;
        }
        if self.outstanding.is_empty() {
            return;
        }
        // The command whose turn this is: popped, not named. It is already on the
        // screen, echoed by the shell right above this line; the code is the news.
        let _cmd = self.outstanding.pop_front();
        let interrupted = self.aborting;
        self.aborting = false;
        // The ladder is over with this command: the next one starts at attempt 0,
        // so an old deadline cannot match a new interrupt by coincidence.
        self.interrupt_attempt = 0;
        self.stall_reported = false;
        self.last_exit = Some(code);
        // Whatever screen the command was holding is gone with it, whether or not
        // the program ever said goodbye.
        self.release_screen_at_command_end();
        self.emit_exit(code, interrupted);
    }

    /// The visible form of a finished command: `exit 0`, or a loud red line for a
    /// failure, with the interrupt case named as itself rather than as an error.
    fn emit_exit(&self, code: i32, interrupted: bool) {
        if interrupted {
            self.note(format!("interrupted (exit {code})"));
        } else if code == 0 {
            self.note("exit 0");
        } else {
            self.err(format!("exit {code}"));
        }
    }

    /// The reader hit EOF: the shell exited (or is exiting).
    ///
    /// The session survives its shell. That is deliberate: the mode is not broken
    /// because bash quit, and the next command brings a shell back. What the user
    /// must not get is silence — the mode would look hung with the input box open
    /// and nothing behind it.
    pub(super) fn stream_end(&mut self) {
        // Say "screen is free" before anything else: the UI must stop teeing into
        // a screen whose owner just died, and it cannot redraw while it thinks the
        // child still holds it.
        self.release_screen_at_command_end();
        let reason = match self.shell.as_mut() {
            None => return,
            Some(shell) => match shell.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        ExitReason::Shutdown
                    } else {
                        ExitReason::Crashed {
                            code: Some(status.exit_code() as i32),
                        }
                    }
                }
                _ => {
                    shell.kill();
                    ExitReason::Unknown
                }
            },
        };
        // Drop the shell (which kills, and removes the generated rc file) rather
        // than keep a corpse borrowed.
        self.shell = None;
        let stuck = self
            .outstanding
            .front()
            .map(|c| c.trim())
            // Naming `exit` as the thing the shell died "while running" would be
            // the user's own request thrown back at them. Anything else is worth
            // naming: it is the command they thought had an exit code coming.
            .filter(|c| !c.is_empty() && !c.starts_with("exit") && !c.starts_with("logout"))
            .map(|c| format!(" while running `{}`", clip(c, 60)));
        let running = stuck.unwrap_or_default();
        self.outstanding.clear();
        self.ready = false;
        self.aborting = false;
        self.interrupt_attempt = 0;
        self.stall_reported = false;
        match &reason {
            ExitReason::Shutdown => {
                self.note(format!(
                    "shell exited{running}; type a command and a new one starts"
                ));
            }
            other => {
                self.err(format!(
                    "shell exited{running} ({}); type a command and a new one starts",
                    exit_text(other)
                ));
            }
        }
    }

    /// Bring the shell down without ever being the thing that blocks on it.
    ///
    /// The old shape of this function was a hang that only a busy machine could
    /// find (looprs-2ck): ask the shell to `exit`, sleep-poll `try_wait` for the
    /// grace, then `kill()` and **block in `Child::wait`**. That block is taken
    /// by the one task in this design that drains the pty byte lane, and the
    /// reader thread's only way out is to land a buffer on that lane. A shell
    /// that dies with output in flight blocks in `exit(2)` with its tty buffer
    /// unflushed, the reader parks in `blocking_send`, this task parks in
    /// `wait4`, and the three of them hold each other down forever — `ps` shows
    /// the child stuck in `?Es`, *trying to exit*, and never reaped. No
    /// tolerance fixes that, because nothing is running to be tolerant of.
    ///
    /// The rule that replaces it is **never reap alone**. Every wait in here
    /// takes the byte lane with it ([`reap_while_draining`]): the reader keeps
    /// emptying the pty, the dying child keeps flushing, and the death arrives
    /// on a `try_wait` instead of on a parked thread. And whatever has not
    /// arrived when the bound runs out is handed to a detached thread that owns
    /// the last blocking wait in this file ([`reap_off_task`]), so the two
    /// properties hold whatever the child decides to do next:
    ///
    /// * **this returns** — bounded by `EXIT_ASK + KILL_REAP`, with no wait of
    ///   any kind left behind on the task; and
    /// * **the reader thread ends** — this task finishing drops the lane's
    ///   receiver, which is exactly the wake-up a parked `blocking_send` is
    ///   waiting for; it returns `Err` and the read end goes with it.
    pub(super) fn shutdown(&mut self, rx: &mut mpsc::Receiver<ByteLane>) {
        // The screen debt is paid *before* the shell is sent anywhere. A program
        // that held the alternate screen and dies without saying so leaves the user
        // stranded inside it — their prompt gone, the dead program's paint still on
        // the glass — and this is the last moment the outer terminal is ours to
        // write to. `release_screen_at_command_end` is the boundary the SIGKILLed
        // vim case already uses; the quit path simply never reached it, because the
        // command never ended.
        //
        // It goes first, ahead of the kill, for the same reason `ScreenWatch` puts
        // a release after its bytes: the owed `ESC[?1049l` has to leave through a
        // UI that still believes the session owns the screen, because that is the
        // only state in which the app tees bytes instead of transcripting them.
        self.release_screen_at_command_end();

        let Some(mut shell) = self.shell.take() else {
            return;
        };

        // Ask first, and close the write end so a shell reading stdin sees EOF:
        // an orderly `exit` is the version of this where bash runs its own exit
        // traps and reports its own last status.
        let _ = shell.writer.write_all(b"\nexit\n");
        let _ = shell.writer.flush();

        // The polite reap. A shell with a foreground command that never ends
        // (`yes`, a build, a pager) never reads that `exit`, so this bound is
        // what turns "polite, indefinitely" into "polite for `EXIT_ASK`".
        if reap_while_draining(self, rx, &mut shell, EXIT_ASK) {
            return;
        }
        tracing::warn!("{}: shell would not exit; killing it", self.id);
        shell.kill();

        // The forced reap, in the same lock-step. A `SIGKILL`ed shell whose only
        // debt is a full tty is reaped inside this bound *because* the draining
        // is what pays it — which is precisely the debt that used to be paid by
        // never returning.
        if reap_while_draining(self, rx, &mut shell, KILL_REAP) {
            return;
        }

        // Two bounds deep and still not reaped: nothing this task can do next is
        // short, so it stops trying to be the one that finishes the job.
        reap_off_task(self.id, shell, &self.readers_live);
    }
}
