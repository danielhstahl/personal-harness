//! EMBED arm of spikes/rho_surface_cost.py for the `matthewyjiang` target
//! (matthewyjiang/rho: rho-sdk). Compiled by that spike's PRICE/EMBED legs into a
//! scratch crate; nothing in looprs links it.
//!
//! It is written against rho-sdk's API (`Rho::builder()`, `Session`, `Run`,
//! `RunEvent`, `RunOutcome`) and will not compile against casonadams/rho, whose
//! shape is `rho::platform::agent_engine` / `engine::run_turn` — see
//! `casonadams-rho.main.rs`. Two libraries that share a name do not share an
//! embed program, so the program is part of the target's identity.
//!
//! ## What this program is for
//!
//! ADR-0010's inventory list says what a host needs from an engine: a way to gate
//! tools per pass, a signal that the turn is over, a way to steer and to cancel,
//! a persistence story, and a fake to test against. Every arm below *runs* one of
//! those rows against rho-sdk and prints what the host process sees, because the
//! previous capture could only quote a README for most of them.
//!
//! ## Arms (`RHO_SPIKE_ARM`)
//!
//! Endpoint arms (need `RHO_SPIKE_BASE_URL`; the driver prints `not measured`
//! for these rather than substituting a scripted answer):
//!   endpoint-headless  one tool-using turn, unattended: the allow-process policy
//!                      stands in for "nobody is here to approve a tool".
//!   endpoint-default   the same turn with the SDK's defaults (no policy, no
//!                      approval handler), i.e. what you get out of the box.
//!
//! Scripted arms — no endpoint involved, each labelled so it cannot be read as
//! the endpoint arm passing:
//!   scripted           the fake-provider seam (`ScriptedProvider`/`ScriptedTurn`)
//!                      driving one real in-process tool round trip.
//!   gate               how tool gating is expressed: policy, approval handler,
//!                      pre-tool hook, per-request visibility — and what an
//!                      unattended host actually hits.
//!   settle             what the end-of-run signal is, including the cases looprs
//!                      cares about: a dropped Run, a dead provider socket, a host
//!                      that stops reading, a run that fails.
//!   steering           looprs' `steer` / `clear_queue` / `abort` mapped onto
//!                      `Run::steer` / retraction / `Run::cancel`, plus the
//!                      host-input round trip, shown as events.
//!   blast              cancellation and the blast wall: what happens to a tool's
//!                      child process on cancel, on drop, on SIGKILL of the host,
//!                      and what a panic inside a tool costs the host.
//!   persist            persistence: what gets written, in what format, and how a
//!                      host opts out.
//!
//! Internal arms (spawned by `blast`, not listed by the driver):
//!   __child_holder     run a tool that holds a marked OS child forever
//!   __panicking_tool   run a tool that panics on purpose
//!
//! Every printed line is `key<TAB>value` (or an indented note) so the driver can
//! filter this program's output from the library's own stderr.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rho_sdk::hooks::{HookDecision, HookGateFuture, PreToolUseGate, PreToolUseRequest};
use rho_sdk::model::{
    ContentBlock, Message, ModelEvent, ModelIdentity, ModelRequest, ModelResponse, ToolCall,
    ToolSpec,
};
use rho_sdk::provider::{
    ModelProvider, ProviderEventSender, ProviderFuture, ProviderSteeringReceiver,
    ScriptedProvider, ScriptedTurn,
};
use rho_sdk::tool::{
    Tool, ToolContext, ToolError, ToolErrorKind, ToolFuture, ToolInvocation, ToolOutput,
    ToolVisibility,
};
use rho_sdk::{
    approval_channel, ApprovalHandler, ApprovalRequest, CapabilityRequest, CapabilitySource,
    HostChoice, HostInputRequest, HostQuestion, InMemorySessionStore, ProcessEnvironment,
    ProcessExecution, ProcessInvocation, ProcessOutputLimits, ProviderError, ProviderErrorKind,
    Retryability, Rho, Run, RunEvent, ScopedWorkspacePolicy, SelectionMode, SessionOptions,
    UserInput,
};
use serde_json::json;

/// Written into the argv of every child a probe spawns, so the process scan can
/// find the ones this spike is responsible for (and clean them up).
const MARK: &str = "LOOPRS_SPIKE_CHILD";
/// Where a tool records the pid of the child it spawned, so a probe outside the
/// run can ask "is that child still alive, and who is its parent now".
const MARK_DIR: &str = "/tmp/rho-scratch-probe";

static TOOL_CALLS: AtomicUsize = AtomicUsize::new(0);

// ─────────────────────────────── output helpers ────────────────────────────────
fn kv(key: &str, value: impl std::fmt::Display) {
    println!("{key}\t{value}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Prose line. Indented, and still tab-bearing, so the driver keeps it.
fn note(text: impl AsRef<str>) {
    for line in text.as_ref().lines() {
        println!("\t{line}");
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// `VmRSS` (now) and `VmHWM` (the kernel's high-water mark for this process) in
/// KiB. `VmHWM` is the number the ADR's memory row needs: what an in-process
/// engine peaks at is not the same fact as what it holds after the turn.
fn rss_kib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut rss = 0;
    let mut peak = 0;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            peak = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    (rss, peak)
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.replace('\n', " ").replace('\t', " ");
    let chars: Vec<char> = flat.trim().chars().collect();
    if chars.len() <= max {
        flat.trim().to_string()
    } else {
        chars[..max].iter().collect::<String>() + "…"
    }
}

/// The variant tag of a `RunEvent`, which is the only thing a host can key a
/// state machine on: rho-sdk events carry no separate "kind" field.
fn event_tag(event: &RunEvent) -> String {
    format!("{event:?}")
        .split([' ', '{'])
        .next()
        .unwrap_or("?")
        .to_string()
}

// ───────────────────────────── process inspection ──────────────────────────────
/// Scan /proc for processes whose cmdline contains `marker`, as
/// (pid, ppid, cmdline). `pgrep` is not assumed to exist on the measuring box.
fn proc_find(marker: &str) -> Vec<(i32, i32, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid): Option<i32> = name.to_string_lossy().parse().ok() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let text = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        if !text.contains(marker) {
            continue;
        }
        out.push((pid, parent_of(pid), one_line(&text, 70)));
    }
    out
}

fn pid_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn parent_of(pid: i32) -> i32 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return -1;
    };
    // ppid is field 4; field 2 (comm) may contain spaces and parentheses.
    if let Some(rest) = stat.rsplit(')').next() {
        if let Some(field) = rest.split_whitespace().nth(1) {
            return field.parse().unwrap_or(-1);
        }
    }
    -1
}

fn pidfile(tag: &str) -> PathBuf {
    Path::new(MARK_DIR).join(format!("{tag}.pid"))
}

fn read_marked_pid(tag: &str) -> Option<i32> {
    std::fs::read_to_string(pidfile(tag))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn kill_pid(pid: i32) {
    // No `kill` binary is assumed on the measuring box; `sh` has it as a builtin.
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -9 {pid} 2>/dev/null"))
        .status();
}

/// Remove every child this spike's tools spawned: the ones recorded in their
/// pid files (a tool's child runs `exec sleep`, so its argv carries no marker)
/// plus anything that still carries the marker in argv (re-executed arms).
fn sweep(marker: &str) -> usize {
    let mut killed = 0;
    if let Ok(entries) = std::fs::read_dir(MARK_DIR) {
        for entry in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    if pid_alive(pid) {
                        kill_pid(pid);
                        killed += 1;
                    }
                }
            }
        }
    }
    let found = proc_find(marker);
    for (pid, _, _) in &found {
        kill_pid(*pid);
    }
    killed + found.len()
}

/// Print whether the marked child of a cancelled/dropped tool is still there.
fn report_child(tag: &str, prefix: &str) {
    match read_marked_pid(tag) {
        Some(pid) => {
            let alive = pid_alive(pid);
            let ppid = parent_of(pid);
            kv(&format!("{prefix}.child_pid"), pid);
            kv(&format!("{prefix}.child_alive"), alive);
            kv(
                &format!("{prefix}.child_ppid"),
                if alive {
                    ppid.to_string()
                } else {
                    "-".to_string()
                },
            );
            kv(
                &format!("{prefix}.child_still_ours"),
                alive && ppid == std::process::id() as i32,
            );
        }
        None => kv(&format!("{prefix}.child_pid"), "never spawned"),
    }
}

// ───────────────────────────────── the tools ───────────────────────────────────
/// The shape of a looprs-ish tool: it takes a shell command and runs it. Whether
/// it asks the SDK for permission first is the `gated` flag, and that flag is the
/// whole gating story — see the `gate` arm.
struct ShellTool {
    /// Ask the SDK to authorize the process capability before running.
    gated: bool,
    /// Command to run, ignoring the model's argument (used by probes).
    fixed: Option<String>,
    /// Record the spawned child's pid to `<MARK_DIR>/<tag>.pid` before exec.
    mark_tag: Option<String>,
    /// Panic instead of running anything.
    panic: bool,
    /// Ask the host a question through the run's host-input channel.
    ask_host: bool,
    /// What `authorize()` answered, for printing after the run.
    last_auth: Arc<Mutex<String>>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            gated: false,
            fixed: None,
            mark_tag: None,
            panic: false,
            ask_host: false,
            last_auth: Arc::new(Mutex::new("never asked".into())),
        }
    }
}

impl ShellTool {
    fn gated() -> Self {
        Self {
            gated: true,
            ..Default::default()
        }
    }

    fn child(tag: &str) -> Self {
        Self {
            gated: true,
            fixed: Some(format!("{MARK} {tag}")),
            mark_tag: Some(tag.to_string()),
            ..Default::default()
        }
    }
}

impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command with sh -c and return stdout and stderr".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        }
    }

    fn call<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        let command = self.fixed.clone().unwrap_or_else(|| {
            invocation
                .arguments()
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        });
        let gated = self.gated;
        let mark = self.mark_tag.clone();
        let panic = self.panic;
        let ask = self.ask_host;
        let auth = self.last_auth.clone();
        Box::pin(async move {
            TOOL_CALLS.fetch_add(1, Ordering::Relaxed);
            if context.cancellation().is_cancelled() {
                return Err(ToolError::cancelled());
            }
            if panic {
                panic!("LOOPRS_SPIKE_PANIC inside a rho-sdk tool call");
            }
            if ask {
                let request = HostInputRequest::questionnaire(
                    "Spike approval",
                    vec![HostQuestion::new(
                        "approve",
                        "Run this command?",
                        vec![HostChoice::new("yes", "yes"), HostChoice::new("no", "no")],
                        SelectionMode::One,
                    )
                    .map_err(|e| ToolError::new(ToolErrorKind::Execution, e.to_string()))?],
                )
                .map_err(|e| ToolError::new(ToolErrorKind::Execution, e.to_string()))?;
                let title = request.title().to_string();
                let response = context
                    .request_host_input(request)
                    .await
                    .map_err(|e| ToolError::new(ToolErrorKind::Execution, format!("{e}")))?;
                return Ok(ToolOutput::text(format!(
                    "host answered {title}: {:?}",
                    response.answers()
                )));
            }
            if gated {
                // This is the only path on which rho-sdk's policy/approval
                // machinery runs at all: a tool has to ask.
                let execution = ProcessExecution::new(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    ProcessInvocation::shell_from_path("sh", vec!["-c".into()], command.clone()),
                    ProcessEnvironment::inherit_except(Vec::<String>::new()),
                    ProcessOutputLimits::new(64 * 1024, None),
                );
                let started = Instant::now();
                let outcome = context
                    .authorize(CapabilityRequest::process(
                        execution,
                        CapabilitySource::host_tool("bash"),
                    ))
                    .await;
                let summary = match &outcome {
                    Ok(ok) => format!("{ok:?}"),
                    Err(err) => format!("denied({:?}): {}", err.kind(), err.message()),
                };
                if let Ok(mut guard) = auth.lock() {
                    *guard = format!("{summary} [auth {} ms]", ms(started.elapsed()));
                }
                if let Err(err) = outcome {
                    return Err(ToolError::new(ToolErrorKind::PolicyDenied, format!("{err}")));
                }
            }
            let pidfile = mark.map(|tag| pidfile(&tag).display().to_string());
            let script = match &pidfile {
                // Record the child's pid, then become sleep(1) so the recorded pid
                // is the child's own.
                Some(path) => format!("echo $$ > {path}; exec sleep 30"),
                None => command.clone(),
            };
            if script.trim().is_empty() {
                return Err(ToolError::new(
                    ToolErrorKind::InvalidArguments,
                    "missing command",
                ));
            }
            let spawned = tokio::task::spawn_blocking(move || {
                std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&script)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .map(|out| {
                        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                        text.push_str(&String::from_utf8_lossy(&out.stderr));
                        text.push_str(&format!(" exit={:?}", out.status.code()));
                        text
                    })
                    .unwrap_or_else(|e| format!("spawn error: {e}"))
            })
            .await
            .map_err(|e| ToolError::new(ToolErrorKind::Execution, e.to_string()))?;
            Ok(ToolOutput::text(spawned))
        })
    }
}

/// The same tool, but the child is spawned with tokio's `kill_on_drop`, which is
/// what looprs relies on for a stdio child. Included to price the work the host
/// has to do itself: the SDK does not do this for you.
struct KillOnDropTool;

impl Tool for KillOnDropTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash_kod".into(),
            description: "Run a shell command, killing the child if the call is dropped".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        }
    }

    fn call<'a>(&'a self, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let path = pidfile("kill_on_drop");
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(format!("echo $$ > {}; exec sleep 30", path.display()))
                .kill_on_drop(true)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| ToolError::new(ToolErrorKind::Execution, e.to_string()))?;
            for _ in 0..100 {
                if std::fs::read_to_string(&path).is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::select! {
                status = child.wait() => Ok(ToolOutput::text(format!("child exited {status:?}"))),
                () = context.cancellation().cancelled() => Err(ToolError::cancelled()),
            }
        })
    }
}

/// A tool that never finishes on its own: used to have a run in flight when the
/// host cancels, drops, or stops reading.
struct HangTool;

impl Tool for HangTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hang".into(),
            description: "Never returns until cancelled".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn call<'a>(&'a self, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            context.cancellation().cancelled().await;
            Err(ToolError::cancelled())
        })
    }
}

struct NopTool {
    name: &'static str,
}

impl Tool for NopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "does nothing".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn call<'a>(&'a self, _i: ToolInvocation, _c: ToolContext) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::text("noop")) })
    }
}

// ───────────────────────── the OpenAI-compatible provider ──────────────────────
// Written by hand in the spike: rho-sdk's own HTTP providers live in
// rho-providers, which is the CLI's provider table (config, credential store,
// models.dev catalog). A host that links rho-sdk either writes this adapter or
// depends on that crate; the spike writes it, because pricing an alternative
// does not mean shipping its application layer.
#[derive(Clone)]
struct SpikeHttpProvider {
    base: String,
    model: String,
    key: String,
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn perr(kind: ProviderErrorKind, message: impl Into<String>, retry: Retryability) -> ProviderError {
    ProviderError::new(kind, message, retry)
}

impl ModelProvider for SpikeHttpProvider {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity::new("spike", "openai-compatible", self.model.clone())
    }

    fn send_turn<'a>(&'a self, request: ModelRequest<'a>) -> ProviderFuture<'a> {
        Box::pin(async move {
            if request.cancellation.is_cancelled() {
                return Err(ProviderError::interrupted("cancelled"));
            }
            let mut messages = Vec::new();
            for m in request.messages {
                match m {
                    Message::System(text) => {
                        messages.push(json!({"role": "system", "content": text}))
                    }
                    Message::User(blocks) => {
                        messages.push(json!({"role": "user", "content": text_of(blocks)}))
                    }
                    Message::Assistant(blocks) => {
                        messages.push(json!({"role": "assistant", "content": text_of(blocks)}))
                    }
                    Message::ToolResult(result) => messages.push(json!({
                        "role": "tool",
                        "tool_call_id": result.id,
                        "content": if result.ok {
                            result.content.clone()
                        } else {
                            format!("tool error: {}", result.content)
                        },
                    })),
                    _ => {}
                }
            }
            let body = json!({
                "model": self.model,
                "messages": messages,
                "tools": request
                    .tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    }))
                    .collect::<Vec<_>>(),
            });
            let client = reqwest::Client::new();
            let res = client
                .post(format!("{}/chat/completions", self.base))
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    perr(
                        ProviderErrorKind::Unavailable,
                        e.to_string(),
                        Retryability::Retryable,
                    )
                })?;
            let code = res.status();
            let text = res.text().await.map_err(|e| {
                perr(
                    ProviderErrorKind::InvalidResponse,
                    e.to_string(),
                    Retryability::Retryable,
                )
            })?;
            if !code.is_success() {
                // A 4xx will not succeed on a retry, and the runtime retries
                // whatever it believes is retryable four times per model turn.
                let permanent = code.as_u16() < 500;
                return Err(perr(
                    ProviderErrorKind::Unavailable,
                    format!("http {code}: {}", one_line(&text, 120)),
                    if permanent {
                        Retryability::Permanent
                    } else {
                        Retryability::Retryable
                    },
                ));
            }
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                perr(
                    ProviderErrorKind::InvalidResponse,
                    e.to_string(),
                    Retryability::Permanent,
                )
            })?;
            let msg = v.pointer("/choices/0/message");
            let mut blocks = Vec::new();
            if let Some(content) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    blocks.push(ContentBlock::Text(content.to_string()));
                }
            }
            if let Some(calls) = msg
                .and_then(|m| m.get("tool_calls"))
                .and_then(|c| c.as_array())
            {
                for call in calls {
                    let f = call.get("function");
                    let args = f
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                        .unwrap_or_else(|| json!({}));
                    blocks.push(ContentBlock::ToolCall(ToolCall {
                        id: call
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("call_0")
                            .to_string(),
                        name: f
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: args,
                    }));
                }
            }
            if blocks.is_empty() {
                return Err(perr(
                    ProviderErrorKind::InvalidResponse,
                    format!("no content in {}", one_line(&text, 120)),
                    Retryability::Permanent,
                ));
            }
            Ok(ModelResponse::Assistant(blocks))
        })
    }
}

// ───────────────── a slow, steerable, self-recording provider ──────────────────
// `ScriptedProvider` answers instantly, which is right for its purpose and wrong
// for measuring steering and first-token behaviour: there is no window in which
// to act. This provider holds each turn open, logs every request it is given
// (message count and advertised tool names), and can claim steering the way a
// provider with native mid-turn steering would.
#[derive(Clone)]
enum FakeResult {
    Response(ModelResponse),
    Fail(ProviderErrorKind, String, Retryability),
}

#[derive(Clone)]
struct FakeTurn {
    /// Wait before emitting anything (throttles the first token).
    pre_delay: Duration,
    /// Deltas to stream, with the gap before each.
    deltas: Vec<(Duration, ModelEvent)>,
    result: FakeResult,
    /// Claim and accept steering while the turn streams, as a steerable provider
    /// would; `false` leaves it to the turn boundary, as the SDK default does.
    accepts_steering: bool,
}

