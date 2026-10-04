//! Minimal client for `pi --mode rpc` (JSONL over stdin/stdout).
//! Not JSON-RPC 2.0: commands are `{"type": ..., "id"?: ...}`, replies are
//! `{"type":"response", "id"?: ...}`, and everything else is an agent event.
//! Check pi's docs/rpc.md for the exact command/event schema.
//!
//! Deps: anyhow, serde_json, tokio (process, io-util, sync, rt).

use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
};

/// Commands awaiting a correlated response, plus the "pi is gone" flag.
/// Both live behind one mutex so that `fail_all` and `register` cannot interleave:
/// a request either sees `exited` and fails immediately, or is registered and is
/// guaranteed to be drained by the reader task when it notices EOF.
/// Without that, a child that dies during startup could leave `request()` parked forever.
#[derive(Default)]
struct PendingState {
    txs: HashMap<String, oneshot::Sender<Value>>,
    exited: bool,
}
type Pending = Arc<Mutex<PendingState>>;

fn error_response(id: &str, msg: &str) -> Value {
    json!({ "type": "response", "id": id, "success": false, "error": msg })
}

pub struct PiRpc {
    child: Child, // kill_on_drop: pi dies with the TUI
    /// `None` once stdin has been deliberately closed for shutdown. An `Option` rather
    /// than a bare sender because "close pi's stdin" *is* "stop feeding the writer
    /// task", and the writer's exit is what drops the handle.
    out_tx: Option<mpsc::UnboundedSender<String>>,
    pending: Pending,
    next_id: AtomicU64,
}

/// `data.disposition` of a command response, defaulting to `"started"`.
///
/// Defaults to "started" rather than "handled" because stalling on a phantom run is
/// easier to spot than silently dropping a real one (see [`PiRpc::prompt`]).
pub fn disposition_of(resp: &Value) -> String {
    resp["data"]["disposition"]
        .as_str()
        .unwrap_or("started")
        .to_string()
}

/// Did the command succeed? `success` is only ever `true` or absent-false on a
/// well-formed response; anything else counts as a refusal.
pub fn succeeded(resp: &Value) -> bool {
    resp["success"].as_bool() == Some(true)
}

/// The text pi pulled out of its own queue in a `clear_queue` response, in the order
/// it would have been processed: steering first, then follow-ups.
pub fn queued_text(resp: &Value) -> Vec<String> {
    ["steering", "followUp"]
        .iter()
        .flat_map(|k| resp["data"][k].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.trim().is_empty())
        .collect()
}

impl PiRpc {
    /// Spawns `pi` and returns the client plus a stream of every non-response
    /// message (agent events, extension UI requests, ...). Events stay as `Value` so protocol
    /// additions never break parsing;
    pub fn spawn(extra_args: &[&str]) -> Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        Self::spawn_with("pi", extra_args)
    }

    /// Same as [`PiRpc::spawn`] but with an explicit executable (fakes in tests).
    pub fn spawn_with(
        bin: &str,
        extra_args: &[&str],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        let mut child = Command::new(bin)
            .args(["--mode", "rpc"])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // or a log file; never the TUI's terminal
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        // writer task: one line per command
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(mut line) = out_rx.recv().await {
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // reader task: route responses by id, forward everything else
        let pending: Pending = Arc::default();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let p = pending.clone();
        let bin_owned = bin.to_string();
        tokio::spawn(async move {
            // tokio's `lines()` splits on '\n' only, which is what pi's framing requires
            // (JS readline also splits on U+2028/2029 and would corrupt messages).
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if v["type"] == "response" {
                    if let Some(id) = v["id"].as_str() {
                        if let Some(tx) = p.lock().unwrap().txs.remove(id) {
                            let _ = tx.send(v);
                            continue;
                        }
                    }
                }
                if ev_tx.send(v).is_err() {
                    break;
                }
            }
            // stdout closed => pi exited. Nobody is going to answer outstanding commands,
            // so answer them ourselves; callers get an error instead of hanging forever.
            let drained: Vec<(String, oneshot::Sender<Value>)> = {
                let mut st = p.lock().unwrap();
                st.exited = true;
                st.txs.drain().collect()
            };
            for (id, tx) in drained {
                let _ = tx.send(error_response(
                    &id,
                    &format!("{bin_owned} exited before responding"),
                ));
            }
            // dropping ev_tx lets the UI see the stream end
        });

        Ok((
            Self {
                child,
                out_tx: Some(out_tx),
                pending,
                next_id: AtomicU64::new(1),
            },
            ev_rx,
        ))
    }

    /// Fire-and-forget (abort, steer, replying to an extension UI request).
    pub fn send(&self, cmd: Value) -> Result<()> {
        let tx = self
            .out_tx
            .as_ref()
            .ok_or_else(|| anyhow!("pi stdin is closed"))?;
        tx.send(cmd.to_string())?;
        Ok(())
    }

    /// Close pi's stdin without signalling anything else.
    ///
    /// This is the orderly shutdown lever (ADR-0002 Q1): the writer task sees its
    /// channel close, drops the handle, and pi reads EOF and disposes its runtime.
    /// Idempotent.
    pub fn close_stdin(&mut self) {
        drop(self.out_tx.take());
    }

    /// Non-blocking: has the child exited, and with what status? `None` = still
    /// running. (Status, not a bare code: "killed by a signal" has no code, and
    /// that is the difference between `137` and "unknown" in a crash report.)
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Await the child's exit. Also drops stdin, which is the polite EOF we want
    /// before waiting. Callers bound this; it has no timeout of its own.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }

    /// Send with a correlation id and await the matching `response`.
    /// Returns Err when pi reports `success: false`, when the child is already gone,
    /// or if the response never comes because the stream ended.
    pub async fn request(&self, cmd: Value) -> Result<Value> {
        let resp = self.request_raw(cmd).await?;
        if !succeeded(&resp) {
            let msg = resp["error"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("pi rpc error: {msg}"));
        }
        Ok(resp)
    }

    /// Send a prompt and return its disposition: `"started"` means a run is coming (wait for
    /// `agent_settled`), `"handled"` means pi took the prompt but started no run, so no
    /// `agent_settled` will ever arrive. Defaults to `"started"` if the field is missing,
    /// because stalling on a phantom run is easier to spot than dropping a real one.
    pub async fn prompt(&self, message: &str) -> Result<String> {
        let resp = self
            .request(json!({ "type": "prompt", "message": message }))
            .await?;
        Ok(disposition_of(&resp))
    }

    /// Send a command and await its `response` record *without* interpreting it.
    ///
    /// `request` collapses `success: false` into an `Err`, which is right for a
    /// prompt but wrong for commands whose "no" is information the caller wants to
    /// branch on — `steer` refused because the run just settled, `clear_queue`
    /// answered empty, and so on. Err here means only "no answer will ever come"
    /// (pi is gone, or the caller's own cancellation).
    pub async fn request_raw(&self, mut cmd: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        {
            // Register (or refuse) while holding the lock so the EOF drain can't miss us.
            let mut st = self.pending.lock().unwrap();
            if st.exited {
                return Err(anyhow!("pi process has already exited"));
            }
            st.txs.insert(id.clone(), tx);
        }
        self.send(cmd)?;
        rx.await
            .map_err(|_| anyhow!("response {id} cancelled before it was answered"))
    }
    pub fn abort(&self) -> Result<()> {
        self.send(json!({ "type": "abort" }))
    }
    pub async fn kill(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }
}
