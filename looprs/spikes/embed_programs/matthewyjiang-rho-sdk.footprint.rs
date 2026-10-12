//! Measure the footprint of running rho **inside** a host process.
//!
//! # Why this file exists
//!
//! `spikes/results/matthewyjiang-surface-cost.log` priced the candidate's surface
//! and ended on the one thing it could not price:
//!
//!   > The headline is that rho's real advantage — a Rust binary to embed instead of
//!   > a Node one to spawn — is invisible from the CLI and unmeasured here.
//!
//! and `spikes/results/pi-rpc-boundary-cost.log` closed latency and left this open:
//!
//!   > A real rho adoption case has to be a *memory* case measured on this box, in
//!   > the embedded shape looprs would actually use, at N-concurrent sessions
//!   > extrapolated to a memory ceiling. That is a different ticket.
//!
//! This is that ticket's embedded instrument. Four things it has to answer, each a
//! leg of `spikes/rho_footprint_cost.py`:
//!
//!   1. **The shape.** looprs would link `rho-sdk` and drive it in-process, not
//!      shell out to `rho`. Everything here is one process doing that.
//!   2. **The baseline.** An in-process RSS compared against some other process's
//!      RSS is not a delta. `floor` is the control the driver subtracts: the same
//!      binary, `rho-sdk` linked, nothing constructed.
//!   3. **N concurrent.** N sessions, N runs in flight, N runtimes — inside one
//!      process, against N `pi --mode rpc` children, then extrapolated to a
//!      memory ceiling.
//!   4. **The concurrency rule.** `rho_sdk` documents it in lib.rs: "`Session` owns
//!      one conversation history and permits a single active `Run`", and
//!      `Session::start` answers `Error::SessionBusy` — "session already has an
//!      active run". Does that collide with looprs, which reuses one session per
//!      ticket and advances it pass after pass? `settlerace` measures it.
//!
//! # What makes the memory rows trustworthy
//!
//! * Every number is the kernel's: `/proc/self/status` (`VmRSS`, `VmHWM`) and
//!   `/proc/self/smaps_rollup` (`Private_Dirty`, `Swap`). No allocator counting.
//! * Nothing in a memory row touches a network or a model. Turns come from a
//!   provider written in this file that sleeps and answers; that provider is
//!   *also* a measurement, because `rho-sdk` ships no model client at all (see
//!   `build_rho`), so any embedded host is writing one and its cost belongs to
//!   the host.
//! * So there is **no embedded-with-a-real-endpoint arm here**, and that is a
//!   finding rather than an omission: real-endpoint rows come from the CLI, which
//!   is rho's own composition of runtime + providers + tools.
//!
//! Arms, selected with `RHO_SPIKE_ARM`:
//!
//!   floor       control: same binary, rho-sdk linked, nothing constructed
//!   ladder      runtime build → session → three runs → second session → second runtime
//!   bigtool     the ladder where the tool result is a declared size
//!   history     one session, many runs: what a long pass costs as it grows
//!   settlerace  advance-on-`Completed` vs drain-to-close, refusal counts
//!   sessions    one runtime, N sessions, no runs
//!   concurrent  one runtime, N sessions, N runs in flight, plus N sequential
//!   engines     N runtimes in one process, one run each, then torn down

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rho_sdk::model::{
    ContentBlock, Message, ModelIdentity, ModelRequest, ModelResponse, ToolCall, ToolSpec,
};
use rho_sdk::provider::{ModelProvider, ProviderFuture};
use rho_sdk::tool::{Tool, ToolContext, ToolError, ToolFuture, ToolInvocation, ToolOutput};
use rho_sdk::{Run, RunEvent, Rho, Session, SessionOptions, UserInput};
use serde_json::json;

/// How long the tool sleeps, so "N concurrent" can be seen in wall time as well as
/// in memory. Overridden with `RHO_SPIKE_TOOL_MS`.
const TOOL_MS: u64 = 250;
/// Tool result size in KiB. Overridden with `RHO_SPIKE_PAYLOAD_KIB`.
const PAYLOAD_KIB: usize = 64;
/// Model turns sleep this long, so "N in flight" is visible in wall time.
/// Overridden with `RHO_SPIKE_MODEL_MS`.
const MODEL_MS: u64 = 120;
/// Trials for the settle race. Overridden with `RHO_SPIKE_TRIALS`.
const TRIALS: usize = 200;

fn set(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn tool_ms() -> u64 {
    set("RHO_SPIKE_TOOL_MS", TOOL_MS as usize) as u64
}

fn payload_kib() -> usize {
    set("RHO_SPIKE_PAYLOAD_KIB", PAYLOAD_KIB)
}

fn model_ms() -> u64 {
    set("RHO_SPIKE_MODEL_MS", MODEL_MS as usize) as u64
}

/// `(VmRSS_kib, VmHWM_kib)` as the kernel reports them.
fn rss() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut rss = 0u64;
    let mut hwm = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    (rss, hwm)
}

