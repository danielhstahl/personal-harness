//! The measurement looprs-pdl.7's scrollback cap is sized from.
//!
//! The ticket's first instruction is "measure the working set first; put the
//! number in the notes", and the reason it is an instruction rather than a
//! suggestion is that every number nearby is a round one invented at a desk.
//! `4096 rows` is not a fact about a beads pass. "1.9 MB of live heap behind
//! twelve tickets' worth of beads transcript at 100 columns" is.
//!
//! **This is not a gate.** It is `#[ignore]`d, prints a report, and asserts one
//! thing (that the cap does what it says), for the same reason
//! `examples/spike_clipboard_cost.rs` asserts nothing: a measurement is a fact
//! about this machine and this corpus, and turning it into an assertion makes
//! green runs lie whenever a kernel, a corpus or a renderer changes. The numbers
//! it produced are kept in `spikes/results/scrollback-cost.log` and in the
//! beads notes, and the constants in [`crate::state::scrollback`] cite that
//! file.
//!
//! # What is counted
//!
//! *Live heap*, by the allocator itself. [`CountingAlloc`] wraps the system
//! allocator with two counters and is installed for the test build of the binary
//! only, behind a switch so ordinary tests pay one relaxed load rather than two
//! atomics. An estimator ("a row is a `Line` plus a `Vec<Span>` plus a
//! `CellMap`, call it N bytes") would have been a model of the allocator rather
//! than the allocator — and the whole point of the ticket's *"bytes, not rows"*
//! is that the structure around a row of base64 costs more than the row of text,
//! which only shows up if something real is counting.
//!
//! # The corpus
//!
//! Real `pi` session transcripts from real beads passes in this project
//! (`~/.pi/agent/sessions/--Users-…-looprs--/*.jsonl`): the answers, the
//! thinking, the tool calls, and — the part that decides the shape of the
//! problem — the *tool results*, which are whole files, `git diff` output,
//! `bd show` dumps, and the occasional single line of many kilobytes. Each
//! session file is one ticket's work, so **N concatenated files is a pass of N
//! tickets**, fed through the same [`SessionView`] the beads mode uses and
//! rendered by the same flusher at the same width.
//!
//! ```sh
//! LOOPRS_MEASURE_CORPUS=~/.pi/agent/sessions/--Users-danielstahl-Documents-code-ml-personal-harness-pi-beads-looprs-- \
//!   cargo test -- --ignored --nocapture --test-threads=1 measure 2>&1 \
//!   | tee spikes/results/scrollback-cost.log
//! ```
//!
//! `LOOPRS_MEASURE_TICKETS` caps how many files are replayed (default 12,
//! oldest first). Without `LOOPRS_MEASURE_CORPUS` the test says so and stops.
//! It does **not** fall back to synthetic filler: a number measured over filler
//! is exactly the kind of round number this ticket exists to replace, and a
//! report that quietly changes what it measured is worse than one that refuses
//! to run.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// The counting allocator, installed for the **test build of the binary only**.
///
/// Counting is behind [`CountingAlloc::begin`] so the ~640 ordinary tests pay
/// one relaxed load per allocation instead of two atomics, and so a window
/// cannot be widened by accident: outside a window, nothing is counted.
pub struct CountingAlloc;

static LIVE: AtomicI64 = AtomicI64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static ON: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
#[global_allocator]
static COUNTER: CountingAlloc = CountingAlloc;

impl CountingAlloc {
    /// Open a window. Counters are zeroed here, so every window is
    /// self-contained and no earlier allocation leaks into the report.
    fn begin() {
        LIVE.store(0, Ordering::Relaxed);
        TOTAL.store(0, Ordering::Relaxed);
        ON.store(true, Ordering::SeqCst);
    }

    /// `(live, total)` as of now, counting still on.
    fn peek() -> (i64, u64) {
        (LIVE.load(Ordering::Relaxed), TOTAL.load(Ordering::Relaxed))
    }

