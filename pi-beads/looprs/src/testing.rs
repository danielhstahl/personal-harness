//! Test-only fake `pi` / `bd` executables.
//!
//! These are *real* subprocesses driven by real pipes, which is the point: the tests
//! then assert process-level truth — was a child spawned at all, was it prompted, was
//! the previous one reaped — instead of trusting a mock that never had a pid to begin with.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `bd ready --json` with nothing to work on.
pub const EMPTY_BOARD: &str = r#"{"data":[],"schema_version":1}"#;

/// `bd ready --json` with a single ready bead.
pub const ONE_BEADED_BOARD: &str = r#"{
  "data": [
    {"id": "looprs-26r", "title": "Beads loop never self-starts", "status": "open", "issue_type": "bug"}
  ],
  "schema_version": 1
}"#;

/// A board with one bead in every place the kanban mapping knows about, at once:
/// `open` / `blocked` / a status this build has never seen in To-do, one in
/// progress, two complete, and one deferred that is a footer count rather than a
/// row. Seven in, six rows plus one deferred — ADR-0007's invariant I1 in a
/// fixture, so "nothing dropped and nothing counted twice" is checkable against
/// a board that exercises every arm instead of one that happens to be tidy.
pub const BOARD_VARIETY: &str = r#"{
  "data": [
    {"id": "looprs-open-1", "title": "plain open work", "status": "open", "issue_type": "task"},
    {"id": "looprs-blocked-1", "title": "waiting on a human", "status": "blocked", "issue_type": "bug"},
    {"id": "looprs-weird-1", "title": "a status from a newer bd", "status": "frobnicated", "issue_type": "task"},
    {"id": "looprs-prog-1", "title": "a worker has this", "status": "in_progress", "issue_type": "task"},
    {"id": "looprs-done-1", "title": "closed by a worker", "status": "closed", "issue_type": "task"},
    {"id": "looprs-done-2", "title": "closed a long time ago", "status": "done", "issue_type": "feature"},
    {"id": "looprs-deferred-1", "title": "taken out of the running", "status": "deferred", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

/// How the fake `pi` behaves when it is started / prompted.
#[derive(Clone, Copy, Debug)]
pub enum PiFake {
    /// Answers a prompt with `success:true, disposition:"started"` and stays alive.
    Started,
    /// Answers a prompt with `disposition:"handled"`: pi took it, started no run.
    Handled,
    /// Answers a prompt with `success:false`.
    Rejects,
    /// Exits immediately, so the pipes close before anything is answered.
    DiesImmediately,
    /// A stateful chat child: streams a turn, remembers the conversation it was
    /// given, and holds each run open until [`Fakes::settle`] (see
    /// [`Fakes::settle`]). This is the one the Pi terminal state tests drive.
    Chat,
}

/// How the fake `bd` behaves.
///
/// The read verbs (`ready`, `list`, `show`) answer from files the test can rewrite
/// mid-run — that is what makes "the board changed under the loop" and "the worker
/// settled but never closed the bead" testable at all.
#[derive(Clone, Copy, Debug)]
pub enum BdFake {
    /// Prints the current board JSON and exits 0.
    Ok,
    /// Prints nothing and exits 3.
    Fails,
    /// Exits 0 with bytes that are not JSON at all.
    Malformed,
    /// Exits 0 printing nothing (the sneaky one: "empty output" must not read as
    /// "empty board").
    EmptyOutput,
    /// Like [`BdFake::Ok`], plus `bd show <id> --json` answers from `show.json`
    /// (set with [`Fakes::set_show`]) — the read the claim guard is built on.
    ShowStatus,
    /// Answers like [`BdFake::Ok`], but **slowly**: holds the read open for
    /// [`SLOW_FAKE_READ`] before it answers.
    ///
    /// The personality that makes *timing* properties testable rather than
    /// assumed. A `bd` that answers instantly makes "two reads never overlap",
    /// "a tick missed while the last read was in flight is skipped, not queued"
    /// and "dropping the poller mid-read kills the child" all unobservable —
    /// there is no window in which they could be false. The board poller
    /// (looprs-5o4.2) is entirely about that window.
    Slow,
}

/// How long [`BdFake::Slow`] holds a read open.
///
/// Long enough to span several of the shortest intervals the poller tests use
/// (60 ms), short enough that the suite does not notice: the whole slow-fake
/// group costs about a second of wall clock.
pub const SLOW_FAKE_READ: std::time::Duration = std::time::Duration::from_millis(350);

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// A notifier that remembers instead of posting.
///
/// The third sink `services::notification` names (production `Ntfy`, default
/// `Noop`, and this). It exists so the *completion edge itself* is assertable: that
/// a closed ticket announced exactly one `BeadDone` carrying its id and title, and —
/// the half that matters more — that an aborted pass, an un-closed ticket, an
/// unreadable board and a planner's settle announced **nothing**. Those four all
/// look like completion to anything watching `agent_settled`, which is exactly why
/// the producer sits where it does.
///
/// It is `Clone` over a shared `Arc`, so the test and the loop under test hold the
/// same recording.
#[derive(Clone, Debug, Default)]
pub struct RecordingNotifier {
    done: Arc<Mutex<Vec<crate::services::notification::BeadDone>>>,
}

impl crate::services::notification::Notifier for RecordingNotifier {
    fn notify(&self, done: crate::services::notification::BeadDone) {
        self.done.lock().unwrap().push(done);
    }
}

/// A clipboard that remembers instead of writing (looprs-pdl.10).
///
/// The third sink shape this crate uses (`QueuedClipboard` in production, `Noop`
/// by default, this in tests) and the reason for its existence is the same as
/// [`RecordingNotifier`]'s: the *exact bytes* and the *exact count* have to be
/// assertable. A clipboard that really wrote would make the assertion "the
/// clipboard is non-empty", which is true whether or not the app copied what the
/// user selected, and is untrue in CI for reasons that have nothing to do with
/// the code under test.
///
/// Answers its receipt **on the spot**, so a test of the copy path never waits on
/// a transport, and a test of the late-failure path is a test of
/// `App::poll_copy`'s deadline rather than a two-second sleep.
///
/// Clone over a shared `Arc`, so the test and the App under test hold the same
/// recording.
#[derive(Clone, Debug)]
pub struct RecordingClipboard {
    copies: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// What to answer with. `None` answers `Verified` with the count of the text
    /// that was actually handed over, which is the "nothing went wrong" default:
    /// a test that does not care about the failure ladder still gets a toast
    /// whose number describes what was copied.
    outcome: Option<crate::services::clipboard::CopyOutcome>,
}

impl Default for RecordingClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingClipboard {
    /// Records, and answers `Verified`.
    pub fn new() -> Self {
        Self {
            copies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            outcome: None,
        }
    }

    /// Records, and answers whatever the test says — the way to drive the
    /// `Not copied` and `Copy failed` toast branches.
    pub fn answering(outcome: crate::services::clipboard::CopyOutcome) -> Self {
        Self {
            copies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            outcome: Some(outcome),
        }
    }

    /// Every string this sink was handed, in order.
    pub fn copies(&self) -> Vec<String> {
        self.copies.lock().unwrap().clone()
    }

    pub fn last(&self) -> Option<String> {
        self.copies.lock().unwrap().last().cloned()
    }

    pub fn count(&self) -> usize {
        self.copies.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.copies.lock().unwrap().is_empty()
    }
}

impl crate::services::clipboard::Clipboard for RecordingClipboard {
    fn copy(&self, text: String) -> crate::services::clipboard::Receipt {
        let chars = crate::services::clipboard::Chars::of(&text);
        let outcome = self
            .outcome
            .clone()
            .unwrap_or(crate::services::clipboard::CopyOutcome::Verified { chars });
        self.copies.lock().unwrap().push(text);
        crate::services::clipboard::Receipt::ready(outcome)
    }

    fn describe(&self) -> &'static str {
        "recording"
    }
}

/// A clipboard that takes the copy and never answers for it.
///
/// The sink the *late failure* path is tested against: a native helper parked on
/// a wedged Wayland compositor, or an OSC 52 write into an SSH connection that
/// has stopped pumping, both look exactly like this from the App's side — an
/// accepted copy with no reply. Without a sink like it in the suite, the
/// `App::poll_copy` deadline is code no test has ever run.
///
/// [`StallClipboard::answer_all`] delivers the reply the test wants *when the
/// test asks*, which is how "the copy came back after the deadline" is a
/// scenario rather than a paragraph.
#[derive(Clone, Debug, Default)]
pub struct StallClipboard {
    copies: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pending: std::sync::Arc<
        std::sync::Mutex<
            Vec<tokio::sync::oneshot::Sender<crate::services::clipboard::CopyOutcome>>,
        >,
    >,
}

impl StallClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // seam kept for the stalled-copy tests: they assert the toast, not the sink's copy log
    pub fn copies(&self) -> Vec<String> {
        self.copies.lock().unwrap().clone()
    }

    /// How many copies are currently unanswered.
    pub fn inflight(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Deliver `outcome` for every copy taken so far.
    pub fn answer_all(&self, outcome: crate::services::clipboard::CopyOutcome) {
        let senders: Vec<_> = self.pending.lock().unwrap().drain(..).collect();
        for s in senders {
            let _ = s.send(outcome.clone());
        }
    }
}

impl crate::services::clipboard::Clipboard for StallClipboard {
    fn copy(&self, text: String) -> crate::services::clipboard::Receipt {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.copies.lock().unwrap().push(text);
        self.pending.lock().unwrap().push(tx);
        crate::services::clipboard::Receipt::stalled(rx)
    }

    fn describe(&self) -> &'static str {
        "stalling"
    }
}