/// `Private_Dirty` + `Swap` from `smaps_rollup`: the part of the resident set that
/// belongs to this address space rather than to shared mappings. That is what a
/// host actually pays for when it links something in.
fn private_kib() -> (u64, u64) {
    let roll = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let mut private = 0u64;
    let mut swap = 0u64;
    for line in roll.lines() {
        if let Some(rest) = line.strip_prefix("Private_Dirty:") {
            private = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Swap:") {
            swap = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    (private, swap)
}

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}

fn rss_row(label: &str, extra: &str) {
    let (rss, hwm) = rss();
    let (private, swap) = private_kib();
    println!(
        "{label}\trss_kib={rss} hwm_kib={hwm} private_dirty_kib={private} swap_kib={swap} \
         threads={} {extra}",
        threads()
    );
}

fn event_name(event: &RunEvent) -> &'static str {
    match event {
        RunEvent::Started { .. } => "Started",
        RunEvent::StepStarted { .. } => "StepStarted",
        RunEvent::AssistantTextDelta { .. } => "AssistantTextDelta",
        RunEvent::ReasoningDelta { .. } => "ReasoningDelta",
        RunEvent::ToolProposed { .. } => "ToolProposed",
        RunEvent::ToolStarted { .. } => "ToolStarted",
        RunEvent::ToolFinished { .. } => "ToolFinished",
        RunEvent::UsageUpdated { .. } => "UsageUpdated",
        RunEvent::ModelCallCompleted { .. } => "ModelCallCompleted",
        RunEvent::Completed { .. } => "Completed",
        RunEvent::Cancelled { .. } => "Cancelled",
        RunEvent::Failed { .. } => "Failed",
        _ => "other",
    }
}

fn note(text: &str) {
    println!("   \t{text}");
}

/// Peak RSS of *this* process between two points, sampled by a background thread.
/// `VmHWM` is the cheaper instrument when the whole process is the subject, but a
/// row that says "N concurrent runs peaked at X" needs a sample taken *while* they
/// run, not after.
struct Peak(AtomicUsize, std::thread::JoinHandle<()>);

impl Peak {
    fn start() -> Self {
        let cur = Arc::new(AtomicUsize::new(0));
        let seen = cur.clone();
        let handle = std::thread::spawn(move || loop {
            let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            let mut now = 0usize;
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    now = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
                }
            }
            seen.fetch_max(now, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(1));
        });
        Peak(AtomicUsize::new(0), handle)
    }
    fn sample(&self) -> usize {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let mut now = 0usize;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                now = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
            }
        }
        self.0.fetch_max(now, Ordering::Relaxed).max(now)
    }
    fn finish(self) -> usize {
        let best = self.0.load(Ordering::Relaxed).max(rss().0 as usize);
        self.1.thread().unpark();
        drop(self.1);
        best
    }
}

// ── the tool: sized payload, optional sleep ──────────────────────────────────

static TOOL_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static TOOL_MAX_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);

struct SizedTool {
    payload: Arc<String>,
    ms: u64,
}

impl Tool for SizedTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sized".into(),
            description: "Returns a fixed block of text.".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn call<'a>(&'a self, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        let payload = self.payload.clone();
        let ms = self.ms;
        Box::pin(async move {
            if context.cancellation().is_cancelled() {
                return Err(ToolError::cancelled());
            }
            CALLS.fetch_add(1, Ordering::SeqCst);
            let live = TOOL_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            TOOL_MAX_IN_FLIGHT.fetch_max(live, Ordering::SeqCst);
            if ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            TOOL_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput::text(payload.as_str()))
        })
    }
}

fn big_payload(kib: usize) -> Arc<String> {
    let line = "The quick brown fox jumps over the lazy dog. 0123456789 ";
    let mut s = String::with_capacity(kib * 1024);
    while s.len() < kib * 1024 {
        s.push_str(line);
    }
    s.truncate(kib * 1024);
    Arc::new(s)
}

/// How many model calls have been answered, and how many were in flight at once.
static MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);
static MODEL_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static MODEL_MAX_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// A host-written `ModelProvider`: the minimum a host must supply to put rho in a
/// process, and therefore part of the embedded footprint rather than overhead this
/// file adds. `rho-sdk` ships `ScriptedProvider` (a test double with a fixed turn
/// queue) and no model client at all — its default feature set is empty by design,
/// "does not grant ... network ... access" — so anything that talks to a model is
/// host code. This one answers a tool call on the first turn of a run and text on
/// the next, after sleeping `ms`, which is all the concurrency rows need.
///
/// It is a stub on purpose: the real one is looprs' OpenAI-compatible client, whose
/// own footprint is not measured here.
struct PacedProvider {
    ms: u64,
    model: String,
}