impl FakeTurn {
    fn text(text: &str) -> Self {
        Self {
            pre_delay: Duration::from_millis(5),
            deltas: vec![(
                Duration::from_millis(1),
                ModelEvent::OutputDelta(text.into()),
            )],
            result: FakeResult::Response(ModelResponse::Assistant(vec![ContentBlock::Text(
                text.into(),
            )])),
            accepts_steering: false,
        }
    }

    /// `n` deltas `gap` apart, then `text`: a turn long enough to act inside.
    fn slow_text(text: &str, n: usize, gap: Duration) -> Self {
        Self {
            pre_delay: Duration::from_millis(5),
            deltas: (0..n)
                .map(|i| (gap, ModelEvent::OutputDelta(format!("w{i} "))))
                .collect(),
            result: FakeResult::Response(ModelResponse::Assistant(vec![ContentBlock::Text(
                text.into(),
            )])),
            accepts_steering: false,
        }
    }

    fn tool_call(name: &str, args: serde_json::Value) -> Self {
        Self {
            pre_delay: Duration::from_millis(5),
            deltas: vec![(
                Duration::from_millis(1),
                ModelEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some(name.into()),
                    arguments: args.to_string(),
                },
            )],
            result: FakeResult::Response(ModelResponse::Assistant(vec![ContentBlock::ToolCall(
                ToolCall {
                    id: "call-1".into(),
                    name: name.into(),
                    arguments: args,
                },
            )])),
            accepts_steering: false,
        }
    }

    fn fail(kind: ProviderErrorKind, message: &str, retry: Retryability) -> Self {
        Self {
            pre_delay: Duration::from_millis(5),
            deltas: vec![(
                Duration::from_millis(1),
                ModelEvent::OutputDelta("partial answer before the connection died".into()),
            )],
            result: FakeResult::Fail(kind, message.into(), retry),
            accepts_steering: false,
        }
    }
}

#[derive(Clone)]
struct FakeProvider {
    identity: ModelIdentity,
    turns: Arc<Mutex<VecDeque<FakeTurn>>>,
    /// One entry per request: (advertised tool names, the newest message).
    requests: Arc<Mutex<Vec<(Vec<String>, String)>>>,
}

impl FakeProvider {
    fn new(turns: impl IntoIterator<Item = FakeTurn>) -> Self {
        Self {
            identity: ModelIdentity::new("spike", "fake", "fake"),
            turns: Arc::new(Mutex::new(turns.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests_log(&self) -> Vec<(Vec<String>, String)> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelProvider for FakeProvider {
    fn identity(&self) -> ModelIdentity {
        self.identity.clone()
    }

    fn send_turn<'a>(&'a self, request: ModelRequest<'a>) -> ProviderFuture<'a> {
        Box::pin(async move { self.run(request, None).await })
    }

    fn send_turn_stream<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ProviderEventSender,
    ) -> ProviderFuture<'a> {
        Box::pin(async move { self.run(request, Some(events)).await })
    }

    fn send_turn_stream_steerable<'a>(
        &'a self,
        request: ModelRequest<'a>,
        _options: rho_sdk::provider::ModelRequestOptions,
        events: ProviderEventSender,
        steering: ProviderSteeringReceiver,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let accepts = {
                let guard = self.turns.lock().unwrap();
                guard
                    .front()
                    .map(|t| t.accepts_steering)
                    .unwrap_or(false)
            };
            if accepts {
                // A provider that can take input mid-turn claims each request and
                // accepts it; the runtime turns that into SteeringDelivered.
                tokio::spawn(async move {
                    let mut rx = steering;
                    while let Some(request) = rx.recv().await {
                        if request.claim() {
                            note(format!(
                                "fake provider accepted mid-turn steering: {}",
                                one_line(&text_of(request.content()), 60)
                            ));
                            request.accept();
                        }
                    }
                });
            } else {
                drop(steering);
            }
            self.run(request, Some(events)).await
        })
    }
}

impl FakeProvider {
    async fn run<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: Option<ProviderEventSender>,
    ) -> Result<ModelResponse, ProviderError> {
        let tools: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
        let newest = request
            .messages
            .last()
            .map(|m| one_line(&format!("{m:?}"), 160))
            .unwrap_or_default();
        self.requests.lock().unwrap().push((tools, newest));
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| FakeTurn::text("no scripted turn left"));
        tokio::select! {
            () = tokio::time::sleep(turn.pre_delay) => {}
            () = request.cancellation.cancelled() => {
                return Err(ProviderError::interrupted("cancelled"));
            }
        }
        for (gap, event) in &turn.deltas {
            tokio::select! {
                () = tokio::time::sleep(*gap) => {}
                () = request.cancellation.cancelled() => {
                    return Err(ProviderError::interrupted("cancelled"));
                }
            }
            if let Some(tx) = &events {
                tx.send(event.clone()).await?;
            }
        }
        match turn.result {
            FakeResult::Response(response) => Ok(response),
            FakeResult::Fail(kind, message, retry) => Err(perr(kind, message, retry)),
        }
    }
}

// ─────────────────────────── the per-request gate ──────────────────────────────
/// A `ToolVisibility` whose allowlist can change between model requests. This is
/// the closest thing rho-sdk has to looprs' per-pass `--tools read,bash`: it is
/// consulted before *every* request, so it can differ per request — but it is
/// installed on the runtime, not passed per call.
#[derive(Default)]
struct SharedVisibility {
    allowed: RwLock<BTreeSet<String>>,
}

impl ToolVisibility for SharedVisibility {
    fn is_advertised(&self, name: &str) -> bool {
        self.allowed.read().unwrap().contains(name)
    }
}

/// A deny-only pre-tool hook. It can only make a decision stricter, and it is
/// consulted only for a capability request a tool actually made.
struct DenyHook {
    deny: bool,
}

impl PreToolUseGate for DenyHook {
    fn evaluate(&self, _request: PreToolUseRequest) -> HookGateFuture<'_> {
        let deny = self.deny;
        Box::pin(async move {
            if deny {
                HookDecision::deny("LOOPRS_SPIKE hook denial")
            } else {
                HookDecision::Continue
            }
        })
    }
}

fn allow_processes() -> ScopedWorkspacePolicy {
    ScopedWorkspacePolicy::new().allow_processes()
}

fn scripted_simple() -> ScriptedProvider {
    ScriptedProvider::new(
        ModelIdentity::new("spike", "scripted", "scripted"),
        [
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::ToolCall(
                ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "echo LOOPRS_SPIKE_OK"}),
                },
            )])),
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::Text(
                "done".into(),
            )])),
        ],
    )
}

fn scripted_tool_call(command: &str) -> ScriptedProvider {
    ScriptedProvider::new(
        ModelIdentity::new("spike", "scripted", "scripted"),
        [
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::ToolCall(
                ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": command}),
                },
            )])),
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::Text(
                "done".into(),
            )])),
        ],
    )
}

// ───────────────────────────── run plumbing ────────────────────────────────────
#[derive(Default)]
struct Collected {
    by_kind: BTreeMap<String, usize>,
    order: Vec<(String, usize)>,
    first_delta: Option<Duration>,
    tool_results: Vec<String>,
    settle: Option<String>,
    model_ttfb: Option<Duration>,
    model_latency: Option<Duration>,
    closed: bool,
    total: usize,
}

impl Collected {
    fn observe(&mut self, event: &RunEvent, t0: Instant) {
        let tag = event_tag(event);
        *self.by_kind.entry(tag.clone()).or_default() += 1;
        self.total += 1;
        match event {
            RunEvent::AssistantTextDelta { .. } if self.first_delta.is_none() => {
                self.first_delta = Some(t0.elapsed())
            }
            RunEvent::ModelCallCompleted { metrics, .. } => {
                self.model_ttfb = metrics.time_to_first_token;
                self.model_latency = Some(metrics.total_latency);
            }
            RunEvent::ToolFinished { result, .. } => self.tool_results.push(format!("{result:?}")),
            RunEvent::Completed { .. }
            | RunEvent::Failed { .. }
            | RunEvent::Cancelled { .. } => self.settle = Some(tag.clone()),
            _ => {}
        }
        if self.order.last().map(|(t, _)| t.as_str()) == Some(tag.as_str()) {
            self.order.last_mut().unwrap().1 += 1;
        } else {
            self.order.push((tag, 1));
        }
    }

