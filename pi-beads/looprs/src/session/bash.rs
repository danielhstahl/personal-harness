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

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::{Result, anyhow};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

use crate::screen::{Piece, ScreenWatch};
use crate::session::{
    ByteStream, ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned,
};

/// `DC1` — starts the exit marker. A control character no program emits by
/// accident, which is what makes an in-band marker safe here.
const MARKER_START: u8 = 0x11;
/// `DC2` — ends it.
const MARKER_END: u8 = 0x12;
/// The marker's payload prefix, between the two control characters.
const MARKER_PREFIX: &str = "looprs:exit:";
/// Cap on a buffered, unterminated marker. Past this the bytes are certainly not a
/// marker, and holding them would be a slow leak, so they are emitted as output.
const MARKER_BUFFER_MAX: usize = 4096;

/// How long a `kill` gets to land before we stop waiting for the child.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// The shell integration, generated at spawn time.
///
/// It sources the user's own `~/.bashrc` first (so Bash mode is *their* shell, not
/// a stripped-down one) and then chains a `PROMPT_COMMAND` that reports the exit
/// status of whatever just finished. Chaining rather than replacing matters: an
/// integration that clobbers the user's `PROMPT_COMMAND` would silently break
/// their title bars, git-status prompts and the rest.
///
/// Ends with `true` so the very first marker — which bash emits before any command
/// of ours has run — reads as success rather than as some half-loaded rc file.
fn shell_integration() -> String {
    r#"# looprs bash integration (generated file, safe to delete).
# 1. Be the user's shell first.
if [ -f "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi
# 2. Keep whatever PROMPT_COMMAND that installed, and report the exit status last.
__looprs_user_pc="${PROMPT_COMMAND:-}"
__looprs_report() {
  printf '\021looprs:exit:%s\022\n' "$1"
}
PROMPT_COMMAND='__looprs_rc=$?; if [ -n "$__looprs_user_pc" ]; then eval "$__looprs_user_pc"; fi; __looprs_report "$__looprs_rc"'
# 3. A prompt of our own only if the user has none, so the pane is never silent.
if [ -z "${PS1:-}" ]; then PS1='\u@\h:\w\$ '; fi
true
"#
    .to_string()
}

/// Write the integration to a temp file and return its path.
///
/// **Unique per spawn**, and that is not tidiness. `Shell::drop` removes this file,
/// and a single process can hold several shells at once — every `#[tokio::test]` in
/// this module runs in the same test binary, in parallel. Name the file after
/// anything shared (the pid alone, say) and one shell's teardown deletes the rcfile
/// another is mid-way through starting with: bash prints "no such file", the
/// `PROMPT_COMMAND` marker never arrives, and that session hangs with its input
/// queued forever.
fn write_integration() -> Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "looprs-bash-integration-{}-{}.sh",
        std::process::id(),
        seq
    ));
    std::fs::write(&path, shell_integration())
        .map_err(|e| anyhow!("could not write the bash integration file {path:?}: {e}"))?;
    Ok(path)
}

/// The live shell: the child, the two ends of its pty, and the reader thread.
struct Shell {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty>,
    integration: PathBuf,
}

impl Shell {
    fn spawn(cfg: &SessionConfig, size: PtySize, tx: mpsc::UnboundedSender<BashCmd>) -> Result<Self> {
        let integration = write_integration()?;
        let pty = native_pty_system();
        let pair = pty
            .openpty(size)
            .map_err(|e| anyhow!("could not open a pty for {}: {e}", cfg.shell_bin))?;

        let mut cmd = CommandBuilder::new(&cfg.shell_bin);
        // The ORDER is the fix, not cosmetics: bash 3.2 (the macOS system bash)
        // rejects a long option that comes after a short one. `bash -i --rcfile X`
        // dies with "--: invalid option" and the mode never starts; `bash --rcfile X
        // -i` works. Measured on 3.2.57; long-first is also fine on 5.x.
        // `-i` so bash reads the rcfile at all; `--rcfile` so the exit-status
        // integration is in place no matter what the user's own startup does.
        cmd.arg("--rcfile");
        cmd.arg(integration.to_string_lossy().as_ref());
        cmd.arg("-i");
        // A stable, non-login shell: no /etc/profile surprises on top of bashrc.
        cmd.env("LOOPRS_BASH", "1");
        // The child must not outlive us even if every orderly path fails.
        cmd.env("TERM", std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()));

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow!("could not spawn `{}`: {e}", cfg.shell_bin))?;
        // The slave is only needed to spawn; holding it open would keep the pty
        // from ever reporting EOF.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("could not read the pty master: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("could not write the pty master: {e}"))?;

        std::thread::Builder::new()
            .name("looprs-bash-reader".into())
            .spawn(move || read_master(reader, tx))
            .map_err(|e| anyhow!("could not start the pty reader thread: {e}"))?;

        Ok(Self {
            child,
            writer,
            master: pair.master,
            integration,
        })
    }

    fn write_line(&mut self, text: &str) -> Result<()> {
        self.writer
            .write_all(text.as_bytes())
            .and_then(|_| self.writer.flush())
            .map_err(|e| anyhow!("the shell's pty rejected input: {e}"))
    }

    /// Raw bytes to the master, verbatim, no newline appended.
    ///
    /// The difference from [`Shell::write_line`] is the whole point: a program in
    /// the alt screen reads keystrokes, not lines, and a newline we added would be
    /// an Enter the user never pressed.
    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
            .map_err(|e| anyhow!("the shell's pty rejected input: {e}"))
    }

    fn resize(&mut self, size: PtySize) {
        // A failed resize is not worth an error dialog: the shell keeps the old
        // size, which is degraded rather than broken.
        if let Err(e) = self.master.resize(size) {
            tracing::warn!("bash pty resize failed: {e}");
        }
    }

    /// Tear the shell down hard, and report how it went.
    fn kill_and_reap(&mut self) -> ExitReason {
        let _ = self.child.kill();
        match self.child.wait() {
            Ok(status) => {
                let code = status.exit_code();
                if status.success() || code == 0 {
                    ExitReason::Shutdown
                } else {
                    ExitReason::Crashed { code: Some(code as i32) }
                }
            }
            Err(_) => ExitReason::Unknown,
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // Whatever path gets us here — shutdown, replacement, a panic — the shell
        // goes with us. A bash that outlives looprs is the orphan looprs-ecr is
        // about, and ADR-0001's "Shutdown discipline".
        let _ = self.child.kill();
        let _ = std::fs::remove_file(&self.integration);
    }
}

