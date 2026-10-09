# ADR-0009: The highlighter is warmed on a side thread, and the regex engine stays pure Rust

- **ID:** looprs-00u.16
- **Status:** Accepted — 2026-10-09
- **Epic:** looprs-00u (Static docs site: what looprs does, how it differs, how to use it well — plus the improvements the reading turns up)
- **Decides for:** the `syntect =` line in `Cargo.toml`; the warm list in
  [`src/utils/md.rs`](../../src/utils/md.rs) (`WARM_PROBES`); the first lines
  `main()` runs; and the sentence in
  [`src/session/view.rs`](../../src/session/view.rs) that has been saying
  "syntect's syntax set — megabytes paid once on the first highlight" since
  before anyone had a number for it
- **Measured by:** [`spikes/regex_backend_cost.sh`](../../spikes/regex_backend_cost.sh)
  → [`spikes/results/highlight-load-cost.log`](../../spikes/results/highlight-load-cost.log):
  release profile, Darwin 25.6.0 arm64, 12 real `pi` passes = 626 fenced blocks
  = 5,959 highlighted lines, every latency block run twice, both regex backends
  built from the same tree. Reproduce with

  ```sh
  ./scripts/capture.sh regex_backend_cost.sh highlight-load-cost-$(date +%u) \
      bash spikes/regex_backend_cost.sh
  ```

---

## Context

The ticket that opened this claimed two unpriced costs sitting on the user's
critical path. Both were real things to ask about. One of them turned out not to
be where the ticket thought it was, and the other turned out to be about
megabytes of a different kind.

### What the highlighter actually does

`src/utils/md.rs` holds one `Highlighter` — a `SyntaxSet` and a `Theme` — behind
a `OnceLock`, initialised on first use. Everything that colours a fenced code
block goes through it: `line_render` opens a fence, asks `md::highlighter()
.start(lang)`, and `md::code_line` highlights each settled line.

Two costs hide behind that first call, and they are not the same cost:

1. **Deserialising the dump.** `SyntaxSet::load_defaults_newlines()` and
   `ThemeSet::load_defaults()` inflate the bundled `.packdump` files.
2. **Compiling the regexes.** syntect's `parsing::Regex` is a pattern string
   plus a `OnceCell`; the engine compiles on **first use** and caches the
   compiled program inside the `Regex`, which is owned by the loaded
   `SyntaxSet`. So this cost is per *pattern*, per *process*, and it outlives
   the `HighlightLines` that triggered it.

Point 2 is the whole ticket. It means the stall arrives in **the first block of
each language**, spread over whichever lines that block happens to reach — not
at startup, and not on line 1 of that block. (A first block that opens with a
comment line looks cheap on its first line and pays on line 4; the measurement
had to be aggregated per block or it would have hidden that.)

### The numbers (release, one machine, the real corpus)

| | load | `warm()` | whole-corpus highlight | steady per line (rust) | peak RSS after warm |
| --- | --- | --- | --- | --- | --- |
| **fancy** (shipped, pure Rust) | 0.9 ms / 431 KiB | **64.1 ms** | **482.6 ms** | **87.2 µs** | **44.8 MiB** |
| **onig** (alternative, C) | 0.9 ms / 431 KiB | 8.9 ms | 153.2 ms | 26.7 µs | 9.38 MiB |

and the stall those two costs produce when nothing has been warmed:

| fancy, cold | value | as a share of a 16 ms frame |
| --- | --- | --- |
| first-block stalls, pooled over the corpus's five fence languages | 58.4 ms | — |
| largest single first block (`bash`) | 20.9 ms | **131%** |
| worst single highlighted line | 13.2 ms | 83% |
| the same process **after `warm()`**: pooled / worst line | 5.7 ms / 2.3 ms | 36% / 14% |

Three things follow, and the first one is a correction to the ticket:

