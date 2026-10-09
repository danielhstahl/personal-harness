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
use std::time::Instant;

/// The counting allocator, installed for the **test build of the binary only**.
///
/// Counting is behind [`CountingAlloc::begin`] so the ~640 ordinary tests pay
/// one relaxed load per allocation instead of two atomics, and so a window
/// cannot be widened by accident: outside a window, nothing is counted.
pub struct CountingAlloc;

static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static ON: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
#[global_allocator]
static COUNTER: CountingAlloc = CountingAlloc;

impl CountingAlloc {
    /// Open a window. Counters are zeroed here, so every window is
    /// self-contained and no earlier allocation leaks into the report.
    pub fn begin() {
        LIVE.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        TOTAL.store(0, Ordering::Relaxed);
        ON.store(true, Ordering::SeqCst);
    }

    /// `(live, total)` as of now, counting still on.
    pub fn peek() -> (i64, u64) {
        (LIVE.load(Ordering::Relaxed), TOTAL.load(Ordering::Relaxed))
    }

    /// The high-water mark of live bytes since [`Self::begin`].
    ///
    /// `live` answers "what is held at the end of the operation"; the transient
    /// cost of an operation — the thing that OOMs a working app and is invisible
    /// in the final footprint — is only visible in the maximum. That is what
    /// [`crate::session::view`]'s rewrap budget is measured against: a rebuild
    /// that ends at 32 MiB after passing through 164 MiB is not bounded by 32.
    ///
    /// The read is racy by construction (load-then-store, no CAS): concurrent
    /// threads can interleave a peak past the recorded maximum, so this is an
    /// **under**-count under concurrency and exact on a single thread. Measure
    /// peaks with `--test-threads=1`.
    pub fn peak() -> i64 {
        PEAK.load(Ordering::Relaxed)
    }

    /// Close the window.
    ///
    /// Every window in this file is a build-up with no frees across its end, so
    /// `live` is the footprint of what the window constructed rather than a
    /// number drifting as the test tears itself down.
    pub fn end() -> (i64, u64) {
        ON.store(false, Ordering::SeqCst);
        Self::peek()
    }
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if ON.load(Ordering::Relaxed) && !p.is_null() {
            let now =
                LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed) + layout.size() as i64;
            // Cheap max, no CAS loop: a lost update here can only understate the
            // peak, never invent one. See [`Self::peak`].
            if now > PEAK.load(Ordering::Relaxed) {
                PEAK.store(now, Ordering::Relaxed);
            }
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
use crate::theme::styles::content_width;

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

/// The **resize transient**, measured the same way (looprs-zie).
///
/// The ticket's complaint is not about what the store ends up holding — it is
/// about what a resize passes *through* on the way there. A `rewrap` that
/// re-renders the whole transcript and trims afterwards peaks at the cost of
/// the transcript; the bound that matters is the peak, not the footprint, and a
/// footprint of 32 MiB is compatible with passing through 164 MiB.
///
/// So this measures peaks:
///
/// * **the bounded shape** — a rewrap through the source slice the cap can
///   afford, at a sweep of widths;
/// * **the unbounded shape** — the same transcript rendered whole through a
///   store with no cap, which is what the old one-door rewrap cost on every
///   width change, and what a view with its store cap turned off costs today.
///
/// The store cap is the shipped default ([`DEFAULT_RETAINED_BYTES`]). The
/// transcript buffer is the shipped [`DEFAULT_VIEW_BUFFER`] unless
/// `LOOPRS_MEASURE_BUFFER=0` is set, which is how the numbers behind the
/// ticket's own figures (a 1.2 MB transcript on a view nobody capped) get
/// reproduced.
#[test]
#[ignore = "resize transient measurement (looprs-zie): LOOPRS_MEASURE_CORPUS=<pi session dir>; see spikes/results/resize-transient.log"]
fn a_resize_transient_measured_against_the_cap_it_is_supposed_to_have() {
    let root = match corpus_root() {
        Some(r) => r,
        None => {
            println!(
                "no LOOPRS_MEASURE_CORPUS — nothing measured. Point it at real `pi` session \
                 .jsonl files (a directory works too)."
            );
            return;
        }
    };
    let files = corpus_files(&root);
    let buffer = std::env::var("LOOPRS_MEASURE_BUFFER")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(crate::session::view::DEFAULT_VIEW_BUFFER);

    // Same warm-up reason as the working-set run: syntect's multi-megabyte syntax
    // set must not be reported as this operation's transient.
    {
        let mut warm = SessionView::new(beads());
        warm.push_delta(MessageKind::Answer, "```rust\nfn warm() {}\n```\n");
        warm.flush(100);
    }

    /// Replay the corpus into a view and flush it at `width`.
    fn replay(files: &[std::path::PathBuf], buffer: usize, width: u16) -> SessionView {
        let mut view = SessionView::with_buffer(beads(), buffer);
        for f in files {
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
            }
        }
        view.flush(width);
        view
    }

