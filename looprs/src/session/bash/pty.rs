//! The pty itself: spawn it, hand it the integration, pump its bytes.
//!
//! [`Shell`] is the pair `portable_pty` gives back — a child plus the master fd —
//! with the four things this app ever asks of it: write a line, write raw bytes,
//! resize, and die politely. Everything above it in this module is what makes
//! the child believe it is on a terminal: the generated rcfile
//! ([`shell_integration`]) that prints the `DC1 looprs:exit:<code> DC2`
//! marker after every command, and the file that carries it to the child
//! ([`write_integration`]).
//!
//! The reader is a thread, not a task, because `portable-pty`'s read is blocking
//! and ADR-0001's whole consequence is that the tokio runtime must not be the
//! thing stuck in it. [`read_master`] is that thread's body; [`ByteLane`] is the
//! bounded channel it pushes into, and [`LiveReaders`] the counter that lets the
//! reaper in [`reap`](super::reap) wait for the lane to drain rather than sleep
//! and hope.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::session::SessionConfig;

/// `DC1` — starts the exit marker. A control character no program emits by
/// accident, which is what makes an in-band marker safe here.
pub(super) const MARKER_START: u8 = 0x11;
/// `DC2` — ends it.
pub(super) const MARKER_END: u8 = 0x12;
/// The marker's payload prefix, between the two control characters.
pub(super) const MARKER_PREFIX: &str = "looprs:exit:";
/// Cap on a buffered, unterminated marker. Past this the bytes are certainly not a
/// marker, and holding them would be a slow leak, so they are emitted as output.
pub(super) const MARKER_BUFFER_MAX: usize = 4096;

/// How many pty read buffers may be sitting between the reader thread and the
/// session task (looprs-6cj).
///
/// A *bound*, not a tuning knob: with the reader charging the UI's output budget
/// one token per read, this is the other half of the ceiling on how far ahead of
/// the screen a shell is ever allowed to run. 8 × 8 KiB = 64 KiB.
pub(crate) const BYTE_LANE_DEPTH: usize = 8;

/// How long the shell gets to leave on its own after being asked, before the
/// kill goes in.
pub(super) const EXIT_ASK: Duration = Duration::from_secs(2);

/// How long to keep pumping the byte lane after the kill, waiting for the reap.
///
/// A `SIGKILL`ed shell is dead already; the only thing that can keep the *wait*
/// waiting is the tty buffer it still has to flush, and that drains at the
/// reader thread's pace. One second of pumping covers a full pty at any speed
/// this pipe can carry it. Past that the shell is not coming back on this
/// task's clock, and the remaining wait moves off the task (see
/// [`reap_off_task`]).
pub(super) const KILL_REAP: Duration = Duration::from_secs(1);

/// The gap between reap polls.
///
/// Short enough to notice a death inside a frame, long enough not to burn a
/// core while a shell drains a full pty buffer.
pub(super) const REAP_POLL: Duration = Duration::from_millis(5);

/// How long the reaper thread polls before it stops trying to be quiet about a
/// child that has not been reaped.
///
/// After this it says out loud what it is waiting for and takes the blocking
/// `wait` — which is only acceptable because the thread taking it is detached
/// and nothing in the process is waiting on *it*.
pub(super) const REAPER_REPORT: Duration = Duration::from_millis(500);

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
pub(super) struct Shell {
    /// The child, until the shutdown handoff takes it (looprs-2ck).
    ///
    /// An `Option` for exactly one reason: the shutdown path has to be able to
    /// leave this struct behind holding everything *except* the thing it cannot
    /// wait for. Once the reaper owns the child this is `None`, which is also
    /// what stops [`Drop for Shell`] from killing a child somebody else has
    /// promised to wait on. A `Shell` with `child == None` is not a broken
    /// shell, it is a shell whose reap is elsewhere.
    pub(super) child: Option<Box<dyn Child + Send + Sync>>,
    pub(super) writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty>,
    integration: PathBuf,
}

impl Shell {
    pub(super) fn spawn(
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

    pub(super) fn write_line(&mut self, text: &str) -> Result<()> {
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
    pub(super) fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
            .map_err(|e| anyhow!("the shell's pty rejected input: {e}"))
    }

    pub(super) fn resize(&mut self, size: PtySize) {
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
    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    /// `SIGKILL` the child, if this handle still owns one.
    pub(super) fn kill(&mut self) {
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
pub(super) enum ByteLane {
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