/// A transcript dump sink that records what it was asked to write and answers on
/// the spot.
///
/// The `RecordingClipboard` of the filesystem: a test asserting "the whole
/// transcript reached the dump sink" does it against this rather than against a
/// real file, which keeps the suite off the disk for the same reason
/// `SessionConfig::default()` carries `Noop` — by construction, not by whoever
/// last remembered to unset an environment variable.
#[derive(Clone, Debug, Default)]
pub struct RecordingTranscriptSink {
    dumps: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, String)>>>,
    outcome: Option<crate::services::transcript_file::DumpOutcome>,
}

impl RecordingTranscriptSink {
    /// Records, and answers `Written` into a fake path.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records, and answers whatever the test says — the way to drive the failed
    /// and refused dump branches.
    pub fn answering(outcome: crate::services::transcript_file::DumpOutcome) -> Self {
        Self {
            dumps: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            outcome: Some(outcome),
        }
    }

    /// Every `(mode, text)` this sink was handed, in order.
    pub fn dumps(&self) -> Vec<(&'static str, String)> {
        self.dumps.lock().unwrap().clone()
    }

    pub fn last(&self) -> Option<String> {
        self.dumps.lock().unwrap().last().map(|(_, t)| t.clone())
    }

    pub fn count(&self) -> usize {
        self.dumps.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.dumps.lock().unwrap().is_empty()
    }
}