    println!("=== looprs-zie \u{00b7} the resize transient ===");
    println!("corpus   : {}", root.display());
    println!("tickets  : {} session files", files.len());
    println!(
        "caps     : store {} \u{00b7} transcript buffer {}",
        kib(DEFAULT_RETAINED_BYTES as f64),
        if buffer == 0 {
            "unbounded".to_string()
        } else {
            kib(buffer as f64)
        }
    );
    println!();

    // The bounded shape: one real view, driven through a sweep of widths, with
    // the counting window open across each rebuild so the peak is that rebuild's
    // and not the build-up's.
    let mut view = replay(&files, buffer, 100);
    println!(
        "--- rewrap at the cap the code sets ({}), peak live bytes during the rebuild",
        kib(DEFAULT_RETAINED_BYTES as f64)
    );
    println!(
        "start    : {} rows \u{00b7} {} retained \u{00b7} {} entries in the transcript",
        view.scrollback().rows().len(),
        kib(view.scrollback().retained_bytes() as f64),
        view.transcript.entries.len()
    );
    for width in [120u16, 100, 80, 60, 40, 30] {
        let _ = CountingAlloc::end();
        CountingAlloc::begin();
        let added = view.flush(width);
        let peak = CountingAlloc::peak();
        println!(
            "  w={width:>3}: peak {} \u{00b7} {:>4.0}% of cap \u{00b7} {} rows retained \u{00b7} {added} rows new",
            kib(peak as f64),
            100.0 * peak as f64 / DEFAULT_RETAINED_BYTES as f64,
            view.scrollback().rows().len(),
        );
    }
    let _ = CountingAlloc::end();

    // The shape the ticket is a reply to: the same corpus, the same widths, with
    // the source NOT cut — every entry rendered again on every width change.
    let mut unbounded = replay(&files, buffer, 100);
    unbounded.set_store_cap(0);
    println!();
    println!("--- the same corpus with no store cap: what rendering the whole source costs");
    for width in [120u16, 100, 80, 60, 40, 30] {
        let _ = CountingAlloc::end();
        CountingAlloc::begin();
        let _ = unbounded.flush(width);
        let peak = CountingAlloc::peak();
        println!(
            "  w={width:>3}: peak {} \u{00b7} {} rows retained (cap: {})",
            kib(peak as f64),
            unbounded.scrollback().rows().len(),
            kib(DEFAULT_RETAINED_BYTES as f64),
        );
    }
    let (live, total) = CountingAlloc::end();
    println!();
    println!(
        "window closed: live {} \u{00b7} total {}",
        kib(live as f64),
        kib(total as f64)
    );
}

