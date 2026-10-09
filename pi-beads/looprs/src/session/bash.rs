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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

use crate::screen::{Piece, ScreenWatch};
use crate::session::{
    ByteStream, ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus,
    Spawned, cancel, publish_liveness,
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

/// How many pty read buffers may be sitting between the reader thread and the
/// session task (looprs-6cj).
///
/// A *bound*, not a tuning knob: with the reader charging the UI's output budget
/// one token per read, this is the other half of the ceiling on how far ahead of
/// the screen a shell is ever allowed to run. 8 × 8 KiB = 64 KiB.
pub(crate) const BYTE_LANE_DEPTH: usize = 8;

/// How long the shell gets to leave on its own after being asked, before the
/// kill goes in.
const EXIT_ASK: Duration = Duration::from_secs(2);

/// How long to keep pumping the byte lane after the kill, waiting for the reap.
///
/// A `SIGKILL`ed shell is dead already; the only thing that can keep the *wait*
/// waiting is the tty buffer it still has to flush, and that drains at the
/// reader thread's pace. One second of pumping covers a full pty at any speed
/// this pipe can carry it. Past that the shell is not coming back on this
/// task's clock, and the remaining wait moves off the task (see
/// [`reap_off_task`]).
const KILL_REAP: Duration = Duration::from_secs(1);

/// The gap between reap polls.
///
/// Short enough to notice a death inside a frame, long enough not to burn a
/// core while a shell drains a full pty buffer.
const REAP_POLL: Duration = Duration::from_millis(5);

/// How long the reaper thread polls before it stops trying to be quiet about a
/// child that has not been reaped.
///
/// After this it says out loud what it is waiting for and takes the blocking
/// `wait` — which is only acceptable because the thread taking it is detached
/// and nothing in the process is waiting on *it*.
const REAPER_REPORT: Duration = Duration::from_millis(500);

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
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
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
    /// The child, until the shutdown handoff takes it (looprs-2ck).
    ///
    /// An `Option` for exactly one reason: the shutdown path has to be able to
    /// leave this struct behind holding everything *except* the thing it cannot
    /// wait for. Once the reaper owns the child this is `None`, which is also
    /// what stops [`Drop for Shell`] from killing a child somebody else has
    /// promised to wait on. A `Shell` with `child == None` is not a broken
    /// shell, it is a shell whose reap is elsewhere.
    child: Option<Box<dyn Child + Send + Sync>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty>,
    integration: PathBuf,
}

impl Shell {
    fn spawn(
        cfg: &SessionConfig,
        size: PtySize,
        tx: mpsc::Sender<ByteLane>,
        readers_live: &Arc<AtomicUsize>,
    ) -> Result<Self> {
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
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        );

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

        let budget = cfg.output_budget.clone();
        // Counted from *before* the spawn until the thread's last instruction.
        // The window worth measuring is "a reader thread exists that might be
        // parked", and a thread that has been spawned but not yet entered its
        // body is exactly that; a count taken inside the thread could read 0
        // while a parked thread already exists.
        let live = LiveReaders::new(readers_live);
        std::thread::Builder::new()
            .name("looprs-bash-reader".into())
            .spawn(move || {
                // Held for the whole thread, including the unwinding exit: the
                // guard is what makes "is a reader still parked?" answerable at
                // shutdown instead of assumed (looprs-2ck).
                let _live = live;
                read_master(reader, tx, budget)
            })
            .map_err(|e| anyhow!("could not start the pty reader thread: {e}"))?;