/// The reader thread: blocking `read` on the master, bytes into the session task.
///
/// Ends at EOF (the child died, or we closed the master) or on a write error to a
/// mailbox nobody is reading any more.
fn read_master(mut reader: Box<dyn Read + Send>, tx: mpsc::UnboundedSender<BashCmd>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send(BashCmd::StreamEnd);
                return;
            }
            Ok(n) => {
                if tx.send(BashCmd::Bytes(buf[..n].to_vec())).is_err() {
                    return; // the session is gone; nothing to read for
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = tx.send(BashCmd::StreamEnd);
                return;
            }
        }
    }
}

/// Commands to the task that owns the pty.
enum BashCmd {
    /// A command line typed into the Bash box.
    Submit(String),
    /// `Esc` / Ctrl-C: `0x03` to the master, which the line discipline turns into
    /// SIGINT for the child's foreground process group. The command dies; the
    /// shell survives.
    Interrupt,
    /// Raw keystrokes for a child that currently holds the screen
    /// ([`ScreenWatch`]): written to the master exactly as typed, with no newline
    /// and no interpretation. This is what makes `Esc` be `Esc` inside vim
    /// instead of `0x03`.
    Keys(Vec<u8>),
    /// Raw bytes off the pty master.
    Bytes(Vec<u8>),
    /// The reader hit EOF — the shell is gone or going.
    StreamEnd,
    /// The real terminal changed shape; ADR-0001 rule 6: the child gets the real
    /// size, not a virtual 80x24.
    Resize { rows: u16, cols: u16 },
    /// Test seam: ack once every command queued before this one is handled.
    Sync(oneshot::Sender<()>),
    /// The app is exiting.
    Shutdown,
}

/// The shell, running inside its own task.
struct BashTask {
    id: SessionId,
    cfg: SessionConfig,
    ev_tx: mpsc::UnboundedSender<SessionEvent>,
    cmd: mpsc::UnboundedSender<BashCmd>,
    shell: Option<Shell>,
    /// Ever had one? `NotStarted` vs `Dead` is a difference the status row shows.
    had_shell: bool,
    /// Has this shell printed its first prompt?
    ///
    /// Readiness is not a nicety. Bash prints a `PROMPT_COMMAND` marker *before* it
    /// has read a single line of its own startup files' output ordering settled, so
    /// a command written too early has its marker answered by the *prompt* rather
    /// than by the command — which reads as "`false` exited 0". The first marker is
    /// therefore consumed as readiness, and input written only after it.
    ready: bool,
    /// Commands typed before the shell was ready, in order, still unwritten.
    queue: VecDeque<String>,
    /// Commands written to the pty that have not reported their exit marker yet.
    ///
    /// A queue of the command *texts* rather than a count because typed-ahead input
    /// is normal: three lines entered at a prompt produce three markers, and each
    /// one has to close the command it belongs to rather than the head of the
    /// queue. Keeping the text is what lets the death notice name the command that
    /// never reported, instead of saying "something was running".
    outstanding: VecDeque<String>,
    /// `0x03` went out and we are waiting for the command to unwind.
    aborting: bool,
    /// Unconsumed marker bytes (a marker can straddle two reads).
    pending: Vec<u8>,
    /// Who owns the real terminal screen, read out of the child's own bytes
    /// (ADR-0001 Q2). The watcher is the single source of truth: `is_held()` is
    /// the answer the whole app obeys.
    screen: ScreenWatch,
    /// Bytes held back only because they may be the start of a multi-byte UTF-8
    /// sequence that a read boundary split in half.
    utf8: Vec<u8>,
    size: PtySize,
    /// The exit code of the last finished command, for the status row.
    last_exit: Option<i32>,
}

impl BashTask {
    fn emit(&self, ev: SessionEvent) {
        let _ = self.ev_tx.send(ev);
    }

    fn note(&self, text: impl Into<String>) {
        self.emit(SessionEvent::System(text.into()));
    }

    fn err(&self, text: impl Into<String>) {
        self.emit(SessionEvent::Error(text.into()));
    }

