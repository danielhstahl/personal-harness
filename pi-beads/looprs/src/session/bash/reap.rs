//! Getting the shell back once it is gone: the reaper, and the text that goes with it.
//!
//! The rule this file exists for is one sentence: **never reap alone**
//! (looprs-2ck). [`reap_while_draining`] takes the byte lane with the reap and
//! drains it, because the task that owns the pty is also the only drainer of the
//! reader thread's bounded lane — a shell dying with output in flight
//! deadlocks three parties at once without that. [`reap_off_task`] is the same
//! rule when the session task itself is already gone: it moves the wait off the
//! task and reports on its own clock.
//!
//! The rest is wording: [`exit_text`] is the sentence the transcript shows for
//! the way the shell ended, [`clip`] bounds a quoted command so a megabyte of
//! argv cannot bury the notice, and [`split_complete_utf8`] is the reason a
//! multi-byte codepoint cannot be cut in half by a read boundary.

use crate::session::bash::pty::{ByteLane, REAP_POLL, REAPER_REPORT, Shell};
use crate::session::bash::task::BashTask;

use std::sync::Arc;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::session::{ExitReason, SessionId};

pub(super) fn exit_text(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Crashed { code: Some(c) } => format!("code {c}"),
        ExitReason::Crashed { code: None } => "signal".to_string(),
        ExitReason::Shutdown => "shutdown".to_string(),
        ExitReason::Cancelled => "cancelled".to_string(),
        ExitReason::Unknown => "unknown reason".to_string(),
    }
}

/// Trim to `max` chars for a single-line notice, with an ellipsis when it bit.
pub(super) fn clip(s: &str, max: usize) -> String {
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
pub(super) fn split_complete_utf8(buf: Vec<u8>) -> (String, Vec<u8>) {
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
pub(super) fn handle_chunk(task: &mut BashTask, bytes: Vec<u8>) {
    let before = task.output_emitted;
    task.on_bytes(bytes);
    if task.output_emitted == before {
        task.cfg.output_budget.release();
    }
}

/// Take everything the reader has already handed over, **without** acting on
/// the end of the stream: the callers that drain for ordering reasons report the
/// exit themselves, and the session's exit must be said exactly once.
pub(super) fn drain_lane_now(task: &mut BashTask, rx: &mut mpsc::Receiver<ByteLane>) {
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
pub(super) fn reap_while_draining(
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
pub(super) fn reap_off_task(id: SessionId, mut shell: Shell, readers_live: &Arc<AtomicUsize>) {
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