* **The load is small, and it was never on the startup path.** 0.9 ms, 430.6 KiB
  of Rust heap, 518.9 KiB peak, **+1.73 MiB of RSS** over a
  never-highlighted baseline — 75 syntaxes in 0.3 ms and 7 themes in 0.4 ms.
  Nothing in `main()`, the Router or the first frame calls the highlighter, so
  the `OnceLock` has never put this cost into startup: it puts it into the first
  *fenced block a session renders*, which in a beads pass is minutes in, while
  the user is reading an answer. "Time-to-first-paint" was the wrong fear;
  "a hitch in the middle of a sentence" was the right one.
* **The megabytes are real, but they are regexes, not syntaxes.** The load is
  0.4 MiB; the *compiled* state of four languages is 26.5 MiB of Rust heap
  (fancy) and the process is **44.8 MiB RSS** after warming versus 3.4 MiB
  before anything touches syntect.
* **The two backends are not equal and the difference is now priced**: ~3.1× the
  highlight time, 3.3× per line, 2.1× the allocator churn, 4.8× the RSS.

### The corpus this was measured over

626 fenced blocks in twelve real passes: **549 with a language tag** — `rust`
506 (92.2%), `bash` 25, `python` 12, `sh` 6, nothing else — and **77 with no
tag at all**, which take the plain-text path. Untagged fences cost 0.6 µs/line
because the plain-text syntax has no regexes to compile, which is why the
untagged 12.5% of the corpus is not a problem and never was.

---

## Decision

### 1. Warm the highlighter at startup, on a detached thread

`utils::md::warm(probes)` initialises the set and theme and then highlights a
probe block per language, which is what forces the lazy compiles.
`utils::md::spawn_warm_from_env()` starts it and `main()` calls it before the
terminal is touched. The numbers: **64.1 ms of CPU moved off the draw path buys
the disappearance of 52.7 ms of first-block stalls**, worst line 13.2 ms → 2.3
ms. It is memoised — a second call returns the first call's report and does
nothing (`warming_twice_is_one_warm_up`).

**Why a `std::thread` and not `tokio::task::spawn_blocking`.** tokio's
`BlockingPool::drop` calls `shutdown(None)`, which **joins** its worker
threads. A warm-up on the blocking pool is therefore a warm-up that a
`Ctrl-Q` taken inside the first second of the run has to wait out — the fix
would put its own cost on quitting. A detached thread dies with the process, and
quitting stays free. Same reasoning `services/journal.rs` and
`session/bash.rs` use their own threads for their readers.

### 2. The warm list is the corpus's list, and the probes are real blocks

`WARM_PROBES` is `rust`, `bash`, `python`, `sh` — **every** language tagged in
the twelve passes, and 100% of the tagged fences. Each entry carries a real
fenced block lifted out of the same passes rather than a `fn main() {}`:
compilation is per pattern, so the patterns a probe reaches are the only ones it
warms, and a toy probe warms a toy's worth. That is also why the warmed run is
not zero — the corpus's first rust block still costs 2.9 ms against 20.2 ms
cold, the tail being constructs the probe did not contain. The residual is
stated rather than wished away.

The list is not extended past the corpus. One more language costs, measured:
**yaml 3.4 ms + 0.9 MiB, javascript 16.3 ms + 6.3 MiB**, both of them paid at
startup to remove a stall in a case this corpus never produces. Add one when the
corpus shows it, with the cost in the commit message.