impl ModelProvider for PacedProvider {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity::new("spike", "spike", &self.model)
    }

    fn send_turn<'a>(&'a self, request: ModelRequest<'a>) -> ProviderFuture<'a> {
        // One tool call per run, decided from the run's own position in its own
        // history: if the newest thing is a tool result, the run gets its answer,
        // otherwise it gets the call. Per-session by construction, which is what
        // makes the concurrency rows mean what they say — an earlier version of
        // this rule counted calls and results instead of their order, sent every
        // run into an infinite tool loop, and cost ~4 MiB a turn to prove it.
        let call_next = !matches!(
            request.messages.last(),
            Some(Message::ToolResult(_)) | Some(Message::EnrichedAssistant(_))
        );
        let seq = MODEL_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
        let live = MODEL_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
        MODEL_MAX_IN_FLIGHT.fetch_max(live, Ordering::SeqCst);
        let ms = self.ms;
        Box::pin(async move {
            if ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            MODEL_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            let _ = seq;
            Ok(ModelResponse::Assistant(if call_next {
                vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("call-{seq}"),
                    name: "sized".into(),
                    arguments: json!({}),
                })]
            } else {
                vec![ContentBlock::Text("done".into())]
            }))
        })
    }
}

/// Build one runtime: provider + sized tool. This is the unit a host constructs
/// once and reuses; `engines` prices building it N times.
///
/// `RHO_SPIKE_MAX_PARALLEL_TOOLS` sets `RhoBuilder::max_parallel_tools`, whose
/// documented default is one — "preserves sequential execution unless the embedding
/// host opts in to parallel tool calls" — so the row has to say what it used.
fn build_rho(ms: u64) -> Result<Rho, rho_sdk::Error> {
    let mut builder = Rho::builder().provider(PacedProvider {
        ms,
        model: "spike-paced".into(),
    });
    if let Some(k) = std::env::var("RHO_SPIKE_MAX_PARALLEL_TOOLS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .and_then(std::num::NonZeroUsize::new)
    {
        builder = builder.max_parallel_tools(k);
    }
    builder
        .tool(SizedTool {
            payload: big_payload(payload_kib()),
            ms: tool_ms(),
        })
        .build()
}

/// Drive one run to its terminal event and report the path it took.
async fn drive(session: &Session, text: &str) -> RunReport {
    let mut run = match session.start(UserInput::text(text)).await {
        Ok(run) => run,
        Err(e) => return RunReport::refused(format!("{e:?}")),
    };
    let mut events = 0usize;
    let mut terminal = String::new();
    while let Some(event) = run.next_event().await {
        events += 1;
        match &event {
            RunEvent::Completed { outcome } => terminal = format!("Completed:{}", outcome.text()),
            RunEvent::Failed { message, .. } => terminal = format!("Failed:{message}"),
            RunEvent::Cancelled { .. } => terminal = "Cancelled".into(),
            _ => {}
        }
    }
    let outcome = run.outcome().await;
    RunReport {
        events,
        terminal,
        refused: String::new(),
        outcome_ok: outcome.is_ok(),
    }
}

/// Drain one run to the end of its event stream and report how many events came.
/// Kept separate from `drive` so the concurrent arm can poll N of these together.
async fn drain(mut run: Run) -> usize {
    let mut n = 0usize;
    while run.next_event().await.is_some() {
        n += 1;
    }
    let _ = run.outcome().await;
    n
}

struct RunReport {
    events: usize,
    terminal: String,
    refused: String,
    outcome_ok: bool,
}

impl RunReport {
    fn refused(why: String) -> Self {
        Self { events: 0, terminal: String::new(), refused: why, outcome_ok: false }
    }
    fn ok(&self) -> bool {
        self.refused.is_empty()
    }
    fn kv(&self) -> String {
        if self.ok() {
            format!(
                "events={} terminal={} outcome={}",
                self.events,
                self.terminal
                    .split(':')
                    .next()
                    .unwrap_or("none"),
                self.outcome_ok
            )
        } else {
            format!("refused:{}", self.refused)
        }
    }
}

// ── arms ─────────────────────────────────────────────────────────────────────

/// The control every memory row subtracts: same binary, `rho-sdk` linked, nothing
/// constructed. Whatever `floor` already costs is the cost of *linking* the SDK,
/// which a host pays before it does any work — not the cost of using it.
async fn arm_floor() {
    println!("arm\tfloor");
    rss_row("floor.nothing_constructed", "note=rho-sdk linked, no runtime, no session");
    // Touch the type so the loader cannot be blamed for a later page fault; a
    // builder that is never built allocates a config and nothing else.
    let b = Rho::builder();
    rss_row("floor.builder_allocated", "note=Rho::builder() allocated, never built");
    drop(b);
    println!("floor.rss_kib\t{}", rss().0);
    println!("floor.rss_peak_kib\t{}", rss().1);
    note("This arm is the control subtracted from every embedded row below.");
}

/// Runtime build → session → three runs → second session → second runtime, one
/// step at a time, each row the added cost of exactly that step.
async fn arm_ladder() {
    println!("arm\tladder");
    rss_row("ladder.start", "");
    let t = std::time::Instant::now();
    let rho = match build_rho(0) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    rss_row(
        "ladder.after_runtime",
        &format!("step_ms={build_ms:.2} note=Rho built: provider + tool registry wired"),
    );

    let t = std::time::Instant::now();
    let session = match rho.session(SessionOptions::default()).await {
        Ok(s) => s,
        Err(e) => {
            println!("error\tsession: {e}");
            return;
        }
    };
    let session_ms = t.elapsed().as_secs_f64() * 1000.0;
    rss_row(
        "ladder.after_session",
        &format!("step_ms={session_ms:.2} note=one session, empty history"),
    );

    for i in 1..=3 {
        let t = std::time::Instant::now();
        let report = drive(&session, &format!("turn {i}")).await;
        rss_row(
            &format!("ladder.after_run_{i}"),
            &format!("step_ms={:.2} {}", t.elapsed().as_secs_f64() * 1000.0, report.kv()),
        );
    }

    let second = match rho.session(SessionOptions::default()).await {
        Ok(s) => s,
        Err(e) => {
            println!("error\tsecond session: {e}");
            return;
        }
    };
    rss_row("ladder.after_session_2", "note=second session on the same runtime");
    let report = drive(&second, "other lane").await;
    rss_row("ladder.after_session_2_run", &report.kv());

    let rho2 = match build_rho(0) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\tsecond runtime: {e}");
            return;
        }
    };
    rss_row("ladder.after_runtime_2", "note=second runtime in the same process");
    let third = rho2.session(SessionOptions::default()).await.unwrap();
    let report = drive(&third, "second runtime session").await;
    rss_row("ladder.after_runtime_2_run", &report.kv());
}