    fn order_string(&self) -> String {
        self.order
            .iter()
            .map(|(tag, n)| if *n > 1 { format!("{tag}×{n}") } else { tag.clone() })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn kinds(&self) -> String {
        self.by_kind
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

enum Action {
    Cancel,
    Steer(String),
}

struct Pump {
    /// Fire an action once at least this many events have been seen.
    actions: Vec<(usize, Action)>,
    /// Report a wedge if nothing arrives for this long.
    idle: Option<Duration>,
    /// Give up entirely.
    hard: Duration,
}

enum PumpEnd {
    Ended,
    IdleWedged,
    HardTimeout,
}

/// Drive one run's event stream the way looprs drives a child: read events in
/// order, act on a schedule, and report whether a terminal event ever arrived.
async fn pump(run: &mut Run, c: &mut Collected, t0: Instant, spec: Pump) -> PumpEnd {
    let steer = run.steering_handle();
    let cancel = run.cancellation_handle();
    let started = Instant::now();
    let mut fired: BTreeSet<usize> = BTreeSet::new();
    loop {
        let wait = spec.idle.unwrap_or(Duration::from_secs(30));
        let next = tokio::select! {
            event = run.next_event() => event,
            () = tokio::time::sleep(wait) => {
                if spec.idle.is_some() { return PumpEnd::IdleWedged; }
                continue;
            }
        };
        let Some(event) = next else {
            c.closed = true;
            return PumpEnd::Ended;
        };
        c.observe(&event, t0);
        if c.settle.is_some() {
            // Keep reading until the stream closes, so "the terminal event
            // arrived" and "the stream ended" stay separate facts.
            for _ in 0..5 {
                match tokio::time::timeout(Duration::from_millis(250), run.next_event()).await {
                    Ok(Some(more)) => c.observe(&more, t0),
                    Ok(None) => {
                        c.closed = true;
                        break;
                    }
                    Err(_) => {}
                }
            }
            return PumpEnd::Ended;
        }
        for (after, action) in &spec.actions {
            if c.total >= *after && !fired.contains(after) {
                fired.insert(*after);
                match action {
                    Action::Cancel => cancel.cancel(),
                    Action::Steer(text) => {
                        let text = text.clone();
                        if let Err(e) = steer.steer(UserInput::text(text)).await {
                            kv("probe.steer_error", one_line(&e.to_string(), 120));
                        }
                    }
                }
            }
        }
        if started.elapsed() > spec.hard {
            return PumpEnd::HardTimeout;
        }
    }
}

fn settle_line(c: &Collected, end: &PumpEnd) -> String {
    match end {
        PumpEnd::Ended => c
            .settle
            .clone()
            .unwrap_or_else(|| "NONE — stream closed with no terminal event".to_string()),
        PumpEnd::IdleWedged => "NONE — no event inside the probe's idle window (run still live)"
            .to_string(),
        PumpEnd::HardTimeout => "NONE — probe hard timeout".to_string(),
    }
}

fn default_pump() -> Pump {
    Pump {
        actions: vec![],
        idle: Some(Duration::from_secs(20)),
        hard: Duration::from_secs(40),
    }
}

async fn report_outcome(run: &mut Run, prefix: &str) -> Option<rho_sdk::RunOutcome> {
    match run.outcome().await {
        Ok(outcome) => {
            kv(&format!("{prefix}.outcome"), "Ok");
            kv(
                &format!("{prefix}.stop_reason"),
                format!("{:?}", outcome.stop_reason()),
            );
            kv(&format!("{prefix}.reply"), one_line(outcome.text(), 70));
            Some(outcome)
        }
        Err(error) => {
            kv(&format!("{prefix}.outcome"), format!("Err({error})"));
            kv(
                &format!("{prefix}.retryable"),
                if error.is_retryable() { "yes" } else { "no" },
            );
            None
        }
    }
}

async fn new_session(rho: &Rho) -> (rho_sdk::Session, Duration) {
    let t = Instant::now();
    let session = rho
        .session(SessionOptions::default())
        .await
        .expect("session");
    (session, t.elapsed())
}

fn summary(prefix: &str, c: &Collected, build_ms: Duration, session_ms: Duration, turn: Duration) {
    kv("engine.build_ms", ms(build_ms));
    kv(&format!("{prefix}.session_ms"), ms(session_ms));
    kv(&format!("{prefix}.turn_ms"), ms(turn));
    kv(
        &format!("{prefix}.first_token_ms"),
        c.first_delta.map(ms).unwrap_or_else(|| "n/a".into()),
    );
    kv(
        &format!("{prefix}.sdk_ttfb_ms"),
        c.model_ttfb.map(ms).unwrap_or_else(|| "n/a".into()),
    );
    kv(
        &format!("{prefix}.sdk_call_ms"),
        c.model_latency.map(ms).unwrap_or_else(|| "n/a".into()),
    );
    let (rss, peak) = rss_kib();
    kv(&format!("{prefix}.rss_kib"), rss);
    kv(&format!("{prefix}.rss_peak_kib"), peak);
    kv(&format!("{prefix}.events_total"), c.total);
    kv(&format!("{prefix}.events_by_kind"), c.kinds());
    kv(
        &format!("{prefix}.events_order"),
        one_line(&c.order_string(), 240),
    );
    kv(&format!("{prefix}.stream_closed"), c.closed);
    kv(&format!("{prefix}.tool_calls"), TOOL_CALLS.load(Ordering::Relaxed));
}

// ───────────────────────────── read helpers ────────────────────────────────────
/// Read events for a fixed wall-clock window (the probe needs a run in flight).
async fn read_for(run: &mut Run, c: &mut Collected, t0: Instant, dur: Duration) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), run.next_event()).await {
            Ok(Some(event)) => c.observe(&event, t0),
            Ok(None) => {
                c.closed = true;
                return;
            }
            Err(_) => {}
        }
    }
}

/// Read until a terminal event or the stream closes, bounded by `dur`.
async fn drain(run: &mut Run, c: &mut Collected, t0: Instant, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let budget = deadline.saturating_duration_since(Instant::now());
        if budget.is_zero() {
            return;
        }
        match tokio::time::timeout(budget, run.next_event()).await {
            Ok(Some(event)) => {
                c.observe(&event, t0);
                if c.settle.is_some() {
                    return;
                }
            }
            Ok(None) => {
                c.closed = true;
                return;
            }
            Err(_) => return,
        }
    }
}

// ─────────────────────────────────── arms ──────────────────────────────────────
fn base_builder(
    provider: impl ModelProvider + 'static,
    tool: impl Tool + 'static,
) -> rho_sdk::RhoBuilder {
    Rho::builder().provider(provider).tool(tool)
}

async fn arm_endpoint(headless: bool) {
    let base = env_or("RHO_SPIKE_BASE_URL", "http://llm.home:8080/v1");
    let model = env_or("RHO_SPIKE_MODEL", "halogen-qwen3.8-flash-next");
    let provider = SpikeHttpProvider {
        base: base.trim_end_matches('/').to_string(),
        model,
        key: env_or("RHO_SPIKE_KEY", "spike"),
    };
    let tool = ShellTool::gated();
    let auth = tool.last_auth.clone();
    let t = Instant::now();
    let rho = if headless {
        base_builder(provider, tool)
            .workspace_policy(allow_processes())
            .build()
    } else {
        base_builder(provider, tool).build()
    }
    .expect("build");
    let build_ms = t.elapsed();
    kv(
        "arm.policy",
        if headless {
            "ScopedWorkspacePolicy::allow_processes() — unattended, no approval handler"
        } else {
            "SDK default: DenyAllPolicy + DenyApprovals (no approval handler configured)"
        },
    );
    let (session, session_ms) = new_session(&rho).await;
    kv(
        "tools.registered",
        rho
            .diagnostics()
            .tool_names()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    );
    let t0 = Instant::now();
    let mut run = session
        .start(UserInput::text(
            "Run the bash tool with command `echo LOOPRS_SPIKE_OK`, then reply with its output.",
        ))
        .await
        .expect("start");
    let mut c = Collected::default();
    let end = pump(
        &mut run,
        &mut c,
        t0,
        Pump {
            actions: vec![],
            idle: Some(Duration::from_secs(90)),
            hard: Duration::from_secs(150),
        },
    )
    .await;
    summary("arm", &c, build_ms, session_ms, t0.elapsed());
    kv("arm.settle_event", settle_line(&c, &end));
    kv("arm.tool_authorization", auth.lock().unwrap().clone());
    note("first_token_ms is measured host-side, at the first AssistantTextDelta. The");
    note("spike's provider implements the non-streaming send_turn, so the SDK has no");
    note("delta to time and sdk_ttfb_ms is n/a; sdk_call_ms is the runtime's own");
    note("per-model-call latency from ModelCallMetrics, which is reported either way.");
    for line in c.tool_results.iter().take(3) {
        kv("arm.tool_result", one_line(line, 200));
    }
    report_outcome(&mut run, "arm").await;
    let audit: Vec<String> = rho
        .diagnostics()
        .approval_audit()
        .iter()
        .map(|r| format!("{:?}:{:?}", r.capability(), r.decision()))
        .collect();
    kv(
        "arm.approval_audit",
        if audit.is_empty() {
            "empty — the audit records denials and cancellations only; an allowed-by-policy call leaves no row".to_string()
        } else {
            audit.join(",")
        },
    );
}

async fn arm_scripted() {
    note("labelled arm: ScriptedProvider — no endpoint involved. It exercises the runtime,");
    note("the in-process tool round trip and the event stream, and nothing about the model.");
    let provider = scripted_tool_call("echo LOOPRS_SPIKE_OK");
    let recorded = provider.clone();
    let tool = ShellTool::gated();
    let auth = tool.last_auth.clone();
    let t = Instant::now();
    let rho = base_builder(provider, tool)
        .workspace_policy(allow_processes())
        .build()
        .expect("build");
    let build_ms = t.elapsed();
    let (session, session_ms) = new_session(&rho).await;
    let t0 = Instant::now();
    let mut run = session
        .start(UserInput::text("run the command"))
        .await
        .expect("start");
    let mut c = Collected::default();
    let end = pump(&mut run, &mut c, t0, default_pump()).await;
    summary("scripted", &c, build_ms, session_ms, t0.elapsed());
    kv("scripted.settle_event", settle_line(&c, &end));
    kv("scripted.tool_authorization", auth.lock().unwrap().clone());
    for line in c.tool_results.iter().take(2) {
        kv("scripted.tool_result", one_line(line, 160));
    }
    report_outcome(&mut run, "scripted").await;
    let requests = recorded.recorded_requests();
    kv("scripted.model_requests", requests.len());
    for (i, req) in requests.iter().enumerate() {
        let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        let bytes: usize = req
            .tools
            .iter()
            .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
            .sum();
        kv(
            &format!("scripted.request{}", i + 1),
            format!(
                "messages={} advertised_tools=[{}] tool_schema_bytes={}",
                req.messages.len(),
                names.join(","),
                bytes
            ),
        );
    }
    note("What the fake cannot do: it is not a process. argv, exit codes, stdin-EOF");
    note("shutdown, stderr, and a child that dies on its own — the things");
    note("LOOPRS_PI_BIN's fakes prove — have no analogue here, because there is no");
    note("child. It replaces the *tool round trip* half of the fake, not the process half.");
}

