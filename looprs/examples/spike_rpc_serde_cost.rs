//! workspace-eqn.1 — what the JSONL boundary costs *the parser looprs actually ships*.
//!
//! `spikes/pi_rpc_boundary_cost.py` prices the process: fork, boot, round trip,
//! RSS, EOF shutdown. It cannot price the other half of the "serialization
//! boundary" sentence, because the parser on the other side of that boundary is
//! Rust and the driver is Python. This example is that parser:
//! `src/services/pi.rs` reads a line and calls `serde_json::from_str::<Value>`,
//! so that is the call timed here, over the real corpus rather than a fixture.
//!
//! It is a measurement and not a gate (`src/measure.rs`'s rule): it prints, it
//! asserts nothing except that it actually read something, and its numbers belong
//! to this machine and this corpus.
//!
//! ## What is being priced, and what only resembles it
//!
//! The corpus is pi **session** files — one per real beads pass — not a captured
//! RPC stream. A session record and an RPC record are the same dialect (JSON
//! objects, parsed by the same function) but not the same population: the RPC
//! stream carries a `message_update` per streamed delta, so it has *more*,
//! *smaller* records than the session file does. So the honest use of this log
//! is the two rate columns — µs per record, MB per second — which are properties
//! of the parser, applied to the record count the RPC stream was measured to
//! carry (`spikes/results/pi-rpc-boundary-cost.log`, leg 4). The per-pass column
//! is the session-file shape, labelled as such.
//!
//! ```sh
//! LOOPRS_MEASURE_CORPUS=~/.pi/agent/sessions/--Users-…-looprs-- \
//!   cargo run -q --release --example spike_rpc_serde_cost \
//!   | tee spikes/results/rpc-serde-cost.log
//! ```
//!
//! Env: `LOOPRS_MEASURE_CORPUS` (dir of real pi session `.jsonl` files; the
//! example refuses rather than inventing one), `LOOPRS_MEASURE_TICKETS` (cap,
//! oldest first, default 40).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

fn corpus_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LOOPRS_MEASURE_CORPUS") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
        eprintln!("LOOPRS_MEASURE_CORPUS={p:?} is not a directory");
        return None;
    }
    None
}

/// The two directions of the boundary, timed separately because they are not the
/// same cost and the client pays both: every inbound record is parsed and every
/// outbound command is written.
fn main() {
    let Some(dir) = corpus_dir() else {
        println!("not measured: LOOPRS_MEASURE_CORPUS is unset or not a directory.");
        println!("This example refuses to run over synthetic filler — a parse rate measured");
        println!("on records nobody wrote is the round number this repo's measurement rule");
        println!("exists to forbid (src/measure.rs).");
        std::process::exit(2);
    };
    let cap: usize = std::env::var("LOOPRS_MEASURE_TICKETS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("corpus dir unreadable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort(); // the file names are timestamps, so this is oldest first
    files.truncate(cap);
    if files.is_empty() {
        println!("not measured: no .jsonl under {}", dir.display());
        std::process::exit(2);
    }

    println!("rpc serde cost — parser: serde_json (the same call src/services/pi.rs makes)");
    println!("corpus: {}", dir.display());
    println!(
        "passes: {} (LOOPRS_MEASURE_TICKETS caps this; default 40)",
        files.len()
    );
    println!();

    let mut per_pass: Vec<(String, usize, usize, f64, f64)> = Vec::new();
    let mut size_hist: BTreeMap<usize, usize> = BTreeMap::new();

    for f in &files {
        let bytes = std::fs::read(f).expect("unreadable session file");
        let mut records = 0usize;
        // Parse: exactly what the reader task in src/services/pi.rs does with a
        // line off the child's stdout.
        let t = Instant::now();
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if serde_json::from_slice::<serde_json::Value>(line).is_err() {
                continue; // the client skips unparseable lines the same way
            }
            records += 1;
        }
        let parse = t.elapsed().as_secs_f64();

        // Encode: the command direction. Re-serialising every parsed record is
        // the closest stand-in for what a host sends and is charged per byte.
        let t = Instant::now();
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                let _ = serde_json::to_vec(&v);
            }
        }
        let encode = t.elapsed().as_secs_f64();

        per_pass.push((
            f.file_name().unwrap().to_string_lossy().to_string(),
            bytes.len(),
            records,
            parse,
            encode,
        ));
        for line in bytes.split(|b| *b == b'\n') {
            if !line.is_empty() {
                *size_hist.entry(line.len().min(4096)).or_default() += 1;
            }
        }
    }

    let n = per_pass.len();
    let rows: Vec<usize> = per_pass.iter().map(|p| p.1).collect();
    let recs: Vec<usize> = per_pass.iter().map(|p| p.2).collect();
    let med = |xs: &[usize]| xs[xs.len() / 2];
    let medf = |xs: &[f64]| xs[xs.len() / 2];
    let parse_ms: Vec<f64> = {
        let mut v: Vec<f64> = per_pass.iter().map(|p| p.3 * 1e3).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    };
    let encode_ms: Vec<f64> = {
        let mut v: Vec<f64> = per_pass.iter().map(|p| p.4 * 1e3).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    };

    println!("one pass, session-file shape (median of {n} passes):");
    println!("  session bytes            {}", med(&rows));
    println!("  records                  {}", med(&recs));
    println!("  parse all records        {:.1} ms", medf(&parse_ms));
    println!(
        "  parse + re-encode        {:.1} ms",
        medf(&parse_ms) + medf(&encode_ms)
    );
    println!(
        "  per record               {:.2} µs parse, {:.2} µs parse+encode",
        medf(&parse_ms) * 1e3 / med(&recs) as f64,
        (medf(&parse_ms) + medf(&encode_ms)) * 1e3 / med(&recs) as f64
    );
    println!(
        "  throughput               {:.1} MB/s parse",
        med(&rows) as f64 / 1e6 / medf(&parse_ms) * 1e3
    );
    println!();
    println!("per-pass table (name, bytes, records, parse µs, parse+encode µs):");
    for (name, bytes, records, parse, encode) in per_pass.iter().take(12) {
        println!(
            "  {name:70} {bytes:>9} {records:>6} {:9.0} {:9.0}",
            parse * 1e6,
            encode * 1e6
        );
    }
    if n > 12 {
        println!("  … {} more passes not listed", n - 12);
    }
    println!();
    println!("record size histogram (bytes → count, capped at 4096):");
    for (k, v) in size_hist.iter().take(16) {
        println!("  {k:>6} → {v}");
    }
    println!();
    println!(
        "Read this with the RPC record count from spikes/results/pi-rpc-boundary-cost.log:\n\
         the session corpus is the *coarser* population (one record per message, not one\n\
         per streamed delta), so µs/record here is a floor on the RPC stream's count and a\n\
         ceiling on its per-record size. Any pass whose boundary cost is claimed to matter\n\
         has to beat the denominator in that log, not this one."
    );
}
