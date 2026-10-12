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
//! Arms, selected with RHO_SPIKE_ARM:
//!   (unset)    the OpenAI-compatible endpoint, driven through a hand-written
//!              `ModelProvider` — rho-sdk's own HTTP providers live in
//!              rho-providers, the CLI's provider table, which a library
//!              consumer does not have to link.
//!   scripted   the fake-provider seam (`ScriptedProvider`/`ScriptedTurn`): no
//!              network, and it still exercises the in-process tool round trip
//!              and the event stream, including which event ends a run.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rho_sdk::model::{
    ContentBlock, Message, ModelIdentity, ModelRequest, ModelResponse, ToolCall, ToolSpec,
};
use rho_sdk::provider::{ModelProvider, ProviderFuture, ScriptedProvider, ScriptedTurn};
use rho_sdk::tool::{
    Tool, ToolContext, ToolError, ToolErrorKind, ToolFuture, ToolInvocation, ToolOutput,
};
use rho_sdk::{
    ProviderError, ProviderErrorKind, Retryability, Rho, RunEvent, RunOutcome, SessionOptions,
    UserInput,
};
use serde_json::json;

static TOOL_CALLS: AtomicUsize = AtomicUsize::new(0);
static MODEL_REQUESTS: AtomicUsize = AtomicUsize::new(0);

fn perr(kind: ProviderErrorKind, message: impl Into<String>, retry: Retryability) -> ProviderError {
    ProviderError::new(kind, message, retry)
}

fn rss_kib() -> u64 {
    for line in std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

// ────────────────────────────── the tool arm ─────────────────────────────────
#[derive(Debug)]
struct BashTool;

impl Tool for BashTool {
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
        let command = invocation
            .arguments()
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Box::pin(async move {
            if context.cancellation().is_cancelled() {
                return Err(ToolError::cancelled());
            }
            if command.is_empty() {
                return Err(ToolError::new(ToolErrorKind::InvalidArguments, "missing command"));
            }
            TOOL_CALLS.fetch_add(1, Ordering::Relaxed);
            let spawned = tokio::task::spawn_blocking(move || {
                std::process::Command::new("sh").arg("-c").arg(&command).output()
            })
            .await
            .map_err(|e| ToolError::new(ToolErrorKind::Execution, e.to_string()))?
            .map_err(|e| ToolError::new(ToolErrorKind::Execution, e.to_string()))?;
            let mut out = String::from_utf8_lossy(&spawned.stdout).to_string();
            out.push_str(&String::from_utf8_lossy(&spawned.stderr));
            out.push_str(&format!("\nexit={}", spawned.status.code().unwrap_or(-1)));
            Ok(ToolOutput::text(out))
        })
    }
}

// ─────────────────────── the OpenAI-compatible provider ──────────────────────
// Written by hand in the spike: rho-sdk's own HTTP providers live in
// rho-providers, which is the CLI's provider table (config, credential store,
// models.dev catalog). A host that links rho-sdk either writes this adapter or
// depends on that crate; the spike writes it, because pricing an alternative
// does not mean shipping its application layer.
#[derive(Debug)]
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

impl ModelProvider for SpikeHttpProvider {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity::new("spike", "openai-compatible", self.model.clone())
    }

    fn send_turn<'a>(&'a self, request: ModelRequest<'a>) -> ProviderFuture<'a> {
        Box::pin(async move {
            if request.cancellation.is_cancelled() {
                return Err(ProviderError::interrupted("cancelled"));
            }
            MODEL_REQUESTS.fetch_add(1, Ordering::Relaxed);
            let mut messages = Vec::new();
            for m in request.messages {
                match m {
                    Message::System(text) => {
                        messages.push(json!({"role": "system", "content": text}))
                    }
                    Message::User(blocks) => messages.push(
                        json!({"role": "user", "content": text_of(blocks)}),
                    ),
                    Message::Assistant(blocks) => messages.push(
                        json!({"role": "assistant", "content": text_of(blocks)}),
                    ),
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
                return Err(perr(
                    ProviderErrorKind::Unavailable,
                    format!("http {code}: {text}"),
                    Retryability::Retryable,
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
                    format!("no content in {text}"),
                    Retryability::Permanent,
                ));
            }
            Ok(ModelResponse::Assistant(blocks))
        })
    }
}