impl crate::services::transcript_file::TranscriptSink for RecordingTranscriptSink {
    fn dump(
        &self,
        mode: &'static str,
        text: String,
    ) -> crate::services::transcript_file::DumpReceipt {
        use crate::services::transcript_file::{DumpOutcome, DumpReceipt};
        let chars = crate::services::clipboard::Chars::of(&text);
        let outcome = self
            .outcome
            .clone()
            .unwrap_or_else(|| DumpOutcome::Written {
                path: std::path::PathBuf::from(format!("/tmp/fake-looprs-{mode}.txt")),
                chars,
            });
        self.dumps.lock().unwrap().push((mode, text));
        DumpReceipt::ready(outcome)
    }

    fn describe(&self) -> &'static str {
        "recording"
    }
}

/// A dump sink that takes the text and never answers.
///
/// The `StallClipboard` of the filesystem: the volume that stopped answering,
/// tested rather than described. Without a fake like it the `App::poll_dump`
/// deadline is code no test has ever run.
#[derive(Clone, Debug, Default)]
pub struct StallTranscriptSink {
    dumps: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, String)>>>,
    pending: std::sync::Arc<
        std::sync::Mutex<
            Vec<tokio::sync::oneshot::Sender<crate::services::transcript_file::DumpOutcome>>,
        >,
    >,
}

impl StallTranscriptSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dumps(&self) -> Vec<(&'static str, String)> {
        self.dumps.lock().unwrap().clone()
    }

    /// How many dumps are currently unanswered.
    pub fn inflight(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Deliver `outcome` for every dump taken so far.
    pub fn answer_all(&self, outcome: crate::services::transcript_file::DumpOutcome) {
        let senders: Vec<_> = self.pending.lock().unwrap().drain(..).collect();
        for s in senders {
            let _ = s.send(outcome.clone());
        }
    }
}

impl crate::services::transcript_file::TranscriptSink for StallTranscriptSink {
    fn dump(
        &self,
        mode: &'static str,
        text: String,
    ) -> crate::services::transcript_file::DumpReceipt {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.dumps.lock().unwrap().push((mode, text));
        self.pending.lock().unwrap().push(tx);
        crate::services::transcript_file::DumpReceipt::stalled(rx)
    }

    fn describe(&self) -> &'static str {
        "stalling"
    }
}

impl RecordingNotifier {
    /// Every completion announced so far, in order.
    pub fn completions(&self) -> Vec<crate::services::notification::BeadDone> {
        self.done.lock().unwrap().clone()
    }