async fn arm_gate() {
    note("rho-sdk has four places a tool can be stopped, and one of them is not a gate.");
    note("Each probe below runs the same one-tool turn against the scripted provider.");

    // (1) SDK defaults, with a tool that asks.
    {
        let rho = base_builder(scripted_simple(), ShellTool::gated())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                idle: Some(Duration::from_secs(10)),
                ..default_pump()
            },
        )
        .await;
        kv("gate1.sdk_default.turn_ms", ms(t0.elapsed()));
        kv("gate1.sdk_default.settle", settle_line(&c, &end));
        for line in c.tool_results.iter().take(1) {
            kv("gate1.sdk_default.tool_result", one_line(line, 200));
        }
        report_outcome(&mut run, "gate1.sdk_default").await;
        note("DenyApprovals answers immediately: an unattended host gets a denial, not a hang.");
    }

    // (2) Same tool, allow-process policy: the unattended arm that does the work.
    {
        let tool = ShellTool::gated();
        let auth = tool.last_auth.clone();
        let rho = base_builder(scripted_simple(), tool)
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        pump(&mut run, &mut c, t0, default_pump()).await;
        kv("gate2.allow_policy.turn_ms", ms(t0.elapsed()));
        kv("gate2.allow_policy.authorization", auth.lock().unwrap().clone());
        for line in c.tool_results.iter().take(1) {
            kv("gate2.allow_policy.tool_result", one_line(line, 160));
        }
        report_outcome(&mut run, "gate2.allow_policy").await;
    }

    // (3) A tool that never asks: same defaults, and the command runs anyway.
    {
        let tool = ShellTool {
            gated: false,
            fixed: Some("echo LOOPRS_SPIKE_UNGATED_RAN".into()),
            ..Default::default()
        };
        let rho = base_builder(scripted_simple(), tool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        pump(&mut run, &mut c, t0, default_pump()).await;
        kv("gate3.ungated_tool.turn_ms", ms(t0.elapsed()));
        for line in c.tool_results.iter().take(1) {
            kv("gate3.ungated_tool.tool_result", one_line(line, 160));
        }
        kv(
            "gate3.ungated_tool.approval_audit",
            if rho.diagnostics().approval_audit().is_empty() {
                "empty — the SDK never saw the command"
            } else {
                "recorded"
            },
        );
        note("A host-provided tool that runs the command itself is outside every gate:");
        note("policy, hook and approval handler all hang off ToolContext::authorize, and");
        note("`ToolOrigin::HostProvided` says so in the source — \"SDK policy cannot");
        note("sandbox it\". looprs' own tool in this spike is exactly that shape.");
    }

    // (4) Per-request visibility on one runtime, allowlist changed mid-run.
    {
        let visibility = Arc::new(SharedVisibility {
            allowed: RwLock::new(BTreeSet::from(["bash".to_string(), "extra".to_string()])),
        });
        let provider = ScriptedProvider::new(
            ModelIdentity::new("spike", "scripted", "scripted"),
            [
                ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::ToolCall(
                    ToolCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "echo LOOPRS_SPIKE_VISIBLE"}),
                    },
                )])),
                ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::Text(
                    "done".into(),
                )])),
            ],
        );
        let recorded = provider.clone();
        let flip = visibility.clone();
        // The tool takes 300 ms, so the flip below lands between the first model
        // request and the second one, which is the per-request property on show.
        let slow_tool = ShellTool {
            gated: true,
            fixed: Some("sleep 0.3".into()),
            ..Default::default()
        };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            *flip.allowed.write().unwrap() = BTreeSet::from(["bash".to_string()]);
        });
        let rho = Rho::builder()
            .provider(provider)
            .tool(slow_tool)
            .tool(NopTool { name: "extra" })
            .tool_visibility_shared(visibility)
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        pump(&mut run, &mut c, t0, default_pump()).await;
        kv("gate4.visibility.turn_ms", ms(t0.elapsed()));
        kv(
            "gate4.visibility.registered",
            rho.diagnostics()
                .tool_names()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        );
        for (i, req) in recorded.recorded_requests().iter().enumerate() {
            let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
            let bytes: usize = req
                .tools
                .iter()
                .map(|t| serde_json::to_string(t).unwrap_or_default().len())
                .sum();
            kv(
                &format!("gate4.visibility.request{}", i + 1),
                format!("advertised=[{}] tool_schema_bytes={}", names.join(","), bytes),
            );
        }
        report_outcome(&mut run, "gate4.visibility").await;
        note("Visibility is per model request (the runtime asks before every request),");
        note("not per pass: there is no argv, and the allowlist is installed on the");
        note("runtime. A loop either mutates shared state like this, or rebuilds the");
        note("runtime per pass — priced on the next line, in microseconds.");
        let t = Instant::now();
        for _ in 0..200 {
            let _ = Rho::builder()
                .provider(scripted_simple())
                .tool(NopTool { name: "x" })
                .workspace_policy(allow_processes())
                .build()
                .expect("build");
        }
        kv(
            "gate4.rebuild_runtime_us_each",
            format!("{:.1}", t.elapsed().as_secs_f64() * 1e6 / 200.0),
        );
    }

    // (5) A deny-only pre-tool hook narrowing an allowed capability.
    {
        let tool = ShellTool::gated();
        let auth = tool.last_auth.clone();
        let rho = Rho::builder()
            .provider(scripted_simple())
            .tool(tool)
            .workspace_policy(allow_processes())
            .pre_tool_gate_shared(Arc::new(DenyHook { deny: true }))
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        pump(&mut run, &mut c, t0, default_pump()).await;
        kv("gate5.hook.turn_ms", ms(t0.elapsed()));
        kv("gate5.hook.authorization", auth.lock().unwrap().clone());
        kv(
            "gate5.hook.audit",
            rho.diagnostics()
                .approval_audit()
                .iter()
                .map(|r| format!("{:?}:{:?}", r.capability(), r.decision()))
                .collect::<Vec<_>>()
                .join(","),
        );
    }

    // (6) The SDK's own approval channel with its receiving half dropped.
    {
        let tool = ShellTool::gated();
        let auth = tool.last_auth.clone();
        let (handler, receiver) = approval_channel(std::num::NonZeroUsize::new(4).unwrap());
        drop(receiver); // the channel the host would read from is closed unread
        let rho = Rho::builder()
            .provider(scripted_simple())
            .tool(tool)
            .workspace_policy(
                ScopedWorkspacePolicy::new()
                    .allow_processes()
                    .require_process_approval(),
            )
            .approval_handler(handler)
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                idle: Some(Duration::from_millis(1500)),
                ..default_pump()
            },
        )
        .await;
        kv("gate6.approval_channel_closed.settle", settle_line(&c, &end));
        kv(
            "gate6.approval_channel_closed.ms",
            ms(t0.elapsed()),
        );
        kv("gate6.approval_channel_closed.authorization", auth.lock().unwrap().clone());
        report_outcome(&mut run, "gate6.approval_channel_closed").await;
        note("The shipped ChannelApprovalHandler fails closed: a responder that is gone");
        note("is a Deny, not a hang — the same fail-closed shape as DenyApprovals. The");
        note("hang is what a *live* handler waiting on a person does, which is next.");
    }
}

/// An approval handler that waits for a person who is not there. This is what
/// looprs' situation actually is: the host is unattended, so nothing answers.
struct NeverAnswers;

impl ApprovalHandler for NeverAnswers {
    fn request<'a>(
        &'a self,
        _request: ApprovalRequest,
    ) -> rho_sdk::ApprovalFuture<'a> {
        Box::pin(async { std::future::pending::<()>().await; unreachable!() })
    }
}

async fn arm_gate_unanswered() {
    let tool = ShellTool::gated();
    let rho = Rho::builder()
        .provider(scripted_simple())
        .tool(tool)
        .workspace_policy(
            ScopedWorkspacePolicy::new()
                .allow_processes()
                .require_process_approval(),
        )
        .approval_handler(NeverAnswers)
        .build()
        .expect("build");
    let (session, _) = new_session(&rho).await;
    let t0 = Instant::now();
    let mut run = session.start(UserInput::text("run it")).await.unwrap();
    let mut c = Collected::default();
    let end = pump(
        &mut run,
        &mut c,
        t0,
        Pump {
            idle: Some(Duration::from_millis(2000)),
            ..default_pump()
        },
    )
    .await;
    kv("gate7.human_never_arrives.settle", settle_line(&c, &end));
    kv(
        "gate7.human_never_arrives.session_state",
        format!("{:?}", session.state()),
    );
    kv(
        "gate7.human_never_arrives.ms_before_outside_cancel",
        ms(t0.elapsed()),
    );
    run.cancel();
    let mut after = Collected::default();
    drain(&mut run, &mut after, t0, Duration::from_secs(2)).await;
    kv(
        "gate7.human_never_arrives.after_outside_cancel",
        format!(
            "events=[{}] settle={}",
            after.kinds(),
            settle_line(&after, &PumpEnd::Ended)
        ),
    );
    report_outcome(&mut run, "gate7.human_never_arrives").await;
    note("A live approval handler that nobody answers holds the run open indefinitely.");
    note("Only the host can end it, by cancelling from outside the run; a stdio child");
    note("dying is a fact you can observe, and an in-process run waiting on your own");
    note("handler is not, unless you put the deadline there yourself.");
}