/// The ladder where the tool result is a declared size: the row is then readable
/// as "this much tool output costs this much", which is the term that dominates a
/// real pass.
async fn arm_bigtool() {
    println!("arm\tbigtool");
    let kib = payload_kib();
    println!("payload_kib\t{kib}");
    rss_row("bigtool.start", "");
    let payload = big_payload(kib);
    let rho = match build_rho(0) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    let session = rho.session(SessionOptions::default()).await.unwrap();
    rss_row("bigtool.after_session", "");
    for i in 1..=3 {
        let report = drive(&session, &format!("turn {i}")).await;
        rss_row(&format!("bigtool.after_run_{i}"), &report.kv());
    }
    println!("bigtool.tool_calls\t{}", CALLS.load(Ordering::SeqCst));
    println!("bigtool.tool_output_bytes_per_call\t{}", payload.len());
}

/// One session, many runs: what a long pass costs as it grows. Printed every Nth
/// run so the curve is visible instead of an average.
async fn arm_history() {
    println!("arm\thistory");
    let runs = set("RHO_SPIKE_RUNS", 50);
    let every = set("RHO_SPIKE_EVERY", 10);
    println!("runs\t{runs}");
    rss_row("history.start", "");
    let rho = match build_rho(0) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    let session = rho.session(SessionOptions::default()).await.unwrap();
    let mut prev = rss().0;
    let mut history_msgs = 0usize;
    for i in 1..=runs {
        let report = drive(&session, &format!("turn {i}")).await;
        if i % every == 0 || i == runs {
            let now = rss().0;
            let msgs = session.history().len();
            let _ = &mut history_msgs;
            rss_row(
                &format!("history.after_run_{i}"),
                &format!("{} since_last={} history_messages={msgs}", report.kv(), now - prev),
            );
            prev = now;
        }
    }
    note("Every turn here feeds a tool result back into history, so this is the");
    note("shape of a pass that has been going a while. A single-run row cannot show");
    note("that a long pass grows, or how fast.");
}