        Ok(Self {
            child: Some(child),
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

    /// Poll the child without blocking it.
    ///
    /// `Ok(None)` covers both "still running" and "this handle gave the child
    /// to the reaper" — every caller of this treats the two the same way,
    /// because in both cases there is nothing here to wait on.
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    /// `SIGKILL` the child, if this handle still owns one.
    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // Whatever path gets us here — shutdown, replacement, a panic — the shell
        // goes with us. A bash that outlives looprs is the orphan looprs-ecr is
        // about, and ADR-0001's "Shutdown discipline".
        //
        // Kill only; **never wait**. A `wait` here would be a blocking reap taken
        // from inside whatever task happened to drop the shell, which is the
        // hang looprs-2ck is about, wearing a `Drop` as a disguise. A child
        // killed and not waited on is a zombie for the rest of this process's
        // life, which costs one pid and blocks nothing; a thread parked in
        // `wait4` on the task that owns the pty costs the whole process.
        self.kill();
        let _ = std::fs::remove_file(&self.integration);
    }
}

/// The byte lane out of the pty: a read buffer, or the end of the stream.
enum ByteLane {
    /// One `read(2)` of the master. Bounded by the reader's own buffer.
    Chunk(Vec<u8>),
    /// EOF or a read error: nothing more will ever come.
    Eof,
}

/// A guard that keeps a session's live-reader count honest.
///
/// Incremented by the spawner *before* the thread exists and decremented when
/// the thread's body finishes — by return, by early exit, or by unwinding, which
/// is the whole reason it is a `Drop` rather than two statements. That pairing
/// is what lets the shutdown path ask "is a reader still parked?" instead of
/// assuming one is not (looprs-2ck).
struct LiveReaders(Arc<AtomicUsize>);

impl LiveReaders {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for LiveReaders {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The reader thread: blocking `read` on the master, bytes into the session task.
///
/// Ends at EOF (the child died, or we closed the master) or on a write error to a
/// mailbox nobody is reading any more.
///
/// **This thread is where the byte bound is enforced** (looprs-6cj). Every read
/// takes a token from the UI's supply first, and only one token per read means
/// only one read buffer can be outstanding per token the UI has given back. When
/// the UI stops draining, the tokens stop coming back, this thread parks here,
/// the pty's kernel buffer fills, and the child blocks in `write(2)`. That is
/// the whole reason the bound has to live this far upstream: a ceiling that sits
/// in front of the UI but behind the producer does not stop the producer, it
/// just chooses a different place for the backlog to grow.
///
/// It parks on a *blocking* send supply rather than a channel, deliberately: the
/// control lane (`BashCmd`) is shared with `Interrupt`, and a bound that can
/// keep `Ctrl-C` queued behind 400 MB of `yes` output is worse than no bound at
/// all. Here, waiting costs the interrupt nothing.
fn read_master(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<ByteLane>,
    budget: crate::bus::Budget,
) {
    let mut buf = [0u8; 8192];
    loop {
        // Charged *before* the read so a slow consumer stops us producing,
        // not after we already have the bytes. A closed supply means the UI is
        // gone; there is nobody left to read for.
        if !budget.acquire_blocking() {
            let _ = tx.blocking_send(ByteLane::Eof);
            return;
        }
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = tx.blocking_send(ByteLane::Eof);
                return;
            }
            Ok(n) => {
                if tx
                    .blocking_send(ByteLane::Chunk(buf[..n].to_vec()))
                    .is_err()
                {
                    budget.release();
                    return; // the session is gone; nothing to read for
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                budget.release();
                continue;
            }
            Err(_) => {
                let _ = tx.blocking_send(ByteLane::Eof);
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

/// The shell, running inside its own task.
struct BashTask {
    id: SessionId,
    cfg: SessionConfig,
    /// The producing end of the pty byte lane, kept so a respawned shell's
    /// reader thread can be pointed at the same lane (looprs-6cj).
    ///
    /// It is a **bounded** lane, and that is the point: the reader thread parks
    /// when the lane is full, which is what lets a fast producer be throttled at
    /// the file descriptor instead of in memory. It is *separate* from
    /// [`BashCmd`] for the same reason: nothing the user types may ever queue
    /// behind output, and the control lane stays unbounded and polled first.
    bytes_tx: mpsc::Sender<ByteLane>,
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
    /// How many `0x03`s this cancel has sent. The stall watchdog carries one, so a
    /// deadline for an interrupt already resolved cannot be mistaken for a deadline
    /// on the current one — the same job `serial` does everywhere else.
    interrupt_attempt: u32,
    /// The stall has been reported for the current attempt. Set once per attempt:
    /// the escalation is one honest sentence, not a nagging timer. The user's next
    /// `Esc` clears it and starts a fresh attempt.
    stall_reported: bool,
    /// Unconsumed marker bytes (a marker can straddle two reads).
    pending: Vec<u8>,
    /// How many `BashOutput` events this task has emitted, ever.
    ///
    /// Used to answer one question per pty read: did this chunk become a
    /// message or not? If it did not, no downstream stage will ever hand the
    /// reader its credit back, so this task has to (see the byte arm of the
    /// task loop).
    output_emitted: u64,
    /// Who owns the real terminal screen, read out of the child's own bytes
    /// (ADR-0001 Q2). The watcher is the single source of truth: `is_held()` is
    /// the answer the whole app obeys.
    screen: ScreenWatch,
    /// How many pty reader threads this session has that have not finished.
    ///
    /// Not a metric: it is the shutdown path's view of the *second* property
    /// looprs-2ck has to hold, and it has to be counted rather than inferred
    /// because "the reader thread ended" is a statement about a thread this
    /// task cannot see. Respawned shells each bring their own reader, so this
    /// counts rather than flips a flag; 0 means no reader is parked anywhere on
    /// this session's lane.
    readers_live: Arc<AtomicUsize>,
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

    /// `Esc` / Ctrl-C: interrupt the command in front of the shell.
    ///
    /// Four cases, and only the last two send a byte:
    ///
    /// * **nothing outstanding, nothing queued** — an idle prompt. `0x03` would
    ///   print a `^C` for no reason, so an idle Esc is a silent no-op, not an
    ///   error (looprs-5g7).
    /// * **queued, not started** — the command is still in this session's own
    ///   queue, because [`Self::submit`] never writes to a shell that has not
    ///   printed its prompt. Nothing has reached the child, so there is nothing to
    ///   signal and the cancel *is* the dequeue. It is said out loud
    ///   ([`cancel::dropped_before_start`]) because the alternative was the worst
    ///   answer a cancel key can give: the keystroke lost **and** the command left
    ///   queued to start a moment later. `status()` calls that window `Running`,
    ///   so from the user's side there is a command to cancel — the branch that
    ///   refuses to see it is the bug, not the status.
    /// * **already cancelling and the attempt is still live** — the byte is already
    ///   in the line discipline. Stacking a second one on it buys nothing and
    ///   makes "which interrupt is the pending one?" unanswerable, so the repeat
    ///   keystroke is dropped rather than queued behind it.
    /// * **cancelling, but the last attempt was reported stalled** — that report
    ///   is the user being told the command ignored us; hitting Esc again means
    ///   "try anyway", so the ladder restarts with a fresh attempt and a fresh
    ///   deadline.
    fn interrupt(&mut self) {
        if self.outstanding.is_empty() {
            let queued: Vec<String> = std::mem::take(&mut self.queue).into();
            if queued.is_empty() {
                return;
            }
            // The whole queue rather than the newest entry: "stop" means the things
            // I have not seen start shall not start. Picking among them would be
            // guessing at the meaning of a key whose whole job is "not that".
            let what = if queued.len() == 1 {
                format!("`{}`", clip(&queued[0], 60))
            } else {
                format!("{} queued commands", queued.len())
            };
            self.note(cancel::dropped_before_start(&what, queued.len() > 1));
            return;
        }
        if self.aborting && !self.stall_reported {
            return;
        }
        let what = clip(&self.foreground(), 60);
        let note = if self.aborting {
            format!("cancelling `{what}` again…")
        } else {
            cancel::started(&format!("`{what}`"))
        };
        self.send_sigint(note);
    }

    /// One `0x03`, one word about it, one deadline armed.
    ///
    /// The word goes out *before* anything can come back, because the thing this
    /// row of the contract buys is the end of the silence: the shell may take
    /// seconds to unwind a `find`, and the user should not have to wonder whether
    /// their keystroke did anything at all.
    fn send_sigint(&mut self, note: String) {
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        if let Err(e) = shell.write_line("\x03") {
            self.err(format!("interrupt did not reach the shell: {e:#}"));
            return;
        }
        self.aborting = true;
        self.stall_reported = false;
        self.interrupt_attempt += 1;
        let attempt = self.interrupt_attempt;
        self.note(note);
        cancel::arm(self.cmd.clone(), BashCmd::InterruptStalled { attempt });
    }

    /// The `0x03` sent `attempt` ago produced no exit marker within
    /// [`cancel::GRACE`].
    ///
    /// A command can be uncancellable by us without anything being wrong with the
    /// shell: `trap '' INT`, a program that reset its own handler, a process stuck
    /// in an uninterruptible syscall. What the session must **not** do is kill the
    /// shell to make the prompt come back — cwd, exports, aliases and background
    /// jobs are the entire reason Bash mode has a pty (ADR-0001), and they are
    /// exactly what a `kill` here would throw away in order to fix somebody
    /// else's command.
    ///
    /// So the escalation is one sentence, and the choice stays with the user:
    /// `Esc` sends another interrupt, `Ctrl-Q` quits and takes the shell along.
    fn interrupt_stalled(&mut self, attempt: u32) {
        // Not ours if this attempt is no longer the live one: the marker may have
        // landed first, or the user may have Esc'd again and a newer attempt now
        // owns the deadline.
        if !self.aborting || self.stall_reported || self.interrupt_attempt != attempt {
            return;
        }
        self.stall_reported = true;
        let what = clip(&self.foreground(), 60);
        self.err(format!(
            "{} — it may be trapping the interrupt. Esc sends another one; Ctrl-Q quits if you meant it.",
            cancel::stalled(&what)
        ));
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
    fn stream_end(&mut self) {
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
    fn shutdown(&mut self, rx: &mut mpsc::Receiver<ByteLane>) {
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

/// One turn of the pty byte lane.
///
/// Returns `false` once the stream has ended, so the task can stop polling that
/// arm.
///
/// The credit bookkeeping is the whole reason this is a function and not inline:
/// a read that produced **no** output message — a chunk that was all exit marker,
/// or one that landed entirely inside a partial UTF-8 sequence — will never have
/// its token handed back downstream, because there is no message for the pump to
/// hand it back with. Without this release the reader would spend a token and
/// never get it back, and after `OUTPUT_BUDGET_TOKENS` such chunks the shell
/// would freeze mid-sentence. That is the failure mode of credit-based flow
/// control, so it is named where it is handled.
/// One pty read buffer through the marker/screen pipeline, with its credit.
fn handle_chunk(task: &mut BashTask, bytes: Vec<u8>) {
    let before = task.output_emitted;
    task.on_bytes(bytes);
    if task.output_emitted == before {
        task.cfg.output_budget.release();
    }
}

/// Take everything the reader has already handed over, **without** acting on
/// the end of the stream: the callers that drain for ordering reasons report the
/// exit themselves, and the session's exit must be said exactly once.
fn drain_lane_now(task: &mut BashTask, rx: &mut mpsc::Receiver<ByteLane>) {
    while let Ok(lane) = rx.try_recv() {
        match lane {
            ByteLane::Chunk(b) => handle_chunk(task, b),
            ByteLane::Eof => break,
        }
    }
}

/// Poll for the shell's death **with the byte lane moving the whole time**.
///
/// `true` means it is gone (or that this handle has nothing left to wait on);
/// `false` means it was still there when `wait` ran out.
///
/// The drain is not a courtesy to the transcript. Every buffer the reader
/// thread is holding is a buffer the dying child is still trying to write, and
/// the only thing in this process that frees it is this task taking it. So a
/// `try_wait` loop that does *not* drain is a loop waiting for a death its own
/// refusal has prevented — which is the deadlock looprs-2ck was called for.
/// Draining first on every turn is what turns it into a wait that ends.
fn reap_while_draining(
    task: &mut BashTask,
    rx: &mut mpsc::Receiver<ByteLane>,
    shell: &mut Shell,
    wait: Duration,
) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        drain_lane_now(task, rx);
        match shell.try_wait() {
            // Reaped, or too far gone to ask about again. Either way there is
            // nothing left here worth waiting for.
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(REAP_POLL);
    }
}

/// Hand a child that will not be reaped to a thread whose only job is waiting
/// for it (looprs-2ck). The only place in this file allowed to call
/// [`Child::wait`].
///
/// Three steps, in this order, because each one is what makes the next safe:
///
/// 1. **take the child out of the shell**, so `Drop for Shell` cannot kill what
///    the reaper has just been promised.
/// 2. **drop the shell**, closing this end of the pty, the writer, and the
///    generated rc file. Note what that is *not*: the reader thread holds its
///    own duplicated read end, so the tty is not destroyed by this step.
/// 3. **start the reaper**, detached and joined by nobody.
///
/// Nothing in here waits for the child before returning. That is the point:
/// once this returns the session task finishes and drops the byte lane's
/// receiver, the reader thread's parked `blocking_send` fails, its read end
/// goes with it — and *that* is the moment the tty is finally released and a
/// child stuck in `exit(2)` can finish. The reaper is where that landing gets
/// logged.
fn reap_off_task(id: SessionId, mut shell: Shell, readers_live: &Arc<AtomicUsize>) {
    let Some(mut child) = shell.child.take() else {
        return;
    };
    let pid = child.process_id();
    drop(shell);
    let readers = readers_live.load(Ordering::SeqCst);
    // Cloned *before* the move: from here the `Child` itself belongs to the
    // reaper thread, and the only thing left to do without it is kill.
    let mut killer = child.clone_killer();

    let spawned = std::thread::Builder::new()
        .name("looprs-bash-reaper".into())
        .spawn(move || {
            let started = Instant::now();
            // Polled first, so a shell that merely has not gotten here yet is
            // reaped without a warning rather than a verdict.
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        tracing::info!(
                            "{id}: reaped the shell it was handed after {:?} (code {})",
                            started.elapsed(),
                            status.exit_code()
                        );
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("{id}: giving up on the shell it was handed: {e}");
                        return;
                    }
                }
                if started.elapsed() >= REAPER_REPORT {
                    break;
                }
                std::thread::sleep(REAP_POLL);
            }
            // Still here, `SIGKILL` already sent, and past the point of
            // pretending this is quick. Say what is being waited on, and where,
            // before taking the blocking reap: a `sample` of a process that
            // will not quit should point at a thread named for waiting, not at a
            // session task or a pty reader that were both made to wait for it.
            tracing::warn!(
                "{id}: shell {pid:?} still unreaped {:?} after SIGKILL ({readers} pty reader \
                 thread(s) still live); waiting for it here, on the detached reaper thread, so that \
                 no task and no reader has to",
                REAPER_REPORT
            );
            let _ = child.kill();
            match child.wait() {
                Ok(status) => tracing::warn!(
                    "{id}: reaped shell {pid:?} after {:?} (code {})",
                    started.elapsed(),
                    status.exit_code()
                ),
                Err(e) => tracing::warn!("{id}: shell {pid:?} never reported back: {e}"),
            }
        });

    if spawned.is_err() {
        // No thread and therefore no reap. Kill it so it cannot run on, and say
        // so plainly: the residue is one unreaped pid, which is the smaller
        // failure next to a task that waited for it.
        tracing::warn!(
            "{id}: could not start the reaper thread; killed the shell and left it unreaped"
        );
        let _ = killer.kill();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TerminalType;

    /// A failure bound, not a wait. Every wait in this module blocks on an event —
    /// the seam ([`BashSession::quiesce`]), the command's own exit marker, the
    /// byte tape — and this number exists only to turn "the event never came" into
    /// a named failure instead of a hung test. Nothing here sleeps and hopes.
    const NO_HANG: Duration = Duration::from_secs(20);

    /// Same shape as [`NO_HANG`]: an upper bound that fails the test, never a wait
    /// the test reasons about.
    ///
    /// Twelve times the one-second bar `cancel::GRACE`'s own doc gives a
    /// responsive child, and well under the `sleep 30` these tests cut short, so
    /// "interrupted" and "ran to its own end" stay two different answers even
    /// here. The reason this is twelve seconds and not one is the reason this
    /// module stopped claiming sub-second things: a wall-clock assertion under a
    /// second measures the runner's scheduler, not the product, and a loaded
    /// eight-core box loses a `0x03`'s round trip to nothing but contention
    /// (looprs-00u.17 — that assertion is where four of these tests flaked for a
    /// year). The promptness claim that *is* load-independent lives in the event
    /// order: `esc_says_cancelling_before_the_command_reports_itself_done`.
    const INTERRUPTED_WITHIN: Duration = Duration::from_secs(12);

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

    /// As [`bash`], with the alternate screen **not** hosted — the inline-pane
    /// passthrough, where a child's `?1049` pair is the child's own business and
    /// its bytes go to the terminal untouched.
    ///
    /// Not the app's configuration since looprs-pdl.4 took the screen for the
    /// frame (ADR-0004 R1), but still the shape the passthrough has to be right
    /// about, and the only way these tests can tell the two behaviours apart.
    fn bash_not_hosting(generation: u64) -> (BashSession, mpsc::UnboundedReceiver<SessionEvent>) {
        let cfg = SessionConfig {
            shell_bin: real_shell(),
            alt_screen_hosted: false,
            ..Default::default()
        };
        let (mut s, rx) = BashSession::build(SessionId::new(TerminalType::Bash, generation), &cfg)
            .expect("build");
        s.resize(24, 80).ok();
        (s, rx)
    }

    fn describe(ev: &SessionEvent) -> String {
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
            out.push(describe(&ev));
        }
        out
    }

    async fn next_event(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> String {
        let ev = tokio::time::timeout(NO_HANG, rx.recv())
            .await
            .expect("the bash session went silent")
            .expect("the bash session stream closed");
        describe(&ev)
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
        let mut ran = Ran {
            out: String::new(),
            code: None,
            notes: Vec::new(),
        };
        loop {
            let line = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => describe(&ev),
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
    async fn run_command(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> (String, Option<i32>) {
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

    /// Read the stream until an event matches, and hand back everything read, in
    /// order — including the one that matched.
    ///
    /// This is what a test that wants "the acknowledgement, then the exit line, in
    /// that order" should be doing instead of collecting whatever shows up in the
    /// next 800 ms and hoping the sample contained both (looprs-00u.17). A window
    /// like that is a *sleep with assertions bolted on afterwards*: it passes when
    /// the machine is fast and fails when it is busy, and the failure says nothing
    /// about the code. Blocking on the event says which one it was.
    ///
    /// The only time in here is [`NO_HANG`], a failure bound.
    async fn until_event<F>(
        rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
        pred: F,
        what: &str,
    ) -> Vec<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = tokio::time::Instant::now() + NO_HANG;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let line = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => describe(&ev),
                Ok(None) => panic!("the stream closed before {what}: {seen:?}"),
                Err(_) => panic!(
                    "{what} never arrived within {NO_HANG:?} (failure bound, not a wait); \
                     stream up to here: {seen:?}"
                ),
            };
            let hit = pred(&line);
            seen.push(line);
            if hit {
                return seen;
            }
        }
    }

    /// The run's output bytes glued together in arrival order, remembering which
    /// event each byte arrived in.
    ///
    /// **Why a tape and not a list of events.** A pty read hands back whatever had
    /// happened to arrive when it returned, and the session is not the only writer:
    /// the line discipline echoes typed input a character at a time (a captured
    /// run of the test below shows the echoed command arriving as `out p`, `out ri`,
    /// `out n`, `out t`, `out f`…), the shell's prompt brings escapes of its own
    /// (`\u{1b}[?1034h`), and a program's `printf` lands as one event or as four
    /// depending on nothing but scheduling. So an assertion of the form *"which
    /// event contains `ESC[?25lpainted`?"* is a question about read coalescing,
    /// and it has a different answer on a loaded runner. That is not a tolerance
    /// problem and cannot be fixed by widening a timeout: the needle was simply not
    /// whole.
    ///
    /// The tape asks what the tests actually mean — *did those bytes go by, and
    /// between which transitions?* — and answers it the same whatever the
    /// boundaries were. The transitions (`screen true` / `screen false`, the exit
    /// marker) are events, so ordering against them stays exact.
    #[derive(Default, Debug)]
    struct Tape {
        text: String,
        /// One entry per byte in `text`: the index of the event that carried it.
        event_of: Vec<usize>,
    }

    impl Tape {
        /// The output tape of a captured event list, as `describe` renders it.
        fn of(events: &[String]) -> Self {
            let mut tape = Tape::default();
            for (idx, line) in events.iter().enumerate() {
                if let Some(chunk) = line.strip_prefix("out ") {
                    tape.push(idx, chunk);
                }
            }
            tape
        }

        fn push(&mut self, event: usize, text: &str) {
            self.text.push_str(text);
            self.event_of.resize(self.text.len(), event);
        }

        /// The event that carried the **first** byte of `needle`, or `None` if
        /// those bytes never went by.
        fn event_carrying(&self, needle: &str) -> Option<usize> {
            self.text.find(needle).map(|at| self.event_of[at])
        }

        /// Everything from the first output byte of event `from` onwards. Used to
        /// scope a search to "after this transition went by", which is the only
        /// way to keep a claim about the wire from being answered by the echo of
        /// the command line that set it up.
        fn after_event(&self, from: usize) -> &str {
            let start = self
                .event_of
                .iter()
                .position(|e| *e >= from)
                .unwrap_or(self.text.len());
            &self.text[start..]
        }
    }

    /// Send a command and know it reached the child, without polling for it.
    ///
    /// The seam is the event: `Sync` is handled by the same task that handled the
    /// `Submit`, ahead of it in the mailbox, and it publishes the status mirror
    /// before acking. So when this returns, the bytes are in the pty master and
    /// the session holds the command as outstanding — "in flight" as a fact about
    /// the queue, not as a guess taken from a 20 ms sampling loop (which is what
    /// `wait_running` used to be: four seconds of polling whose only failure mode
    /// was running out of samples, and whose only success condition was the
    /// mirror having flipped at some point in that window).
    ///
    /// Read the doc on [`warm_shell`] first: on a **cold** shell a submit cannot
    /// be written at all, so it sits in the session's queue and reads as
    /// `Running` there too. This helper asserts the command was written, which is
    /// only true of a shell that had already printed its prompt.
    async fn in_flight(s: &mut BashSession, cmd: &str) {
        s.send_text(cmd.into()).unwrap();
        assert!(
            s.quiesce().await,
            "the submit of `{cmd}` was never handled by the session task"
        );
        assert_eq!(
            s.status(),
            SessionStatus::Running,
            "`{cmd}` was written to the pty and has not reported its exit"
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

    fn clean_lines(out: &str) -> Vec<String> {
        let bytes = strip_ansi_escapes::strip(out);
        String::from_utf8_lossy(&bytes)
            .split('\n')
            .map(|l| {
                l.trim_end_matches('\r')
                    .rsplit('\r')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
            .collect()
    }
    /// **Acceptance: cwd persists.** The whole reason for a persistent shell: a
    /// one-shot `bash -c` cannot remember a `cd`.
    #[tokio::test]
    async fn cwd_persists_across_commands() {
        let (mut s, mut rx) = bash(3);
        let target = "/tmp";
        s.send_text(format!("cd {target}")).unwrap();
        run_command(&mut rx).await;
        s.send_text("pwd".into()).unwrap();
        let (out, code) = run_command(&mut rx).await;
        assert_eq!(code, Some(0));
        let lines = clean_lines(&out);
        assert!(
            lines.iter().any(|l| l == "/tmp" || l == "/private/tmp"),
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
        assert!(one < two && two < three, "streams were reordered: {out:?}");
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
    ///
    /// Every wait in here is an event: the session's own seam for "the keystroke
    /// has been acted on", the command's own exit line for "the interrupt
    /// landed". The only clocks are [`NO_HANG`] and [`INTERRUPTED_WITHIN`], and
    /// both are failure bounds.
    #[tokio::test]
    async fn esc_interrupts_a_running_command_without_killing_the_shell() {
        let (mut s, mut rx) = bash(7);
        warm_shell(&mut s, &mut rx).await;
        in_flight(&mut s, "sleep 30").await;

        let at_esc = std::time::Instant::now();
        s.abort().unwrap();
        // The keystroke is acted on inside the session's own mailbox, and the
        // seam *is* that handling: waiting for it is waiting for the event, not
        // for a clock. `Aborting` is what the state was when it completed.
        assert!(s.quiesce().await, "the Esc was handled");
        assert_eq!(s.status(), SessionStatus::Aborting, "the cancel is live");

        // The interrupt's own exit line is the proof that the sleep was cut short
        // rather than still running: block on the line instead of on a second of
        // wall clock, so a loaded runner costs this test nothing and a lost
        // interrupt cannot hide inside a tolerance.
        let ran = run_logged(&mut rx).await;
        assert!(
            ran.code == Some(130) || ran.code == Some(143) || ran.code == Some(1),
            "sleep should have been interrupted, got {:?} (out {:?})",
            ran.code,
            ran.out
        );
        assert!(
            at_esc.elapsed() < INTERRUPTED_WITHIN,
            "an interrupt exit arrived {:?} after the keystroke, which is `sleep 30` winding \
             down eventually rather than being interrupted by us (failure bound, not a wait)",
            at_esc.elapsed()
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

    /// The invisible half of the contract: an idle Esc must not even announce
    /// itself. "cancelling…" with nothing cancelled is a claim about work in
    /// flight that is not true, and the status row would carry that claim around
    /// for the rest of the session.
    ///
    /// Shell *output* is not counted as noise — the prompt keeps arriving on its
    /// own and has nothing to do with the keystroke. What must not appear is a
    /// word from the session about a command that was never running, including a
    /// stall report, which is why the window runs past `cancel::GRACE`.
    #[tokio::test]
    async fn esc_at_an_idle_prompt_says_nothing_at_all() {
        let (mut s, mut rx) = bash(21);
        s.send_text("echo first".to_string()).unwrap();
        run_command(&mut rx).await;
        assert_eq!(s.status(), SessionStatus::Idle);
        drain(&mut rx);

        s.abort().unwrap();
        assert!(s.quiesce().await, "the Esc was handled");
        let late =
            crate::testing::collect_within(&mut rx, cancel::GRACE + Duration::from_secs(1), |ev| {
                describe(ev)
            })
            .await;
        let noise: Vec<&String> = late.iter().filter(|l| !l.starts_with("out ")).collect();
        assert!(noise.is_empty(), "an idle Esc made noise: {late:?}");
        assert_eq!(s.status(), SessionStatus::Idle, "and changed nothing");
    }

    /// **The cold-start window: `Esc` while the command has not reached the shell.**
    ///
    /// `submit` never writes to a shell that has not printed its prompt, so a
    /// command typed during start-up sits in the session's own `queue` — and
    /// `status()` reports that as `Running`, because something *is* pending. The
    /// interrupt used to bail out on "nothing outstanding" in exactly that window,
    /// which lost the keystroke **and** left the command queued to start as soon
    /// as the prompt arrived. A cancel that cancels nothing, silently, while the
    /// thing it was aimed at runs a moment later.
    ///
    /// `/bin/cat` is the fixture that holds that state open instead of racing for
    /// it: it never prints the readiness marker, so the command never leaves the
    /// queue and the window cannot close underneath the test. That is the same
    /// window the interrupt tests used to fall into by accident whenever CI was
    /// slow enough to widen it — here it is the subject rather than the accident.
    #[tokio::test]
    async fn an_esc_aimed_at_a_queued_command_takes_it_out_of_the_queue() {
        let cfg = SessionConfig {
            // Not a shell, and on purpose: nothing `cat` prints is the readiness
            // marker, so `ready` stays false and every submit stays queued.
            shell_bin: "/bin/cat".into(),
            ..Default::default()
        };
        let (mut s, mut rx) =
            BashSession::build(SessionId::new(TerminalType::Bash, 26), &cfg).expect("build");
        s.resize(24, 80).ok();

        s.send_text("sleep 30".into()).unwrap();
        // Through the seam rather than by polling: `status` is a mirror the session
        // task writes — at the seam and on the byte lane — and with `cat` as the
        // child no bytes ever arrive to push it. The seam is the one thing that
        // both waits for the `Submit` to have been handled and publishes what the
        // state was when it was.
        assert!(s.quiesce().await, "the submit was handled");
        assert_eq!(
            s.status(),
            SessionStatus::Running,
            "a queued command reads as work pending: the status is *right* here, and it is the \
             keystroke that has to catch up with it"
        );
        drain(&mut rx);

        s.abort().unwrap();
        assert!(s.quiesce().await, "the Esc was handled");

        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| m.contains("cancelled")
                && m.contains("sleep 30")
                && m.contains("before it started")),
            "the queued cancel said nothing: {msgs:?}"
        );
        assert_eq!(
            s.status(),
            SessionStatus::Idle,
            "the queue is empty and nothing was ever in flight, so nothing is left busy"
        );
        assert!(
            msgs.iter().all(|m| !m.contains("cancelling")),
            "a queued cancel must not claim a byte was sent at something: {msgs:?}"
        );

        // And it stays cancelled: no late line reporting the command as having run,
        // which is exactly what the swallowed version produced.
        let late =
            crate::testing::collect_within(&mut rx, Duration::from_millis(500), describe).await;
        assert!(
            late.iter()
                .all(|l| !l.contains("exit ") && !l.contains("cancelling")),
            "the dropped command came back to life: {late:?}"
        );
    }

    /// **Acceptance: the word comes before the child does.** A `sleep 30` stops
    /// fast but not instantly, and the user must not spend the gap wondering
    /// whether Esc reached anything. So the contract is an *ordering*: the
    /// acknowledgement precedes the exit line.
    ///
    /// **Read the ordering to its end; do not sample it.** This test used to
    /// collect whatever arrived in the next 800 ms and assert the acknowledgement
    /// was in the sample — a sleep wearing an assertion's clothes. On a loaded
    /// runner the sample came up empty and the failure read "the keystroke was not
    /// acknowledged" about a session that had acknowledged it late, which is a
    /// verdict about the machine (looprs-00u.17). Blocking on the *far* end of
    /// the ordering — the interrupted command's exit line — makes both halves
    /// exact: the ack is there or the test says so, and nothing about how long
    /// anything took changes the answer.
    ///
    /// Why the order is structural rather than probable: one task owns the pty and
    /// emits both ends. `send_sigint` writes the `0x03` and then puts the word
    /// out, and the reply can only be read on a later turn of that task's loop —
    /// so a session that cancels at all says so first. That is what makes this a
    /// test of the ordering rather than a race against it.
    #[tokio::test]
    async fn esc_says_cancelling_before_the_command_reports_itself_done() {
        let (mut s, mut rx) = bash(22);
        warm_shell(&mut s, &mut rx).await;
        in_flight(&mut s, "sleep 30").await;
        drain(&mut rx);

        s.abort().unwrap();
        // The far end of the ordering. Everything between the keystroke and this
        // line is the evidence, in the order the session put it on the wire.
        let got = until_event(
            &mut rx,
            |l| exit_code_of(l).is_some(),
            "the interrupted command's exit line",
        )
        .await;
        let ack = got
            .iter()
            .position(|l| l.starts_with("system: cancelling") && l.contains("sleep 30"))
            .unwrap_or_else(|| panic!("the keystroke was not acknowledged, by name: {got:?}"));
        let done = got
            .iter()
            .rposition(|l| exit_code_of(l).is_some())
            .expect("until_event stopped on the exit line");
        // "Cancelled" arriving before "cancelling" is the silence this row exists
        // to remove, arriving late instead of never.
        assert!(
            ack < done,
            "the command reported itself done before the cancel was acknowledged: {got:?}"
        );
        // And the exit that followed is the same story as the acknowledgement, not
        // an unrelated line that happened to be shaped like an exit: a cancel
        // acknowledged and then reported as a plain `exit 130` would mean the
        // session had already forgotten it was cancelling.
        assert!(
            got[ack..=done]
                .iter()
                .any(|l| l.contains("interrupted (exit ")),
            "the exit that followed the acknowledgement did not say it was an interrupt: {got:?}"
        );
    }

    /// **The escalation: a command that will not be interrupted.**
    ///
    /// `trap '' INT` hands the ignore disposition to the child, so the tty's
    /// SIGINT arrives, is thrown away, and no exit marker ever comes back. From
    /// here that is indistinguishable from a hang, which is exactly why the stall
    /// has to be a sentence rather than a spinner — and why the session must not
    /// "solve" it by killing the shell the command was running inside of.
    ///
    /// **The child says when it is ready to be uninterruptible.** A trap is not
    /// installed when `send_text` returns: a `0x03` that overtakes the builtin
    /// kills the sleep like any other, no stall ever happens, and every assertion
    /// below is then aimed at a run that never had the property under test. So the
    /// builtin is followed by an `echo` of the test's own marker and the test
    /// waits for that byte — readiness declared by the child through the pty, not
    /// assumed from a delay (looprs-00u.17: this is the ticket's "make the
    /// observable deterministic when the observable really is time").
    ///
    /// **"Not before the grace" is a lower bound between two events, not a 500 ms
    /// sample.** The stall deadline is armed *after* the keystroke is handled, so
    /// a correct implementation cannot report early however loaded the runner is:
    /// measure the keystroke-to-report gap and compare it with
    /// [`cancel::GRACE`], and the only way to fail the check is to actually cry
    /// wolf. A sample window cannot make that claim at all — it only ever shows
    /// "not yet", which is what an early report and a merely slow one have in
    /// common.
    #[tokio::test]
    async fn a_command_that_traps_the_interrupt_is_reported_not_silently_wedged() {
        let (mut s, mut rx) = bash(23);
        warm_shell(&mut s, &mut rx).await;

        s.send_text("trap '' INT; echo looprs-trap-set".into())
            .unwrap();
        let armed = run_logged(&mut rx).await;
        assert_eq!(armed.code, Some(0), "the trap builtin failed: {armed:?}");
        assert!(
            armed.out.contains("looprs-trap-set"),
            "the trap never reported itself installed, so the interrupt below would be aimed \
             at a shell that may not be trapping anything: {armed:?}"
        );

        in_flight(&mut s, "sleep 30").await;
        drain(&mut rx);

        let at_esc = std::time::Instant::now();
        s.abort().unwrap();
        assert!(s.quiesce().await, "the Esc was handled");
        assert_eq!(s.status(), SessionStatus::Aborting);

        // Block on the escalation itself; where it landed in time is read off the
        // same two events afterwards.
        let late = until_event(
            &mut rx,
            |l| l.starts_with("error:") && l.contains("still running"),
            "the stalled-interrupt report",
        )
        .await;
        assert!(
            at_esc.elapsed() >= cancel::GRACE,
            "the escalation cried wolf: it landed {:?} after the keystroke, inside the {:?} a \
             responsive child is still allowed to take: {late:?}",
            at_esc.elapsed(),
            cancel::GRACE
        );
        assert!(
            s.status().is_alive(),
            "the escalation must not kill the shell: cwd, env and jobs are the reason this mode exists"
        );

        // And the mode is not wedged: a second Esc is a *retry*, not a keystroke
        // swallowed by the first one's pending state.
        drain(&mut rx);
        s.abort().unwrap();
        assert!(s.quiesce().await, "the second Esc was handled");
        let again = until_event(
            &mut rx,
            |l| l.contains("cancelling") && l.contains("again"),
            "the retry acknowledgement",
        )
        .await;
        assert!(
            again.last().is_some_and(|l| l.contains("sleep 30")),
            "the retry did not name the command it is cancelling again: {again:?}"
        );
    }

    /// Bring the shell up to its first prompt *before* the test's real command.
    ///
    /// **This is what makes "the command is running" mean running.** A cold
    /// `submit` cannot be written at all — the shell has not printed its prompt —
    /// so the command sits in the session's queue, `status()` reports `Running`
    /// because something is pending, and an `Esc` aimed at "the running command"
    /// is in fact aimed at a queue entry that has not started. Nothing on the
    /// outside can tell those two apart: both are `Running`, and that conflation
    /// is what made these tests CI-flaky, with the window widening with load.
    /// Running one cheap command to completion first leaves the shell ready, so
    /// the next `send_text` goes straight to the child and [`in_flight`]'s claim
    /// — *written to the pty, not parked behind readiness* — is true of it.
    ///
    /// This replaced a poll of `status()` every 20 ms (`wait_running`), whose
    /// success condition was "the mirror flipped somewhere in the next eight
    /// seconds" and whose failure condition was "we ran out of samples": a
    /// tolerance with assertions bolted on either side of it.
    async fn warm_shell(s: &mut BashSession, rx: &mut mpsc::UnboundedReceiver<SessionEvent>) {
        s.send_text("echo looprs-warm".into()).unwrap();
        let (out, code) = run_command(rx).await;
        assert_eq!(code, Some(0), "the shell did not come up warm: {out:?}");
        assert_eq!(s.status(), SessionStatus::Idle, "warm means idle");
        drain(rx);
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
    ///
    /// The command is put in flight through the seam ([`in_flight`]) rather than by
    /// polling `status()` until it reads `Running`, and the notice is waited for as
    /// an event: the two things this test needs to be sure of are *which* message
    /// arrived and *what it named*, neither of which a sample window can answer
    /// (looprs-00u.17).
    #[tokio::test]
    async fn a_command_that_never_reported_is_named_when_the_shell_dies() {
        let (mut s, mut rx) = bash(16);
        warm_shell(&mut s, &mut rx).await;
        in_flight(
            &mut s,
            "(sleep 1; kill -KILL $$) >/dev/null 2>&1 & sleep 30",
        )
        .await;

        // `down` before the notice is the failure this used to guard against by
        // hand, so it stays part of what we wait for: stop on either, then say
        // which one it was.
        let seen = until_event(
            &mut rx,
            |l| l.contains("shell exited") || l.starts_with("down "),
            "the shell's death notice",
        )
        .await;
        let notice = seen.last().expect("until_event stopped on something");
        assert!(
            notice.contains("shell exited"),
            "the session reported itself gone without a notice: {seen:?}"
        );
        assert!(
            notice.contains("while running `"),
            "the unreported command was never named: {notice}"
        );
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

    /// How long the fixed shutdown is allowed to take: `EXIT_ASK + KILL_REAP`
    /// is 3 s of design, and this is that with the room a loaded runner needs to
    /// spend it — a **failure bound**, never a wait the test reasons about.
    ///
    /// It is also the shape of the bug: the pre-looprs-2ck code did not exceed
    /// this bound, it never came back at all, which is why the stress command
    /// used to carry this test's name in a `--skip`.
    const SHUTDOWN_RETURNED_WITHIN: Duration = Duration::from_secs(12);

    /// Poll until this session has no live reader thread, up to `bound`.
    ///
    /// A poll on an atomic rather than a sleep-and-hope, because the thing being
    /// watched is the existence of a thread this task cannot see. `readers_live`
    /// is incremented before the thread is spawned and decremented when its
    /// body finishes by any exit, so 0 is a fact about the thread and not a
    /// guess from a quiet channel.
    async fn readers_gone(s: &BashSession, bound: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + bound;
        loop {
            let live = s.readers_live();
            if live == 0 || tokio::time::Instant::now() >= deadline {
                return live;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// **Acceptance: shutdown returns while the byte lane is full and the reader
    /// is parked on it** (looprs-2ck).
    ///
    /// `yes` is the worst case the exit path can be handed, for three reasons
    /// at once, and the bug needed all three: it never reads the `exit` we send
    /// (a foreground job owns the shell), so the polite phase always runs out;
    /// it outruns the lane immediately, so the reader thread is parked in
    /// `blocking_send` instead of in `read(2)`; and it leaves the pty's own
    /// kernel buffer full of bytes nobody has taken, which is what a dying
    /// child blocks on inside `exit(2)`.
    ///
    /// The old code put the session task into `wait4` there and the three of
    /// them held each other down: task waiting on child, reader waiting on lane,
    /// child waiting on the tty the reader had stopped emptying. What is
    /// asserted is the ticket's two properties rather than the mechanism that
    /// now provides them — **the session reports itself gone** (the `down` line
    /// is emitted only after `BashTask::shutdown` returns) and **the reader
    /// thread ends** (`readers_live` reaches 0, which needs the parked send to
    /// fail against a dropped receiver).
    #[tokio::test]
    async fn shutdown_returns_while_the_lane_is_full_and_the_reader_is_parked() {
        let (mut s, mut rx) = bash(32);
        warm_shell(&mut s, &mut rx).await;
        in_flight(&mut s, "yes").await;
        // Let the producer get far enough ahead that the reader has something
        // in hand and nowhere to put it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            s.readers_live(),
            1,
            "the shell we are about to shut down should have exactly one reader thread"
        );

        let started = Instant::now();
        s.shutdown().unwrap();

        let seen = until_event(&mut rx, |l| l.starts_with("down "), "the shutdown notice").await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < SHUTDOWN_RETURNED_WITHIN,
            "shutdown took {elapsed:?} to report the session gone, past the {SHUTDOWN_RETURNED_WITHIN:?} \
             the whole reap is budgeted for: {seen:?}"
        );
        assert_eq!(
            s.status(),
            SessionStatus::Dead,
            "the session said it was down without making itself dead"
        );

        // The second property, and the one that used to be unverifiable: a
        // reader parked on a lane that has stopped draining is the failure, so
        // watch the count rather than the bytes.
        let readers = readers_gone(&s, Duration::from_secs(5)).await;
        assert_eq!(
            readers, 0,
            "a reader thread is still parked on this session's byte lane after {elapsed:?}"
        );
    }

    /// **Acceptance: a shell that will not take the `exit` gets killed, and the
    /// kill is bounded too** (looprs-2ck).
    ///
    /// The quiet half of the case above: `sleep 30` fills nothing, so the only
    /// thing that can go wrong here is the wait itself. The assertion that this
    /// really travelled the kill path rather than exiting politely is timing in
    /// the *only* direction load cannot bend: it took **at least** `EXIT_ASK`,
    /// because the ask deadline is armed before the shell is even asked and
    /// nothing can make it elapse sooner.
    #[tokio::test]
    async fn a_shell_that_ignores_the_exit_is_killed_within_the_bound() {
        let (mut s, mut rx) = bash(33);
        warm_shell(&mut s, &mut rx).await;
        in_flight(&mut s, "sleep 30").await;

        let started = Instant::now();
        s.shutdown().unwrap();
        let seen = until_event(&mut rx, |l| l.starts_with("down "), "the shutdown notice").await;
        let elapsed = started.elapsed();

        assert!(
            elapsed >= EXIT_ASK,
            "this shutdown finished in {elapsed:?}, before the {EXIT_ASK:?} ask was even over, so it \
             never reached the kill it was meant to test: {seen:?}"
        );
        assert!(
            elapsed < EXIT_ASK + KILL_REAP * 4,
            "the kill-and-reap phase outstayed its budget ({elapsed:?} total, {KILL_REAP:?} budgeted): \
             {seen:?}"
        );
        assert_eq!(
            readers_gone(&s, Duration::from_secs(5)).await,
            0,
            "the reader thread outlived the shell it was reading"
        );
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
                Ok(Some(ev)) => describe(&ev),
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
    ///
    /// Since the frame owns the alternate screen (ADR-0004 R22), what is handed
    /// over is *our* screen and a blank canvas, not a screen switch: the child's
    /// `?1049h` and `?1049l` are cut out of the stream and never reach the
    /// terminal. The takeover and the release are still reported to the UI
    /// exactly as if the switch had happened, because the UI's question — "is
    /// something else painting right now?" — is answered the same either way.
    ///
    /// **Every byte check below runs on the tape ([`Tape`]), and that is the whole
    /// of what made this test a flake.** It used to ask which *event* contained
    /// `\u{1b}[?25lpainted`. A pty read returns whatever had arrived when it
    /// returned — the same stream delivers this very command's echo one or two
    /// characters at a time — so there is no promise that an escape and the bytes
    /// after it land in the same event, and on a loaded runner they did not. The
    /// needle was not whole, and a needle that does not exist cannot be found by
    /// widening a tolerance. The transitions *are* events, so ordering against
    /// them stays exact while the byte search stops depending on where the reads
    /// happened to fall.
    ///
    /// The "not on the wire" check moves for the same reason and keeps its scope:
    /// the echo of the command line is full of the literal characters `?1049`, so
    /// only output that went by **after** the takeover can say what reached the
    /// terminal.
    #[tokio::test]
    async fn a_full_screen_program_is_handed_the_screen_and_gives_it_back() {
        let (mut s, mut rx) = bash(21);
        s.send_text("printf '\\033[?1049h\\033[?25lpainted\\033[?1049l'".into())
            .unwrap();
        let events = until_exit(&mut rx).await;
        let tape = Tape::of(&events);

        let took = events
            .iter()
            .position(|e| e == "screen true")
            .unwrap_or_else(|| panic!("the takeover was never reported: {events:?}"));
        // The *program's* paint, and not the two other things in this stream that
        // say "painted": the command's echo (the literal characters `\033[`, no
        // escape byte anywhere) and the shell's own prompt (a real escape of its
        // own, `\u{1b}[?1034h`, which used to satisfy "contains `painted` and
        // contains an escape" whenever the pty batched the prompt with the echo).
        // The needle is an escape exactly where the program put one.
        let painted = tape
            .event_carrying("\u{1b}[?25lpainted")
            .unwrap_or_else(|| panic!("the program's paint never went by: {events:?}"));
        let released = events
            .iter()
            .position(|e| e == "screen false")
            .unwrap_or_else(|| panic!("the release was never reported: {events:?}"));
        assert!(
            took < painted,
            "the UI must be teeing before the bytes that change the screen: {events:?}"
        );
        assert!(
            painted < released,
            "the release must come after the paint it follows: {events:?}"
        );
        // The switch itself never reached the wire: neither the enter nor the
        // leave, anywhere in the output that went by after the takeover.
        let after = tape.after_event(took);
        assert!(
            !after.contains("?1049"),
            "the child's alt-screen bytes reached the terminal; the frame owns \
             that screen now: {events:?}"
        );
        // …and in place of the enter, the canvas the child expected: a blank
        // screen, not our previous frame showing through wherever it did not paint.
        assert!(
            after.contains("\u{1b}[H\u{1b}[2J"),
            "no blank canvas was handed over: {events:?}"
        );
        assert_no_marker_bytes(&events[took..].join("|"));
        assert_eq!(s.status(), SessionStatus::Idle, "and the shell is fine");
    }

    /// The same program on the **non-hosting** path: nothing is cut, the pair
    /// goes through as written, and the leave lands before the release is
    /// reported — which is the shape that path had before the frame took the
    /// screen, kept tested so a change to the cutter cannot silently widen.
    ///
    /// Read on the tape ([`Tape`]) for the same reason as the hosted test, and
    /// with a stronger reason: the check this replaces wanted **one event** to
    /// carry the program's `\u{1b}[?1049h` *and* the `painted` that follows it
    /// three escapes later, which is a demand about read coalescing that this path
    /// never made any promise about.
    #[tokio::test]
    async fn a_childs_alt_screen_pair_passes_through_when_we_do_not_host_the_screen() {
        let (mut s, mut rx) = bash_not_hosting(21);
        s.send_text("printf '\\033[?1049h\\033[?25lpainted\\033[?1049l'".into())
            .unwrap();
        let events = until_exit(&mut rx).await;
        let tape = Tape::of(&events);

        let took = events
            .iter()
            .position(|e| e == "screen true")
            .unwrap_or_else(|| panic!("the takeover was never reported: {events:?}"));
        let released = events
            .iter()
            .position(|e| e == "screen false")
            .unwrap_or_else(|| panic!("the release was never reported: {events:?}"));
        let after = tape.after_event(took);
        assert!(
            after.contains("\u{1b}[?1049h"),
            "the enter was cut on the path that must cut nothing: {events:?}"
        );
        let painted = tape
            .event_carrying("\u{1b}[?25lpainted")
            .unwrap_or_else(|| panic!("the program's paint never went by: {events:?}"));
        // The order that matters here: the leave bytes reach the terminal *before*
        // the release is reported, so a terminal watching the wire is back on the
        // main screen by the time the UI is told it may draw again.
        let left = tape
            .event_carrying("\u{1b}[?1049l")
            .unwrap_or_else(|| panic!("the leave bytes never reached the terminal: {events:?}"));
        assert!(took < painted, "{events:?}");
        assert!(
            left < released,
            "the leave bytes must reach the terminal before the release: {events:?}"
        );
        assert_no_marker_bytes(&events.join("|"));
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// Everything the session says from now until its stream closes (the session
    /// task ending is what closes it, so this is "what it said on the way out").
    async fn until_closed(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + NO_HANG;
        let mut events = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => events.push(describe(&ev)),
                Ok(None) => return events,
                Err(_) => panic!("the session never closed its stream: {events:?}"),
            }
        }
    }

    /// **Acceptance: quitting while a full-screen program holds the screen pays the
    /// alternate screen back.**
    ///
    /// `printf` takes the alt screen and never returns it — the shape of vim
    /// killed, `less` closed by a signal, a pager that died mid-paint. The command
    /// boundary pays that debt for a command that finishes; this is the other way
    /// out, the quit that arrives *while* the program still holds it. Nobody else
    /// can pay it: this session is the only thing that watched the bytes go by,
    /// and the moment `shutdown` returns the terminal belongs to whatever started
    /// us. Left unpaid, the user is inside a dead program's screen with their prompt
    /// gone, and the only way out is `reset` typed blind.
    ///
    /// Driven on the **non-hosting** path, because that is the only shape where
    /// the child's switch is real. With the frame hosting the alternate screen
    /// this debt cannot exist at all — see
    /// [`quitting_while_a_hosted_child_holds_the_screen_leaves_no_debt_to_pay`].
    #[tokio::test]
    async fn quitting_while_a_full_screen_program_holds_the_screen_leaves_the_alt_screen() {
        let (mut s, mut rx) = bash_not_hosting(24);
        s.send_text("printf '\\033[?1049hpainted and stuck'; sleep 30".into())
            .unwrap();

        // Wait for the takeover, then quit mid-hold. The `sleep 30` keeps a
        // command in front of the shell so nothing can end it politely for us: the
        // only thing that can pay the screen back here is the quit path itself.
        let deadline = tokio::time::Instant::now() + NO_HANG;
        loop {
            let line = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("the session went silent before taking the screen")
                .expect("stream closed");
            if describe(&line) == "screen true" {
                break;
            }
        }

        s.shutdown().unwrap();
        let events = until_closed(&mut rx).await;
        let joined = events.join("|");
        assert!(
            joined.contains("\u{1b}[?1049l"),
            "the alt screen the program died holding must be paid back on the quit path: {events:?}"
        );
        assert!(
            events.contains(&"screen false".to_string()),
            "and the UI must be told the screen is free on the way out: {events:?}"
        );
    }

    /// The hosted shape of the same quit. The child asks for the alternate screen
    /// while the **frame** owns it, so its `?1049h` is cut (ADR-0004 R22) and
    /// nothing is switched — which means there is no debt for the quit path to
    /// pay, and the session must write **no** leave of its own. A second
    /// `?1049l` after the ledger's one would be aimed at the user's real main
    /// screen.
    ///
    /// What the quit still owes is the *report*: `screen false`, so nothing
    /// downstream keeps waiting for a screen that was never switched away. Proved
    /// end to end by `spikes/fullscreen_e2e.py` ("a SIGKILLed child owes no
    /// leave"); this is the same promise at unit speed.
    #[tokio::test]
    async fn quitting_while_a_hosted_child_holds_the_screen_leaves_no_debt_to_pay() {
        let (mut s, mut rx) = bash(25);
        let mut seen: Vec<String> = Vec::new();
        s.send_text("printf '\\033[?1049hpainted and stuck'; sleep 30".into())
            .unwrap();

        let deadline = tokio::time::Instant::now() + NO_HANG;
        loop {
            let line = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("silent before taking the screen, got: {seen:?}"))
                .expect("stream closed");
            let d = describe(&line);
            seen.push(d.clone());
            if d == "screen true" {
                break;
            }
        }

        s.shutdown().unwrap();
        let mut events = until_closed(&mut rx).await;
        events.splice(0..0, seen);
        // Measured from the takeover on: everything before it is the echo of the
        // command line, whose literal `?1049` characters are text, not wire.
        let took = events
            .iter()
            .position(|e| e == "screen true")
            .expect("no takeover in the merged stream");
        assert!(
            !events[took..].join("|").contains("?1049"),
            "the session charged or repaid a screen it had already cut: {events:?}",
        );
        assert!(
            events.contains(&"screen false".to_string()),
            "the UI must still be told the screen is free on the way out: {events:?}",
        );
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
        s.send_text("printf 'first-line\\n'; sleep 0.3; printf '\\033[2Aover-the-top'".into())
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
    ///
    /// **The witness is the child's own report of the bytes, not the line
    /// discipline's echo.** The old form read the echo, and the echo turned out not
    /// to be a witness at all: whether it fires depends on whether readline had
    /// the tty in raw mode at the instant of the write, and whether `ESC :` is
    /// echoed as `^[:` or swallowed by readline as an escape prefix. Three runs of
    /// the old form through the same pty: two showed `^[:wq!` before the command
    /// finished, one showed nothing until `bash: wq!: command not found` — which is
    /// not `:wq!`, so that run failed the assertion it was supposed to be
    /// guaranteeing (looprs-00u.17). `head -c 6 | od -An -tx1` answers the
    /// question by value instead: exactly these bytes arrived, in this order, and
    /// nothing else arrived with them.
    ///
    /// The command is put in flight through the seam ([`in_flight`]) rather than by
    /// polling `status()` until it reads `Running`, which made "is it safe to type
    /// at it yet?" a question with a timer for an answer.
    #[tokio::test]
    async fn raw_keys_reach_the_child_verbatim_and_the_shell_still_survives() {
        let (mut s, mut rx) = bash(24);
        warm_shell(&mut s, &mut rx).await;
        // `GOT[` … `]` brackets the report so it can be picked out of the stream
        // without guessing at which other bytes are hex-looking; `stty -echo` on
        // the way in and back out again so the run neither reads the echo as the
        // report nor leaves the tty muted for whoever inherits it.
        in_flight(
            &mut s,
            "printf 'first-line\\n'; stty -echo; printf 'GOT['; head -c 6 | od -An -tx1 | tr -d ' \\n'; stty echo; printf ']\\n'",
        )
        .await;

        // Type a keystroke sequence at the running command: Esc, ':', 'w', 'q',
        // '!' and CR — the shape of `:wq!`.
        s.send_bytes(vec![0x1b, b':', b'w', b'q', b'!', 0x0d])
            .unwrap();
        let ran = run_logged(&mut rx).await;
        assert!(ran.out.contains("first-line"), "{:?}", ran.out);
        let reported = between(&ran.out, "GOT[", "]").unwrap_or_else(|| {
            panic!(
                "the child never reported the bytes it was handed, so they never reached \
                 the pty: {:?}",
                ran.out
            )
        });
        // The ESC came through as `0x1b` — not translated to `0x03`, not eaten —
        // and the five typed bytes are the five reported bytes, in order.
        assert!(
            reported.starts_with("1b3a777121"),
            "the child got {reported:?}, not the ESC : w q ! we typed"
        );
        // Six bytes and not seven: the "nothing added" half. The sixth is the line
        // terminator we typed — CR, which the line discipline's own ICRNL turns
        // into NL on the way in. A `\\n` appended by the session would make seven.
        assert_eq!(
            reported.len(),
            12,
            "typed six bytes, the child reported {reported:?}: a byte added or lost"
        );
        assert!(
            reported.ends_with("0a") || reported.ends_with("0d"),
            "the terminator is not the CR we typed (possibly ICRNL-translated): {reported:?}"
        );
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// The stretch of `hay` between the **last** `open` and the `close` after it.
    ///
    /// "Last" because the command that printed the marker also put the marker's
    /// characters into the stream once before that — as the echo of the command
    /// line — and it is the report we want, not the recital of the recipe.
    fn between<'a>(hay: &'a str, open: &str, close: &str) -> Option<&'a str> {
        let start = hay.rfind(open)? + open.len();
        let end = hay[start..].find(close)?;
        Some(&hay[start..start + end])
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
        assert!(
            ran.notes.iter().any(|n| n.contains("exit 0")),
            "{:?}",
            ran.notes
        );
    }
}