(*`toml` measures 0 ms / 14 KiB because the default syntax set contains no TOML
syntax at all. A ` ```toml ` fence is plain text today. If that ever matters, it
is a syntax-set question, not a warm-list one.*)

### 3. Keep `default-fancy`. The pure-Rust engine is bought with a 3.1× highlight tax, on purpose

The trade is written in `Cargo.toml` at the line itself, because that is the
file a person changing the backend edits, and no doc in `docs/` is ever opened
from a manifest by accident. The shape of it:

| | `default-fancy` (kept) | `default-onig` |
| --- | --- | --- |
| what it is | `fancy-regex`, pure Rust | oniguruma, a C library built by `onig_sys`'s build script |
| corpus highlight (warm) | 482.6 ms | 153.2 ms |
| steady per line (rust) | 87.2 µs | 26.7 µs |
| allocator churn per line | 38.2 KiB | 18.2 KiB |
| RSS after warm | 44.8 MiB | 9.38 MiB |
| first-block stalls, warmed | 5.7 ms | 1.5 ms |
| shipped-graph crates | 137 | 135 |
| release binary | 7,157,632 B | 6,781,424 B |
| build needs | `rustc` | `rustc` **and a working C toolchain** |

**Kept, for these reasons.**

* The stall the decision had to answer is already gone, and going was backend
  independent: 13.2 ms → 2.3 ms came from warming, not from the engine.
* What is left of fancy's tax lands **outside the redraw path**. Highlighting
  happens once per *completed* line when it settles into the store, not per
  frame; the redraw paints cached lines. A later block — 9.5 lines, the
  corpus's average (5,959 lines over 626 blocks) — is 0.77 ms = 4.8% of a
  frame, and streaming settles roughly one line per frame, which is 87 µs =
  0.5% of one.
* What switching would buy is real but sits in places this app can feel less
  badly than it looks: a 3× faster rewrap pass, ~35 MiB less RSS.
* What switching would cost is a **C toolchain in every build of a terminal
  tool**: `onig_sys` compiles oniguruma from source on the target. That is a
  compiler requirement on contributor machines, in `pi-beads/Dockerfile`, and
  on every cross-target — the exact class of thing ADR-0001 already had to
  write down a follow-up for once `nix`'s build script started emitting a
  future-incompatibility warning from a transitive C-adjacent crate.

**The trigger to revisit — written down so the decision has an expiry instead of
an opinion.** Switch to `default-onig`, citing this log, if any of these is
measured on the real corpus:

1. warm steady-state highlighting exceeds **250 µs/line** (≈2.9× today's 87.2 µs)
   — i.e. a single settling line costs more than 1.5% of a frame;
2. a first block of a warmed language exceeds **8 ms** in steady state, so the
   residual tail is visible again;
3. the highlighter's ~45 MiB RSS stops being a rounding error next to the
   retained-history ceiling — today `RETAINED_BYTES_WORST_CASE` is ~97 MiB, so
   this is roughly "when the highlighter is half the app's retention" — or
4. the corpus's language mix moves enough that warming costs **>250 ms** of
   startup CPU.

Re-run `spikes/regex_backend_cost.sh`; it prints all four against these
numbers in one pass.

### 4. The "megabytes" sentence gets its number, in the place that said megabytes

`RETAINED_BYTES_WORST_CASE`'s exclusion bullet now reads
*"syntect's compiled regex state — 26.5 MiB of Rust heap / 44.8 MiB of RSS for
the four languages the corpus fences in, paid once by the startup warm thread,
shared process-wide, and not history"*, with the load kept separate at 431 KiB.
A sentence that gestures at a magnitude is a magnet for decisions; the fix is to
give it the number, in the same place, with the log behind it.

### 5. `LOOPRS_WARM_HIGHLIGHT`

Default on; `0`/`off`/`no`/`false` turns it off, and the log says so. The knob
exists so the cold half of the before/after stays one environment variable
away. A fix you cannot turn off is a fix whose before/after you cannot retake
without checking out an old commit.

---

## Consequences

**What this makes true.**

* The frame that renders a fence no longer compiles regexes. The worst line in
  the corpus went from 13.2 ms to 2.3 ms; the largest first block from 20.9 ms
  (over a frame) to 2.9 ms.
* Startup latency is unchanged — measured, not assumed: nothing that was
  already on the startup path moved onto it, because the load was never there.
* The retained-history ceiling in `view.rs` now excludes a *number* rather than
  a vibe, and that number is cited to a capture.
* The backend choice is visible where it is made, and re-priceable by anyone in
  one command.

**What this makes harder, honestly.**

* **+44.8 MiB of RSS in every process that highlights**, whether the user ever
  sees a code fence or not. That is what warming buys, and it is what it
  costs: it is one-time and bounded (it does not grow with session length the
  way the scrollback does), but it is paid by default. `LOOPRS_WARM_HIGHLIGHT=0`
  hands the choice back.
* The warm thread competes for a core for 64 ms at startup, during a window in
  which `bd` and `pi` are also starting. On a single-core machine that is a
  real interaction and it was **not measured** — the machine this ran on has
  performance cores to spare. If that ever bites, the fix is a delay on the
  warm thread, not the removal of the warm.
* A fence in a language outside the four still pays its own compile — ~20 ms for
  a language as rich as rust, ~3 ms for a thin one. Today that is rarer than
  everything we cover; it is bounded; and it is now a *measurable* thing rather
  than a guess.
* `WARM_PROBES` is a maintained list. It will drift from the corpus. The
  coverage line printed in the log — "WARM_PROBES covers 549 of 549 tagged
  blocks" — is the check, and it is printed on every run rather than asserted
  in a test, because the right answer for a new language is a judgement about
  frequency and cost, not a red test.

---

## What this forbids

1. **Do not move the warm-up into `highlighter()`'s init or onto any path a
   frame can take.** The whole point is that the first highlighted draw must
   not be the one that pays. Warming from `line_render`, or lazily "when we
   know which language the user will use", re-creates the stall one layer
   down and makes it unattributable again.
2. **Do not add a `--features regex-onig` to `Cargo.toml` to "make the
   backend configurable".** The measurement runs from a *copy of the tree with
   one line changed* precisely so that the alternative stays priced without
   being shipped. An unbuilt feature is a feature that rots — that is the
   reason `[features] notify` carries the paragraph it does, and two rotting
   doors in one manifest is worse than one closed one. When the §3 trigger
   fires, the change is to the manifest line itself, with a number, not to a
   feature that CI never compiles.
3. **Do not compare backends with `CountingAlloc` alone.** The counting
   allocator wraps the Rust global allocator; oniguruma's compiled patterns
   are `malloc`'d in C and never pass through it. In Rust-heap terms fancy looks
   26× heavier and the difference is 4.8× — the RSS column in the capture is
   the only fair witness, and it is why the RSS block runs the test binary
   directly rather than through `cargo test`, under which the answer is
   cargo's own 87 MiB every time.
4. **Do not retype the numbers in this ADR into another page.** Link here, or
   link the capture. `docs_check.sh` catches contradictory knob defaults; it
   cannot catch a number quoted twice and retuned once.
5. **Do not "optimise" the warm list by dropping to `rust` only.** `bash` costs
   22.7 ms to warm and 20.9 ms of first-block stall to skip — skipping is a
   worse trade, and it is now visible as one.
6. **Do not let the residual (2.9 ms per first warmed block) be reported as
   "warm means free".** The probe does not contain every construct the corpus
   contains. That gap is the honest shape of the fix.

---

## How to re-litigate this in one command

```sh
LOOPRS_MEASURE_CORPUS=~/.pi/agent/sessions/--Users-…-looprs-- \
  ./scripts/capture.sh regex_backend_cost.sh highlight-load-cost-$(date +%u) \
    bash spikes/regex_backend_cost.sh
```

It prints: the load split into set and theme; cold and warmed first-block stalls
per language; steady-state per-line and per-block cost against a 16 ms frame;
the price of one more language on the warm list; max RSS for
nothing-highlighted / load-only / warmed, for **both** engines; and the
shipped-graph crate count and artifact size for both. The onig leg builds a
copy of this tree with the one manifest line changed — nothing else differs,
which is what makes the delta the engine.