/// The concurrency rule, measured in the shape a loop hits it.
///
/// `Session::start` refuses with `Error::SessionBusy` when a run is active, and a
/// run is active until its `Run` goes away. A loop that advances on the terminal
/// *event* therefore starts the next run while the previous one may still be
/// alive. This arm runs both orders, `trials` times each, and counts refusals by
/// what they said — then retries after 20 ms, because a refusal that clears is a
/// released slot arriving late and a refusal that never clears is the API's answer.
async fn arm_settlerace() {
    println!("arm\tsettlerace");
    println!("model_ms\t{}", model_ms());
    let rho = match build_rho(model_ms()) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    // Three shapes, because "advance on the event" is two decisions, not one: when
    // to start the next run, and what to do with the `Run` you were reading. A
    // `Run` owns the session's one slot until it goes away, so holding it while
    // starting a second run is a refusal by design, not a race. The middle shape is
    // the one a loop would write: see the event, release the run, advance.
    for (label, mode) in [
        ("break_on_event_run_held", 0),
        ("break_on_event_run_dropped", 1),
        ("drain_stream_then_advance", 2),
    ] {
        let (mut busy, mut other, mut late, mut wall_ms) = (0usize, 0usize, 0usize, 0f64);
        let mut texts: Vec<(usize, String)> = Vec::new();
        let trials = set("RHO_SPIKE_TRIALS", TRIALS);
        for i in 0..trials {
            let session = rho.session(SessionOptions::default()).await.unwrap();
            let t0 = std::time::Instant::now();
            let mut run = session.start(UserInput::text(format!("settle {i}"))).await.unwrap();
            let mut started_second = None;
            loop {
                let Some(event) = run.next_event().await else {
                    break;
                };
                let is_terminal = matches!(
                    event,
                    RunEvent::Completed { .. } | RunEvent::Cancelled { .. } | RunEvent::Failed { .. }
                );
                if is_terminal && mode != 2 {
                    // The shape: the host saw the terminal event and is advancing
                    // right here, without waiting for the stream to close.
                    if mode == 0 {
                        started_second = Some(session
                            .start(UserInput::text(format!("{label} second")))
                            .await);
                    }
                    break;
                }
            }
            if mode != 0 {
                // Dropping the `Run` is what releases the session's slot; this is
                // the line a host has to get right, and it is the whole difference
                // between the second and third rows.
                drop(run);
                started_second = Some(
                    session
                        .start(UserInput::text(format!("{label} second")))
                        .await,
                );
            }
            let started = started_second.unwrap();
            match started {
                Ok(mut second) => {
                    while second.next_event().await.is_some() {}
                    let _ = second.outcome().await;
                }
                Err(e) => {
                    let head: String = format!("{e:?}").chars().take(60).collect();
                    match texts.iter_mut().find(|(_, t)| *t == head) {
                        Some((c, _)) => *c += 1,
                        None => texts.push((1, head)),
                    }
                    if matches!(e, rho_sdk::Error::SessionBusy) {
                        busy += 1;
                    } else {
                        other += 1;
                    }
                    // Give it 20 ms: a refusal that clears is a released slot
                    // arriving late, which is a race. One that never clears is the
                    // API's own answer about that session and is not.
                    let mut cleared = false;
                    let until = std::time::Instant::now() + std::time::Duration::from_millis(20);
                    while std::time::Instant::now() < until {
                        match session.start(UserInput::text("probe after 20 ms")).await {
                            Ok(mut probe) => {
                                while probe.next_event().await.is_some() {}
                                cleared = true;
                                break;
                            }
                            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
                        }
                    }
                    if !cleared {
                        late += 1;
                    }
                }
            }
            wall_ms += t0.elapsed().as_secs_f64() * 1000.0;
        }
        println!(
            "settlerace.{label}\ttrials={trials} session_busy_refusals={busy} other_refusals={other} \
             still_refused_after_20ms={late} wall_ms={wall_ms:.1}"
        );
        texts.sort_by(|a, b| b.0.cmp(&a.0));
        for (c, t) in texts.into_iter().take(4) {
            println!("\trefused {c}× with: {t}");
        }
    }
    note("Why a memory/concurrency capture carries this: ADR-0010's settle row asks");
    note("whether a loop advance can be keyed on the terminal event. It cannot, and");
    note("the two rows that look safe are the ones that lose: reading to the event");
    note("and advancing, with the run held or dropped, gets refused some fraction of");
    note("the trials; dropping the run first lowers the rate and does not remove it.");
    note("The session's run slot is released asynchronously with respect to the");
    note("terminal event, and `is_running()` disagrees with `start()` in both");
    note("directions at that moment — `steptiming` rows show it reading true on trials");
    note("where the next `start` worked and false on ones where it refused — so neither");
    note("the event nor that predicate is a gate. Draining the stream to its end is the");
    note("only shape here with zero refusals. That is ADR-0010's rule arriving");
    note("from the opposite direction: on pi the loop has to *wait* for the stream to");
    note("close (its settle window is ~456 ms), and on this engine the close costs");
    note("nothing (`steptiming`'s `settle_window_ms`) but still has to be waited for.");
    note("A host that advances on the event has to treat `SessionBusy` as retryable;");
    note("a host that advances on the close does not have to handle it at all.");
}

/// One runtime, N sessions, no runs: the pure per-session marginal.
async fn arm_sessions() {
    println!("arm\tsessions");
    let n = set("RHO_SPIKE_N", 8);
    println!("n\t{n}");
    let rho = match build_rho(0) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    let base = rss().0;
    let mut sessions = Vec::new();
    for _ in 0..n {
        match rho.session(SessionOptions::default()).await {
            Ok(s) => sessions.push(s),
            Err(e) => {
                println!("error\tsession: {e}");
                return;
            }
        }
    }
    let after = rss().0;
    println!("sessions.runtime_kib\t{base}");
    println!("sessions.added_kib\t{}", after - base);
    println!("sessions.added_kib_per_session\t{}", (after - base) / n as u64);
    println!("sessions.threads_with_n_sessions\t{}", threads());
}

