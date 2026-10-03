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

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

pub struct PiRpc {
    _child: Child, // kill_on_drop: pi dies with the TUI
    out_tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
}

impl PiRpc {
    /// Spawns pi and returns the client plus a stream of every non-response message (agent events,
    /// extension UI requests, ...). Events stay as `Value` so protocol additions never break parsing;
    pub fn spawn(extra_args: &[&str]) -> Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        let mut child = Command::new("pi")
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
                        if let Some(tx) = p.lock().unwrap().remove(id) {
                            let _ = tx.send(v);
                            continue;
                        }
                    }
                }
                if ev_tx.send(v).is_err() {
                    break;
                }
            }
            // stdout closed => pi exited; dropping ev_tx lets the UI see the stream end
        });

        Ok((
            Self {
                _child: child,
                out_tx,
                pending,
                next_id: AtomicU64::new(1),
            },
            ev_rx,
        ))
    }

    /// Fire-and-forget (abort, steer, replying to an extension UI request).
    pub fn send(&self, cmd: Value) -> Result<()> {
        self.out_tx.send(cmd.to_string())?;
        Ok(())
    }

    /// Send with a correlation id and await the matching `response`.
    pub async fn request(&self, mut cmd: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.out_tx.send(cmd.to_string())?;
        Ok(rx.await?)
    }

    pub async fn prompt(&self, message: &str) -> Result<Value> {
        self.request(json!({ "type": "prompt", "message": message }))
            .await
    }

    pub fn abort(&self) -> Result<()> {
        self.send(json!({ "type": "abort" }))
    }
}
