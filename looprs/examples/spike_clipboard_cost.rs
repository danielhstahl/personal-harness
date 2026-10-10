//! looprs-pdl.1: what the two clipboard transports *cost*, and what the three
//! candidate units for "Copied N" actually measure.
//!
//! ADR-0004 Q2 has to pick a default transport and a fallback, and the fallback's
//! price is the whole argument. `looprs-pdl.2` measured OSC 52's **capability**
//! per terminal (what lands, how fast, what prompts). What it did not measure is
//! the thing that decides the fallback: the per-copy cost of spawning a native
//! helper, next to the per-copy cost of putting the same bytes on the wire with
//! `crossterm`'s own OSC 52 writer. This example measures that, with the same
//! crate the app will use, on this machine, so the ADR cites a number and not a
//! guess.
//!
//! Q4 has to say whether `Copied N` counts **characters, cells or bytes**, and
//! the three differ. Half of that argument is arithmetic nobody has done out loud,
//! so the second half of this example runs a corpus through the same
//! `unicode-width` the renderer measures with and prints all three numbers per
//! string.
//!
//! Nothing here is production code and nothing is asserted: it is a measurement.
//! The assertions live in the app (see ADR-0005 for the cell/character
//! invariant this leans on).
//!
//! ```sh
//! cargo run -q --example spike_clipboard_cost | tee spikes/results/clipboard-cost.log
//! ```

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

use crossterm::clipboard::CopyToClipboard;
use unicode_width::UnicodeWidthStr;

/// Sizes the ladder runs, chosen to straddle what a real selection is: one line,
/// one answer, one `cargo test` failure dump, one whole transcript.
const SIZES: [usize; 4] = [128, 4 * 1024, 64 * 1024, 1024 * 1024];

/// Runs per size. Median, not mean: a single scheduler hiccup on a 5 ms sample is
/// a 100 % error, and the tail is the thing a user waits on anyway.
const RUNS: usize = 7;

fn main() {
    println!("=== clipboard transport cost (median of {RUNS} runs per size) ===");
    println!();
    println!("--- the OSC 52 path: crossterm's own writer, buffered (no terminal in the loop)");
    println!("    this is encode + format only: base64 and the escape framing, in-process");
    for size in SIZES {
        let payload = filler(size);
        let (median, best, worst, wire) = measure_osc52(&payload);
        println!(
            "    {size:>9} B payload -> {wire:>9} B on the wire ({:.1}x)  \
             median {median:>7.3} ms  best {best:>7.3} ms  worst {worst:>7.3} ms",
            wire as f64 / size as f64
        );
    }
    println!();

    for helper in helpers() {
        println!(
            "--- native helper: `{}` + `{}` (spawn, pipe, wait; then read back and compare)",
            helper.write.join(" "),
            helper.read.join(" ")
        );
        for size in SIZES {
            let payload = filler(size);
            match measure_helper(&helper, &payload) {
                Some((w_median, w_best, w_worst, r_median, verified)) => println!(
                    "    {size:>9} B payload   write median {w_median:>7.3} ms (best {w_best:>7.3}, worst {w_worst:>7.3})  \
                     + read-back median {r_median:>7.3} ms   {}",
                    if verified { "bytes match" } else { "MISMATCH" }
                ),
                None => println!(
                    "    {size:>9} B payload                    helper failed (see stderr)"
                ),
            }
        }
        println!();
    }

    println!("=== what N could mean: the same text in three units ===");
    println!("    cells are measured with unicode-width 0.2, the crate the renderer uses");
    println!();
    println!(
        "{:<34} {:>7} {:>7} {:>7}",
        "text", "chars", "cells", "bytes"
    );
    for text in corpus() {
        // The three candidate units, counted the only three ways they can be:
        let chars = text.chars().count(); // unicode scalar values
        let cells = UnicodeWidthStr::width(text); // terminal columns
        let bytes = text.len(); // UTF-8
        let label = text.replace('\n', "\\n");
        println!("{label:<34} {chars:>7} {cells:>7} {bytes:>7}");
    }
    println!();

    // The ZWJ family deserves its own row: the per-character widths do not sum to
    // the family's width, which is why "cells" is a property of the layout and not
    // of the text. ADR-0005's cluster rule is the app's answer to this.
    let family = "👩\u{200d}👩\u{200d}👦";
    let sum: usize = family
        .chars()
        .map(unicode_width::UnicodeWidthChar::width)
        .map(|w| w.unwrap_or(0))
        .sum();
    println!(
        "    ZWJ family {family}: per-character cell widths sum to {sum}, \
         the whole family measures {} cell(s)",
        UnicodeWidthStr::width(family)
    );
    println!(
        "    (so a cell count of a wrapped paragraph changes when the window is \
         resized; the text does not change. That is the whole argument about N.)"
    );
}