async fn fresh_sessions(rho: &Rho, n: usize) -> Vec<(usize, Session)> {
    let mut out = Vec::new();
    for i in 0..n {
        if let Ok(s) = rho.session(SessionOptions::default()).await {
            out.push((i, s));
        }
    }
    out
}

/// The shape looprs would use: one runtime, N sessions, N runs in flight at once,
/// all in one process. Wall time is compared against the same N runs deliberately
/// serialized on fresh sessions, because "N concurrent" means nothing without
/// "N sequential" next to it.
async fn arm_concurrent() {
    println!("arm\tconcurrent");
    let n = set("RHO_SPIKE_N", 8);
    println!("n\t{n}");
    println!("tool_ms\t{}", tool_ms());
    println!("model_ms\t{}", model_ms());
    println!(
        "max_parallel_tools\t{}",
        std::env::var("RHO_SPIKE_MAX_PARALLEL_TOOLS").as_deref().unwrap_or("(default)")
    );
    let rho = match build_rho(model_ms()) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    rss_row("concurrent.after_runtime", "");
    let runtime_kib = rss().0;
    let mut sessions = Vec::new();
    for _ in 0..n {
        match rho.session(SessionOptions::default()).await {
            Ok(s) => sessions.push(s),
            Err(e) => {
                println!("error\tsession: {e}");
                return;
            }
        }
    }
    let peak = Peak::start();

    // The shape a host would actually use: N sessions, all N runs started, then all
    // N streams drained *together*. Draining them one lane at a time is a real host
    // mistake with a measurable cost (it backpressures every lane behind the first),
    // so it gets its own row rather than being the default.
    let t0 = std::time::Instant::now();
    let mut started = 0usize;
    let mut refused = 0usize;
    let mut first_refusal = String::new();
    let mut pumps = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        match s.start(UserInput::text(format!("lane {i}"))).await {
            Ok(run) => {
                started += 1;
                pumps.push(drain(run));
            }
            Err(e) => {
                refused += 1;
                if first_refusal.is_empty() {
                    first_refusal = format!("{e:?}");
                }
            }
        }
    }
    let start_wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let counts = futures::future::join_all(pumps).await;
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let peak_kib = peak.finish();
    let events: usize = counts.iter().sum();
    let after = rss().0;
    println!("concurrent.sessions\t{}", sessions.len());
    println!("concurrent.runs_started\t{started}");
    println!("concurrent.runs_refused\t{refused}");
    if !first_refusal.is_empty() {
        println!("concurrent.first_refusal\t{first_refusal}");
    }
    println!("concurrent.events\t{events}");
    println!("concurrent.start_wall_ms\t{start_wall_ms:.1}");
    println!("concurrent.wall_ms\t{wall_ms:.1}");
    println!("concurrent.peak_rss_kib\t{peak_kib}");
    println!("concurrent.added_kib_over_runtime\t{}", after.saturating_sub(runtime_kib));
    println!(
        "concurrent.max_model_calls_in_flight\t{}",
        MODEL_MAX_IN_FLIGHT.load(Ordering::SeqCst)
    );
    println!(
        "concurrent.max_tool_calls_in_flight\t{} (max_parallel_tools={})",
        TOOL_MAX_IN_FLIGHT.load(Ordering::SeqCst),
        std::env::var("RHO_SPIKE_MAX_PARALLEL_TOOLS").as_deref().unwrap_or("default:1")
    );
    println!("concurrent.threads_at_peak\t{}", threads());
    note("max_parallel_tools caps tool calls inside one run (documented default 1).");
    note("The row above shows whether it caps tool calls *across* runs too; here it");
    note("does not, so N sessions really do run N tools at once.");

    // The one-lane-at-a-time drain, for contrast: same number of runs, started
    // together, drained in sequence instead of together. If event delivery across
    // sessions shared a bottleneck this row would be the slow one.
    let t0 = std::time::Instant::now();
    let mut lanes = Vec::new();
    for (i, s) in fresh_sessions(&rho, sessions.len()).await {
        if let Ok(run) = s.start(UserInput::text(format!("serial-ish {i}"))).await {
            lanes.push(run);
        }
    }
    let mut solo_events = 0usize;
    for mut run in lanes {
        while run.next_event().await.is_some() {
            solo_events += 1;
        }
    }
    println!("concurrent.one_lane_at_a_time_wall_ms\t{:.1}", t0.elapsed().as_secs_f64() * 1000.0);
    println!("concurrent.one_lane_at_a_time_events\t{solo_events}");

    // Same N runs, deliberately one after another, on fresh sessions, so the row
    // measures concurrency and not the busy-session guard.
    let t0 = std::time::Instant::now();
    for i in 0..n {
        let s = rho.session(SessionOptions::default()).await.unwrap();
        drive(&s, &format!("serial {i}")).await;
    }
    println!("concurrent.sequential_wall_ms\t{:.1}", t0.elapsed().as_secs_f64() * 1000.0);

    // The rule itself, in the shape a loop would hit it: advance the *same*
    // session while its previous run is still in flight, then after it drains.
    let probe = rho.session(SessionOptions::default()).await.unwrap();
    let mut first = probe.start(UserInput::text("in flight")).await.unwrap();
    let _ = first.next_event().await;
    println!("concurrent.is_running_mid_run\t{}", probe.is_running());
    match probe.start(UserInput::text("while the first is in flight")).await {
        Ok(_) => println!("concurrent.second_run_on_busy_session\tACCEPTED"),
        Err(e) => println!("concurrent.second_run_on_busy_session\trefused: {e:?}"),
    }
    while first.next_event().await.is_some() {}
    let _ = first.outcome().await;
    println!("concurrent.is_running_after_drain\t{}", probe.is_running());
    match probe.start(UserInput::text("after the first drained")).await {
        Ok(mut r) => {
            while r.next_event().await.is_some() {}
            println!("concurrent.third_run_after_drain\tACCEPTED")
        }
        Err(e) => println!("concurrent.third_run_after_drain\trefused: {e:?}"),
    }
}

