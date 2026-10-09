//! `beads`' tests, split along the banners that already sectioned the one module
//! this came out of. Each file in here is one of those sections:
//!
//! * [`startup`](startup) — the first pass, and the two ways the first spawn fails;
//! * [`handle`](handle) — entering the mode, leaving it, shutting down;
//! * [`drive`](drive) — who drives the loop: the worker's own settle, plus `Esc`
//!   (looprs-msj);
//! * [`planner`](planner) — the plan is not believed until the board says so
//!   (looprs-k7v);
//! * [`claim`](claim) — the harness claims the ticket it is paying for, and
//!   cannot re-work one it did not close (looprs-w7q);
//! * [`announce`](announce) — the one `notify` call, and the five other ways a
//!   pass can end without it;
//! * [`notes_tests`](notes_tests) — the wording, pinned without a subprocess;
//! * [`tables`](tables) — the step machine and the guards as enumerable tables
//!   (looprs-6ol).
//!
//! What more than one section drives lives here: the fake-binary fixtures, the
//! board JSON the sections need, and the readers that turn a [`SessionEvent`]
//! into a string a test can compare. They are private to this suite on purpose —
//! every file above reaches them through the globs below, and nothing outside
//! the suite should be assembling a beads session with them.

use crate::wire::PiEvent;

use crate::services::bd::Bead;
use crate::session::BeadStep;

mod announce;
mod claim;
mod drive;
mod handle;
mod notes_tests;
mod planner;
mod startup;
mod tables;

use super::*;
use crate::session::cancel;

use crate::session::TerminalType;

use crate::testing::{BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD, PiFake, process_alive};

use tokio::time::timeout;