/// A payload with no structure to compress and, more importantly, with a marker at
/// the end so a truncated copy cannot be mistaken for a whole one.
fn filler(size: usize) -> Vec<u8> {
    let marker = b"<<END-PAYLOAD>>";
    let word = b"lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
    let keep = size.saturating_sub(marker.len());
    let mut v = Vec::with_capacity(size);
    while v.len() < keep {
        let take = (keep - v.len()).min(word.len());
        v.extend_from_slice(&word[..take]);
    }
    v.extend_from_slice(&marker[..size - v.len()]);
    v
}

/// Time `crossterm`'s OSC 52 writer producing the wire bytes for `payload`.
fn measure_osc52(payload: &[u8]) -> (f64, f64, f64, usize) {
    let mut times = Vec::with_capacity(RUNS);
    let mut wire_len = 0usize;
    for _ in 0..RUNS {
        let t = Instant::now();
        let mut wire: Vec<u8> = Vec::new();
        // The exact call the app would make: queue the command, let crossterm do
        // the base64 and the ST framing.
        let _ = crossterm::queue!(&mut wire, CopyToClipboard::to_clipboard_from(payload));
        wire_len = wire.len();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let (median, best, worst) = summarise(times);
    (median, best, worst, wire_len)
}

struct Helper {
    write: Vec<&'static str>,
    read: Vec<&'static str>,
}

fn helpers() -> Vec<Helper> {
    let mut out = Vec::new();
    if which("pbcopy").is_some() && which("pbpaste").is_some() {
        out.push(Helper {
            write: vec!["pbcopy"],
            read: vec!["pbpaste"],
        });
    }
    if which("wl-copy").is_some() && which("wl-paste").is_some() {
        out.push(Helper {
            write: vec!["wl-copy", "--no-clipboard"],
            read: vec!["wl-paste", "--no-newline"],
        });
    }
    if which("xclip").is_some() {
        out.push(Helper {
            write: vec!["xclip", "-selection", "clipboard"],
            read: vec!["xclip", "-selection", "clipboard", "-o"],
        });
    }
    if out.is_empty() {
        println!("    no native clipboard helper found on this host (pbcopy/wl-copy/xclip)");
    }
    out
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .map(std::path::Path::new)
        .find(|p| p.join(cmd).is_file())
        .map(|p| p.join(cmd))
}

/// Spawn the helper, pipe `payload` in, wait, then read the clipboard back with
/// the matching reader and compare. The read is timed separately because it is a
/// separate spawn, and it is the route a confirmation travels: a verification we
/// cannot run is a verification we cannot make.
fn measure_helper(helper: &Helper, payload: &[u8]) -> Option<(f64, f64, f64, f64, bool)> {
    let mut write_times = Vec::with_capacity(RUNS);
    let mut read_times = Vec::with_capacity(RUNS);
    let mut verified = true;
    for _ in 0..RUNS {
        let t = Instant::now();
        let mut child = Command::new(helper.write[0])
            .args(&helper.write[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        {
            let stdin = child.stdin.as_mut()?;
            stdin.write_all(payload).ok()?;
        }
        let status = child.wait_with_output().ok()?;
        write_times.push(t.elapsed().as_secs_f64() * 1000.0);
        if !status.status.success() {
            eprintln!(
                "    write helper failed: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            );
            return None;
        }
        // Read it back through a second spawn: the same route a user's paste takes.
        let r = Instant::now();
        let mut back = Command::new(helper.read[0])
            .args(&helper.read[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut buf = Vec::new();
        back.stdout.as_mut()?.read_to_end(&mut buf).ok()?;
        let _ = back.wait();
        read_times.push(r.elapsed().as_secs_f64() * 1000.0);
        if buf != payload {
            verified = false;
        }
    }
    let (a, b, c) = summarise(write_times);
    let (r, _, _) = summarise(read_times);
    Some((a, b, c, r, verified))
}

fn summarise(mut times: Vec<f64>) -> (f64, f64, f64) {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (times[times.len() / 2], times[0], times[times.len() - 1])
}

/// The corpus: the shapes that make characters, cells and bytes disagree.
fn corpus() -> Vec<&'static str> {
    vec![
        "Copied 1,284 characters",
        "漢字",
        "日本語のコード",
        "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F466}", // ZWJ family: 5 scalars
        "\u{65}\u{301}",      // decomposed e + combining acute: 2 scalars, 1 cell
        "\u{1F1EF}\u{1F1F5}", // regional indicator pair, renders as one flag
        "a\tb",
        "Café ☕ naïve",
        "const s: &str = \"日本語\";",
        "→ ← ↑ ↓",
    ]
}