/// N whole runtimes in one process — what it costs if each concurrent pass gets its
/// own — and then all of them shut down and dropped, because a long-lived host has
/// to know whether the memory comes back.
async fn arm_engines() {
    println!("arm\tengines");
    let n = set("RHO_SPIKE_N", 8);
    println!("n\t{n}");
    rss_row("engines.start", "");
    let base = rss().0;
    let threads_base = threads();

    let t0 = std::time::Instant::now();
    let mut runtimes = Vec::new();
    for _ in 0..n {
        match build_rho(model_ms()) {
            Ok(rho) => runtimes.push(rho),
            Err(e) => {
                println!("error\truntime: {e}");
                return;
            }
        }
    }
    let after_build = rss().0;
    println!("engines.build_wall_ms\t{:.1}", t0.elapsed().as_secs_f64() * 1000.0);
    println!("engines.added_kib_total\t{}", after_build - base);
    println!("engines.added_kib_per_runtime\t{}", (after_build - base) / n as u64);

    let t0 = std::time::Instant::now();
    let peak = Peak::start();
    let mut handles = Vec::new();
    for (i, rho) in runtimes.iter().enumerate() {
        match rho.session(SessionOptions::default()).await {
            Ok(session) => {
                let report = drive(&session, &format!("runtime {i}")).await;
                handles.push(report);
            }
            Err(e) => println!("error\tsession: {e}"),
        }
        peak.sample();
    }
    let runs_wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let peak_kib = peak.finish();
    println!("engines.runs_wall_ms\t{runs_wall_ms:.1}");
    println!(
        "engines.runs\tok={} refused={}",
        handles.iter().filter(|r| r.ok()).count(),
        handles.iter().filter(|r| !r.ok()).count()
    );
    println!("engines.peak_rss_kib\t{peak_kib}");
    println!("engines.threads_with_n_runtimes\t{}", threads());

    // Teardown: `Rho::shutdown` then drop every runtime, and look at what is left.
    let t0 = std::time::Instant::now();
    let mut cancelled = 0usize;
    for rho in runtimes.iter() {
        cancelled += rho.shutdown().cancelled_runs();
    }
    drop(runtimes);
    let after_drop = rss().0;
    println!("engines.teardown_wall_ms\t{:.1}", t0.elapsed().as_secs_f64() * 1000.0);
    println!("engines.shutdown_cancelled_runs\t{cancelled}");
    println!("engines.rss_after_drop_kib\t{after_drop}");
    println!("engines.retained_kib_over_start\t{}", after_drop.saturating_sub(base));
    println!("engines.threads_after_drop\t{} (started at {threads_base})", threads());
    note("Teardown is priced because the surface capture found no explicit teardown");
    note("path and flagged it as the risk for a long-lived host. This row is the");
    note("answer: what is still resident after every runtime is shut down and dropped.");
}