async fn arm_settle() {
    note("The question this arm exists for: is there an event a loop can key its advance");
    note("on, in every case where the thing driving the turn stops working.");

    // (1) Happy path.
    {
        let rho = base_builder(scripted_simple(), ShellTool::gated())
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("run it")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                idle: Some(Duration::from_secs(5)),
                ..default_pump()
            },
        )
        .await;
        kv("settle1.completed.settle_event", settle_line(&c, &end));
        kv("settle1.completed.stream_closed", c.closed);
        kv("settle1.completed.turn_ms", ms(t0.elapsed()));
        kv("settle1.completed.events", one_line(&c.order_string(), 200));
        report_outcome(&mut run, "settle1.completed").await;
        kv(
            "settle1.completed.session_state",
            format!("{:?}", session.state()),
        );
    }

    // (2) The provider dies permanently after streaming a partial answer.
    {
        let provider = FakeProvider::new([FakeTurn::fail(
            ProviderErrorKind::Unavailable,
            "connection reset by peer",
            Retryability::Permanent,
        )]);
        let rho = base_builder(provider, HangTool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                idle: Some(Duration::from_secs(5)),
                ..default_pump()
            },
        )
        .await;
        kv("settle2.permanent_failure.settle_event", settle_line(&c, &end));
        kv("settle2.permanent_failure.events", one_line(&c.order_string(), 200));
        kv("settle2.permanent_failure.turn_ms", ms(t0.elapsed()));
        report_outcome(&mut run, "settle2.permanent_failure").await;
        kv(
            "settle2.permanent_failure.session_state",
            format!("{:?}", session.state()),
        );
        kv(
            "settle2.permanent_failure.history_kinds",
            session
                .history()
                .iter()
                .map(|m| match m {
                    Message::System(_) => "System",
                    Message::User(_) => "User",
                    Message::Assistant(_) => "Assistant",
                    Message::ToolResult(_) => "ToolResult",
                    _ => "aborted-or-other",
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        note("A dead provider is a Failed event *and* an Err from outcome(); the partial");
        note("answer is committed as AbortedAssistant history, not thrown away.");
    }

    // (3) Retryable provider failure, then recovery.
    {
        let provider = FakeProvider::new([
            FakeTurn::fail(ProviderErrorKind::Timeout, "stream stalled", Retryability::Retryable),
            FakeTurn::text("recovered"),
        ]);
        let rho = base_builder(provider, HangTool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                idle: Some(Duration::from_secs(10)),
                ..default_pump()
            },
        )
        .await;
        kv("settle3.retry_then_ok.settle_event", settle_line(&c, &end));
        kv("settle3.retry_then_ok.events", one_line(&c.order_string(), 200));
        kv("settle3.retry_then_ok.turn_ms", ms(t0.elapsed()));
        report_outcome(&mut run, "settle3.retry_then_ok").await;
        note("The retry is observable (ProviderStreamReset) and costs the SDK's own");
        note("backoff — 1s, then 2s, then 4s — before the run fails for good.");
    }

    // (4) A real dead socket, through the real HTTP provider (not a scripted answer).
    {
        let provider = SpikeHttpProvider {
            base: "http://127.0.0.1:9/v1".into(),
            model: "nobody-listening".into(),
            key: "spike".into(),
        };
        let rho = base_builder(provider, HangTool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                idle: Some(Duration::from_secs(25)),
                hard: Duration::from_secs(60),
                actions: vec![],
            },
        )
        .await;
        kv("settle4.dead_socket.settle_event", settle_line(&c, &end));
        kv("settle4.dead_socket.events", one_line(&c.order_string(), 200));
        kv("settle4.dead_socket.turn_ms", ms(t0.elapsed()));
        report_outcome(&mut run, "settle4.dead_socket").await;
    }

    // (5) A dropped Run: what the runtime actually does, and what it does not.
    {
        // The docs claim a dropped Run cancels *and* aborts. Aborting a tokio task
        // is not cancelling it: the tool future is dropped at its next await, so a
        // tool that would have noticed cancellation never gets polled again. Five
        // trials, because abort-vs-cancel is a race the docs do not settle.
        let mut saw_cancel = 0;
        let trials = 5;
        for _ in 0..trials {
            let provider = FakeProvider::new([FakeTurn::tool_call("hang", json!({}))]);
            let rho = base_builder(provider, HangTool).build().expect("build");
            let (session, _) = new_session(&rho).await;
            let t0 = Instant::now();
            let mut run = session.start(UserInput::text("go")).await.unwrap();
            let mut c = Collected::default();
            read_for(&mut run, &mut c, t0, Duration::from_millis(300)).await;
            drop(run);
            tokio::time::sleep(Duration::from_millis(150)).await;
            if c.settle.is_none() {
                // nothing was observed before the drop, which is the point
            }
            if c.settle.is_some() {
                saw_cancel += 1;
            }
        }
        kv("settle5.dropped_run.trials", trials);
        kv("settle5.dropped_run.terminal_events_observed", saw_cancel);
        note("A dropped Run produces no event anyone can read: the reader is the thing");
        note("that was dropped. looprs' `child died mid-run` case has no in-process");
        note("analogue because there is no process to die — see the blast arm for the");
        note("children that survive the equivalent of a child dying.");
    }

    // (5b) Cooperative cancel with nothing else in flight: what does the stream say?
    {
        let provider = FakeProvider::new([FakeTurn::tool_call("hang", json!({}))]);
        let rho = base_builder(provider, HangTool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                actions: vec![(3, Action::Cancel)],
                idle: Some(Duration::from_secs(5)),
                ..default_pump()
            },
        )
        .await;
        kv("settle5b.cancel_mid_tool.settle", settle_line(&c, &end));
        kv("settle5b.cancel_mid_tool.events", one_line(&c.order_string(), 200));
        kv("settle5b.cancel_mid_tool.turn_ms", ms(t0.elapsed()));
        report_outcome(&mut run, "settle5b.cancel_mid_tool").await;
        kv(
            "settle5b.cancel_mid_tool.session_state",
            format!("{:?}", session.state()),
        );
        note("Cancel is cooperative: the run advances to Cancelled only when the tool");
        note("future yields, so a tool that blocks without polling the token holds the");
        note("run open — with no thread to interrupt and no process to kill.");
    }

    // (6) A host that stops reading: backpressure, and no watchdog.
    {
        let mut turn = FakeTurn::slow_text("eventually done", 200, Duration::from_millis(5));
        turn.deltas = (0..200)
            .map(|_| (Duration::from_millis(5), ModelEvent::OutputDelta("x".into())))
            .collect();
        let provider = FakeProvider::new([turn]);
        let rho = base_builder(provider, HangTool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        read_for(&mut run, &mut c, t0, Duration::from_millis(100)).await;
        let read_so_far = c.total;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let state_while_stalled = session.state();
        let after_idle = c.total;
        kv("settle6.stop_reading.events_read_before_idle", read_so_far);
        kv("settle6.stop_reading.events_arrived_during_idle", after_idle - read_so_far);
        kv("settle6.stop_reading.session_state_while_stalled", format!("{state_while_stalled:?}"));
                let mut after = Collected::default();
        drain(&mut run, &mut after, t0, Duration::from_secs(5)).await;
        kv("settle6.stop_reading.after_resuming_reads", one_line(&after.kinds(), 200));
        kv("settle6.stop_reading.settle", settle_line(&after, &PumpEnd::Ended));
        report_outcome(&mut run, "settle6.stop_reading").await;
        note("The event channel is bounded (64), so a host that stops reading wedges the");
        note("run silently: no error, no timeout, no terminal event, and the session");
        note("still reports Running. A stdio child that stopped draining is observable");
        note("as a process; a wedged in-process run is not.");
    }

    // (7) Two runs on one session.
    {
        let provider = FakeProvider::new([FakeTurn::tool_call("hang", json!({}))]);
        let rho = base_builder(provider, HangTool).build().expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let second = session.start(UserInput::text("second")).await;
        kv(
            "settle7.second_run_on_busy_session",
            match second {
                Ok(_) => "accepted".to_string(),
                Err(e) => format!("refused: {e}"),
            },
        );
        run.cancel();
        let mut c = Collected::default();
        drain(&mut run, &mut c, t0, Duration::from_secs(2)).await;
    }
}