    /// Close the window.
    ///
    /// Every window in this file is a build-up with no frees across its end, so
    /// `live` is the footprint of what the window constructed rather than a
    /// number drifting as the test tears itself down.
    fn end() -> (i64, u64) {
        ON.store(false, Ordering::SeqCst);
        Self::peek()
    }
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if ON.load(Ordering::Relaxed) && !p.is_null() {
            LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed);
            TOTAL.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ON.load(Ordering::Relaxed) && !ptr.is_null() {
            LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

use crate::session::view::SessionView;
use crate::session::{SessionId, TerminalType};
use crate::state::scrollback::{DEFAULT_RETAINED_BYTES, DisplayRow, RowKind};
use crate::state::transcript::MessageKind;

/// One transcript entry read out of a real session file, ready to feed a view.
struct Item {
    kind: Kind,
    text: String,
    /// Tool entries only: the tool's name and its argument blob — the
    /// `start_tool` half, which the result then closes.
    call: Option<(String, String)>,
}

enum Kind {
    User,
    Thinking,
    Answer,
    Tool,
}

/// Replay one `pi` session transcript into a list of items, in the order the
/// session saw them.
fn load_session(path: &std::path::Path) -> anyhow::Result<Vec<Item>> {
    let raw = std::fs::read_to_string(path)?;
    let mut out: Vec<Item> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let m = msg.get("message").cloned().unwrap_or_default();
        match m.get("role").and_then(|v| v.as_str()).unwrap_or("") {
            "user" => push(&mut out, Kind::User, blocks_text(&m, &["text"]), None),
            "assistant" => {
                push(
                    &mut out,
                    Kind::Thinking,
                    blocks_text(&m, &["thinking"]),
                    None,
                );
                push(&mut out, Kind::Answer, blocks_text(&m, &["text"]), None);
                for b in blocks(&m) {
                    if b.get("type").and_then(|v| v.as_str()) == Some("toolCall") {
                        let name = b
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let args = b
                            .get("arguments")
                            .map(|a| a.to_string())
                            .unwrap_or_default();
                        out.push(Item {
                            kind: Kind::Tool,
                            text: String::new(),
                            call: Some((name, args)),
                        });
                    }
                }
            }
            "toolResult" => {
                // The result *text* is the payload — the content that made a
                // beads pass big, and the reason a row-count is the wrong unit
                // for the cap. It closes the most recent still-open tool entry,
                // exactly as `Transcript::finish_tool` does by id.
                let text = blocks_text(&m, &["text"]);
                if text.trim().is_empty() {
                    continue;
                }
                match out
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i.kind, Kind::Tool) && i.text.is_empty())
                {
                    Some(open) => open.text = text,
                    None => out.push(Item {
                        kind: Kind::Tool,
                        text,
                        call: Some(("tool".into(), String::new())),
                    }),
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn push(out: &mut Vec<Item>, kind: Kind, text: String, call: Option<(String, String)>) {
    if !text.trim().is_empty() {
        out.push(Item { kind, text, call });
    }
}

fn blocks(m: &serde_json::Value) -> Vec<serde_json::Value> {
    m.get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Concatenate every content block of the given types. `content` is either a
/// plain string or a list of typed blocks; both shapes occur, and a block of a
/// type that was not asked for contributes nothing — which is how `toolCall`
/// stays out of the assistant's prose.
fn blocks_text(m: &serde_json::Value, types: &[&str]) -> String {
    let mut out = String::new();
    match m.get("content") {
        Some(serde_json::Value::String(s)) if types.contains(&"text") => out.push_str(s),
        Some(serde_json::Value::Array(_)) => {
            for b in blocks(m) {
                let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if !types.contains(&ty) {
                    continue;
                }
                if let Some(s) = b
                    .get("text")
                    .or_else(|| b.get("thinking"))
                    .and_then(|v| v.as_str())
                {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
    out
}

/// Percentile of a sorted slice, by index (no interpolation: this is a report,
/// not a statistic).
fn pct(sorted: &[usize], p: usize) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * p as f64 / 100.0).round() as usize;
    sorted[i]
}

fn kib(n: f64) -> String {
    format!("{:.1} KiB", n / 1024.0)
}

fn beads() -> SessionId {
    SessionId::new(TerminalType::Beeds, 0)
}

#[test]
#[ignore = "working-set measurement (pdl.7): LOOPRS_MEASURE_CORPUS=<pi session file or dir>; recorded numbers are in spikes/results/scrollback-cost.log"]
fn a_long_beads_pass_measured_through_the_real_render_path() {
    let root = match corpus_root() {
        Some(r) => r,
        None => {
            println!(
                "no LOOPRS_MEASURE_CORPUS — nothing measured. Point it at real `pi` session \
                 .jsonl files (a directory works too); the recorded run lives in \n\
                 \x20 spikes/results/scrollback-cost.log"
            );
            return;
        }
    };
    let files = corpus_files(&root);

    // Warm the one-time allocations first. syntect's syntax set is several MB and
    // loads lazily on the first fenced code block rendered; if it landed inside
    // the window the report would call it the scrollback's working set, which is
    // a true number about the wrong thing.
    {
        let mut warm = SessionView::new(beads());
        warm.push_delta(MessageKind::Answer, "```rust\nfn warm() {}\n```\n");
        warm.flush(100);
    }

    let width: u16 = 100;
    let band: usize = 40;
    let mut view = SessionView::with_buffer(beads(), 0);
    view.set_store_cap(0);

    println!("=== looprs-pdl.7 · scrollback working set ===");
    println!("corpus   : {}", root.display());
    println!("tickets  : {} session files (oldest first)", files.len());
    println!("geometry : {width} cols, {band}-row transcript band");
    println!();

    CountingAlloc::begin();
    let mut text_bytes = 0usize;
    let mut tool_bytes = 0usize;
    let mut tool_max = 0usize;
    let mut longest_line = 0usize;
    let mut entries = 0usize;

    // (1) Build the transcript, exactly as the beads pass would, and take the
    // transcript's own footprint before a single row is rendered.
    for f in &files {
        for it in load_session(f).unwrap_or_default() {
            match it.kind {
                Kind::User => view.push_note(MessageKind::User, it.text.clone()),
                Kind::Thinking => view.push_delta(MessageKind::Thinking, &it.text),
                Kind::Answer => view.push_delta(MessageKind::Answer, &it.text),
                Kind::Tool => {
                    let (name, args) = it.call.clone().unwrap_or_default();
                    view.start_tool("id".into(), name, args);
                    view.finish_tool("id".into(), it.text.clone(), false);
                }
            }
            entries += 1;
            text_bytes += it.text.len();
            if matches!(it.kind, Kind::Tool) {
                tool_bytes += it.text.len();
                tool_max = tool_max.max(it.text.len());
            }
            for l in it.text.lines() {
                longest_line = longest_line.max(l.len());
            }
        }
    }
    // (1b) Before a single row is rendered: what the transcript alone costs.
    let (live_text, total_text) = CountingAlloc::peek();
    // (1c) Render it. One flush is enough — the store is built by the same door
    // the frame drives, and it is the same content the pass would have produced
    // spread over frames.
    view.flush(width);
    let (live_all, total_all) = CountingAlloc::peek();
    let _ = (live_text, total_text, entries);

    // The transcript's live bytes are `text + entry/Vec overhead`; the store's
    // are everything the render added on top. Everything the report needs from
    // the uncapped store is taken here, into owned values, before the cap runs
    // and changes what `rows` would answer.
    let (row_count, row_text, cells, lens) = {
        let rows: Vec<&DisplayRow> = view
            .scrollback()
            .rows()
            .iter()
            .filter(|r| r.kind == RowKind::Transcript)
            .collect();
        let mut lens: Vec<usize> = rows.iter().map(|r| r.to_string().len()).collect();
        lens.sort_unstable();
        (
            rows.len(),
            lens.iter().sum::<usize>(),
            rows.iter().map(|r| r.cells.cells()).sum::<usize>(),
            lens,
        )
    };
    // The store's own footprint, split off from the transcript's by the flush:
    // same window, two reads.
    let store_live = live_all - live_text;

    println!("--- the pass, in the units the ticket asked for");
    println!(
        "entries       : {}   ({} of entry text)",
        view.transcript.entries.len(),
        kib(text_bytes as f64)
    );
    println!(
        "tool results  : {} of it ({:.0}%), single largest {}",
        kib(tool_bytes as f64),
        100.0 * tool_bytes as f64 / (text_bytes.max(1) as f64),
        kib(tool_max as f64)
    );
    println!("longest line  : {longest_line} bytes");
    println!();
    println!("--- the unbounded store, at {width} cols");
    println!("rendered rows : {row_count}");
    println!(
        "row text      : {}  (avg {:.0} B/row)",
        kib(row_text as f64),
        row_text as f64 / row_count.max(1) as f64
    );
    println!("cells         : {cells}");
    println!(
        "row text len  : p50 {} B · p95 {} B · p99 {} B · max {} B",
        pct(&lens, 50),
        pct(&lens, 95),
        pct(&lens, 99),
        pct(&lens, 100)
    );
    println!(
        "live heap     : {} = {} transcript + {} store rows",
        kib(live_all as f64),
        kib(live_text as f64),
        kib(store_live as f64)
    );
    println!(
        "churn         : {} of temporaries came and went",
        kib((total_all as i64 - live_all) as f64)
    );
    println!();

    // (2) What the cap does to the same corpus.
    let cap = DEFAULT_RETAINED_BYTES;
    view.set_store_cap(cap);
    let kept: Vec<&DisplayRow> = view
        .scrollback()
        .rows()
        .iter()
        .filter(|r| r.kind == RowKind::Transcript)
        .collect();
    let kept_text: usize = kept.iter().map(|r| r.to_string().len()).sum();
    let (live_capped, _) = CountingAlloc::peek();
    let _ = live_capped;

    println!("--- with the cap the code sets ({})", kib(cap as f64));
    println!(
        "kept          : {} rows \u{b7} {} of row text \u{b7} {:.0}% of the pass's content",
        kept.len(),
        kib(kept_text as f64),
        100.0 * kept_text as f64 / (row_text.max(1) as f64)
    );
    println!(
        "dropped       : {} lines reported on the marker",
        view.scrollback().dropped_lines()
    );
    println!(
        "history       : {:.1} screens of {band} rows kept",
        kept.len() as f64 / band as f64
    );
    let marker = view
        .scrollback()
        .rows()
        .first()
        .map(|r| r.to_string())
        .unwrap_or_default();
    println!("marker        : {marker}");
    println!();
    println!(
        "(a) live heap per byte of row text: {:.2} — the reason the cap counts \n\
         \x20    structure and not just text.",
        store_live as f64 / (row_text.max(1) as f64)
    );
    println!(
        "(b) live heap per rendered row   : {:.0} B — what `ROW_STRUCT_BYTES` is \
         sized against.",
        store_live as f64 / row_count.max(1) as f64
    );
    println!();
    println!("--- the journal's cost (disk, not memory)");
    println!(
        "a pass of this shape writes about {} of journal: the entry text, in order, \
         with the parts the cap dropped still in it.",
        kib(text_bytes as f64)
    );
    let (live_end, total_end) = CountingAlloc::end();
    println!(
        "window closed: live {} · total {}",
        kib(live_end as f64),
        kib(total_end as f64)
    );
}

fn corpus_root() -> Option<std::path::PathBuf> {
    let raw = std::env::var("LOOPRS_MEASURE_CORPUS").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| std::path::Path::new(&h).join(rest))
            .unwrap_or_else(|_| std::path::PathBuf::from(raw))
    } else {
        std::path::PathBuf::from(raw)
    };
    p.exists().then_some(p)
}

fn corpus_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = if root.is_dir() {
        std::fs::read_dir(root)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![root.to_path_buf()]
    };
    // Names start with the UTC timestamp, so lexical order is the order the loop
    // actually worked them.
    v.sort();
    let tickets = std::env::var("LOOPRS_MEASURE_TICKETS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(12);
    v.truncate(tickets.max(1));
    v
}
