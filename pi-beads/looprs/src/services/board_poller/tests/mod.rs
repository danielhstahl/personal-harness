//! `board_poller`'s tests, split along the four things the split of the module
//! made visible:
//!
//! * [`knobs`](knobs) — the environment resolved into policy, leniently and
//!   loudly, never silently;
//! * [`reads`](reads) — what the poller asks `bd` for and what the snapshot
//!   owes for each answer (including the three ways an answer can be misread);
//! * [`lifetime`](lifetime) — what is running, what must not be, and what a
//!   dropped owner leaves behind;
//! * [`journal`](journal) — the change detector, and every way a quiet journal
//!   could be lying;
//! * [`decision_table`](decision_table) — the schedule enumerated instead of
//!   walked.
//!
//! What more than one section drives lives here: the fake `bd`, the `Running`
//! session a section starts and asserts against, and the counters that turn
//! "the probe ran twice" into an assertion. Every file above reaches them
//! through the glob below.

use crate::services::board_poller::config::JournalConfig;
use std::time::Duration;

mod decision_table;
mod journal;
mod knobs;
mod lifetime;
mod reads;

use super::*;

use crate::state::board::{BoardRead, Column};

use crate::testing::{
    BOARD_VARIETY, BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD, PiFake, process_alive,
};

use std::time::Instant;

/// The board read, exactly as ADR-0007 §3 fixes it — the string the tests
/// count command lines against.
const BOARD_READ: &str = "--readonly list --all --limit 0 --json";

/// How long a test waits before concluding the poller is not going to answer.
/// Everything awaited here is answered by a fake in milliseconds, so timing
/// out means it is broken, not that the machine was slow.
const WAIT: Duration = Duration::from_secs(10);

/// The change detector with its sweep pinned to the tick: every tick is a
/// sweep, so the read cadence is exactly what it was before the detector
/// existed.
///
/// Tests about *the tick* — its cadence, its error ladder, its shutdown,
/// the three states it publishes — take this. They are not asking what the
/// journal saved; leaving the production 30 s sweep in them would mean they
/// read once and then wait for nothing to happen.
fn sweep_every(interval: Duration) -> JournalConfig {
    JournalConfig {
        enabled: true,
        reconcile: interval,
    }
}

/// A detector that will not sweep within the test's lifetime: the journal is
/// then the *only* thing that can make the poller re-read the board, which is
/// what turns "the board moved because something said it did" into a real
/// assertion rather than one the sweep quietly satisfies.
fn never_sweep() -> JournalConfig {
    JournalConfig {
        enabled: true,
        reconcile: Duration::from_secs(3600),
    }
}

/// A poller plus its fake `bd`, kept alive together for the test's duration.
///
/// Field order **is** teardown order: the poller is declared first so it is
/// dropped first, which ends the task before the scratch dir it reads from is
/// deleted. The other order leaves a task able to start one more read into a
/// directory that no longer exists.
struct Running {
    _poller: BoardPoller,
    fakes: Fakes,
    handle: BoardHandle,
}

/// A poller aimed at a fake `bd`.
///
/// Callers pass the interval: 40–60 ms is what makes a 350 ms fake read span
/// half a dozen ticks, which is the shape a 1 s interval and a 6 s read has
/// for real — and it keeps the whole slow-fake group at about a second of
/// wall clock.
fn running(tag: &str, fake: BdFake, board: &str, interval: Duration) -> Running {
    running_journal(tag, fake, board, interval, sweep_every(interval))
}

/// A poller aimed at a fake `bd`, with the detector's own knobs set by the
/// test rather than defaulted to "sweep every tick".
fn running_journal(
    tag: &str,
    fake: BdFake,
    board: &str,
    interval: Duration,
    journal: JournalConfig,
) -> Running {
    let fakes = Fakes::new(tag, PiFake::Started, fake, board);
    let (poller, handle) = BoardPoller::spawn(BoardConfig {
        bin: fakes.bd_bin().to_string(),
        interval,
        enabled: true,
        journal,
    });
    Running {
        _poller: poller,
        fakes,
        handle,
    }
}