    fn status(&self) -> SessionStatus {
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
        let shell = Shell::spawn(&self.cfg, self.size, self.cmd.clone())?;
        self.shell = Some(shell);
        self.had_shell = true;
        // A new shell is an unread shell: no prompt has come from it yet.
        self.ready = false;
        self.outstanding.clear();
        self.aborting = false;
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
    fn submit(&mut self, text: String) {
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

    /// `Esc` / Ctrl-C.
    ///
    /// Only meaningful while something is running. At an idle prompt `0x03` would
    /// print a `^C` for no reason, so an idle Bash session ignores it rather than
    /// making noise about a no-op.
    fn interrupt(&mut self) {
        if self.outstanding.is_empty() || self.aborting {
            return;
        }
        if let Some(shell) = self.shell.as_mut() {
            if let Err(e) = shell.write_line("\x03") {
                self.err(format!("interrupt did not reach the shell: {e:#}"));
                return;
            }
            self.aborting = true;
        }
    }

    /// Raw keystrokes for a child that holds the screen (ADR-0001 Q2).
    ///
    /// Written straight to the master with nothing added. No readiness queue: a
    /// child can only hold the screen if its shell is up and running it.
    fn write_keys(&mut self, bytes: Vec<u8>) {
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

    fn resize(&mut self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        self.size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Some(shell) = self.shell.as_mut() {
            shell.resize(self.size);
        }
    }

    /// Bytes off the master: split the exit markers out, forward everything else.
    ///
    /// The marker is consumed *here*, at the edge of the child's byte stream, so
    /// nothing downstream has to know it exists — neither the transcript nor the
    /// screen ever sees `0x11 … 0x12`.
    fn on_bytes(&mut self, bytes: Vec<u8>) {
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

    /// The screen changed hands. The session says so; the UI decides what that
    /// means for its own drawing.
    fn on_screen_change(&mut self, change: crate::screen::ScreenChange) {
        match change {
            crate::screen::ScreenChange::Takeover { alt } => {
                let what = self.foreground();
                self.note(format!(
                    "`{}` took the screen ({}); looprs stops drawing until it gives it back",
                    clip(&what, 60),
                    if alt {
                        "alt screen"
                    } else {
                        "cursor-addressed output"
                    }
                ));
                self.emit(SessionEvent::ScreenHeld { active: true });
            }
            crate::screen::ScreenChange::Release => {
                self.emit(SessionEvent::ScreenHeld { active: false });
            }
        }
    }

    /// The command currently in front of the shell, for a notice that has to name
    /// something rather than say "a child".
    fn foreground(&self) -> String {
        self.outstanding
            .front()
            .cloned()
            .unwrap_or_else(|| "the shell".to_string())
    }

    /// Take the screen back when a command ends without the child having said it
    /// did — `vim` killed with `SIGKILL`, `less` closed by a signal, a program
    /// that never emits a leave sequence.
    ///
    /// The watcher can only read a release out of bytes; the command boundary is
    /// the other thing that certainly means the screen is free. Without this the UI
    /// would keep teeing into a screen nobody owns and never redraw itself, which
    /// looks exactly like the frozen pane this whole path replaced.
    fn release_screen_at_command_end(&mut self) {
        if !self.screen.is_held() {
            return;
        }
        // Bytes the watcher was still deciding on belong to the screen that just
        // ended, and an alt-screen leave the program died before paying is paid on
        // its way out — otherwise the terminal stays on the dead program's screen
        // and looprs redraws into a buffer nobody is looking at.
        let owed = self.screen.force_release();
        if !owed.is_empty() {
            self.emit_bytes(&owed);
        }
        self.emit(SessionEvent::ScreenHeld { active: false });
    }

    /// The UTF-8-safe half of forwarding output: hold a split multi-byte sequence
    /// for the next read, emit the rest.
    fn emit_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut held = std::mem::take(&mut self.utf8);
        held.extend_from_slice(bytes);
        let (text, rest) = split_complete_utf8(held);
        self.utf8 = rest;
        if !text.is_empty() {
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
    fn stream_end(&mut self) {
        // Say "screen is free" before anything else: the UI must stop teeing into
        // a screen whose owner just died, and it cannot redraw while it thinks the
        // child still holds it.
        self.release_screen_at_command_end();
        let reason = match self.shell.as_mut() {
            None => return,
            Some(shell) => match shell.child.try_wait() {
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
                    let _ = shell.child.kill();
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

    fn shutdown(&mut self) {
        if let Some(mut shell) = self.shell.take() {
            // Close the write end first so a shell reading stdin sees EOF, then
            // make sure it is actually gone.
            let _ = shell.writer.write_all(b"\nexit\n");
            let _ = shell.writer.flush();
            let deadline = std::time::Instant::now() + KILL_WAIT;
            loop {
                match shell.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() > deadline {
                            tracing::warn!("{}: shell would not exit; killing it", self.id);
                            let _ = shell.kill_and_reap();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        }
        // Dropping `shell` kills anything still alive and removes the rc file.
    }
}

fn exit_text(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Crashed { code: Some(c) } => format!("code {c}"),
        ExitReason::Crashed { code: None } => "signal".to_string(),
        ExitReason::Shutdown => "shutdown".to_string(),
        ExitReason::Cancelled => "cancelled".to_string(),
        ExitReason::Unknown => "unknown reason".to_string(),
    }
}

/// Trim to `max` chars for a single-line notice, with an ellipsis when it bit.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Decode as much UTF-8 as is complete; keep the trailing partial sequence.
///
/// A pty read boundary can land in the middle of a multi-byte character, and a
/// `from_utf8_lossy` there would turn a perfectly good `é` into `?` forever. So the
/// incomplete tail is held for the next read instead.
fn split_complete_utf8(buf: Vec<u8>) -> (String, Vec<u8>) {
    match String::from_utf8(buf) {
        Ok(s) => (s, Vec::new()),
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            let bytes = e.into_bytes();
            let text = String::from_utf8_lossy(&bytes[..valid]).to_string();
            (text, bytes[valid..].to_vec())
        }
    }
}

/// Handle onto the Bash session.
pub struct BashSession {
    id: SessionId,
    cmd: mpsc::UnboundedSender<BashCmd>,
    status: Arc<StdMutex<SessionStatus>>,
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
        let status = Arc::new(StdMutex::new(SessionStatus::NotStarted));
        let task_status = status.clone();

        let mut task = BashTask {
            id,
            cfg: cfg.clone(),
            ev_tx: ev_tx.clone(),
            cmd: cmd_tx.clone(),
            shell: None,
            had_shell: false,
            ready: false,
            queue: VecDeque::new(),
            outstanding: VecDeque::new(),
            aborting: false,
            pending: Vec::new(),
            screen: ScreenWatch::new(),
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

        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    BashCmd::Submit(text) => task.submit(text),
                    BashCmd::Interrupt => task.interrupt(),
                    BashCmd::Keys(b) => task.write_keys(b),
                    BashCmd::Bytes(b) => task.on_bytes(b),
                    BashCmd::StreamEnd => task.stream_end(),
                    BashCmd::Resize { rows, cols } => task.resize(rows, cols),
                    BashCmd::Sync(tx) => {
                        // Publish the mirror before the ack, so a test that waits
                        // on the seam then reads `status()` sees the state as of
                        // that ack rather than one command stale.
                        *task_status.lock().unwrap() = task.status();
                        let _ = tx.send(());
                    }
                    BashCmd::Shutdown => {
                        task.shutdown();
                        *task_status.lock().unwrap() = SessionStatus::Dead;
                        let _ = ev_tx.send(SessionEvent::Exited {
                            reason: ExitReason::Shutdown,
                        });
                        break;
                    }
                }
                *task_status.lock().unwrap() = task.status();
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
            },
            ev_rx,
        ))
    }

    /// Test seam: returns once every command queued before this call has been
    /// fully handled.
    pub async fn quiesce(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(BashCmd::Sync(tx)).is_err() {
            return true;
        }
        tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .is_ok()
    }

    /// Ask the shell to interrupt whatever it is running. Distinct from
    /// [`Session::abort`] only in that a test can name what it is asking for.
    pub fn interrupt(&self) -> Result<()> {
        self.cmd
            .send(BashCmd::Interrupt)
            .map_err(|_| anyhow!("bash session task is gone"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TerminalType;

    const NO_HANG: Duration = Duration::from_secs(20);

    /// The real system bash. A fake cannot prove that a pty keeps its cwd, that
    /// `0x03` interrupts, or that `PROMPT_COMMAND` fires the way the ADR says it
    /// does — those are properties of the shell and the line discipline, so the
    /// test uses the real thing.
    fn real_shell() -> String {
        for cand in ["/bin/bash", "/opt/homebrew/bin/bash", "/usr/local/bin/bash"] {
            if std::path::Path::new(cand).exists() {
                return cand.to_string();
            }
        }
        panic!("no bash to test against");
    }

    fn bash(generation: u64) -> (BashSession, mpsc::UnboundedReceiver<SessionEvent>) {
        let cfg = SessionConfig {
            shell_bin: real_shell(),
            ..Default::default()
        };
        // Start the pty at a known width so wrapped output is predictable.
        let (mut s, rx) = BashSession::build(SessionId::new(TerminalType::Bash, generation), &cfg)
            .expect("build");
        s.resize(24, 80).ok();
        (s, rx)
    }

    fn describe(ev: SessionEvent) -> String {
        match ev {
            SessionEvent::BashOutput { chunk, .. } => format!("out {chunk}"),
            SessionEvent::System(t) => format!("system: {t}"),
            SessionEvent::Error(t) => format!("error: {t}"),
            SessionEvent::Exited { reason } => format!("down {reason:?}"),
            SessionEvent::ScreenHeld { active } => format!("screen {active}"),
            other => format!("{other:?}"),
        }
    }

    /// Everything already arrived.
    fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(describe(ev));
        }
        out
    }

    async fn next_event(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> String {
        let ev = tokio::time::timeout(NO_HANG, rx.recv())
            .await
            .expect("the bash session went silent")
            .expect("the bash session stream closed");
        describe(ev)
    }

    /// What a finished command gave us: the raw bytes it wrote, its exit code, and
    /// every non-output event (notice / error / marker-shaped surprise) that came
    /// with it. The `notes` half exists so a failure says what happened instead of
    /// timing out twenty seconds later with nothing to look at.
    #[derive(Debug)]
    struct Ran {
        out: String,
        code: Option<i32>,
        notes: Vec<String>,
    }

    /// `"system: exit 1"` / `"error: exit 1"` / `"system: interrupted (exit 130)"`.
    fn exit_code_of(line: &str) -> Option<i32> {
        if let Some(rest) = line
            .strip_prefix("system: exit ")
            .or_else(|| line.strip_prefix("error: exit "))
        {
            return rest.trim().parse::<i32>().ok();
        }
        if line.contains("interrupted (exit ") {
            return line
                .trim_end_matches(')')
                .rsplit(' ')
                .next()
                .and_then(|c| c.parse::<i32>().ok());
        }
        None
    }

    /// Read until the shell reports the exit of the command just sent.
    async fn run_logged(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Ran {
        let deadline = tokio::time::Instant::now() + NO_HANG;
        let mut ran = Ran { out: String::new(), code: None, notes: Vec::new() };
        loop {
            let line = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => describe(ev),
                Ok(None) => panic!("the bash session stream closed: {:?}", ran.notes),
                Err(_) => panic!(
                    "the shell never reported an exit. events so far: {:?}",
                    ran.notes
                ),
            };
            if let Some(rest) = line.strip_prefix("out ") {
                ran.out.push_str(rest);
                continue;
            }
            if line.starts_with("down ") {
                panic!("the shell died while running the command: {line}");
            }
            ran.notes.push(line.clone());
            if let Some(code) = exit_code_of(&line) {
                ran.code = Some(code);
                return ran;
            }
        }
    }

    /// As [`run_logged`], keeping only the two answers most tests care about.
    async fn run_command(
        rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
    ) -> (String, Option<i32>) {
        let ran = run_logged(rx).await;
        (ran.out, ran.code)
    }

    /// The marker must never reach anyone downstream. It is stripped at the edge,
    /// so no event may contain the control characters.
    fn assert_no_marker_bytes(text: &str) {
        assert!(
            !text.contains('\u{11}') && !text.contains('\u{12}'),
            "exit-marker control bytes leaked into the stream: {text:?}"
        );
    }

    /// Cold start is lazy: nothing exists until the first command.
    #[tokio::test]
    async fn no_shell_exists_before_the_first_command() {
        let (s, mut rx) = bash(1);
        assert_eq!(s.status(), SessionStatus::NotStarted);
        assert!(rx.try_recv().is_err(), "and it says nothing either");
    }

    /// **Acceptance: `echo hi` -> `hi` appears, exit 0.**
    #[tokio::test]
    async fn echo_streams_its_output_and_reports_exit_0() {
        let (mut s, mut rx) = bash(2);
        s.send_text("echo hi".into()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert!(out.contains("hi"), "output was {out:?}");
        assert_eq!(code, Some(0));
        assert_no_marker_bytes(&out);
        assert_eq!(s.status(), SessionStatus::Idle, "settled back to idle");
    }

    /// **Acceptance: cwd persists.** The whole reason for a persistent shell: a
    /// one-shot `bash -c` cannot remember a `cd`.
    #[tokio::test]
    async fn cwd_persists_across_commands() {
        let (mut s, mut rx) = bash(3);
        let tmp = std::env::temp_dir();
        let target = "/tmp";
        s.send_text(format!("cd {target}")).unwrap();
        run_command(&mut rx).await;
        assert!(
            tmp.exists(),
            "test precondition: {target} should exist"
        );

        s.send_text("pwd".into()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0));
        let trimmed: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('/') && !l.is_empty())
            .collect();
        assert!(
            trimmed
                .iter()
                .any(|l| *l == "/tmp" || *l == "/private/tmp" || l.ends_with("/tmp")),
            "cwd did not survive the previous command: {out:?}"
        );
    }

    /// **Acceptance: `false` -> visibly exit 1.**
    #[tokio::test]
    async fn a_failing_command_is_loudly_not_zero() {
        let (mut s, mut rx) = bash(4);
        s.send_text("false".into()).unwrap();
        let (_out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(1));
        let msgs = drain(&mut rx);
        assert_no_marker_bytes(&msgs.join("|"));
    }

    /// **Readiness, and the counter, together.** Three commands typed into a cold
    /// shell with no waiting between them: none of them may be written before the
    /// shell prints its first prompt, and each exit marker must close the command
    /// it belongs to rather than the head of the queue.
    ///
    /// This is the regression that read as "`false` exited 0": bash emits a
    /// `PROMPT_COMMAND` marker for its *first* prompt before it has read any of
    /// our input, so with a plain `running` bool that marker answered the command
    /// typed at cold start.
    #[tokio::test]
    async fn commands_typed_before_the_shell_is_up_run_in_order_and_each_owns_its_exit() {
        let (mut s, mut rx) = bash(14);
        s.send_text("echo first-cmd".into()).unwrap();
        s.send_text("false".into()).unwrap();
        s.send_text("echo third-cmd".into()).unwrap();

        let a = run_logged(&mut rx).await;
        assert_eq!(a.code, Some(0), "first: {:?}", a);
        assert!(a.out.contains("first-cmd"), "first: {:?}", a.out);

        let b = run_logged(&mut rx).await;
        assert_eq!(b.code, Some(1), "second: {:?}", b.notes);

        let c = run_logged(&mut rx).await;
        assert_eq!(c.code, Some(0), "third: {:?}", c.notes);
        assert!(c.out.contains("third-cmd"), "third: {:?}", c.out);

        assert_eq!(s.status(), SessionStatus::Idle, "everything settled");
    }

    /// **Acceptance: stderr shows up, interleaved with stdout.** A pty merges the
    /// two, so ordering is the program's own; a piped backend loses it.
    #[tokio::test]
    async fn stderr_and_stdout_arrive_in_the_programs_own_order() {
        let (mut s, mut rx) = bash(5);
        // One command, both streams, ordered: stdout, stderr, stdout.
        s.send_text("echo one; echo two 1>&2; echo three".into())
            .unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0));
        let one = out.find("one").expect("stdout missing");
        let two = out.find("two").expect("stderr missing");
        let three = out.find("three").expect("later stdout missing");
        assert!(
            one < two && two < three,
            "streams were reordered: {out:?}"
        );
    }

    /// `ls /nope` is the acceptance case that *stderr reaches the transcript*.
    #[tokio::test]
    async fn a_missing_path_shows_its_stderr() {
        let (mut s, mut rx) = bash(6);
        s.send_text("ls /definitely-not-a-real-path-xyz".into())
            .unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_ne!(code, Some(0), "ls of a missing path must not be a success");
        assert!(
            out.contains("No such file") || out.contains("cannot access"),
            "stderr never arrived: {out:?}"
        );
    }

    /// **Acceptance: Esc interrupts a long command and the shell survives.**
    #[tokio::test]
    async fn esc_interrupts_a_running_command_without_killing_the_shell() {
        let (mut s, mut rx) = bash(7);
        s.send_text("sleep 30".into()).unwrap();
        // Wait until the command is actually in flight.
        for _ in 0..200 {
            if s.status() == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(s.status(), SessionStatus::Running, "sleep is running");

        let started = std::time::Instant::now();
        s.abort().unwrap();
        // Esc must be *acted on* fast; the interrupt's own exit line is the proof.
        let _ = s.quiesce().await;
        assert!(started.elapsed() < Duration::from_secs(1), "Esc was slow");
        assert_eq!(s.status(), SessionStatus::Aborting);

        let (out, code) = run_command(&mut rx).await;
        assert!(
            code == Some(130) || code == Some(143) || code == Some(1),
            "sleep should have been interrupted, got {code:?} (out {out:?})"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the interrupt took {:?}; the command ran to completion instead",
            started.elapsed()
        );
        assert_eq!(s.status(), SessionStatus::Idle, "shell is still alive");

        // …and still usable afterwards.
        s.send_text("echo still-here".into()).unwrap();
        let (out2, code2) = run_command(&mut rx).await;
        assert_eq!(code2, Some(0));
        assert!(out2.contains("still-here"), "{out2:?}");
    }

    /// Esc at an idle prompt is a no-op: it must not interrupt anything, and must
    /// not produce a spurious exit line or error.
    #[tokio::test]
    async fn esc_at_an_idle_prompt_is_silent() {
        let (mut s, mut rx) = bash(8);
        s.send_text("echo first".to_string()).unwrap();
        run_command(&mut rx).await;
        assert_eq!(s.status(), SessionStatus::Idle);
        drain(&mut rx);

        s.abort().unwrap();
        assert!(s.quiesce().await);
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().all(|m| !m.starts_with("error")),
            "an idle Esc is not an error: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .all(|m| !m.contains("exit ") && !m.contains("interrupted")),
            "an idle Esc reported a command that never ran: {msgs:?}"
        );
    }

    /// **Acceptance: `exit` -> a notice, and the next command works.**
    #[tokio::test]
    async fn exiting_the_shell_is_a_notice_and_the_next_command_starts_a_new_one() {
        let (mut s, mut rx) = bash(9);
        s.send_text("echo before".to_string()).unwrap();
        run_command(&mut rx).await;

        s.send_text("exit".into()).unwrap();
        let mut notices: Vec<String> = Vec::new();
        loop {
            let line = next_event(&mut rx).await;
            if line.starts_with("out ") {
                continue;
            }
            notices.push(line.clone());
            if line.contains("shell exited") {
                break;
            }
        }
        assert!(
            notices.iter().any(|l| l.contains("shell exited")),
            "the shell's exit was silent: {notices:?}"
        );
        assert_eq!(s.status(), SessionStatus::Dead);

        // The mode is not wedged: the next command brings a shell back, and says
        // so. The restart notice arrives *during* the command's own stream, so it
        // has to be read from there — draining afterwards misses it entirely, and
        // an assertion that cannot see it is an assertion that cannot fail.
        s.send_text("echo after".into()).unwrap();
        let ran = run_logged(&mut rx).await;
        assert_eq!(ran.code, Some(0));
        assert!(ran.out.contains("after"), "{:?}", ran.out);
        assert!(
            ran.notes.iter().any(|m| m.contains("restarted")),
            "the respawn was invisible: {:?}",
            ran.notes
        );
    }

    /// Env and shell state survive too, not just cwd — the persistent-shell claim.
    #[tokio::test]
    async fn exported_variables_persist_across_commands() {
        let (mut s, mut rx) = bash(10);
        s.send_text("export LOOPRS_PROBE=present".into()).unwrap();
        run_command(&mut rx).await;
        s.send_text("echo $LOOPRS_PROBE".into()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0));
        assert!(out.contains("present"), "env did not persist: {out:?}");
    }

    /// A command with a non-UTF-8-safe boundary must not garble: the reader holds
    /// the partial sequence until the rest arrives.
    #[test]
    fn a_utf8_sequence_split_across_reads_is_not_lost() {
        let bytes = "héllo".as_bytes();
        // Split in the middle of the two-byte `é`: `h` + the first half of it, and
        // nothing else may be handed back — 0xc3 on its own is not text.
        let (a, b) = split_complete_utf8(bytes[..2].to_vec());
        assert_eq!(a, "h");
        assert_eq!(b, vec![0xc3]);
        let (c, rest) = split_complete_utf8(b.into_iter().chain(bytes[2..].to_vec()).collect());
        assert_eq!(a + &c, "héllo");
        assert!(rest.is_empty());
    }

    /// The marker can straddle two reads, and a byte of it in the wrong place must
    /// not corrupt the output around it.
    #[tokio::test]
    async fn a_command_whose_output_ends_without_a_trailing_newline_still_terminates() {
        let (mut s, mut rx) = bash(11);
        s.send_text("printf 'no-newline-end'".to_string()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0));
        assert!(out.contains("no-newline-end"), "{out:?}");
        assert_no_marker_bytes(&out);
    }

    /// **ADR-0001 rule 6, asked of the shell itself**: `stty size` answers the
    /// window we set rather than one frozen at process start, and a resize that
    /// arrives *before* the shell exists is remembered and used for the pty when it
    /// finally comes up. Nothing downstream can fix a wrong wrap afterwards: the
    /// child already wrote its lines for the width it believed in.
    #[tokio::test]
    async fn the_shell_is_sized_to_the_real_window() {
        let (mut s, mut rx) = bash(15);
        // Before the first command: there is no shell to resize yet, so the size
        // is remembered for the spawn.
        s.resize(40, 132).unwrap();
        s.send_text("stty size".into()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0), "stty said: {out:?}");
        assert!(
            out.contains("40 132"),
            "the pty was opened at the wrong size: {out:?}"
        );

        // With the shell running, a live resize reaches it too.
        s.resize(12, 90).unwrap();
        s.send_text("stty size".into()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0), "stty said: {out:?}");
        assert!(
            out.contains("12 90"),
            "the running shell kept the old size: {out:?}"
        );
    }

    /// A command that dies with the shell is **named**. "Something was running" is
    /// not actionable; "shell exited while running `sleep 30`" tells the user
    /// which exit code they are never going to see.
    ///
    /// Driven by the shell's own death (a background job SIGKILLs it mid-`sleep`),
    /// not by app shutdown: that is the case the user hits, and the one the StreamEnd
    /// path exists for.
    #[tokio::test]
    async fn a_command_that_never_reported_is_named_when_the_shell_dies() {
        let (mut s, mut rx) = bash(16);
        s.send_text("(sleep 1; kill -KILL $$) >/dev/null 2>&1 & sleep 30".into())
            .unwrap();
        for _ in 0..200 {
            if s.status() == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(s.status(), SessionStatus::Running, "sleep is in flight");

        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + NO_HANG;
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await
            else {
                continue;
            };
            let line = describe(ev);
            seen.push(line.clone());
            if line.contains("shell exited") {
                assert!(
                    line.contains("while running `"),
                    "the unreported command was never named: {line}"
                );
                return;
            }
            if line.starts_with("down ") {
                panic!("the session reported itself gone without a notice: {seen:?}");
            }
        }
        panic!("the shell's death was silent: {seen:?}");
    }

    /// Shutdown kills the shell: no bash left behind.
    #[tokio::test]
    async fn shutdown_leaves_no_shell_running() {
        let (mut s, mut rx) = bash(12);
        s.send_text("echo hi".to_string()).unwrap();
        run_command(&mut rx).await;
        assert_eq!(s.status(), SessionStatus::Idle);

        s.shutdown().unwrap();
        let mut down = false;
        for _ in 0..50 {
            let line = next_event(&mut rx).await;
            if line.starts_with("down ") {
                down = true;
                break;
            }
        }
        assert!(down, "shutdown did not report the session gone");
        assert_eq!(s.status(), SessionStatus::Dead);
    }

    /// A shell that cannot be spawned is a reported error, not a hang: the mode
    /// must say what went wrong rather than sit with the input box open.
    #[tokio::test]
    async fn a_shell_that_will_not_start_is_reported() {
        let cfg = SessionConfig {
            shell_bin: "/definitely/not/a/shell".into(),
            ..Default::default()
        };
        let (mut s, mut rx) =
            BashSession::build(SessionId::new(TerminalType::Bash, 13), &cfg).unwrap();
        s.send_text("echo hi".into()).unwrap();
        let line = next_event(&mut rx).await;
        assert!(line.starts_with("error"), "{line}");
        assert!(line.contains("shell"), "{line}");
        assert_eq!(s.status(), SessionStatus::NotStarted);
    }

    /// Read every event until the command's exit line, keeping the stream order.
    ///
    /// Order is the thing this test needs: the screen takeover has to be reported
    /// *before* the bytes that switch screens, and the release *after* the bytes
    /// that switch back. Anything that flattens the stream into a set cannot see
    /// the difference between those two and a bug.
    async fn until_exit(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + NO_HANG;
        let mut events = Vec::new();
        loop {
            let line = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => describe(ev),
                Ok(None) => panic!("stream closed: {events:?}"),
                Err(_) => panic!("no exit line; events: {events:?}"),
            };
            let done = exit_code_of(&line).is_some();
            events.push(line);
            if done {
                return events;
            }
        }
    }

    /// **Acceptance: a full-screen program is handed the screen.**
    ///
    /// Driven with `printf` rather than real vim because what is being proved is
    /// the session's own behaviour — announce the takeover, then the paint, then
    /// the release — and `printf` emits the same alt-screen bytes vim does without
    /// depending on vim's timing. That vim itself reaches the screen is proved in
    /// the real terminal by `spikes/vim_fullscreen.py`.
    #[tokio::test]
    async fn a_full_screen_program_is_handed_the_screen_and_gives_it_back() {
        let (mut s, mut rx) = bash(21);
        s.send_text("printf '\\033[?1049h\\033[?25lpainted\\033[?1049l'".into())
            .unwrap();
        let events = until_exit(&mut rx).await;

        let took = events
            .iter()
            .position(|e| e == "screen true")
            .unwrap_or_else(|| panic!("the takeover was never reported: {events:?}"));
        // The *program's* paint, not the echo of the command that printed it: the
        // echo contains the literal characters `\033[?1049h…` and the word
        // "painted", so only an actual escape byte tells them apart.
        let painted = events
            .iter()
            .position(|e| {
                e.starts_with("out ") && e.contains("painted") && e.contains('\u{1b}')
            })
            .unwrap_or_else(|| panic!("the paint never arrived: {events:?}"));
        let released = events
            .iter()
            .position(|e| e == "screen false")
            .unwrap_or_else(|| panic!("the release was never reported: {events:?}"));
        assert!(
            took < painted,
            "the UI must be teeing before the bytes that switch the screen: {events:?}"
        );
        assert!(
            painted < released,
            "the leave bytes must reach the terminal before the release: {events:?}"
        );
        let joined = events.join("|");
        assert_no_marker_bytes(&joined);
        assert_eq!(s.status(), SessionStatus::Idle, "and the shell is fine");
    }

    /// The non-alt-screen case: a program that repaints in place without ever
    /// switching screens (the shape ADR-0001's measurement found — cursor
    /// addressing in a chunk with no linefeeds). The screen comes back at the
    /// command boundary, because that program has no leave sequence to wait for.
    #[tokio::test]
    async fn a_repainting_program_takes_the_screen_and_loses_it_with_the_command() {
        let (mut s, mut rx) = bash(22);
        // Two separate writes: the first is a line (the transcript's business), the
        // second moves the cursor back up over it, which is the paint.
        s.send_text(
            "printf 'first-line\\n'; sleep 0.3; printf '\\033[2Aover-the-top'".into(),
        )
        .unwrap();
        let events = until_exit(&mut rx).await;

        let took = events
            .iter()
            .position(|e| e == "screen true")
            .expect("the repaint should take the screen");
        let painted = events
            .iter()
            .rposition(|e| e.starts_with("out ") && e.contains("over-the-top"))
            .expect("the repaint output never arrived");
        let released = events
            .iter()
            .position(|e| e == "screen false")
            .expect("the command boundary must release the screen");
        assert!(took < painted, "announced before the paint: {events:?}");
        assert!(
            painted <= released,
            "and released only once the command is over: {events:?}"
        );
    }

    /// **Ordinary output must never trip the screen path.** If it did, `ls` would
    /// suspend the UI and stop the transcript, which is a much worse bug than a
    /// full-screen program that does not show.
    #[tokio::test]
    async fn ordinary_output_never_claims_the_screen() {
        let (mut s, mut rx) = bash(23);
        s.send_text("printf 'one\\ntwo\\nthree\\n'".into()).unwrap();
        let events = until_exit(&mut rx).await;
        assert!(
            events.iter().all(|e| !e.starts_with("screen")),
            "a plain three-line command took the screen: {events:?}"
        );
        // Colour is the common false-positive risk: `ls --color`, `git log`.
        s.send_text("printf '\\033[31mred\\033[0m text with no newline'".into())
            .unwrap();
        let events = until_exit(&mut rx).await;
        assert!(
            events.iter().all(|e| !e.starts_with("screen")),
            "an SGR-coloured chunk is still a line of output: {events:?}"
        );
    }

    /// **Acceptance: raw keystrokes reach the child with nothing added.**
    ///
    /// `Esc` has to arrive as one `0x1b` byte, not `0x03` and not with a newline
    /// on it: this is what lets a program in the alt screen read the keyboard.
    /// Checked end to end on a real pty, because the line discipline's echo is the
    /// only honest witness to what actually went down the master.
    #[tokio::test]
    async fn raw_keys_reach_the_child_verbatim_and_the_shell_still_survives() {
        let (mut s, mut rx) = bash(24);
        s.send_text("printf 'first-line\\n'; sleep 1.5".into()).unwrap();
        // Wait until the command is genuinely in flight, then type a keystroke
        // sequence: Esc, ':' , 'w', 'q', '!' and CR — the shape of `:wq!`.
        for _ in 0..200 {
            if s.status() == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(s.status(), SessionStatus::Running, "sleep is running");

        s.send_bytes(vec![0x1b, b':', b'w', b'q', b'!', 0x0d]).unwrap();
        let ran = run_logged(&mut rx).await;
        // The bytes were echoed by the line discipline: `:wq!` appearing in the
        // output is the witness that they went down the master verbatim, which is
        // the only thing a session-side test can honestly see. `first-line` proves
        // the shell was alive to be typed at.
        assert!(ran.out.contains("first-line"), "{:?}", ran.out);
        assert!(
            ran.out.contains(":wq!"),
            "the keystrokes did not come back echoed, so they never reached the pty: {:?}",
            ran.out
        );
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// A held screen must not swallow the command boundary: the `exit 0` still has
    /// to reach the transcript after the release, or the user sees no result at all.
    #[tokio::test]
    async fn the_command_result_is_still_said_after_a_screen_session() {
        let (mut s, mut rx) = bash(25);
        s.send_text("printf '\\033[?1049hscreen\\033[?1049l'; echo done-marker".into())
            .unwrap();
        let ran = run_logged(&mut rx).await;
        assert_eq!(ran.code, Some(0), "{:?}", ran.notes);
        assert!(ran.out.contains("done-marker"), "{:?}", ran.out);
        assert!(ran.notes.iter().any(|n| n.contains("exit 0")), "{:?}", ran.notes);
    }
}