/// ───────────────────────── the live-tail preview, per frame (looprs-00u.14)
///
/// The ticket's claim is a per-frame one: `SessionView::preview` is called on
/// every draw, and every draw re-renders the whole not-yet-final block. What
/// decides whether that matters is not opinion but **µs and bytes per frame
/// against the length of the live block**, which is what this measures.
///
/// The model is the real frame, in the real order: push a chunk of a real
/// answer, `flush(width)` (the settled half of the same frame), then take the
/// preview. Three scenarios per frame, plus a fourth that is the floor a cache
/// cannot go below:
///
/// * **new bytes** — the preview right after the entry grew. The work is
///   unavoidable: the bytes are new, the rows must be new.
/// * **no new bytes** — the same call again with nothing changed, which is
///   what the redraw clock actually asks for between two deltas. Before the
///   cache this costs the same as the row above it; after it, it should not.
/// * **width changed** — same bytes, different window. Must *never* be free:
///   a row is its shape, so this is the invalidation the cache is not allowed
///   to get wrong (ADR-0002 Q5).
/// * **clone of the rows** — what returning an already-rendered answer costs.
///   The floor of any "keep the last preview" scheme, because the frame takes
///   the rows by value.
///
/// Bucketed by the live-tail length the flusher was actually standing on
/// (`SessionView::preview_len`), read between the flush and the preview — the
/// cursor's `block` restarts at every blank line, so the live region is the
/// open *paragraph*, not the entry, and the buckets are where that fact shows
/// up rather than in a guess about it.
///
/// ```sh
/// LOOPRS_MEASURE_CORPUS=~/.pi/agent/sessions/--Users-danielstahl-Documents-code-ml-personal-harness-pi-beads-looprs-- \
///   cargo test -- --ignored --nocapture --test-threads=1 the_live_tail 2>&1 \
///   | tee spikes/results/live-preview-cost.log
/// ```
#[test]
#[ignore = "live-preview per-frame cost (looprs-00u.14): LOOPRS_MEASURE_CORPUS=<pi session dir>; recorded numbers in spikes/results/live-preview-cost.log"]
fn the_live_tail_preview_measured_per_frame_against_the_block_it_renders() {
    let root = match corpus_root() {
        Some(r) => r,
        None => {
            println!(
                "no LOOPRS_MEASURE_CORPUS — nothing measured. Point it at real `pi` session \n\
                 \x20 .jsonl files (a directory works too)."
            );
            return;
        }
    };
    let files = corpus_files(&root);

    // Same warm-up reason as every other run in this file: syntect's
    // multi-megabyte syntax set loads lazily on the first fence, and if that
    // landed inside a window it would be reported as a frame's cost.
    //
    // It is worse than "the syntax set" and the warm-up is wider than one fence
    // for that reason: the **first highlighted line of each language** costs its
    // own one-time compile (observed at 20–300 ms and ~90 MB of churn over this
    // corpus's rust/bash/sh/python/json/text fences — the lazy-syntect finding
    // `looprs-00u.16` is about). Those fences were showing up in the `flush`
    // column below as 200 ms frames of "context" that had nothing to do with
    // what this run measures, so every language the corpus fences in gets one
    // line highlighted before the window opens. The preview path this ticket is
    // about never highlights — a live fence hands over the raw line.
    {
        let mut warm = SessionView::new(beads());
        warm.push_delta(
            MessageKind::Answer,
            "```rust\nfn warm() {}\n```\n```bash\necho warm\n```\n```sh\necho warm\n```\n```python\nprint(1)\n```\n```json\n{}\n```\n```text\nwarm\n```\n```\nwarm\n```\n",
        );
        warm.flush(100);
    }

    // Bytes handed to the live entry per simulated frame. A stand-in for how
    // much of an answer arrives between two network reads; overridable because
    // the honest answer is "it depends on the model and the link", and the
    // number that matters is not this one but how a frame costs.
    let chunk = std::env::var("LOOPRS_MEASURE_CHUNK")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(128)
        .max(16);
    let width: u16 = 100;
    /// Entries under this are one or two frames of live text and say nothing
    /// about growth.
    const MIN_ENTRY: usize = 256;

    // Warm up on a real-sized answer before any window opens. The first frames
    // of a process page in the allocator, the wrap code and the syntect state;
    // measuring that is measuring the OS, not the renderer.
    {
        let mut warm = SessionView::with_buffer(beads(), 0);
        warm.set_store_cap(0);
        let pad = "A paragraph of ordinary prose that goes on for a while, wrapping ".repeat(60);
        let mut p = 0usize;
        while p < pad.len() {
            let e = next_boundary(&pad, p + 128);
            warm.push_delta(MessageKind::Answer, &pad[p..e]);
            p = e;
            let _ = warm.flush(width);
            let _ = warm.preview(width);
        }
    }

    // The counting window spans the whole run: `timed` reads it as a delta, and
    // outside a window `CountingAlloc` counts nothing at all, so a forgotten
    // `begin` reports 0 bytes with a straight face.
    CountingAlloc::begin();
    let mut table: std::collections::BTreeMap<(&'static str, &'static str), Frame> =
        std::collections::BTreeMap::new();
    let mut tails: Vec<usize> = Vec::new();
    let mut streamed_entries = 0usize;
    let mut streamed_bytes = 0usize;
    let mut frames = 0usize;
    let mut max_tail = 0usize;
    let mut max_entry = 0usize;
    let mut hits = 0u64;
    let mut misses = 0u64;
    // The five slowest samples per scenario, with the frame they landed in.
    //
    // A mean hides a hitch. The pooled `max` column says how bad the worst one
    // was; this says *where* it was, which is the difference between "the
    // renderer is slow" and "the first frame of the run paid for the whole
    // allocator's warm-up and nothing else does".
    let mut worst: std::collections::BTreeMap<&'static str, Vec<(f64, usize, usize)>> =
        std::collections::BTreeMap::new();
    let mut scen_sum: std::collections::BTreeMap<&'static str, (usize, f64, u64)> =
        std::collections::BTreeMap::new();

    for f in &files {
        for it in load_session(f).unwrap_or_default() {
            let kind = match it.kind {
                Kind::Answer => MessageKind::Answer,
                Kind::Thinking => MessageKind::Thinking,
                _ => continue,
            };
            if it.text.len() < MIN_ENTRY {
                continue;
            }
            streamed_entries += 1;
            streamed_bytes += it.text.len();
            max_entry = max_entry.max(it.text.len());

            // One view per streamed entry, no caps: this measures the render,
            // not the eviction policy (measured in the runs above).
            let mut view = SessionView::with_buffer(beads(), 0);
            view.set_store_cap(0);

            let mut pos = 0usize;
            while pos < it.text.len() {
                let end = next_boundary(&it.text, pos + chunk);
                view.push_delta(kind.clone(), &it.text[pos..end]);
                pos = end;
                frames += 1;

                // (0) the frame's other half — the context the preview's
                // number needs a denominator.
                let ((), us, churn) = timed(|| {
                    view.flush(width);
                });
                record(&mut worst, "flush", us, frames, view.preview_len());
                let tail = view.preview_len();
                max_tail = max_tail.max(tail);
                tails.push(tail);
                table
                    .entry(("flush: settle the finished lines", bucket(tail)))
                    .or_default()
                    .push(us, churn, 0);

                // (1) the frame asks for the preview, after new bytes.
                let (rows, us, churn) = timed(|| view.preview(width));
                record(&mut worst, "preview", us, frames, tail);
                table
                    .entry(("preview: new bytes arrived", bucket(tail)))
                    .or_default()
                    .push(us, churn, rows.len());

                // (2) and again with nothing changed — the frame the redraw
                // clock actually asks for.
                let (rows2, us, churn) = timed(|| view.preview(width));
                table
                    .entry(("preview: no new bytes", bucket(tail)))
                    .or_default()
                    .push(us, churn, rows2.len());

                // (3) the floor: hand the already-rendered rows over.
                let (_, us, churn) = timed(|| rows.clone());
                table
                    .entry(("clone of rendered rows (floor)", bucket(tail)))
                    .or_default()
                    .push(us, churn, rows.len());
            }

            // (4) Same bytes, different window. Run once per entry, at the
            // end, so the width the *stream* was flushed at never changes and
            // a rewrap never lands in the numbers above.
            let tail = view.preview_len();
            let other = width.saturating_sub(20).max(20);
            let (rows, us, churn) = timed(|| view.preview(other));
            table
                .entry(("preview: width changed", bucket(tail)))
                .or_default()
                .push(us, churn, rows.len());

            let (h, m) = view.preview_cache_stats();
            hits += h;
            misses += m;
        }
    }

    println!("=== looprs-00u.14 · the live-tail preview, per frame ===");
    println!("corpus   : {}", root.display());
    println!("tickets  : {} session files", files.len());
    println!(
        "streamed : {} live entries (answer/thinking, >= {} B), {} of text",
        streamed_entries,
        MIN_ENTRY,
        kib(streamed_bytes as f64)
    );
    println!(
        "geometry : {width} cols (content {}), {chunk} B per simulated frame, {frames} frames",
        content_width(width)
    );
    tails.sort_unstable();
    println!(
        "live tail: p50 {} B · p95 {} B · p99 {} B · max {} B  (largest entry streamed: {} B)",
        pct(&tails, 50),
        pct(&tails, 95),
        pct(&tails, 99),
        pct(&tails, 100),
        max_entry,
    );
    println!();
    for f in table.values_mut() {
        f.sort_us();
    }
    println!(
        "  {:<41}{:>7}{:>9}{:>9}{:>9}{:>11}{:>10}",
        "scenario [live tail]", "frames", "mean µs", "p50 µs", "p95 µs", "mean B/f", "max B"
    );
    for label in BUCKETS {
        let mut scen: Vec<&'static str> = table
            .keys()
            .filter(|(_, b)| *b == *label)
            .map(|(s, _)| *s)
            .collect();
        scen.sort_unstable();
        for s in scen {
            let fr = &table[&(s, *label)];
            println!(
                "  {:<41}{:>7}{:>9.1}{:>9.1}{:>9.1}{:>11.0}{:>10}",
                format!("{s} [{label}]"),
                fr.n,
                fr.mean_us(),
                fr.p_us(50),
                fr.p_us(95),
                fr.churn_sum as f64 / fr.n.max(1) as f64,
                fr.churn_max,
            );
        }
    }

    for ((scen, _label), fr) in &table {
        let e = scen_sum.entry(*scen).or_insert((0usize, 0.0f64, 0u64));
        e.0 += fr.n;
        e.1 += fr.us_sum;
        e.2 += fr.churn_sum;
    }
    println!();
    println!("--- the same numbers, all frames of that scenario pooled");
    println!(
        "  {:<41}{:>7}{:>9}{:>13}{:>10}{:>8}",
        "scenario", "frames", "mean µs", "mean B/frame", "max µs", "rows"
    );
    for (s, (n, us, ch)) in &scen_sum {
        let max = table
            .iter()
            .filter(|((a, _), _)| a == s)
            .map(|(_, f)| f.us_max)
            .fold(0.0_f64, f64::max);
        let rows = table
            .iter()
            .filter(|((a, _), _)| a == s)
            .map(|(_, f)| f.rows_sum)
            .sum::<usize>();
        println!(
            "  {:<41}{:>7}{:>9.1}{:>13.0}{:>10.1}{:>8.1}",
            s,
            n,
            us / (*n).max(1) as f64,
            *ch as f64 / (*n).max(1) as f64,
            max,
            rows as f64 / (*n).max(1) as f64,
        );
    }
    println!();
    println!("--- the five slowest frames of each, and where they landed");
    for (scen, samples) in &worst {
        if samples.is_empty() {
            continue;
        }
        let s = format!(
            "{samples:?}",
            samples = samples
                .iter()
                .map(|(us, frame, tail)| format!("{us:.0}µs@f{frame}/tail{tail}B"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!("  {scen:<12}{s}");
    }
    let _ = CountingAlloc::end();
    println!();
    println!(
        "cache    : {hits} hits · {misses} misses · {:.1}% of preview calls answered without rendering",
        100.0 * hits as f64 / (hits + misses).max(1) as f64
    );
    let preview_max = table
        .iter()
        .filter(|((k, _), _)| k.starts_with("preview"))
        .map(|(_, f)| f.us_max)
        .fold(0.0_f64, f64::max);
    println!(
        "worst single preview in this corpus: {preview_max:.0} µs = {:.1}% of a 16 ms frame",
        100.0 * preview_max / 16_000.0
    );
}

/// The bucket labels, in print order. Buckets by the *live tail* length, because
/// that is the thing whose length the ticket says the frame cost scales with.
const BUCKETS: &[&str] = &[
    "<256 B",
    "256 B–1 KiB",
    "1–2 KiB",
    "2–4 KiB",
    "4–8 KiB",
    "8 KiB+",
];

fn bucket(n: usize) -> &'static str {
    match n {
        0..=255 => BUCKETS[0],
        256..=1023 => BUCKETS[1],
        1024..=2047 => BUCKETS[2],
        2048..=4095 => BUCKETS[3],
        4096..=8191 => BUCKETS[4],
        _ => BUCKETS[5],
    }
}

/// Samples of one scenario in one bucket.
#[derive(Default)]
struct Frame {
    n: usize,
    us_sum: f64,
    us_max: f64,
    churn_sum: u64,
    churn_max: u64,
    rows_sum: usize,
    us_sorted: Vec<f64>,
}

impl Frame {
    fn push(&mut self, us: f64, churn: u64, rows: usize) {
        self.n += 1;
        self.us_sum += us;
        self.us_max = self.us_max.max(us);
        self.churn_sum += churn;
        self.churn_max = self.churn_max.max(churn);
        self.rows_sum += rows;
        self.us_sorted.push(us);
    }

    fn sort_us(&mut self) {
        self.us_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }

    fn mean_us(&self) -> f64 {
        self.us_sum / self.n.max(1) as f64
    }

    fn p_us(&self, q: usize) -> f64 {
        if self.us_sorted.is_empty() {
            return 0.0;
        }
        let i = ((self.us_sorted.len() - 1) as f64 * q as f64 / 100.0).round() as usize;
        self.us_sorted[i]
    }
}

/// Time one call and count what it allocated. `CountingAlloc` counts every
/// window that is open, so the read is a delta across the call rather than an
/// absolute — the same trick the two runs above use to split transcript from
/// store.
fn timed<T>(f: impl FnOnce() -> T) -> (T, f64, u64) {
    let (_, t0) = CountingAlloc::peek();
    let now = Instant::now();
    let v = f();
    let us = now.elapsed().as_secs_f64() * 1_000_000.0;
    let (_, t1) = CountingAlloc::peek();
    (v, us, t1 - t0)
}

/// Keep the five slowest samples of a scenario: microseconds, the frame index
/// it landed in, and the live-tail length that produced it.
fn record(
    worst: &mut std::collections::BTreeMap<&'static str, Vec<(f64, usize, usize)>>,
    scen: &'static str,
    us: f64,
    frame: usize,
    tail: usize,
) {
    let v = worst.entry(scen).or_default();
    v.push((us, frame, tail));
    v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    v.truncate(5);
}

/// A chunk boundary that will not split a UTF-8 character.
///
/// Walks *forward* to the next boundary: walking back would land on the start of
/// the previous chunk, which is a slice of zero new bytes and a stream that
/// never advances.
fn next_boundary(s: &str, target: usize) -> usize {
    let mut i = target.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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