async fn arm_steering() {
    note("looprs sends four RPC commands: prompt, steer, clear_queue, abort. The mapping");
    note("onto rho-sdk is prompt→Session::start, steer→Run::steer, abort→Run::cancel.");
    note("clear_queue is the awkward one — shown in probe 3.");

    // (1) Steer a live turn, with a provider that accepts mid-turn steering.
    {
        let mut t1 = FakeTurn::tool_call("bash", json!({"command": "echo SPIKE_TARGET"}));
        t1.accepts_steering = true;
        t1.deltas = (0..12)
            .map(|_| (Duration::from_millis(30), ModelEvent::OutputDelta("tok ".into())))
            .collect();
        let provider = FakeProvider::new([t1, FakeTurn::text("done")]);
        let seen = provider.clone();
        let rho = base_builder(provider, ShellTool { gated: false, ..Default::default() })
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                actions: vec![(4, Action::Steer("SPIKE_STEER_MARK do the other thing".into()))],
                idle: Some(Duration::from_secs(10)),
                ..default_pump()
            },
        )
        .await;
        for (i, (tools, newest)) in seen.requests_log().into_iter().enumerate() {
            kv(
                &format!("steer1.accepting_provider.provider_request{}", i + 1),
                format!("advertised=[{}] newest_message={}", tools.join(","), newest),
            );
        }
        kv("steer1.accepting_provider.settle", settle_line(&c, &end));
        kv(
            "steer1.accepting_provider.events",
            one_line(&c.order_string(), 220),
        );
        let history = serde_json::to_string(&session.history()).unwrap_or_default();
        kv(
            "steer1.accepting_provider.steer_in_history",
            history.contains("SPIKE_STEER_MARK"),
        );
        kv("steer1.accepting_provider.model_requests", c.by_kind.get("StepStarted").copied().unwrap_or(0));
        report_outcome(&mut run, "steer1.accepting_provider").await;
    }

    // (2) Same steer against a provider that does not implement mid-turn steering.
    {
        let mut t1 = FakeTurn::tool_call("bash", json!({"command": "echo SPIKE_TARGET"}));
        t1.accepts_steering = false;
        t1.deltas = (0..12)
            .map(|_| (Duration::from_millis(30), ModelEvent::OutputDelta("tok ".into())))
            .collect();
        let provider = FakeProvider::new([t1, FakeTurn::text("done")]);
        let rho = base_builder(provider, ShellTool { gated: false, ..Default::default() })
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        let end = pump(
            &mut run,
            &mut c,
            t0,
            Pump {
                actions: vec![(4, Action::Steer("SPIKE_STEER_MARK do the other thing".into()))],
                idle: Some(Duration::from_secs(10)),
                ..default_pump()
            },
        )
        .await;
        kv("steer2.default_provider.settle", settle_line(&c, &end));
        kv(
            "steer2.default_provider.events",
            one_line(&c.order_string(), 220),
        );
        let history = serde_json::to_string(&session.history()).unwrap_or_default();
        kv(
            "steer2.default_provider.steer_in_history",
            history.contains("SPIKE_STEER_MARK"),
        );
        report_outcome(&mut run, "steer2.default_provider").await;
        note("SteeringDelivered is the provider saying it took the input inside the");
        note("current turn; SteeringApplied is the SDK putting it into history at the");
        note("turn boundary. rho-sdk's own default provider path drops the former, so a");
        note("host can only rely on the boundary — the same guarantee pi's steer gives.");
    }

    // (3) clear_queue: rho-sdk has no bulk clear, only per-id retraction, and only
    //     before the input reaches history.
    {
        let provider = FakeProvider::new([
            FakeTurn::tool_call("bash", json!({"command": "sleep 0.8"})),
            FakeTurn::text("done"),
        ]);
        let rho = base_builder(provider, ShellTool { gated: false, ..Default::default() })
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let handle = run.steering_handle();
        let mut staged = Vec::new();
        for i in 0..3 {
            match handle.steer_retractable(UserInput::text(format!("SPIKE_QUEUE_{i}"))).await {
                Ok(id) => staged.push(id),
                Err(e) => kv("steer3.stage_error", one_line(&e.to_string(), 120)),
            }
        }
        let mut retracted = Vec::new();
        for id in &staged {
            match handle.retract_steering(id.clone()).await {
                Ok(outcome) => retracted.push(format!("{outcome:?}")),
                Err(e) => retracted.push(format!("Err({e})")),
            }
        }
        kv(
            "steer3.clear_queue_analogue.retraction_outcomes",
            retracted.join(","),
        );
        let mut c = Collected::default();
        drain(&mut run, &mut c, t0, Duration::from_secs(10)).await;
        let history = serde_json::to_string(&session.history()).unwrap_or_default();
        kv(
            "steer3.clear_queue_analogue.steer_still_in_history",
            history.contains("SPIKE_QUEUE_"),
        );
        kv("steer3.clear_queue_analogue.events", one_line(&c.order_string(), 220));
        report_outcome(&mut run, "steer3.clear_queue_analogue").await;
        note("pi's clear_queue empties a queue the harness owns. Here the host must keep");
        note("every SteeringId it staged and retract each one, and a retraction that");
        note("loses the race reports AlreadyApplied — there is no state where queued");
        note("input is guaranteed discardable, because the loop may apply it at any step.");
    }

    // (4) The host-input round trip (the interactive path: a question back to the host).
    {
        let tool = ShellTool {
            ask_host: true,
            ..Default::default()
        };
        let rho = base_builder(scripted_tool_call("ignored"), tool)
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("ask")).await.unwrap();
        let mut c = Collected::default();
        let mut answered = false;
        loop {
            let budget = Duration::from_millis(500);
            match tokio::time::timeout(budget, run.next_event()).await {
                Ok(Some(event)) => {
                    if let RunEvent::ToolHostInputRequested { request, .. } = &event {
                        let id = request.id().clone();
                        let response =
                            rho_sdk::HostInputResponse::new().answer("approve", vec!["yes"]);
                        match run.respond(id, response).await {
                            Ok(()) => {
                                answered = true;
                                kv("steer4.host_input.answered_from", "ToolHostInputRequested")
                            }
                            Err(e) => kv("steer4.host_input.respond_error", e),
                        }
                    }
                    c.observe(&event, t0);
                    if c.settle.is_some() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    if Instant::now().duration_since(t0) > Duration::from_secs(10) {
                        break;
                    }
                }
            }
        }
        kv("steer4.host_input.answered", answered);
        kv("steer4.host_input.events", one_line(&c.order_string(), 220));
        for line in c.tool_results.iter().take(1) {
            kv("steer4.host_input.tool_result", one_line(line, 200));
        }
        report_outcome(&mut run, "steer4.host_input").await;
    }
}

async fn arm_blast() {
    note("looprs' blast wall today is a process: kill_on_drop reaps the child, and a");
    note("panic in pi cannot take the terminal with it. In process, none of that is");
    note("automatic — these probes measure what the host has to build itself.");
    let _ = std::fs::create_dir_all(MARK_DIR);
    sweep(MARK);

    // (1) Cooperative cancel while a tool holds an OS child.
    {
        let tag = "cancel";
        let _ = std::fs::remove_file(pidfile(tag));
        let provider = FakeProvider::new([FakeTurn::tool_call("bash", json!({}))]);
        let rho = base_builder(provider, ShellTool::child(tag))
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        read_for(&mut run, &mut c, t0, Duration::from_millis(600)).await;
        run.cancel();
        drain(&mut run, &mut c, t0, Duration::from_secs(3)).await;
        kv("blast1.cancel.settle", settle_line(&c, &PumpEnd::Ended));
        kv("blast1.cancel.turn_ms", ms(t0.elapsed()));
        report_outcome(&mut run, "blast1.cancel").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        report_child(tag, "blast1.cancel");
    }

    // (2) The same, but the tool spawns with kill_on_drop: what looprs does today.
    {
        let _ = std::fs::remove_file(pidfile("kill_on_drop"));
        let provider = FakeProvider::new([FakeTurn::tool_call("bash_kod", json!({}))]);
        let rho = base_builder(provider, KillOnDropTool)
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        read_for(&mut run, &mut c, t0, Duration::from_millis(600)).await;
        run.cancel();
        drain(&mut run, &mut c, t0, Duration::from_secs(3)).await;
        kv("blast2.kill_on_drop.settle", settle_line(&c, &PumpEnd::Ended));
        report_outcome(&mut run, "blast2.kill_on_drop").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        report_child("kill_on_drop", "blast2.kill_on_drop");
        note("blast2 is the argument looprs has to make against itself: killing the");
        note("child is easy when you wrote the spawn. In process, every tool author has");
        note("to remember it, and the SDK has no view of what a host tool spawned.");
    }

    // (3) Drop the Run with a child in flight (the docs' "drop cancels and aborts").
    {
        let tag = "dropped";
        let _ = std::fs::remove_file(pidfile(tag));
        let provider = FakeProvider::new([FakeTurn::tool_call("bash", json!({}))]);
        let rho = base_builder(provider, ShellTool::child(tag))
            .workspace_policy(allow_processes())
            .build()
            .expect("build");
        let (session, _) = new_session(&rho).await;
        let t0 = Instant::now();
        let mut run = session.start(UserInput::text("go")).await.unwrap();
        let mut c = Collected::default();
        read_for(&mut run, &mut c, t0, Duration::from_millis(600)).await;
        drop(run);
        tokio::time::sleep(Duration::from_millis(400)).await;
        kv("blast3.drop_run.events_seen", c.total);
        kv("blast3.drop_run.session_state_after_drop", format!("{:?}", session.state()));
        report_child(tag, "blast3.drop_run");
        note("The Run's Drop cancels the token and aborts the worker task. Aborting is");
        note("not awaiting: the tool future is dropped where it stands, so a blocking");
        note("spawn is not unwound and its child outlives the run.");
    }

    // (4) SIGKILL the host process with a tool child in flight.
    {
        let tag = "holder";
        let _ = std::fs::remove_file(pidfile(tag));
        let exe = std::env::current_exe().expect("exe");
        let child = std::process::Command::new(&exe)
            .env("RHO_SPIKE_ARM", "__child_holder")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn holder");
        let holder_pid = child.id();
        let mut holder = child;
        let mut waited = 0;
        let tool_child = loop {
            if let Some(pid) = read_marked_pid(tag) {
                break Some(pid);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 50;
            if waited > 5000 {
                break None;
            }
        };
        kv("blast4.sigkill_holder.holder_pid", holder_pid);
        kv("blast4.sigkill_holder.tool_child_pid", format!("{tool_child:?}"));
        // `Child::kill` is SIGKILL — the same thing a supervisor with no
        // kill_on_drop would do to the process holding the engine.
        let _ = holder.kill();
        // Reap the holder so its child is genuinely reparented rather than parked
        // behind a zombie parent, which is what a supervisor would leave behind.
        let _ = holder.wait();
        tokio::time::sleep(Duration::from_millis(400)).await;
        kv(
            "blast4.sigkill_holder.holder_reaped",
            !pid_alive(holder_pid as i32),
        );
        if let Some(pid) = tool_child {
            let alive = pid_alive(pid);
            kv("blast4.sigkill_holder.child_alive_after_host_kill", alive);
            kv(
                "blast4.sigkill_holder.child_ppid_after_host_kill",
                if alive { parent_of(pid) } else { -1 },
            );
            kv(
                "blast4.sigkill_holder.child_reparented_to_init",
                alive && parent_of(pid) != holder_pid as i32,
            );
            if alive {
                kill_pid(pid);
            }
        }
        note("Nothing reaps a tool's children when the engine dies inside the host");
        note("process: there is no kill_on_drop on a process looprs did not spawn.");
        note("ADR-0006 makes leaving the terminal clean possible; it cannot help with a");
        note("panic or a kill in looprs' own process.");
    }

    // (5) A panic inside a tool.
    {
        let exe = std::env::current_exe().expect("exe");
        let out = std::process::Command::new(&exe)
            .env("RHO_SPIKE_ARM", "__panicking_tool")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run panicking arm");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        kv(
            "blast5.tool_panic.stderr_bytes",
            format!("{}", out.stderr.len()),
        );
        kv(
            "blast5.tool_panic.written_to_terminal",
            one_line(
                stderr
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("(nothing)"),
                160,
            ),
        );
        kv(
            "blast5.tool_panic.exit_code",
            out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
        );
        kv(
            "blast5.tool_panic.host_survived",
            stdout.contains("host_survived"),
        );
        for line in stdout.lines().filter(|l| l.starts_with("inner.")) {
            kv("blast5.tool_panic.inner", one_line(line, 200));
        }
        note("The panic is contained by the task the runtime spawned on, so the host");
        note("process lives — but the panic text goes to this process' stderr, which in");
        note("looprs is the user's terminal, mid-frame. That is the ADR-0006 hazard the");
        note("process boundary was also buying against.");
    }

    kv("blast.probe_children_swept", sweep(MARK));
}