    /// Just the ids, for the assertion that only cares *which* tickets spoke.
    pub fn completed_ids(&self) -> Vec<String> {
        self.completions().into_iter().map(|d| d.id).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.done.lock().unwrap().is_empty()
    }
}

/// A scratch dir holding the fake binaries plus their recording logs.
/// Removing the dir on drop keeps test runs from leaving debris.
pub struct Fakes {
    dir: PathBuf,
    pi_bin: PathBuf,
    bd_bin: PathBuf,
    pi_log: PathBuf,
    bd_log: PathBuf,
    board_file: PathBuf,
    show_file: PathBuf,
    /// The sink the loop under test was handed, so a test can read completions off
    /// the same `Fakes` it reads `bd`'s log from.
    pub notifier: RecordingNotifier,
}

impl Fakes {
    pub fn new(tag: &str, pi: PiFake, bd: BdFake, board: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("looprs-fakes-{}-{}-{}", tag, std::process::id(), n));
        // A previous crashed run may have left the dir behind; start clean.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let pi_log = dir.join("pi.log");
        let bd_log = dir.join("bd.log");
        let board_file = dir.join("board.json");
        let show_file = dir.join("show.json");
        let pi_bin = dir.join("pi");
        let bd_bin = dir.join("bd");

        let script = pi_script(&pi_log, pi);
        write_script(&pi_bin, &script);
        write_script(
            &bd_bin,
            &bd_script(
                &bd_log,
                &board_file,
                &show_file,
                &dir.join("fail"),
                &dir.join("refuse_claim"),
                bd,
            ),
        );
        std::fs::write(&board_file, board).unwrap();
        // No bead is known to `bd show` until a test says otherwise: an unset
        // show file reads as "no such bead", which is the safe default.
        std::fs::write(&show_file, "[]").unwrap();