// ────────────────────────────── the measurement ──────────────────────────────
async fn one_turn(label: &str, rho: Rho, prompt: &str) -> Result<(), String> {
    let t0 = Instant::now();
    let tools = rho.diagnostics().tool_names().to_vec();
    let session = rho
        .session(SessionOptions::default())
        .await
        .map_err(|e| format!("{e}"))?;
    let t_session = t0.elapsed();
    let mut run = session
        .start(UserInput::text(prompt))
        .await
        .map_err(|e| format!("{e}"))?;
    let mut events: BTreeMap<String, usize> = BTreeMap::new();
    let mut settle = None;
    while let Some(ev) = run.next_event().await {
        // The variant name is the event's kind; rho has no settle *event* field
        // to read, so the Debug tag is what a host keys on (see the ADR-0010
        // "is there a settle event?" question).
        let d = format!("{ev:?}");
        let tag = d.split([' ', '{']).next().unwrap_or("?").to_string();
        *events.entry(tag.clone()).or_default() += 1;
        if matches!(
            ev,
            RunEvent::Completed { .. } | RunEvent::Failed { .. } | RunEvent::Cancelled { .. }
        ) {
            settle = Some(tag);
            break;
        }
    }
    let outcome: RunOutcome = run.outcome().await.map_err(|e| format!("{e}"))?;
    let t_turn = t0.elapsed();
    println!("arm.label\t{label}");
    println!("arm.settle_event\t{}", settle.unwrap_or_else(|| "NONE".into()));
    println!("arm.stop_reason\t{:?}", outcome.stop_reason());
    println!(
        "arm.reply\t{}",
        outcome.text().trim().chars().take(70).collect::<String>()
    );
    println!(
        "arm.model_requests\t{}",
        *events.get("ModelCallCompleted").unwrap_or(&MODEL_REQUESTS.load(Ordering::Relaxed))
    );
    println!("arm.events_total\t{}", events.values().sum::<usize>());
    println!("arm.tool_calls\t{}", TOOL_CALLS.load(Ordering::Relaxed));
    println!("tools.registered\t{}\t{}", tools.len(), tools.join(","));
    println!(
        "events.by_kind\t{}",
        events
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("ms.session\t{:.1}", t_session.as_secs_f64() * 1000.0);
    println!("ms.turn\t{:.1}", t_turn.as_secs_f64() * 1000.0);
    println!("rss_after_turn_kib\t{}", rss_kib());
    Ok(())
}

fn scripted_provider() -> ScriptedProvider {
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
                "LOOPRS_SPIKE_OK".into(),
            )])),
        ],
    )
}

#[tokio::main]
async fn main() {
    let arm = env_or("RHO_SPIKE_ARM", "endpoint");
    let base = env_or("RHO_SPIKE_BASE_URL", "http://llm.home:8080/v1");
    let model = env_or("RHO_SPIKE_MODEL", "halogen-qwen3.8-flash-next");
    let prompt = "Run the bash command `echo LOOPRS_SPIKE_OK` and then reply with its output.";
    println!("crate.linked\trho-sdk\t{}", env_or("RHO_SPIKE_CRATE_VERSION", "unpinned"));
    let built = if arm == "scripted" {
        Rho::builder().provider(scripted_provider()).tool(BashTool).build()
    } else {
        let provider = SpikeHttpProvider {
            base: base.trim_end_matches('/').to_string(),
            model,
            key: env_or("RHO_SPIKE_KEY", "spike"),
        };
        Rho::builder().provider(provider).tool(BashTool).build()
    };
    let result = match built {
        Ok(rho) => {
            let label = if arm == "scripted" {
                "scripted provider (the fake-engine seam)"
            } else {
                "openai-compatible endpoint, default gating"
            };
            one_turn(label, rho, prompt).await
        }
        Err(e) => Err(format!("engine.build\t{e}")),
    };
    if let Err(e) = result {
        println!("arm.{arm}.status\terr\t{e}");
    }
}
