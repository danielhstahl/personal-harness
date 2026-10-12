# ADR-0010: The engine stays a child — keep `pi --mode rpc`, take the typed `bd` tools, and leave `rho` where it is

- **ID:** workspace-eqn
- **Status:** Accepted — 2026-10-12
- **Epic:** workspace-eqn (Assessment: keep `pi --mode rpc` child, or embed the rho Rust SDK as looprs' engine)
- **Decides for:** [`src/services/pi.rs`](../../src/services/pi.rs) (it stays, and this is why), the beads
  worker's `bd` surface, the `--tools` gating in
  [`src/session/beads/machine.rs`](../../src/session/beads/machine.rs), `Cargo.toml`
  (what is *not* in it, and what would have to be true to change that), and every
  future report that starts "the child process is slow"
- **Measured by:** [`spikes/pi_rpc_boundary_cost.py`](../../spikes/pi_rpc_boundary_cost.py)
  → [`spikes/results/pi-rpc-boundary-cost.log`](../../spikes/results/pi-rpc-boundary-cost.log),
  [`examples/spike_rpc_serde_cost.rs`](../../examples/spike_rpc_serde_cost.rs)
  → [`spikes/results/rpc-serde-cost.log`](../../spikes/results/rpc-serde-cost.log),
  [`spikes/rho_surface_cost.py`](../../spikes/rho_surface_cost.py)
  → [`spikes/results/rho-surface-cost.log`](../../spikes/results/rho-surface-cost.log),
  [`spikes/bd_tools_cost.py`](../../spikes/bd_tools_cost.py)
  → [`spikes/results/bd-tools-cost.log`](../../spikes/results/bd-tools-cost.log)
- **Reproduced by:** the one command at the bottom. Every number below is from
  those captures, on one Linux box (8 cpus), against the local
  `llama.cpp`/`halogen-qwen3.8-flash-next` endpoint and the real beads corpus
  (87 passes of this project's own session files). Rule 4 of ADR-0009 applies
  here without amendment: do not retype these numbers anywhere else.

---

## Context

The proposal was that looprs stops driving [`pi --mode rpc`](https://github.com/earendil-works/pi)
as a stdio child — one per beads pass, spawned at
[`src/session/beads/machine.rs`](../../src/session/beads/machine.rs) — and instead
links [the `rho` crate](https://crates.io/crates/rho), a Rust re-implementation of
this same harness shape, as a library. Two things were claimed for it: **(a)** typed
tool definitions for the `bd` interactions instead of the worker shelling `bd` out
of its bash tool, and **(b)** less overhead and lower latency from removing a Node
process and a serialization boundary.

The epic's instruction was that the process boundary gets measured rather than
assumed, that the benefit that is real gets tested on the cheapest path first, and
that rho gets priced as an unknown supply chain rather than evaluated on its README.
All three were done, and the headline is that the two claims did not survive being
separated from each other:

* **(b) is not where anyone thought it was, and it is tiny.** The whole boundary —
  fork, Node boot, JSONL framing, shutdown — is **308 ms** of a median pass of
  **3095 s**, i.e. **0.0100%**. Charged at the measured *p95* of a single round trip
  rather than its median it is **415 ms**, still a hundredth of one percent of a
  pass. There is
  no denominator under which "the child is slow" is a finding about this loop.
* **(a) is real, and it does not need the engine.** pi registers schema-validated
  tools from an extension today; [`spikes/bd_tools/bd_tools.ts`](../../spikes/bd_tools/bd_tools.ts)
  is a working one — five typed `bd` verbs, called by the real model on a real pass
  in this ticket's own A/B. The engine swap was never what was buying that.
* **And (a) buys less than it sounds like.** `bd` is a Go binary over a SQLite file.
  There is no `bd` library to link in *any* engine, so a typed tool removes the
  shell from the call path and nothing else; rho's engine would shell out to the
  same CLI. See §2.

Three premise corrections belong next to the numbers, because a ticket is a set of
premises and some of this one's were wrong in ways that changed the answer:

1. **"pulling rig, rig-memory, rmcp, iroh, syntect, merman"** — in 0.12.0 `rig` and
   `iroh` are **dev-dependencies**, i.e. built for rho's own tests and not by a
   library consumer. What a consumer actually links is leg 1 of the rho capture,
   and quoting the ticket's list would have over-stated the shipped graph.
2. **"no LICENSE file at the repo root although Cargo.toml declares MIT OR Apache-2.0"**
   — confirmed, and it is worse than the phrasing: the **published crates.io
   artifact** of all three crates (`rho`, `rho-engine`, `rho-harness-core`) contains
   no licence file either. A `license =` string in a manifest is a declaration; it
   is not a grant of rights you can hand to a lawyer. This, not performance, is the
   blocking objection (§3).
3. **the serialization boundary is not free on the other side either.** The engine
   that gets embedded still parses, still serialises to the provider, still writes a
   session. What the child adds is one JSON encode/decode of the *same* events, and
   that was measurable with looprs' own parser: **2.2 ms** to parse everything a
   median pass puts on its session file, at 8.90 µs/record and 556 MB/s.

### What looprs actually depends on (the inventory the migration would have to re-establish)

Read mechanically out of the tree, because "we only use a little of it" is not a
number. The client is ~250 lines in `src/services/pi.rs` plus the typed envelope in
`src/wire.rs`.

| Dependency | How much |
| --- | --- |
| RPC commands looprs sends | **4 of the 33 pi documents** — `prompt`, `steer`, `clear_queue`, `abort` |
| Wire spellings behaviour is keyed on | **40**, enumerated by `WIRE_INVENTORY` in `src/wire.rs` (`agent_start`, `agent_settled`, `message_update`, `tool_execution_start`, … plus the role and stop-reason sets) |
| Protocol semantics | `data.disposition` (`"handled"` ⇒ no run and no settle ever arrives), `agent_settled` as the *only* end-of-pass signal, `success: false` as a refusal rather than an error, correlation by client-assigned `id` |
| Process semantics | stdin-EOF-as-shutdown (ADR-0002 Q1), `kill_on_drop` so a failed pass cannot orphan a child, stderr never wired to the user's terminal, `--tools read,bash` gating for the planner |
| Test seam | `LOOPRS_PI_BIN` and the fakes in `tests/fixtures/` — every process-level test, plus the beads e2e spikes, runs against a fake child and costs no model call |
| Corpus coupling | `src/measure.rs` and three spikes replay pi **session files**; the scrollback, rewrap, highlighter and live-preview numbers are all pi-format-specific |

Any engine has to answer all six rows, not the first one. rho answers the first two
differently (below), has no equivalent of the third's `disposition`, and makes the
fifth seam meaningless — an in-process engine has no binary to point a fake at, so
each fake becomes a Rust trait impl compiled into the test binary, and the
`--tools`/argv-level behaviours those fakes prove stop existing as things to prove.

---

## The numbers

### 1. The boundary, measured (`pi-rpc-boundary-cost.log`)

| shape | spawn→first response | RTT median | RTT p95 | peak RSS | child CPU | stdin-EOF→exit |
| --- | --- | --- | --- | --- | --- | --- |
| worker `pi --mode rpc` | **299.1 ms** (min 297.0, max 313.9 over 7) | 0.13 ms | 10.79 ms | 122.2 MiB | 400 ms | 7.9 ms |
| planner `--tools read,bash` | 300.0 ms | 0.14 ms | 11.04 ms | 121.8 MiB | 400 ms | 8.0 ms |
| `--no-session` | 298.6 ms | 0.15 ms | 10.81 ms | 121.8 MiB | 400 ms | 8.7 ms |
| looprs' fake child (control) | 12.9 ms | 0.07 ms | 0.22 ms | 9.9 MiB | 0 ms | 2.9 ms |

Reading the table, in order: the three pi rows are the same number, so **none of it
is session bookkeeping** — it is Node booting and pi reading its resources. The fake
row says the harness itself waits 12.9 ms, so ~286 ms of the 299 is pi's own startup
and not looprs' doing; a different engine would pay its own version of that line and
the ticket's premise that removing the *boundary* removes it is wrong. And the p95
column (10.79 ms against a 0.13 ms median) is the one real tail on the seam — one
stalled reply per run, Node's event loop rather than the pipe — which is why the
budget below is quoted both ways.

Against the denominator, measured over 87 real passes of this project's own session
files rather than invented:

| | value |
| --- | --- |
| median pass wall clock | **3095 s** (p10 36 s, p90 7206 s) |
| model time per pass | 2441 s = **78.9%** of the pass |
| tool-and-gap time per pass | 409 s = 13.2% |
| assistant steps per pass | 114 |
| **per-pass boundary total** (spawn 299 + EOF 8 + 10 correlated commands) | **308 ms = 0.0100% of a pass**; 415 ms at the measured p95 |
| what crosses the boundary per pass | ~5.5k records, ~2.8 MB |
| what parsing that costs with looprs' own parser | 2.2 ms (session-file shape), 8.90 µs/record, 556 MB/s |

Token time dominates by a factor of **~8,000** (2441 s of model against 308 ms of
boundary). That is the denominator the ticket asked for, and it kills claim (b) on
its own: even if the embedded engine were *free*, it would hand back a hundredth of
the pass.

What an embedded engine does **not** remove: the model client, tool execution,
session writing, compaction, syntax highlighting, and the ~100 MiB of runtime those
need — rho's in-process engine measured **84.1 MiB** RSS (§3), pi's child measured
**122.2 MiB**. That is a real difference, and it is a *memory* argument, not a latency
one, and it lands inside looprs' own process instead of beside it.

### 2. The typed `bd` tools, measured (`bd-tools-cost.log`)

Cheapest-path arm: no engine change, a pi extension, the worker's existing `--tools`
gating. [`spikes/bd_tools/bd_tools.ts`](../../spikes/bd_tools/bd_tools.ts) registers
`bd_ready`, `bd_show`, `bd_claim`, `bd_close`, `bd_remember` with TypeBox schemas and
`exposure: "model"`.

* It works end to end on a real pass: asked "how many tickets are ready", the model
  called `bd_ready` (not `bash`) and answered from its result, on both repeats.
* The **per-call cost is `bd`, not the shell**. `bash -c 'true'` is **1.3 ms**; `bd ready`
  direct is **171.0 ms** and through a shell **170.9 ms** — indistinguishable, i.e. the
  whole layer a typed tool removes is inside the noise of the call it sits on. The
  extension's own `spawnSync` path measures **178.0 ms**, and a real `bd show` **186.9 ms**.
  A median pass issues **91.2** bash tool calls, of which **9.4** mention `bd`, so the
  arithmetic ceiling on "faster `bd` calls" from typing them is **single-digit
  milliseconds out of a pass measured in thousands of seconds**.
* What the arm genuinely buys is not latency: schema-validated arguments with no
  shell quoting, `bd`'s refusal (exit 1, "someone else holds the lease") returned as
  **data** instead of as a string the model has to read, and `--tools` gating — a
  worker can be given `bd_show` without being given `bash` at all.
* What it costs is **prompt**: a tool schema is tokens, on every request of the pass.
  Read out of a real run's first system message rather than estimated: the tool
  section is **197 bytes** for `read,bash` and **587 bytes** with the five `bd` tools —
  **+390 bytes, ≈97 tokens, on every model request**. The A/B on a two-request
  question came out **2,795 vs 3,802 input tokens** (+1,007), and a median pass is
  **114** assistant steps, which is how that gets priced.

So the tooling win is available today, at a prompt cost, with no engine change — and
the engine buys none of it. rho's own tool surface is eight Rust built-ins plus MCP
servers; there is no `--tools`-shaped, per-pass allowlist at all, only the policy
layer described in §3's embed note. Typing `bd` in rho means writing Rust into a
dependency looprs does not control; typing it here means a TypeBox schema in a file
next to the loop.

### 3. rho, priced as a dependency (`rho-surface-cost.log`)

| | rho consumer (scratch crate, cold `--release`) | looprs today, same machine, same method |
| --- | --- | --- |
| cold build | **210 s**, 472 crates compiled | **33 s**, 144 crates compiled |
| packages in the shipped graph | **432** | **138** |
| packages it would add that looprs does not ship | **326** (+294 net of what both already share) | — |
| artifact | 26.9 MiB | 8.2 MiB (**3.30×**) |
| source, published artifact | `rho` + `rho-engine` + `rho-harness-core`: **669 files, 120,799 LOC, 44 `unsafe`** | — |
| licence | **no licence file in any of the three published crates**, and GitHub's licence detector sees `NONE`, though `Cargo.toml` says `MIT OR Apache-2.0` | — |
| maintenance | 33 releases since 2026-09-02, 480 downloads, repo created 2026-08-19, 7 stars, 1 subscriber, and **823 of 865 commits (95.1%) by one human** — bus factor 1 | — |
| pins | one exact pin, `merman = "=0.8.0-alpha.6"` (an alpha, exact) | — |
| event vocabulary overlap with looprs' 40 wire spellings | **3 of 40** (`turn_start`, `turn_end`, `error`), out of 53 variants rho's protocol can name | — |

And the part of the proposal that was actually at issue — can it be embedded, and
what does the host get — was **done, not argued**: the spike builds a program that
calls `rho::platform::agent_engine`, runs `engine::run_turn(TurnRequest, presenter)`
in process, and drives one **tool-using** turn against the same local model. It
works:

```
engine init        12.1–15.8 ms         RSS after turn   86,152–86,344 KiB (≈84 MiB)
tools registered   8 (bash, edit, fd, read, rg, web_fetch, web_search, write)
events handed to the host   token ×7–106, thinking_token ×51–215, tool_started ×2,
                            tool_chunk ×1, tool_finished ×2, activity_* ×5–8
```

Two findings came out of running it that no reading of the README would have given,
and both are exactly the load-bearing list above:

* **There is no settle event.** Turn completion is the `run_turn` **return value**,
  and the events are a presenter callback with 22 methods. That is arguably a nicer
  API, and it is a different contract: looprs' entire loop advance is keyed on
  `agent_settled` arriving on a stream, including the "the child died mid-run" path
  where no return value ever arrives. Re-establishing that is a design change in the
  beads machine, not a shim.
* **Tools are gated by a permission system that defaults to asking a human.** With
  rho's defaults, the embedded run *refused* to run `echo`: "the `bash` tool requires
  permission appr…". The unattended equivalent is `config.permission.enabled = false`
  — one boolean that turns the whole gate off — plus, if you want per-tool rules,
  rho's rule-based policy (`load_policy`/`save_allow_rule`), which is a policy file
  to learn rather than a `--tools read,bash` flag to pass. looprs gates *per pass,
  per child, from argv*; rho gates by policy inside one long-lived engine.

The overlap row is the cost that the ticket did not price: of the **40** wire
spellings `src/wire.rs` keys behaviour on, rho's protocol can name **3**. Not the
spelling — the meaning. Every row of `WIRE_INVENTORY`, the generated
`docs/guide/wire-protocol.md`, `app::apply_pi`, the token window, and the tests that
make each row state what the harness does with the value get re-derived by hand.
Against a saving of 308 ms per pass, that is the wrong trade even before the
maintainer question is asked.

---

## Decision

### 1. The engine stays a child: `pi --mode rpc` stays, and `src/services/pi.rs` stays

Not because the seam is beautiful — it is ~250 lines with correlation, an EOF drain
and a fake-child test seam, and pi's own `docs/rpc.md` names RPC as the interface
for exactly this case ("other languages, isolated processes, IDEs"), reserving the
in-process SDK for Node/Bun hosts. looprs is not fighting a wrong interface. It
stays because the measured price of the seam is **0.0100% of a pass**, because the
seam is what makes the fake-child test arm possible at all, and because the process
boundary is where a harness that runs unattended for hours wants its blast wall:
`kill_on_drop` reaps a crashed engine, and an engine that panics inside looprs'
process takes the user's terminal with it — ADR-0006 exists to make leaving the
terminal clean, and it cannot help with a panic in its own process.

### 2. Take the typed `bd` tools, on the pi side, as the follow-up ticket

`spikes/bd_tools/bd_tools.ts` graduates from spike to shipped extension when that
ticket runs, and the ticket owes three things, all measured above: it must keep
`bd`'s CLI as the single source of truth (the schema mirrors argv; if `bd` changes,
the schema changes with it — a second vocabulary for the board is how looprs-037-style
constructions happen again); it must state the prompt cost in tokens per pass, not
per call, since schemas ride on every request of a median **114**-step pass; and it
must be gated with `--tools`, so the worker gets `bd_show`/`bd_claim`/`bd_close`
without `bash` where the prompt does not need a shell.

What it must **not** be sold as: a latency win, or a step toward embedding an engine.
The `bd` process is still there, and that is the honest shape of it.

### 3. `rho` does not enter the tree, and the door is priced instead of left ajar

Same rule ADR-0009 §2 arrived at from the other direction: **pricing an alternative
does not require shipping it**, which is why the rho leg lives in a scratch crate that
this spike builds in a temp dir and deletes. No `rho` dependency, and no `[features]`
door for it either — an unbuilt door rots.

The blocker is not the performance number, which is now known and is irrelevant. It
is the **bus factor and the licence**, stated plainly so nobody re-derives it:
95.1% of 865 commits by one person, a repo ~7 weeks old at the time of writing, 7
stars, 1 subscriber, 480 downloads, 120,799 LOC across three crates, and **no
licence file in the published artifact of any of them** while the manifest declares
`MIT OR Apache-2.0`. A harness that runs unattended for hours against a user's repo
does not take that in, and a declared-but-unshipped licence is not something to
resolve later.

### 4. The triggers that re-open this, written as numbers rather than as an opinion

Re-litigating this ADR requires re-running the bottom command **and** meeting one of:

1. **The denominator moves.** The boundary exceeds **1% of pass wall** — i.e. above
   ~31 s of a 3095 s pass, **~100×** today's 308 ms. Realistic route: a much faster
   local model makes token time stop dominating, so re-measure the *ratio*; the ms
   will barely move.
2. **The seam stops being honoured.** pi's RPC drops or changes one of the six rows
   in the inventory table (4 commands, 40 spellings, `disposition`, `agent_settled`,
   EOF-shutdown, `--tools`), or the compatibility budget for `src/wire.rs` stops
   being absorbable. At that point the engine question is forced, and it is forced
   by pi, not by rho.
3. **rho becomes adoptable as a dependency**, which is a maintenance test and not a
   benchmark: a licence file in the published artifact, ≥2 human contributors each
   carrying >5% of commits, ≥12 months of history, and a documented event vocabulary
   that carries a settle/turn-complete signal looprs can key a loop on. Today: 0, 1,
   7 weeks, and no.
4. **Passes become concurrent.** The boundary is priced per pass because looprs spawns
   one child per pass. If the loop ever runs N children at once on a constrained
   machine, re-measure: the cost is 299 ms × N of wall and 122 MiB × N of RSS, and the
   second is the one that bites. (rho in process is 84 MiB, so the memory argument
   for sharing one engine is real and belongs to whoever builds concurrency.)

---

## Consequences

**What this makes true.**

* The engine question is closed on evidence, and the two claims that motivated it are
  separated: latency is not available (0.0100% of a pass), typed tools are available
  and cost prompt tokens instead.
* The beads loop keeps its test seam. `LOOPRS_PI_BIN`, `tests/fixtures/fake_pi*.py|sh`,
  and the beads e2e spikes continue to cost no model call, which is the property that
  lets 840-odd tests run in CI.
* The typed-tool follow-up arrives with its price known and its limits written down
  in the file it is implemented in, instead of being discovered mid-migration.
* Anyone who wants to try rho again spends one command and a cold build, not a
  re-argument.

**What this makes harder, honestly.**

* looprs keeps a Node runtime as a de-facto requirement, and keeps paying ~300 ms and
  ~122 MiB per pass for the child. Those are now numbers in a decision rather than  vibes in an issue, and they are also now the *stated price* of the blast wall and
  the test seam — which is what they are, and not an oversight.
* The `bd` surface lives in two places once the extension ships: `bd`'s CLI and the
  extension's schema. The ADR forbids the third thing that would make that dangerous
  (a looprs-side vocabulary of its own), and the schema must stay a thin mirror.
* The typed tools add prompt tokens to every request of a pass. If that turns out to
  matter on a small-context local model, the fix is fewer tools or shorter
  descriptions, and the measurement is already written (§2).
* Two spikes are slow. `rho_surface_cost.py` costs a cold 215-second build of the
  alternative plus a 38-second build of looprs; `bd_tools_cost.py` spends two real
  model passes. Both say so in their own docstring, and both print `not measured`
  rather than guessing when their prerequisites are absent.

---

## What this forbids

1. **Do not open a migration ticket because this one closed.** Phase 2 is gated on a
   §4 trigger being met and named, with a fresh capture beside it. "The engine seems
   slow" is not a trigger; the ratio is.
2. **Do not add `rho` to `Cargo.toml` — not as an optional dependency, not behind a
   feature flag.** ADR-0009 rule 2, verbatim in effect: an unbuilt feature rots, and
   the rho leg here deliberately builds a *copy* of the idea in a scratch crate so it
   stays priced without being shipped.
3. **Do not cite latency for this decision after reading the table.** If someone
   re-litigates on "overhead", the answer is 308 ms, 415 ms at p95, out of 3095 s, and
   the capture is two clicks away.
4. **Do not let the typed `bd` tools grow a looprs-side vocabulary.** No derived
   statuses, no re-interpreted board states. The schema mirrors `bd` argv and returns
   what `bd` said — `bd`'s own read stays the one honest read (ADR-0007).
5. **Do not remove or narrow the fake-child seam** to accommodate a hypothetical
   in-process engine. The seam is the reason the process contract is testable at all;
   if an engine switch is ever proposed, it has to *add* a fake seam, not ask for
   this one's removal.
6. **Do not treat the licence gap as a formality.** "Cargo.toml says MIT OR
   Apache-2.0" is not a licence grant, and nothing upstream (the repo's three open
   issues are a release bot and two feature suggestions) is about fixing it. Nor
   does an upstream fix change the other four rows of §4's trigger 3.
7. **Do not retype these numbers into another page.** Link here, or link the capture.
   `docs_check.sh` catches contradictory knob defaults; it cannot catch a number
   quoted twice and retuned once.

---

## What this epic did not measure

Stated rather than left to be inferred, because an assessment that hides its gaps
gets re-run by someone else who assumes they were covered:

* **Native `bd` tools inside the rho engine (workspace-eqn.7) were not built.** The
  question was priced out rather than measured: the per-call cost is `bd`'s own
  process spawn (§2), which is the same in every host, so an in-engine version could
  only differ in the schema and gating story — and the gating story is what differs,
  since rho gates by policy inside one long-lived engine (`config.permission`,
  `load_policy`) where looprs gates per pass at argv (`--tools read,bash`). Writing
  custom tools into rho means writing Rust into a dependency looprs does not control
  or fork, which is a larger commitment than the ADR is deciding on. If §4 trigger 3
  ever fires, this becomes the first thing worth measuring, and it should be measured
  against the shipped extension, not against nothing.
* **Memory under concurrency.** One child per pass is looprs' shape, so the
  122 MiB-per-child figure has no multiplier today. The comparison to rho's 84 MiB
  *in-process* only becomes interesting if that shape changes; trigger 4 says what to
  re-measure when it does.
* **A macOS leg.** The repo's earlier captures were taken on Darwin; this one was
  taken on the Linux container the loop actually runs in, and says so in its header.
  Absolute ms differ per kernel; the *ratio* is the finding and it is four orders of
  magnitude, which is not kernel-sensitive.
* **Long-run stability of a shared engine.** An embedded engine that leaks or wedges
  after N passes is a different class of risk from a child that does, because there is
  no `kill_on_drop` and no blast wall. That was argued from ADR-0002 and ADR-0006, not
  measured, and it is the argument most likely to be re-litigated.

---

## How to re-litigate this in one command

```sh
# the boundary: spawn, round trip, RSS, one real streamed pass, and the corpus denominator
python3 spikes/pi_rpc_boundary_cost.py            | tee spikes/results/pi-rpc-boundary-cost.log
# what looprs' own parser charges for the JSONL
LOOPRS_MEASURE_CORPUS=~/.pi/agent/sessions/--Users-…-looprs-- \
  cargo run -q --release --example spike_rpc_serde_cost | tee spikes/results/rpc-serde-cost.log
# the alternative: cold build of a consumer of rho, its supply chain, and one embedded turn (~6 min)
python3 spikes/rho_surface_cost.py                | tee spikes/results/rho-surface-cost.log
# the control arm: typed bd tools, bash+bd vs typed, on the real model
python3 spikes/bd_tools_cost.py                   | tee spikes/results/bd-tools-cost.log
```

Each prints `not measured` with the reason when its prerequisite is missing — no
model endpoint, no corpus, no network for the supply-chain leg — so a partial
environment never silently becomes a passing answer.