/// Poll a condition rather than sleep a guessed amount: the fake answers in
/// milliseconds, so a wait that runs past its budget is a failure rather
/// than a race this test hopes to win.
async fn until(what: &str, timeout: Duration, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < timeout,
            "{what} never happened in {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_reads(fakes: &Fakes, n: usize) {
    until(&format!("{n} completed board read(s)"), WAIT, || {
        fakes.bd_stops() >= n
    })
    .await;
}

// ───────────────── reading the poller's own command log ─────────────────
//
// The watermark is state inside a task a test cannot reach, so every
// assertion about it is made against the one place it is observable: the
// `--since` number that went out on the last probe. That is also the
// stronger form of the claim — it is the watermark as `bd` saw it, not as
// the poller remembers it.

/// The board-read lines in the fake's log, in order.
fn board_reads(fakes: &Fakes) -> Vec<String> {
    fakes
        .bd_log()
        .into_iter()
        .filter(|l| l.as_str() == BOARD_READ)
        .collect()
}

/// The journal-probe lines in the fake's log, in order.
fn probes(fakes: &Fakes) -> Vec<String> {
    fakes
        .bd_log()
        .into_iter()
        .filter(|l| l.starts_with("--readonly events tail"))
        .collect()
}

/// One `--flag <value>` out of a logged command line.
fn flag(line: &str, want: &str) -> i64 {
    let parts: Vec<&str> = line.split_whitespace().collect();
    parts
        .iter()
        .position(|p| *p == want)
        .and_then(|i| parts.get(i + 1))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("no {want} <number> in {line:?}"))
}

/// The `--since` of every probe, in order: the watermark's trajectory.
fn probe_since(fakes: &Fakes) -> Vec<i64> {
    probes(fakes).iter().map(|l| flag(l, "--since")).collect()
}

/// The `--limit` of every probe, in order: which end of the catch-up /
/// steady-state pair each tick was asking with.
fn probe_limit(fakes: &Fakes) -> Vec<i64> {
    probes(fakes).iter().map(|l| flag(l, "--limit")).collect()
}

/// Wait until the poller's first board read has landed and it has had at
/// least `ticks` probes since — i.e. it is settled and quiet, which is the
/// state every detector test starts from.
async fn wait_settled(r: &Running, ticks: usize) {
    until("the first read plus a settled probe stream", WAIT, || {
        probes(&r.fakes).len() >= ticks && !board_reads(&r.fakes).is_empty()
    })
    .await;
}

/// Wait for the published **snapshot** to satisfy a condition.
///
/// Not the same thing as counting the fake's finished reads: the fake marks
/// itself done when it exits, and the read still has to be parsed and
/// published before a reader can see it. A test about the snapshot waits on
/// the snapshot; a test about the command line waits on the log.
async fn wait_snapshot(handle: &BoardHandle, what: &str, cond: impl Fn(&BoardSnapshot) -> bool) {
    until(what, WAIT, || cond(&handle.borrow())).await
}

/// The distinct `fetched_at` stamps the poller has published, oldest first,
/// waiting until `n` of them have landed.
///
/// Consecutive-equality is what "distinct" means here: a watch holds one
/// value, so a reader that arrives between two publishes sees the same stamp
/// twice. Skipping a whole publish is possible under load, and can only ever
/// make the *distance* between the stamps a test looks at longer.
async fn distinct_stamps(handle: &BoardHandle, n: usize) -> Vec<Instant> {
    let mut stamps: Vec<Instant> = Vec::new();
    let began = Instant::now();
    while stamps.len() < n {
        assert!(
            began.elapsed() < WAIT,
            "only {} snapshot(s) arrived in {WAIT:?}, wanted {n}",
            stamps.len()
        );
        if let Some(at) = handle.borrow().fetched_at
            && stamps.last() != Some(&at)
        {
            stamps.push(at);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stamps
}