const SECOND_BOARD: &str = r#"{
  "data": [
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

/// Generous, but bounded: a hang is a failure of this ticket, and a bounded test
/// reports it instead of wedging the suite.
const NO_HANG: Duration = Duration::from_secs(10);

/// A board with two tickets the planner "created" mid-run.
const PLAN_TWO_TICKETS: &str = r#"{
  "data": [
    {"id": "looprs-101", "title": "first planned ticket", "status": "open", "issue_type": "task"},
    {"id": "looprs-102", "title": "second planned ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

/// A board carrying one ticket that predates any planner.
const PRE_EXISTING: &str = r#"{
  "data": [
    {"id": "looprs-old", "title": "was already here", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

/// The same pre-existing ticket, plus one the planner added on top of it. The
/// diff must report only the second one.
const OLD_PLUS_ONE_NEW: &str = r#"{
  "data": [
    {"id": "looprs-old", "title": "was already here", "status": "open", "issue_type": "task"},
    {"id": "looprs-101", "title": "first planned ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

/// Two open tickets, so "the loop moved on to the next one" is observable
/// rather than merely not-stopped.
const TWO_OPEN: &str = r#"{
  "data": [
    {"id": "looprs-26r", "title": "Beads loop never self-starts", "status": "open", "issue_type": "bug"},
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

/// The first ticket blocked and the second left workable: the shape `bd ready`
/// is not supposed to produce, and the shape the loop has to cope with anyway.
const BLOCKED_THEN_READY: &str = r#"{
  "data": [
    {"id": "looprs-26r", "title": "waiting on somebody else", "status": "blocked", "issue_type": "bug"},
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

fn fakes_cfg(fakes: &Fakes) -> SessionConfig {
    SessionConfig {
        pi_bin: fakes.pi_bin().to_string(),
        bd_bin: fakes.bd_bin().to_string(),
        // The recorder, not `Noop`: the completion edge is as observable here
        // as the `bd` log is, and a test that cannot see it cannot pin it.
        notifier: Arc::new(fakes.notifier.clone()),
        ..Default::default()
    }
}

/// A bare loop, with its two output streams held open by the test.
///
/// The control receiver comes back rather than being dropped: these tests drive
/// the loop by calling it directly, so nothing is expected on that mailbox —
/// but dropping the receiver would silently discard the worker's control edges,
/// and a test that silently discards the thing under test is worse than one that
/// fails.
fn loop_with(
    fakes: &Fakes,
) -> (
    BeadsLoop,
    mpsc::UnboundedReceiver<SessionEvent>,
    mpsc::UnboundedReceiver<BeadsCmd>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<BeadsCmd>();
    let id = SessionId::new(TerminalType::Beads, 0);
    (BeadsLoop::new(id, tx, ctl_tx, fakes_cfg(fakes)), rx, ctl_rx)
}

/// One event, described as a stable string so a failing assertion prints
/// something readable instead of four nested enums.
fn describe(m: &SessionEvent) -> String {
    match m {
        SessionEvent::BeadStep(BeadStep::AwaitInput) => "step:await".into(),
        SessionEvent::BeadStep(BeadStep::CreateTickets) => "step:plan".into(),
        SessionEvent::BeadStep(BeadStep::WorkTickets) => "step:work".into(),
        // The claim, as the UI is told it: which ticket the loop holds, and
        // when it lets go of it.
        SessionEvent::ActiveBead { bead: Some(bead) } => format!("active:{}", bead.id),
        SessionEvent::ActiveBead { bead: None } => "active:-".into(),
        SessionEvent::Error(text) => format!("error: {text}"),
        SessionEvent::System(text) => format!("system: {text}"),
        SessionEvent::RestoreInput { text } => format!("restore: {text}"),
        SessionEvent::Agent(PiEvent::AgentSettled) => "agent_settled".into(),
        SessionEvent::Agent(_) => "agent".into(),
        SessionEvent::Exited { .. } => "session-down".into(),
        SessionEvent::BashOutput { .. } => "bash".into(),
        SessionEvent::Status(s) => format!("status:{s:?}"),
        // Only Bash mode ever takes a screen over; a beads session saying it did
        // would be a bug worth seeing in the test output.
        SessionEvent::ScreenHeld { active } => format!("screen:{active}"),
    }
}

/// Read events until one satisfies `done`, returning everything said on the way.
///
/// "Every pass that ends" is not one end state — a verified plan stops at
/// `step:work`, a refusal stops at `step:await` — so the parked variant below is
/// just this with a predicate, and tests that care about ordering can use their own.
async fn drain_until<F>(rx: &mut mpsc::UnboundedReceiver<SessionEvent>, done: F) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let mut out = Vec::new();
    loop {
        let line = match timeout(NO_HANG, rx.recv()).await {
            Ok(Some(m)) => describe(&m),
            Ok(None) => panic!("the session stream closed first: {out:?}"),
            Err(_) => panic!("no matching event within {NO_HANG:?}: {out:?}"),
        };
        let stop = done(&line);
        out.push(line);
        if stop {
            return out;
        }
    }
}

/// Snapshot of what the loop told the UI, as stable strings.
fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(describe(&m));
    }
    out
}

/// Read events until the loop lands in `AwaitInput`, and return everything it
/// said on the way.
///
/// "Parked" is the observable end state of every one of these tests, and it is
/// the state a human has to be able to reach: a loop that keeps saying it is
/// working while nothing is running is a loop that hid the input box forever.
async fn drain_until_parked(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
    drain_until(rx, |line| line == "step:await").await
}

fn has_error(msgs: &[String]) -> bool {
    msgs.iter().any(|m| m.starts_with("error: "))
}

/// The last thing the loop called an error. "Last" because the park note is
/// written after the verdict in some paths, and the assertion should be about
/// the verdict rather than about whichever string happened to land first.
fn last_error(msgs: &[String]) -> Option<String> {
    msgs.iter()
        .rev()
        .find(|m| m.starts_with("error: "))
        .cloned()
}

/// Send a planner instruction the way the beads input box sends it, and return
/// only once the pass is demonstrably live — a test that plans "and hopes" is a
/// test that settles the wrong run half the time.
async fn start_planner(s: &mut BeadsSession, text: &str) -> u64 {
    s.send_text(text.to_string()).unwrap();
    assert!(s.quiesce().await, "the submit was handled");
    assert_eq!(
        s.status(),
        SessionStatus::Running,
        "the planner pass is live"
    );
    s.in_flight().expect("the planner pass is in flight")
}

/// How many times the harness asked the board for the open tickets — the two
/// halves of the planner diff, counted separately from every other `bd` call.
fn board_reads(fakes: &Fakes) -> usize {
    fakes
        .bd_log()
        .iter()
        .filter(|l| l.starts_with("list --status open"))
        .count()
}

fn bead(id: &str, title: &str) -> Bead {
    bead_status(id, title, crate::services::bd::BeadStatus::Open)
}

/// As [`bead`], with a chosen status — the knob for "this ticket is not for a
/// worker", which the guard tests use without needing a board file for it.
fn bead_status(id: &str, title: &str, status: crate::services::bd::BeadStatus) -> Bead {
    Bead {
        id: id.to_string(),
        title: title.to_string(),
        status: crate::services::bd::BeadStatusFallback::Known(status),
        issue_type: crate::services::bd::BeadIssueType::Task,
    }
}

/// Tell the fake `bd` what status a ticket now has, for the one read the loop
/// cannot fake for itself: `bd show <id> --json`, the post-settle check
/// (looprs-w7q).
///
/// Tests that want the loop to *keep going* have to say the worker closed its
/// ticket, because a settle that leaves the ticket open now stops the loop on
/// purpose. That is the point of the guard, and it is why every "the settle
/// drove the next pass" test below carries this line: the loop advances on a
/// closed ticket, not on a quiet one.
fn show_status(fakes: &Fakes, id: &str, status: &str) {
    fakes.set_show(&format!(
            r#"{{"id":"{id}","title":"whatever {id} was called","status":"{status}","issue_type":"task"}}"#
        ));
}

/// As [`show_status`], with the ticket finished — the shape of a worker that
/// did the job.
fn show_closed(fakes: &Fakes, id: &str) {
    show_status(fakes, id, "closed");
}

// ------------------ the session handle: entry, parking, resumption ------------------

fn beads(fakes: &Fakes, generation: u64) -> (BeadsSession, mpsc::UnboundedReceiver<SessionEvent>) {
    // The un-boxed constructor: the test keeps a concrete, cloneable handle and
    // the event stream the Router would otherwise pump.
    BeadsSession::build(
        SessionId::new(TerminalType::Beads, generation),
        &fakes_cfg(fakes),
    )
    .expect("beads session must start")
}

async fn collect_until_down(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    loop {
        match timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(ev)) => {
                let down = matches!(ev, SessionEvent::Exited { .. });
                out.push(ev);
                if down {
                    return out;
                }
            }
            // Stream closed, or the wait expired: stop collecting. The
            // exactly-one-Exited guarantee itself is pinned in router::tests.
            Ok(None) => return out,
            Err(_) => return out,
        }
    }
}