        Self {
            dir,
            pi_bin,
            bd_bin,
            pi_log,
            bd_log,
            board_file,
            show_file,
            notifier: RecordingNotifier::default(),
        }
    }

    /// Point the loop's fakes at a different board without restarting them.
    pub fn set_board(&self, board: &str) {
        std::fs::write(&self.board_file, board).unwrap();
    }

    /// Make every *subsequent* fake `bd` call fail (exit 3), without restarting it.
    ///
    /// The lever for "the board was readable when the pass started and is not now".
    /// That distinction is not expressible with a `BdFake` personality, which is
    /// fixed at construction, and it is exactly what the planner verification
    /// (looprs-k7v) has to survive: a read that fails halfway through a pass must
    /// read as "unverifiable", never as "the board is empty".
    pub fn fail_bd(&self, on: bool) {
        let mark = self.dir.join("fail");
        if on {
            std::fs::write(&mark, b"").unwrap();
        } else {
            let _ = std::fs::remove_file(&mark);
        }
    }

    /// Make every *subsequent* fake `bd` **claim** fail (exit 4), leaving reads and
    /// other writes alone.
    ///
    /// The lever for "`bd` would not give us that ticket", which [`Fakes::fail_bd`]
    /// cannot express: failing *everything* proves only that a broken `bd` stops
    /// the loop, not that a refused *claim* does — and the claim is now the call
    /// the harness makes before it spends anything (looprs-w7q). Exit 4 rather
    /// than 3 keeps "refused this claim" from reading as "bd is down".
    pub fn refuse_claim(&self, on: bool) {
        let mark = self.dir.join("refuse_claim");
        if on {
            std::fs::write(&mark, b"").unwrap();
        } else {
            let _ = std::fs::remove_file(&mark);
        }
    }

    /// What `bd show <id> --json` answers: a single bead object, or `[]`.
    ///
    /// This is the fake's lever for "the worker settled but never closed the bead",
    /// which is the whole looprs-w7q hazard and is not expressible through the
    /// `ready` board alone.
    pub fn set_show(&self, bead: &str) {
        std::fs::write(&self.show_file, bead).unwrap();
    }

    pub fn pi_bin(&self) -> &str {
        self.pi_bin.to_str().unwrap()
    }

    pub fn bd_bin(&self) -> &str {
        self.bd_bin.to_str().unwrap()
    }

    fn read(&self, path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Every fake `pi` process that was spawned, in order.
    pub fn pi_pids(&self) -> Vec<u32> {
        self.read(&self.pi_log)
            .lines()
            .filter_map(|l| l.strip_prefix("spawn pid="))
            .filter_map(|l| l.split(' ').next()?.parse().ok())
            .collect()
    }

    pub fn pi_spawns(&self) -> usize {
        self.pi_pids().len()
    }

    /// Every prompt command line a fake `pi` actually received.
    pub fn pi_prompts(&self) -> Vec<String> {
        self.read(&self.pi_log)
            .lines()
            .filter_map(|l| l.strip_prefix("prompt ").map(str::to_string))
            .collect()
    }

    /// Every command the fake `pi` received, in the order it received it, as
    /// `"<verb> <rest>"` lines. Order is the whole point of several tests: Esc has
    /// to `clear_queue` *before* it `abort`s, a follow-up has to arrive as
    /// `steer` and not as a second `prompt`.
    pub fn pi_commands(&self) -> Vec<String> {
        self.read(&self.pi_log)
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("recv ")?;
                Some(rest.to_string())
            })
            .collect()
    }

    /// Just the verbs, in order: `["prompt", "steer", "clear_queue", "abort"]`.
    pub fn pi_verbs(&self) -> Vec<String> {
        self.pi_commands()
            .iter()
            .map(|c| c.split(' ').next().unwrap_or("").to_string())
            .collect()
    }

    /// Touch the trigger that lets the chat fake finish the run it is holding open.
    ///
    /// Without this there is no way for a test to do anything *during* a run, and
    /// "during a run" is where steer, Esc and crash recovery all live.
    pub fn settle(&self) {
        std::fs::write(self.dir.join("settle"), b"").unwrap();
    }

    /// Wait until the fake `pi` has written `needle` at the start of a log line.
    /// Polls, because a test that sleeps is a test that is wrong half the time.
    pub async fn wait_for_log_line(&self, needle: &str) {
        for _ in 0..400 {
            if self
                .read(&self.pi_log)
                .lines()
                .any(|l| l.starts_with(needle) || l.contains(&format!("recv {needle}")))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "fake pi never logged {needle:?}; log was:\n{}",
            self.read(&self.pi_log)
        );
    }

    /// Wait until the fake `pi` has been spawned at least `n` times.
    ///
    /// Same polling rationale as [`Fakes::wait_for_log_line`]: a test that sleeps a
    /// fixed amount is a test that fails on a slow machine and passes for the wrong
    /// reason on a fast one.
    pub async fn wait_for_pi_spawns(&self, n: usize) {
        for _ in 0..800 {
            if self.pi_spawns() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "only {} fake pi spawn(s) after the wait, wanted {n}; log was:\n{}",
            self.pi_spawns(),
            self.read(&self.pi_log)
        );
    }

    pub fn bd_calls(&self) -> usize {
        self.read(&self.bd_log)
            .lines()
            .filter(|l| l.starts_with("bd "))
            .count()
    }

    /// Every `bd` command line the fake received, in order.
    pub fn bd_log(&self) -> Vec<String> {
        self.read(&self.bd_log)
            .lines()
            .filter_map(|l| l.strip_prefix("bd ").map(str::to_string))
            .collect()
    }

    /// Did the harness claim this bead, with the command it was supposed to use?
    pub fn claimed(&self, id: &str) -> bool {
        self.bd_log()
            .iter()
            .any(|l| l.contains(&format!("update {id} --claim")))
    }

    /// How many times this exact `bd` command line was run (for "the board is
    /// queried once per decision, not once per question" style assertions).
    pub fn bd_call_count(&self, cmd: &str) -> usize {
        self.bd_log().iter().filter(|l| *l == cmd).count()
    }

    // The pid markers `tests/fixtures/fake_bd.sh` writes on the way in and out
    // of every invocation. `bd_log()` answers "what was this `bd` asked to do";
    // these answer the question a log of invocations cannot: **was one alive
    // just now** — which is the only form in which "two reads never overlap"
    // and "no child outlived its poller" are checkable facts (looprs-5o4.2).

    /// Every fake-`bd` pid that started, in start order.
    pub fn bd_pids(&self) -> Vec<u32> {
        self.read(&self.bd_log)
            .lines()
            .filter_map(|l| l.strip_prefix("start "))
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// The most recent fake-`bd` pid — the read that is probably still in
    /// flight, if `bd_starts() > bd_stops()`.
    pub fn last_bd_pid(&self) -> Option<u32> {
        self.bd_pids().last().copied()
    }

    fn bd_marker_lines(&self) -> Vec<String> {
        self.read(&self.bd_log)
            .lines()
            .filter(|l| l.starts_with("start ") || l.starts_with("stop "))
            .map(str::to_string)
            .collect()
    }

    /// Every invocation that started, in start order.
    pub fn bd_starts(&self) -> usize {
        self.bd_marker_lines()
            .iter()
            .filter(|l| l.starts_with("start "))
            .count()
    }

    /// Every invocation that came back.
    pub fn bd_stops(&self) -> usize {
        self.bd_marker_lines()
            .iter()
            .filter(|l| l.starts_with("stop "))
            .count()
    }

    /// The high-water mark of concurrent fake-`bd` processes the log records.
    ///
    /// Computed by walking the start/stop lines in the order they were written:
    /// `1` means the reads were strictly serial, which is what "no two reads in
    /// flight" is supposed to mean; `2` or more is the bug, caught red-handed.
    /// A `stop` with no matching `start` is clamped at zero rather than allowed
    /// to hide a later overlap under a negative count.
    pub fn bd_max_concurrency(&self) -> usize {
        let mut live = 0usize;
        let mut peak = 0usize;
        for marker in self.bd_marker_lines() {
            if marker.starts_with("start ") {
                live += 1;
                peak = peak.max(live);
            } else if marker.starts_with("stop ") {
                live = live.saturating_sub(1);
            }
        }
        peak
    }

    /// The raw start/stop lines, for a failure message that has to show what
    /// actually happened rather than a count of it.
    pub fn bd_trace(&self) -> Vec<String> {
        self.bd_marker_lines()
    }

    /// Make the chat fake **ignore `abort`**.
    ///
    /// The lever for the half of looprs-5g7 that only shows up when the child
    /// fights back: a run that answers `abort` and then keeps going is exactly
    /// what a tool which traps the signal, or a runtime wedged mid-call, looks
    /// like from here. Without it every cancel test is a test of the easy case,
    /// and the escalation ladder — the part that decides whether a stuck run is
    /// a stalled sentence or a hung app — never gets exercised at all.
    ///
    /// The fake still *answers* the abort, because that is the realistic shape:
    /// the failure is not "no reply", it is "reply, and no unwind".
    pub fn stubborn_pi(&self, on: bool) {
        let mark = self.dir.join("stubborn");
        if on {
            std::fs::write(&mark, b"").unwrap();
        } else {
            let _ = std::fs::remove_file(&mark);
        }
    }
}