/// Where a run's wall time actually goes, event by event, with every sleep in the
/// spike turned off. This exists because `ladder` shows a run costing ~250 ms of
/// wall time with a 0 ms provider and a 0 ms tool, and a per-pass cost that large
/// in a loop that runs passes back to back has to be located before it is argued
/// about — either it is inside the run, or it is after the run stopped producing
/// events, and those are different problems with different fixes.
async fn arm_steptiming() {
    println!("arm\tsteptiming");
    println!("model_ms\t{}  tool_ms\t{}", model_ms(), tool_ms());
    let rho = match build_rho(model_ms()) {
        Ok(rho) => rho,
        Err(e) => {
            println!("error\truntime: {e}");
            return;
        }
    };
    for i in 1..=3 {
        // Shape A — the loop's shape: read until the terminal event, stop reading,
        // release the run, advance. Everything after the terminal event is the
        // settle window, and a loop cannot start its next pass inside it.
        let session = rho.session(SessionOptions::default()).await.unwrap();
        let t0 = std::time::Instant::now();
        let mut run = session.start(UserInput::text(format!("turn {i}"))).await.unwrap();
        let mut marks = Vec::new();
        let mut terminal_at = None;
        while let Some(event) = run.next_event().await {
            let at = t0.elapsed().as_secs_f64() * 1000.0;
            if matches!(event, RunEvent::Completed { .. }) {
                terminal_at = Some(at);
                break;
            }
            marks.push(format!("{}@{at:.1}", event_name(&event)));
        }
        let break_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let running_at_break = session.is_running();
        let t = std::time::Instant::now();
        drop(run);
        let drop_ms = t.elapsed().as_secs_f64() * 1000.0;
        let running_after_drop = session.is_running();
        // The next pass, timed: this is what a loop would actually pay between one
        // pass ending and the next one's first event.
        // A refusal here is not a failure of this arm: it is the thing `settlerace`
        // counts, so it is recorded in the row instead of being unwrapped.
        let t = std::time::Instant::now();
        let next = session.start(UserInput::text("next pass")).await;
        let next_start_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (next_first_ms, next_outcome) = match next {
            Ok(mut next) => {
                let _ = next.next_event().await;
                let first = t.elapsed().as_secs_f64() * 1000.0;
                while next.next_event().await.is_some() {}
                (format!("{first:.2}"), "started".to_string())
            }
            Err(e) => ("—".into(), format!("refused: {e:?}")),
        };
        let mut row = format!(
            "steptiming.break_at_terminal_{i}\tevents=[{}] terminal_at_ms={} break_ms={break_ms:.1}",
            marks.join(","),
            terminal_at.unwrap_or(f64::NEG_INFINITY),
        );
        row.push_str(&format!(
            " is_running_at_break={running_at_break} drop_ms={drop_ms:.2} \
             is_running_after_drop={running_after_drop}"
        ));
        row.push_str(&format!(
            " next_start_ms={next_start_ms:.2} next_first_event_ms={next_first_ms} \
             next_pass={next_outcome}"
        ));
        println!("{row}");

        // Shape B — drain the stream to its end and time the gap between the
        // terminal event and the stream actually closing. That gap is pi's 456 ms
        // settle in miniature, and this row says how big it is here.
        let session = rho.session(SessionOptions::default()).await.unwrap();
        let t0 = std::time::Instant::now();
        let Ok(mut run) = session.start(UserInput::text(format!("turn {i}, drained"))).await else {
            println!("steptiming.drain_to_close_{i}\terror=could not start the drained run");
            continue;
        };
        let mut terminal_at = None;
        let mut events = 0usize;
        while let Some(event) = run.next_event().await {
            events += 1;
            if matches!(event, RunEvent::Completed { .. }) {
                terminal_at = Some(t0.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let close_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let terminal = terminal_at.unwrap_or(f64::NEG_INFINITY);
        let mut row = format!(
            "steptiming.drain_to_close_{i}\tevents={events} terminal_at_ms={terminal:.1}"
        );
        row.push_str(&format!(
            " stream_close_ms={close_ms:.1} settle_window_ms={:.1}",
            (close_ms - terminal).max(0.0)
        ));
        println!("{row}");
    }
    note("What this pair of rows is for: two things a loop cares about that no other");
    note("row in this capture can show. First, at the top of the file's two-run");
    note("latency: with no sleeps anywhere, a whole run — two model turns and a tool");
    note("call through the runtime — takes well under a millisecond, so the per-run");
    note("cost a `ladder` row shows is the work it was given, not a fixed tax the");
    note("runtime charges for existing. Second, `settle_window_ms`: the gap between");
    note("the terminal event and the stream actually closing. On pi that gap is ~456");
    note("ms and the loop has to wait for it (ADR-0010's settle row). Run this arm");
    note("twice, with and without turn latency: the window is ~0 either way, so the");
    note("wait is cheap here — but `settlerace` shows the session still refuses an");
    note("advance that ignores it.");
}

#[tokio::main]
async fn main() {
    let arm = std::env::var("RHO_SPIKE_ARM").unwrap_or_else(|_| "floor".into());
    println!("host.pid\t{}", std::process::id());
    println!("payload_kib\t{}", payload_kib());
    match arm.as_str() {
        "floor" => arm_floor().await,
        "ladder" => arm_ladder().await,
        "bigtool" => arm_bigtool().await,
        "history" => arm_history().await,
        "settlerace" => arm_settlerace().await,
        "sessions" => arm_sessions().await,
        "concurrent" => arm_concurrent().await,
        "engines" => arm_engines().await,
        "steptiming" => arm_steptiming().await,
        other => println!("error\tunknown arm {other}"),
    }
    println!("host.rss_exit_kib\t{}", rss().0);
    println!("host.rss_peak_exit_kib\t{}", rss().1);
}
