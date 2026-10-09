//! `bash`'s tests, split along the five things the module does:
//!
//! * [`roundtrip`](roundtrip) — a command in, its bytes out, the shell's own
//!   exit code, the marker stripped at the boundary;
//! * [`interrupt`](interrupt) — `Esc` against a real shell: queued, running,
//!   trapped;
//! * [`lifecycle`](lifecycle) — the shell bought on the first command, sized to
//!   the window, reported when it dies or will not start;
//! * [`shutdown`](shutdown) — the reap that never runs alone (looprs-2ck);
//! * [`screen`](screen) — the full-screen handover and what it owes (ADR-0001 Q2).
//!
//! What more than one section drives lives here: the shell builders, the event
//! tape, the wait-on-event helpers (`until_event`, `in_flight`, `until_exit`)
//! and the marker-bytes assertion. Every file above reaches them through the
//! glob below; nothing outside this suite should be assembling a Bash session
//! with them.

mod interrupt;
mod lifecycle;
mod roundtrip;
mod screen;
mod shutdown;

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
    let (mut s, rx) =
        BashSession::build(SessionId::new(TerminalType::Bash, generation), &cfg).expect("build");
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
    let (mut s, rx) =
        BashSession::build(SessionId::new(TerminalType::Bash, generation), &cfg).expect("build");
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
                "no exit after {NO_HANG:?}\nnotes: {:?}\nout: {:?}",
                ran.notes, ran.out
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