impl Drop for Fakes {
    fn drop(&mut self) {
        // Kill every child we recorded before deleting the scratch dir. A fake that
        // outlives its test is a fake that turns up in somebody else's process
        // count assertion, and the chat fake holds runs open on purpose. The `bd`
        // fakes are on the list for the same reason now that one of them can sleep
        // for a third of a second (BdFake::Slow): a slow fake that outlives its
        // test is a slow fake turning up in the next one's timing.
        for pid in self.pi_pids().into_iter().chain(self.bd_pids()) {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_script(path: &Path, body: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    f.flush().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The fake binaries live under `tests/fixtures/`, as real files rather than string
/// literals (looprs-6ol: "keep the fixtures under `tests/`"). They are programs —
/// bash and python, a hundred-plus lines each — and a program inside a Rust string
/// literal cannot be linted, syntax-checked, diffed readably, or opened in an
/// editor without quoting soup.
///
/// `include_str!` rather than a runtime read: a missing or renamed fixture is a
/// compile error at the place that needs it, not a spawn failure in whichever test
/// happens to get there first.
const FAKE_PI: &str = include_str!("../tests/fixtures/fake_pi.sh");
const FAKE_PI_DIES: &str = include_str!("../tests/fixtures/fake_pi_dies.sh");
const FAKE_PI_CHAT: &str = include_str!("../tests/fixtures/fake_pi_chat.py");
const FAKE_BD: &str = include_str!("../tests/fixtures/fake_bd.sh");

/// Build the fake `pi` for one personality out of the fixtures.
///
/// The personality is a token rather than a separate file because the three of them
/// differ only in the one line that answers a prompt, and three copies of a script
/// that drift apart is worse than one template.
fn pi_script(log: &Path, mode: PiFake) -> String {
    let log = log.display().to_string();
    match mode {
        // The chat fake is a whole program of its own; it needs no tokens because it
        // finds its own state next to its own file.
        PiFake::Chat => FAKE_PI_CHAT.to_string(),
        // This one logs its pid and leaves; there is no reply to build.
        PiFake::DiesImmediately => FAKE_PI_DIES.replace("{{LOG}}", &log),
        PiFake::Started | PiFake::Handled | PiFake::Rejects => {
            let reply = match mode {
                PiFake::Started => {
                    "printf '{\"type\":\"response\",\"id\":\"%s\",\"command\":\"prompt\",\"success\":true,\"data\":{\"disposition\":\"started\"}}\\n' \"$id\""
                }
                PiFake::Handled => {
                    "printf '{\"type\":\"response\",\"id\":\"%s\",\"command\":\"prompt\",\"success\":true,\"data\":{\"disposition\":\"handled\"}}\\n' \"$id\""
                }
                _ => {
                    "printf '{\"type\":\"response\",\"id\":\"%s\",\"command\":\"prompt\",\"success\":false,\"error\":\"fake pi refused the prompt\"}\\n' \"$id\""
                }
            };
            FAKE_PI.replace("{{LOG}}", &log).replace("{{REPLY}}", reply)
        }
    }
}

/// Build the fake `bd` for one personality out of `tests/fixtures/fake_bd.sh`.
///
/// The personality table below is the only thing that varies; the script itself is
/// the fixture. Note that the substituted strings are *bash program text*, because
/// the fake's job is to answer each verb like a different `bd` would, and a verb
/// that has to fail differently per personality (`exit 3` vs `cat the board` vs
/// "not json at all") is a program-level answer, not a value.
fn bd_script(
    log: &Path,
    board: &Path,
    show: &Path,
    fail_mark: &Path,
    refuse_mark: &Path,
    mode: BdFake,
) -> String {
    // (read verb, `bd show`, write verb) per personality.
    let (read_cmd, show_cmd, write_cmd): (String, String, String) = match mode {
        BdFake::Fails => ("exit 3".into(), "exit 3".into(), "exit 3".into()),
        BdFake::Malformed => (
            "printf 'not json at all\\n'".into(),
            "printf 'not json at all\\n'".into(),
            "exit 0".into(),
        ),
        BdFake::EmptyOutput => ("true".into(), "true".into(), "exit 0".into()),
        BdFake::Ok => (
            format!("cat {}", board.display()),
            "printf '[]\\n'".into(),
            "exit 0".into(),
        ),
        BdFake::ShowStatus => (
            format!("cat {}", board.display()),
            format!("cat {}", show.display()),
            "exit 0".into(),
        ),
        BdFake::Slow => (
            format!(
                "sleep {}; cat {}",
                SLOW_FAKE_READ.as_secs_f64(),
                board.display()
            ),
            "printf '[]\\n'".into(),
            "exit 0".into(),
        ),
    };
    FAKE_BD
        .replace("{{LOG}}", &log.display().to_string())
        .replace("{{FAIL_MARK}}", &fail_mark.display().to_string())
        .replace("{{REFUSE_MARK}}", &refuse_mark.display().to_string())
        .replace("{{READ_CMD}}", &read_cmd)
        .replace("{{SHOW_CMD}}", &show_cmd)
        .replace("{{WRITE_CMD}}", &write_cmd)
}

/// Is this pid still a live process (not a reaped one)?
pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stop a process dead, from outside. This is the ticket's "kill the pi child out
/// from under the app": the app is not cooperating, and the child just goes away.
pub fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------------
// Fakes for the Router: a `Session` that records instead of spawning.
//
// The repo's test style is process-level (see the module docs), and the Beads backend
// keeps that: `session::beads::tests` asserts on real pids. But the Router's rules —
// one session per mode, park/resume, "a replaced generation has no route to the UI",
// "a submit that lost the race is dropped" — are lifecycle rules, and asserting them
// through three half-built child processes would test the fakes rather than the
// Router. So these record what was asked, in order, and let a test hold the event
// sender of a specific generation so it can make a *dead* one try to speak.
// ---------------------------------------------------------------------------------

use crate::session::{
    Session, SessionEvent, SessionFactory, SessionId, SessionStatus, Spawned, TerminalType,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Default)]
struct FakeInner {
    log: Vec<String>,
    senders: HashMap<SessionId, mpsc::UnboundedSender<SessionEvent>>,
    status: HashMap<SessionId, SessionStatus>,
    /// When true, `shutdown()` is recorded but the event stream is never closed,
    /// so the owning pump never finishes: a wedged session.
    silent_exit: bool,
}

/// A recorder shared by every fake session one test creates.
#[derive(Clone, Default)]
pub struct FakeBackend {
    inner: Arc<Mutex<FakeInner>>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// `shutdown()` records but never ends the stream — the wedged-child case.
    pub fn with_silent_exit(self) -> Self {
        self.inner.lock().unwrap().silent_exit = true;
        self
    }

    /// A factory that builds fakes for whatever mode the Router asks for.
    pub fn factory(&self) -> SessionFactory {
        self.factory_claiming(None)
    }

    /// As [`FakeBackend::factory`], but every session claims `Some(mode)` no matter
    /// what was requested — for the "a lying factory is refused" test.
    pub fn factory_claiming(&self, claimed: Option<TerminalType>) -> SessionFactory {
        let me = self.clone();
        Arc::new(move |mode: TerminalType, generation: u64| {
            let id = SessionId::new(claimed.unwrap_or(mode), generation);
            let (tx, rx) = mpsc::unbounded_channel::<SessionEvent>();
            {
                let mut st = me.inner.lock().unwrap();
                st.log
                    .push(format!("spawn {} #{generation}", id.mode.label()));
                st.senders.insert(id, tx.clone());
                st.status.entry(id).or_insert(SessionStatus::Idle);
            }
            Ok(Spawned {
                session: Box::new(FakeSession {
                    id,
                    backend: me.clone(),
                    events: tx,
                }),
                events: rx,
            })
        })
    }

    pub fn log(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.clone()
    }

    pub fn clear_log(&self) {
        self.inner.lock().unwrap().log.clear();
    }

    pub fn calls(&self, verb: &str) -> usize {
        self.log().iter().filter(|c| c.starts_with(verb)).count()
    }

    pub fn was_called(&self, verb: &str) -> bool {
        self.calls(verb) > 0
    }

    pub fn spawn_count(&self, mode: TerminalType) -> usize {
        self.log()
            .iter()
            .filter(|c| c.starts_with(&format!("spawn {}", mode.label())))
            .count()
    }

    /// The event sender for one specific generation, so a test can make a *dead*
    /// generation try to reach the UI.
    pub fn events(&self, id: SessionId) -> mpsc::UnboundedSender<SessionEvent> {
        self.inner
            .lock()
            .unwrap()
            .senders
            .get(&id)
            .cloned()
            .unwrap_or_else(|| panic!("no fake session {id} was created"))
    }

    pub fn set_status(&self, id: SessionId, status: SessionStatus) {
        self.inner.lock().unwrap().status.insert(id, status);
    }
}

/// A [`Session`] that does nothing but say what it was asked to do.
struct FakeSession {
    id: SessionId,
    backend: FakeBackend,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl Session for FakeSession {
    fn id(&self) -> SessionId {
        self.id
    }

    fn send_text(&mut self, text: String) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "send_text {} #{}: {text}",
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn abort(&mut self) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "abort {} #{}",
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        let silent = {
            let mut st = self.backend.inner.lock().unwrap();
            st.log.push(format!(
                "shutdown {} #{}",
                self.id.mode.label(),
                self.id.generation
            ));
            st.silent_exit
        };
        if !silent {
            // An orderly goodbye: the stream ends, so the pump completes.
            let _ = self.events.send(SessionEvent::Exited {
                reason: crate::session::ExitReason::Shutdown,
            });
        }
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "set_active {} {} #{}",
            if active { "true" } else { "false" },
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    /// Recorded so a test can assert the real window size reached the sessions
    /// that were up at the time — and, just as importantly, that no session was
    /// brought up merely because a window got dragged.
    fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "resize {} #{} {rows}x{cols}",
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn status(&self) -> SessionStatus {
        self.backend
            .inner
            .lock()
            .unwrap()
            .status
            .get(&self.id)
            .copied()
            .unwrap_or(SessionStatus::Idle)
    }
}

/// Standalone factory that always produces `claimed`, ignoring the requested mode.
pub fn fake(claimed: TerminalType) -> SessionFactory {
    FakeBackend::new().factory_claiming(Some(claimed))
}

/// Collect every session event that arrives within `wait`, described.
///
/// The cancel tests need to say *how long* they waited, because the two halves of
/// the contract are timings and not just contents:
///
/// * "cancelling…" must arrive while the child is still running, so the window has
///   to be shorter than the child's unwind; and
/// * "still stalled" must **not** arrive early, so the window has to be shorter
///   than [`cancel::GRACE`](crate::session::cancel).
///
/// A test that only asserts "the message eventually showed up" cannot tell those
/// apart, and cannot fail on the one thing that matters — a cancel acknowledged
/// *after* the fact is the same silence as no acknowledgement.
///
/// `describe` is passed in because each session module renders the same events in
/// its own words for its own failure messages, and a shared formatter would make
/// every module's assertion output belong to none of them.
pub async fn collect_within<F>(
    rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
    wait: std::time::Duration,
    describe: F,
) -> Vec<String>
where
    F: Fn(&SessionEvent) -> String,
{
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(ev)) => out.push(describe(&ev)),
            // Silence or a closed stream ends the window; the caller asserts on
            // whatever the window contained.
            Ok(None) | Err(_) => return out,
        }
    }
}
