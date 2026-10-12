//! EMBED arm of spikes/rho_surface_cost.py for the `casonadams` target
//! (casonadams/rho: rho, rho-engine, rho-harness-core). Compiled by that spike's
//! PRICE/EMBED legs into a scratch crate; nothing in looprs links it.
//!
//! It is written against casonadams/rho's API (`rho::platform::agent_engine`,
//! `engine::run_turn`, `StructuredPresenter`) and will NOT compile against
//! matthewyjiang/rho's rho-sdk — which is the whole reason the EMBED program is
//! part of a target's identity. See `matthewyjiang-rho-sdk.main.rs` for the
//! other arm.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rho::auth::AuthStore;
use rho::config::{Config, ProviderConfig};
use rho::engine::runner::{TurnRequest, TurnOutput};
use rho::presentation::{RecordingSink, StructuredPresenter};

fn rss_kib() -> u64 {
    for line in std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

#[tokio::main]
async fn main() {
    let base = Instant::now();
    let mut config = match Config::load(None) {
        Ok(c) => c,
        Err(e) => { println!("config.load\terr\t{e}"); std::process::exit(2); }
    };
    let base_url =
        std::env::var("RHO_SPIKE_BASE_URL").unwrap_or_else(|_| "http://llm.home:8080/v1".into());
    let model = std::env::var("RHO_SPIKE_MODEL")
        .unwrap_or_else(|_| "halogen-qwen3.8-flash-next".into());
    config.provider = "spike".into();
    config.model = model.clone();
    config.allow_private_network = true;
    config.providers.insert(
        "spike".into(),
        ProviderConfig {
            base_url,
            key_env: Some("RHO_SPIKE_KEY".into()),
            default_model: Some(model.clone()),
        },
    );
    // A host that runs unattended has to say what happens when a tool wants
    // approval and nobody is there. rho's knob for that is this one boolean.
    if std::env::var("RHO_SPIKE_NO_PERMISSION").is_ok() {
        config.permission.enabled = false;
    }
    let auth = match AuthStore::load(&config.auth_file) {
        Ok(a) => a,
        Err(_) => {
            std::fs::write(&config.auth_file, "{}").ok();
            match AuthStore::load(&config.auth_file) {
                Ok(a) => a,
                Err(e) => { println!("auth.load\terr\t{e}"); std::process::exit(2); }
            }
        }
    };
    let t_config = base.elapsed();

    let engine = match rho::platform::agent_engine(config.clone(), auth, None).await {
        Ok(e) => e,
        Err(e) => { println!("engine.build\terr\t{e}"); std::process::exit(3); }
    };
    let t_engine = base.elapsed();
    let tools = engine.tool_names();

    let sink = RecordingSink::new();
    let presenter = Arc::new(StructuredPresenter::recording(sink.clone()));
    let prompt = "Run the bash command `echo LOOPRS_SPIKE_OK` and then reply with its output.";
    let out = engine.run_turn(TurnRequest::new(prompt), presenter.clone()).await;
    let t_turn = base.elapsed();

    match out {
        Ok(TurnOutput { final_text, tool_calls_count, requests, status, .. }) => {
            println!("turn.status\t{status:?}");
            println!("turn.tool_calls\t{tool_calls_count}");
            println!("turn.model_requests\t{requests}");
            println!("turn.reply\t{}", final_text.trim().chars().take(70).collect::<String>());
        }
        Err(e) => println!("turn.status\terr\t{e}"),
    }
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for ev in sink.events() {
        let tag = serde_json::to_value(&ev).ok()
            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .unwrap_or_else(|| "?".into());
        *kinds.entry(tag).or_default() += 1;
    }
    println!("events.total\t{}", sink.events().len());
    for (k, v) in &kinds {
        println!("event.{k}\t{v}");
    }
    println!("tools.registered\t{}\t{}", tools.len(), tools.join(","));
    println!("ms.config\t{:.1}", t_config.as_secs_f64() * 1000.0);
    println!("ms.engine\t{:.1}", t_engine.as_secs_f64() * 1000.0);
    println!("ms.turn\t{:.1}", t_turn.as_secs_f64() * 1000.0);
    println!("rss_after_turn_kib\t{}", rss_kib());
}