/// What `blast` (4) re-executes this binary as: a run whose tool holds a child.
async fn arm_child_holder() {
    let _ = std::fs::create_dir_all(MARK_DIR);
    let provider = FakeProvider::new([FakeTurn::tool_call("bash", json!({}))]);
    let rho = base_builder(provider, ShellTool::child("holder"))
        .workspace_policy(allow_processes())
        .build()
        .expect("build");
    let (session, _) = new_session(&rho).await;
    let _run = session.start(UserInput::text("go")).await.unwrap();
    // Hold the run open until the harness kills this process. Bounded so a run
    // whose parent forgot to reap it cannot leak a process forever.
    kv("holder.holding", "yes");
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    kv("holder.exited_on_its_own", "the harness never killed us");
}

/// What `blast` (5) re-executes this binary as: a tool that panics.
async fn arm_panicking_tool() {
    let provider = scripted_tool_call("whatever");
    let tool = ShellTool {
        panic: true,
        ..Default::default()
    };
    let rho = base_builder(provider, tool).build().expect("build");
    let (session, _) = new_session(&rho).await;
    let t0 = Instant::now();
    let mut run = session.start(UserInput::text("go")).await.unwrap();
    let mut c = Collected::default();
    let end = pump(
        &mut run,
        &mut c,
        t0,
        Pump {
            idle: Some(Duration::from_secs(5)),
            ..default_pump()
        },
    )
    .await;
    kv("inner.settle", settle_line(&c, &end));
    kv("inner.events", one_line(&c.order_string(), 200));
    report_outcome(&mut run, "inner").await;
    kv("host_survived", "yes");
}

// ─────────────────────────────── persistence ───────────────────────────────────
fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                out.push(rel.display().to_string());
            }
            if out.len() > 2000 {
                return out;
            }
        }
    }
    out.sort();
    out
}

async fn arm_persist() {
    let dir = std::env::temp_dir().join(format!("rho-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let home = std::env::var("HOME").unwrap_or_default();
    let dot_rho = PathBuf::from(&home).join(".rho");
    let rho_home_existed_before = dot_rho.exists();
    let before = walk(&dir);
    let env_home_before = std::env::var("RHO_HOME").ok();

    let provider = ScriptedProvider::new(
        ModelIdentity::new("spike", "scripted", "scripted"),
        [
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::ToolCall(
                ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "echo LOOPRS_SPIKE_PERSIST"}),
                },
            )])),
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::Text(
                "first answer".into(),
            )])),
            ScriptedTurn::completed(ModelResponse::Assistant(vec![ContentBlock::Text(
                "restored answer".into(),
            )])),
        ],
    );
    let recorded = provider.clone();
    let rho = base_builder(provider, ShellTool::gated())
        .workspace_policy(allow_processes())
        .build()
        .expect("build");
    let (session, _) = new_session(&rho).await;
    let store = Arc::new(InMemorySessionStore::new());
    let _ = session.set_checkpoint_store(Some(store.clone()));
    let t0 = Instant::now();
    let mut run = session.start(UserInput::text("go")).await.unwrap();
    let mut c = Collected::default();
    pump(
        &mut run,
        &mut c,
        t0,
        Pump {
            idle: Some(Duration::from_secs(5)),
            ..default_pump()
        },
    )
    .await;
    drop(run);
    kv("persist.turn_ms", ms(t0.elapsed()));
    let snapshot = session.snapshot();
    let json = serde_json::to_string(&snapshot).unwrap_or_default();
    let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|v| {
            v.as_object().map(|map| {
                map.keys().cloned().collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    kv("persist.sdk_writes_files_by_default", "no path exists in the SDK to write to");
    kv(
        "persist.schema_version",
        rho_sdk::SESSION_SNAPSHOT_SCHEMA_VERSION,
    );
    kv(
        "persist.schema_version_oldest_accepted",
        rho_sdk::MIN_SESSION_SNAPSHOT_SCHEMA_VERSION,
    );
    kv("persist.snapshot_bytes", json.len());
    kv("persist.snapshot_top_level_keys", keys.join(","));
    kv("persist.snapshot_history_messages", session.history().len());
    kv("persist.snapshot_mentions_reasoning", json.contains("reasoning"));
    kv("persist.snapshot_mentions_credentials", json.to_lowercase().contains("api_key"));
    kv(
        "persist.checkpoint_store_saved",
        store.load(session.id()).is_some(),
    );
    kv("persist.checkpoint_store_is", "host-supplied trait impl; the in-memory one is in-process only");

    // Restore: the snapshot is the only thing a host has to persist.
    let restored = rho
        .session(SessionOptions::from_snapshot(snapshot.clone()))
        .await
        .expect("restore");
    let t1 = Instant::now();
    let mut run2 = restored.start(UserInput::text("again")).await.unwrap();
    let mut c2 = Collected::default();
    pump(
        &mut run2,
        &mut c2,
        t1,
        Pump {
            idle: Some(Duration::from_secs(5)),
            ..default_pump()
        },
    )
    .await;
    drop(run2);
    let requests = recorded.recorded_requests();
    kv(
        "persist.restored_first_request_messages",
        requests
            .last()
            .map(|r| r.messages.len())
            .unwrap_or(0),
    );
    kv("persist.restored_reply", one_line(&c2.kinds(), 120));
    kv("persist.restored_history_messages", restored.history().len());

    let after = walk(&dir);
    let mut new_files: Vec<String> = after
        .iter()
        .filter(|f| !before.contains(f))
        .cloned()
        .collect();
    new_files.sort();
    kv("persist.files_created_in_scratch_dir", new_files.len());
    for name in new_files.iter().take(5) {
        kv("persist.file_created", name);
    }
    kv(
        "persist.home_dot_rho_created",
        dot_rho.exists() && !rho_home_existed_before,
    );
    kv(
        "persist.rho_home_env_touched",
        format!("{:?}", env_home_before),
    );
    note("rho-sdk writes nothing: persistence is a value (`SessionSnapshot`, versioned");
    note("JSON) plus a host-implemented `SessionStore` trait, and `InMemorySessionStore`");
    note("is the only shipped impl. There is no session file, so there is nothing to opt");
    note("out of — which is the opposite of pi's session-file behaviour looprs' corpus");
    note("measurements currently depend on.");
    let _ = std::fs::remove_dir_all(&dir);
}

// ─────────────────────────────────── main ──────────────────────────────────────
#[tokio::main]
async fn main() {
    let arm = env_or("RHO_SPIKE_ARM", "endpoint-headless");
    kv(
        "crate.linked",
        format!(
            "rho-sdk\t{}",
            env_or("RHO_SPIKE_CRATE_VERSION", "unpinned")
        ),
    );
    kv("host.pid", std::process::id());
    let (rss, _) = rss_kib();
    kv("host.rss_before_kib", rss);
    match arm.as_str() {
        "endpoint-headless" => {
            kv("arm.label", "openai-compatible endpoint, unattended (allow-process policy)");
            arm_endpoint(true).await;
        }
        "endpoint-default" => {
            kv("arm.label", "openai-compatible endpoint, SDK default gating");
            arm_endpoint(false).await;
        }
        "scripted" => {
            kv("arm.label", "scripted provider — the fake-provider seam (no endpoint)");
            arm_scripted().await;
        }
        "gate" => {
            kv("arm.label", "tool gating probes — scripted provider (no endpoint)");
            arm_gate().await;
            arm_gate_unanswered().await;
        }
        "settle" => {
            kv("arm.label", "end-of-run signal probes — scripted + dead socket (no endpoint)");
            arm_settle().await;
        }
        "steering" => {
            kv("arm.label", "steering / clear_queue / host-input probes (no endpoint)");
            arm_steering().await;
        }
        "blast" => {
            kv("arm.label", "cancellation and blast-wall probes (no endpoint)");
            arm_blast().await;
        }
        "persist" => {
            kv("arm.label", "persistence probes (no endpoint)");
            arm_persist().await;
        }
        "__child_holder" => arm_child_holder().await,
        "__panicking_tool" => arm_panicking_tool().await,
        other => kv("arm.unknown", other),
    }
    let (_, peak) = rss_kib();
    kv("host.rss_peak_kib", peak);
}
