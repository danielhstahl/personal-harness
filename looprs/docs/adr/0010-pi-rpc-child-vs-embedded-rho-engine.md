# ADR-0010: The engine stays a child — keep `pi --mode rpc`, take the typed `bd` tools, and leave both `rho` libraries where they are

- **ID:** workspace-eqn
- **Status:** Accepted — 2026-10-12
- **Amended:** 2026-10-12 by [workspace-8jk.7](#amendment-0010-a--which-rho-workspace-8jk7) — naming
  only. Every mention of `rho` in this record that carries a claim now says *which* library it
  carries it about. No number, verdict or prohibition changed.
- **Epic:** workspace-eqn (Assessment: keep `pi --mode rpc` child, or embed the rho Rust SDK as
  looprs' engine — the epic's own wording, and the SDK it meant is `matthewyjiang/rho`'s `rho-sdk`)
- **Subjects (pinned, because this record is the page that got them confused):** three unrelated
  projects publish crates whose names begin `rho`, and two of them matter here:
  * **`matthewyjiang/rho`** — crates `rho-sdk` (the embeddable runtime a library consumer links),
    `rho-providers`, `rho-coding-agent` (the CLI), `rho-agent-tools`, `rho-tui-pty`. **The library
    this assessment was *requested* against**, and therefore what "the candidate" means below
    **except where a section pins a different one** — §3 prices `casonadams/rho`, and says so at
    its own heading.
  * **`casonadams/rho`** — crates `rho`, `rho-engine`, `rho-harness-core`. **The library §3 was
    *filed* against and measured**, and the one
    [crates.io/crates/rho](https://crates.io/crates/rho) serves.
  * **`pyrex41/rho`** — `rho-core`, `rho-tools`, `rho-server`, … — unrelated to this decision, the
    subject of no sentence below, and not banned by `scripts/dep_audit.py` (see its `NEAR_MISS`).

  Bare `rho` after this bullet means whichever library the section it appears in has named; a
  sentence whose claim belongs to exactly one of them now says which.
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
  → [`spikes/results/rho-surface-cost.log`](../../spikes/results/rho-surface-cost.log) for
  `casonadams/rho` (the crate set §3 was filed against) and
  [`spikes/results/matthewyjiang-surface-cost.log`](../../spikes/results/matthewyjiang-surface-cost.log)
  for `matthewyjiang/rho` / `rho-sdk` (the library this decision is about),
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
links [a `rho` crate](https://crates.io/crates/rho) as a library. **Which one the proposal meant is
the defect this record spent an epic on:** the crate that URL serves is `casonadams/rho`'s `rho`, and
"a Rust re-implementation of this same harness shape" was a description of *that* library — the one
whose published crates are `rho`, `rho-engine`, `rho-harness-core` — while the SDK this assessment
was requested against is `matthewyjiang/rho`'s `rho-sdk`, a different crate set with a different API.
The proposal named neither, and both readings were available to a reader. Two things were claimed for
it: **(a)** typed
tool definitions for the `bd` interactions instead of the worker shelling `bd` out
of its bash tool, and **(b)** less overhead and lower latency from removing a Node
process and a serialization boundary.

The epic's instruction was that the process boundary gets measured rather than
assumed, that the benefit that is real gets tested on the cheapest path first, and
that the candidate gets priced as an unknown supply chain rather than evaluated on its README.
All three were done, and the headline is that the two claims did not survive being
separated from each other:

* **(b) is not where anyone thought it was, and it is tiny.** The whole boundary —
  fork, Node boot, JSONL framing, shutdown — is **308 ms** of a median pass of
  **3095 s**, i.e. **0.0100%**. Charged at the measured *p95* of a single round trip
  rather than its median it is **415 ms**, still a hundredth of one percent of a
  pass. There is
  no denominator under which "the child is slow" is a finding about this loop.
* **(a) is real, and it does not need the engine.** pi registers schema-validated
  tools from an extension today; the five typed `bd` verbs in
  [`spikes/bd_tools_cost.py`](../../spikes/bd_tools_cost.py)'s arm are a working one —
  called by the real model on a real pass in this ticket's own A/B, and shipped since
  as [`extensions/bd_tools.ts`](../../extensions/bd_tools.ts). The engine swap was
  never what was buying that.
* **And (a) buys less than it sounds like.** `bd` is a Go binary over a SQLite file.
  There is no `bd` library to link in *any* engine, so a typed tool removes the
  shell from the call path and nothing else; an engine from either candidate family
  would shell out to the
  same CLI. See §2.

Three premise corrections belong next to the numbers, because a ticket is a set of
premises and some of this one's were wrong in ways that changed the answer:

1. **"pulling rig, rig-memory, rmcp, iroh, syntect, merman"** — in `casonadams/rho`
   0.12.0 `rig` and
   `iroh` are **dev-dependencies**, i.e. built for that library's own tests and not by a
   library consumer. What a consumer actually links is leg 1 of the
   [`casonadams` capture](../../spikes/results/rho-surface-cost.log),
   and quoting the ticket's list would have over-stated the shipped graph.
2. **"no LICENSE file at the repo root although Cargo.toml declares MIT OR Apache-2.0"**
   — confirmed, and it is worse than the phrasing: the **published crates.io
   artifact** of all three of `casonadams/rho`'s crates (`rho`, `rho-engine`, `rho-harness-core`) contains
   no licence file either. A `license =` string in a manifest is a declaration; it
   is not a grant of rights you can hand to a lawyer. This, not performance, is the
   blocking objection (§3). It is worth saying now, because it is the one premise
   correction the identity swap did *not* undo: `matthewyjiang/rho`'s `rho-sdk` ships
   no licence file in its artifact either, and that finding is scored in
   [trigger 3's memo](0010-trigger-3-dependency-maintenance-metrics.md) rather than restated here.
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

Any engine has to answer all six rows, not the first one. `casonadams/rho` answers the first two
differently (below), has no equivalent of the third's `disposition`, and makes the
fifth seam meaningless — an in-process engine has no binary to point a fake at, so
each fake becomes a Rust trait impl compiled into the test binary, and the
`--tools`/argv-level behaviours those fakes prove stop existing as things to prove.
(`rho-sdk` answers the first two differently again, and the fifth row the same way;
the row-by-row re-scoring is the capture indexed as the
[`matthewyjiang` inventory rescore](../../spikes/results/matthewyjiang-inventory-rescore.log),
and reading that row against this table is workspace-8jk.8's job, not this page's.)

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
need — `casonadams/rho`'s in-process engine measured **84.1 MiB** RSS (§3), pi's child measured
**122.2 MiB**. That is a real difference, and it is a *memory* argument, not a latency
one, and it lands inside looprs' own process instead of beside it.

### 2. The typed `bd` tools, measured (`bd-tools-cost.log`)

Cheapest-path arm: no engine change, a pi extension, the worker's existing `--tools`
gating. The spike's extension — now the shipped
[`extensions/bd_tools.ts`](../../extensions/bd_tools.ts) — registered `bd_ready`,
`bd_show`, `bd_claim`, `bd_close`, `bd_remember` with TypeBox schemas.

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
the engine buys none of it. `casonadams/rho`'s own tool surface is eight Rust built-ins plus MCP
servers; there is no `--tools`-shaped, per-pass allowlist at all, only the policy
layer described in §3's embed note (`rho-sdk` gates through its own policy/approval
surface rather than an argv allowlist — same shape of answer, different names, and
that leg is the [`matthewyjiang` capture](../../spikes/results/matthewyjiang-surface-cost.log)).
Typing `bd` into a candidate engine means writing Rust into a
dependency looprs does not control; typing it here means a TypeBox schema in a file
next to the loop.

### 3. `casonadams/rho`, priced as a dependency (`rho-surface-cost.log`)

> **Which library this table prices — pinned by workspace-8jk.7.** Every number in this table is
> **`casonadams/rho`** at 0.12.0 — crates `rho` + `rho-engine` + `rho-harness-core` — because that
> is the crate set the ticket was *filed* against, and it is **not** the library this decision was
> requested against. `matthewyjiang/rho`'s own four-leg run is
> [`spikes/results/matthewyjiang-surface-cost.log`](../../spikes/results/matthewyjiang-surface-cost.log),
> indexed beside this one in [spikes/README.md](../../spikes/README.md). Re-deriving these rows
> against `rho-sdk` is workspace-8jk.8's amendment; until it lands, **no row below may be quoted as
> `rho-sdk`'s price**, and no row below may be quoted as "`rho`'s price" without saying which of the
> two libraries it is a price for.

| | `casonadams/rho` consumer (scratch crate, cold `--release`) | looprs today, same machine, same method |
| --- | --- | --- |
| cold build | **210 s**, 472 crates compiled | **33 s**, 144 crates compiled |
| packages in the shipped graph | **432** | **138** |
| packages it would add that looprs does not ship | **326** (+294 net of what both already share) | — |
| artifact | 26.9 MiB | 8.2 MiB (**3.30×**) |
| source, published artifact | `casonadams/rho`'s `rho` + `rho-engine` + `rho-harness-core`: **669 files, 120,799 LOC, 44 `unsafe`** | — |
| licence | **no licence file in any of the three published crates**, and GitHub's licence detector sees `NONE`, though `Cargo.toml` says `MIT OR Apache-2.0` | — |
| maintenance | 33 releases since 2026-09-02, 480 downloads, repo created 2026-08-19, 7 stars, 1 subscriber, and **823 of 865 commits (95.1%) by one human** — bus factor 1 | — |
| pins | one exact pin, `merman = "=0.8.0-alpha.6"` (an alpha, exact) | — |
| event vocabulary overlap with looprs' 40 wire spellings | **3 of 40** (`turn_start`, `turn_end`, `error`), out of 53 variants `casonadams/rho`'s protocol can name | — |

And the part of the proposal that was actually at issue — can it be embedded, and
what does the host get — was **done, not argued**: the spike builds a program that
calls `casonadams/rho`'s `rho::platform::agent_engine`, runs `engine::run_turn(TurnRequest, presenter)`
in process, and drives one **tool-using** turn against the same local model. It
works:

```
# casonadams/rho's embed arm, as §3 was filed. The `rho-sdk` embed arm is a
# different program (spikes/embed_programs/matthewyjiang-rho-sdk.main.rs) and a
# different set of numbers; both are indexed in spikes/README.md.
engine init        12.1–15.8 ms         RSS after turn   86,152–86,344 KiB (≈84 MiB)
tools registered   8 (bash, edit, fd, read, rg, web_fetch, web_search, write)
events handed to the host   token ×7–106, thinking_token ×51–215, tool_started ×2,
                            tool_chunk ×1, tool_finished ×2, activity_* ×5–8
```

Two findings came out of running it that no reading of the README would have given,
and both are exactly the load-bearing list above — and both, like everything else in
this section, are findings about **`casonadams/rho`'s** API, which is not the API a
`rho-sdk` migration would be written against:

* **There is no settle event.** Turn completion is `casonadams/rho`'s `run_turn` **return value**,
  and the events are a presenter callback with 22 methods. That is arguably a nicer
  API, and it is a different contract: looprs' entire loop advance is keyed on
  `agent_settled` arriving on a stream, including the "the child died mid-run" path
  where no return value ever arrives. Re-establishing that is a design change in the
  beads machine, not a shim. (Read this row as *that library's*, and only that
  library's: `rho-sdk` does name terminal events — `Completed`, `Failed`, `Cancelled` — and
  disclaims delivering one on every path, which is a different objection with a
  different shape. The five sub-tests that keep the two claims apart are in
  [trigger 3's memo](0010-trigger-3-dependency-maintenance-metrics.md), not here.)
* **Tools are gated by a permission system that defaults to asking a human.** With
  that library's defaults, the embedded run *refused* to run `echo`: "the `bash` tool requires
  permission appr…". The unattended equivalent is `config.permission.enabled = false`
  — one boolean that turns the whole gate off — plus, if you want per-tool rules,
  its rule-based policy (`load_policy`/`save_allow_rule`), which is a policy file
  to learn rather than a `--tools read,bash` flag to pass. looprs gates *per pass,
  per child, from argv*; `casonadams/rho` gates by policy inside one long-lived engine
  (`rho-sdk` gates through its own policy, approval handler and per-request tool
  visibility — the same answer in shape, a different surface, and the reason the
  gating row had to be re-run rather than re-quoted).

The overlap row is the cost that the ticket did not price: of the **40** wire
spellings `src/wire.rs` keys behaviour on, `casonadams/rho`'s protocol can name **3**. Not the
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

**Shipped by workspace-01z as [`extensions/bd_tools.ts`](../../extensions/bd_tools.ts),
with the loop's side of it in [`src/services/bd_tools.rs`](../../src/services/bd_tools.rs)
and the knob family indexed at
[configuration → the beads worker's `bd` verbs](../guide/configuration.md#the-beads-workers-bd-verbs).**
The three obligations below are what that ticket owed. The numbers it owed them with
are in the capture named at the top for the decision, and in
[`spikes/results/bd-tools-cost-shipped.log`](../../spikes/results/bd-tools-cost-shipped.log)
for the surface as shipped — that second capture is the same driver run against the
shipped file and the allowlists the loop actually passes, and it is the one that prices
the cost per *pass*. This ADR does not restate either set, per its own rule 7.

The ticket owes three things, all measured above: it must keep
`bd`'s CLI as the single source of truth (the schema mirrors argv; if `bd` changes,
the schema changes with it — a second vocabulary for the board is how looprs-037-style
constructions happen again); it must state the prompt cost in tokens per pass, not
per call, since schemas ride on every request of a median **114**-step pass; and it
must be gated with `--tools`, so the worker gets `bd_show`/`bd_claim`/`bd_close`
without `bash` where the prompt does not need a shell.

What it must **not** be sold as: a latency win, or a step toward embedding an engine.
The `bd` process is still there, and that is the honest shape of it.

### 3. Neither `rho` enters the tree — not `casonadams/rho`, not `matthewyjiang/rho` — and the door is priced instead of left ajar

Same rule ADR-0009 §2 arrived at from the other direction: **pricing an alternative
does not require shipping it**, which is why the candidate leg lives in a scratch crate that
this spike builds in a temp dir and deletes. No dependency from either family, and no `[features]`
door for either of them either — an unbuilt door rots. The ban in
`scripts/dep_audit.py --gate` is therefore a set of **two** families, and the first version of it
carried only the casonadams crate set, which is how a `rho-sdk` dependency walked through a gate
whose only job was to fail it (workspace-8jk.2).

The blocker is not the performance number, which is now known and is irrelevant. It
is the **bus factor and the licence**, stated plainly so nobody re-derives it — and
pinned to the library whose numbers these are. "the rho crate", "a Rust
re-implementation of this same harness shape" and "rho does not enter the tree" were
all written about `casonadams/rho` and read, for an epic, as if they had been written
about `matthewyjiang/rho`. The sentence as filed, about **`casonadams/rho`**: 95.1% of
865 commits by one person, a repo ~7 weeks old at the time of writing, 7
stars, 1 subscriber, 480 downloads, 120,799 LOC across three crates, and **no
licence file in the published artifact of any of them** while the manifest declares
`MIT OR Apache-2.0`. A harness that runs unattended for hours against a user's repo
does not take that in, and a declared-but-unshipped licence is not something to
resolve later. **`matthewyjiang/rho`** (`rho-sdk`) fails the same four conditions, on
different numbers and with a different settle story —
[the score, the definitions and the dates each row can next move are in trigger 3's
memo](0010-trigger-3-dependency-maintenance-metrics.md), which is the only place in
this record that carries them. The decision therefore covers both libraries, and no
longer depends on the numbers in the paragraph above being the right library's.

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
   by pi, not by either candidate.
3. **A `rho` becomes adoptable as a dependency** — the sentence as filed said
   **"rho becomes adoptable as a dependency"**, singular, and it took an epic to learn that there
   are two libraries that answer to that name and that the trigger has to be run against each of
   them. It is a maintenance test and not a
   benchmark. It has four conditions — a licence in the published artifact, more than one
   human carrying the repo, twelve months of history, and a documented settle/turn-complete
   signal looprs can key a loop on — and **the definitions behind all four, and the score
   for both libraries that answer to the name `rho`, live in
   [ADR-0010 trigger 3 as testable metrics](0010-trigger-3-dependency-maintenance-metrics.md)**:
   the denominator behind "share of commits", whether a drive-by commit counts toward the
   ≥2, which clock the twelve months run on, and the five sub-tests that make "a documented
   settle signal" mean *a loop advance can be keyed on it* rather than *the spelling exists*.
   This page deliberately carries no numbers for this trigger, and no `Today: …` row — the
   four it used to print were measured against the wrong library, and rule 7 below is why
   they are not simply replaced with four new ones. **Re-score it rather than re-argue it:**
   `python3 spikes/dependency_maintenance_metrics.py` reads every input off the public record
   in seconds — no build, no model call, no endpoint — and prints one verdict per definition
   per candidate with the URL each number was read from, plus the earliest date a failed row
   can next be worth re-running.
4. **Passes become concurrent.** The boundary is priced per pass because looprs spawns
   one child per pass. If the loop ever runs N children at once on a constrained
   machine, re-measure: the cost is 299 ms × N of wall and 122 MiB × N of RSS, and the
   second is the one that bites. (`casonadams/rho` in process is 84 MiB per §3's embed arm, so the
   memory argument
   for sharing one engine is real and belongs to whoever builds concurrency. `rho-sdk`'s own
   embed arms measured the RSS level and the kernel peak per arm in the
   [`matthewyjiang` capture](../../spikes/results/matthewyjiang-surface-cost.log); that is the
   number a concurrency proposal gets compared against, not the 84 MiB.)

### 5. Phase 2 (`workspace-bt8`) is closed unworked — the record of what "no" declined to build

The costing was done before the decision rather than after it: a second epic,
`workspace-bt8`, held the seven tickets the GO branch would have needed, filed and
parked and gated on this record, with the instruction that if this ADR came back
"stay on pi RPC" the epic was to be closed unworked — and that closing it unworked was
the good outcome, not the lazy one.

It came back that way, so on 2026-10-12 the epic and all seven of its children were
closed unworked with this record as the close reason. Nothing from it was built, and
the tempting half-measures are worth naming, because they are the ones that rot:

| the parked ticket | why it does not get built "anyway, just the seam" |
| --- | --- |
| an `EngineRuntime` trait over the pi RPC transport, behaviour-preserving | a trait with exactly one implementation is §3's unbuilt door wearing a different hat: it pays the indirection in the file that ADR-0002 says is load-bearing and buys an abstraction no second implementation has ever been tested through |
| an engine backend for either `rho` library behind `--features rho` | forbidden below, rule 2; and the default-off build of it would be compiled by nobody, which is how a feature rots |
| porting the beads machine off `PiRpc` | the machine's control logic — `agent_settled` as the only end-of-pass signal, the serial tag, per-pass `--tools` gating — is correct and would be rewritten against an engine nothing runs |
| an in-tree fake engine plus a transport contract suite | the shell fake is the seam every process-level test stands on; rule 5 says a second engine would have to *add* a fake, not ask for this one's removal |
| engine selection: `LOOPRS_ENGINE`, status band, docker image, docs | a knob with one valid value is a knob to be miscorrected, and it would be documented in the same pages that today say the engine is the pi child |
| re-baselining every corpus-justified constant and generated page | the largest ticket in the set, and it is pure cost unless an engine actually changes — this repo's constants are justified by committed logs, and moving engines invalidates the logs |
| shipping the typed `bd` tool surface | **the exception: already shipped**, by decision 2 above (`extensions/bd_tools.ts`), which is the proof that this ticket never needed an engine at all |

The decision now has a check behind it, not only prose. `scripts/dep_audit.py --gate`
— a step of `scripts/check.sh`, and therefore of CI — fails if a crate from either
`rho` family appears in `Cargo.toml` **or in `Cargo.lock`**, direct, renamed, optional,
feature-gated or transitive: casonadams/rho's `rho`, `rho-engine`, `rho-harness-core`
(the set §3 above was filed against) **and** matthewyjiang/rho's `rho-sdk`,
`rho-providers`, `rho-coding-agent`, `rho-agent-tools`, `rho-tui-pty` (the library this
decision was requested against). The gate's list is name-exact per family rather than a
`rho-*` prefix, so an unrelated third project's crate of that shape is not caught — `pyrex41/rho`
publishes `rho-core`, `rho-tools` and friends, none of which is in scope here; the
check prints which rule it applied. Note that the enforcement list was corrected first:
`--gate` carried only the casonadams set until workspace-8jk.2, which is why a
`rho-sdk` dependency passed it; the record's own identity and §3 attribution are
amended by workspace-8jk.7 (naming, every page that carries a claim) and
workspace-8jk.8 (§3's rows and decision 3, re-derived). A rule that lives only in a
decision record is re-litigated by the next person with a benchmark; one that fails a gate is not — provided the gate
names the right crate, which is what `dep_audit.py --self-test` now checks on every run
of this step.

Re-opening this work costs more than a wish: a §4 trigger named in the ticket, a fresh
capture beside it, an amendment to this ADR, and then `bd show workspace-bt8.1` …
`.7`, the seven parked tickets, which are closed rather than deleted and still carry
the full text of the costing.

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
* Anyone who wants to try either `rho` library again spends one command
  (`LOOPRS_SPIKE_RHO_ID=matthewyjiang` or `=casonadams`) and a cold build, not a
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

1. **Do not open a migration ticket because this one closed.** Phase 2 is
   `workspace-bt8`, and §5 records that it was closed unworked rather than worked.
   Re-opening it means re-opening *it*, with a §4 trigger met and named and a fresh
   capture beside it, not filing a new ticket that has not paid for the evidence.
   "The engine seems slow" is not a trigger; the ratio is.
2. **Do not add a crate from either `rho` family to `Cargo.toml` — not as an optional
   dependency, not behind a
   feature flag.** That is `matthewyjiang/rho`'s set (`rho-sdk`, `rho-providers`,
   `rho-coding-agent`, `rho-agent-tools`, `rho-tui-pty`) **and** `casonadams/rho`'s set
   (`rho`, `rho-engine`, `rho-harness-core`); naming one of them and not the other is how
   this record got its identity wrong in the first place, and `scripts/dep_audit.py --gate`
   now fails on both. ADR-0009 rule 2, verbatim in effect: an unbuilt feature rots, and
   the candidate leg here deliberately builds a *copy* of the idea in a scratch crate so it
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
   Apache-2.0" is not a licence grant, and nothing upstream — `casonadams/rho`'s three open
   issues are a release bot and two feature suggestions — is about fixing it. Nor
   does an upstream fix change the other conditions of §4's trigger 3 — and "fix"
   now means *the scored row moves*, in the capture, not a page remembers it: the
   licence a consumer receives is the one inside the artifact of the crate they link,
   and the intended library has that gap too
   ([trigger 3 as testable metrics](0010-trigger-3-dependency-maintenance-metrics.md)).
7. **Do not retype these numbers into another page.** Link here, or link the capture.
   `docs_check.sh` catches contradictory knob defaults; it cannot catch a number
   quoted twice and retuned once.

---

## What this epic did not measure

Stated rather than left to be inferred, because an assessment that hides its gaps
gets re-run by someone else who assumes they were covered:

* **Native `bd` tools inside a candidate engine (workspace-eqn.7) were not built** —
  "the rho engine" was the ticket's wording, and the engine it meant was
  `casonadams/rho`'s; the same question has never been asked of `rho-sdk` in that shape. The
  question was priced out rather than measured: the per-call cost is `bd`'s own
  process spawn (§2), which is the same in every host, so an in-engine version could
  only differ in the schema and gating story — and the gating story is what differs,
  since `casonadams/rho` gates by policy inside one long-lived engine (`config.permission`,
  `load_policy`) where looprs gates per pass at argv (`--tools read,bash`). Writing
  custom tools into either library means writing Rust into a dependency looprs does not control
  or fork, which is a larger commitment than the ADR is deciding on. If §4 trigger 3
  ever fires, this becomes the first thing worth measuring, and it should be measured
  against the shipped extension, not against nothing.
* **Memory under concurrency.** One child per pass is looprs' shape, so the
  122 MiB-per-child figure has no multiplier today. The comparison to `casonadams/rho`'s 84 MiB
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

## Amendment 0010-A — which `rho` (workspace-8jk.7)

- **Date:** 2026-10-12
- **Ticket:** workspace-8jk.7, under the epic that found the mis-identification
  (workspace-8jk). **workspace-8jk.8** owns the separate, larger job: re-deriving §3's rows and
  decision 3 against `rho-sdk`. This section is the naming sweep only.
- **Amends:** the identity of every `rho` mentioned in this record, and in the pages that repeat
  its conclusion — `docs/README.md`, `docs/testing.md`, `docs/guide/contributing.md`,
  `spikes/README.md`, `spikes/rho_surface_cost.py`, `spikes/dependency_maintenance_metrics.py`,
  `scripts/dep_audit.py`. Two were checked and needed nothing: `docs/guide/beads-loop.md` cites
  §2 and §4 of this record and carries no bare `rho` claim, and `scripts/check.sh` already named
  both families and their crate sets after workspace-8jk.2. **No number, verdict or prohibition
  changed.**

**What was wrong.** Every page that repeated this record's conclusion repeated it as a word:
"`rho` does not enter the tree", "the rho crate", "a Rust re-implementation of this same harness
shape". A bare word is not an identity. Three unrelated projects publish crates whose names begin
`rho`; two were in play here; the numbers in §3 are one library's and the decision was requested
about the other. A conclusion written as a bare word can be attached to whichever library the next
reader has in mind — which is what happened, and it survived a re-runnable spike because the spike's
own header asserted the wrong owner/repo too.

**What changed (wording, at each claim-bearing mention):**

* the header now carries a **Subjects** bullet naming all three projects and their crate sets, and
  licenses the bare `rho` used later on the page;
* **Context** says which crate `crates.io/crates/rho` actually serves and which SDK the assessment
  was requested against, instead of leaving "the `rho` crate" ambiguous;
* premise corrections 1 and 2 name `casonadams/rho` as the library whose `dev-dependencies` and
  licence gap they describe, and record that the licence finding is the one the identity swap did
  *not* undo;
* §3 is headed and fenced as `casonadams/rho`'s, crate set and capture named, with the
  `matthewyjiang/rho` capture linked beside it and an explicit ban on quoting a §3 row as
  `rho-sdk`'s price;
* §3's two embed findings (no settle event, permission-gated tools) are attributed to
  `casonadams/rho`'s API, with the different `rho-sdk` shape named rather than glossed;
* decision 3 is now "neither `rho` enters the tree", names both families, and says which library
  the blocker paragraph's numbers belong to;
* trigger 2 says the seam is forced by pi, not by either candidate; trigger 3 records that it is
  one test **per library**, and trigger 4 names which library the 84 MiB figure is;
* forbids 2 names both crate sets instead of the word, and forbids 6 names whose open issues it
  means;
* §5's gate paragraph names `pyrex41/rho` as the third project the ban deliberately does not catch;
* `docs/README.md` now carries the rule that keeps this class of error out of future records — a
  priced third-party dependency is named by `owner/repo` **plus the published artifact name**,
  at minimum in the capture header — and `docs/guide/contributing.md` points at it rather than
  restating it. The process version of that rule (who checks it, and when) is workspace-8jk.10's
  to write, and is deliberately not duplicated here.

**What stands, unchanged:** every measurement in this record and in its captures; the 0.0100%-
of-a-pass denominator and what it kills; decision 1 and decision 2; the §5 record of the epic closed
unworked; `scripts/dep_audit.py`'s behaviour (it already banned both families after workspace-8jk.2);
and the verdict itself — **neither library enters the tree**, now stated as the two-library claim it
always needed to be.

**What this amendment deliberately does not do:** it re-prices nothing, quotes no new measurement,
and adds no number, per forbids 7. Where it names a different library it links the capture that
measured it. And it does not pretend the sweep was exhaustive by accident: `rho` kept bare in this
tree means looprs' own naming — the Greek-alphabet filler in `src/main.rs` and
`src/app/tests/scrollback_band.rs`, the crate-path `rho::…` calls inside
`spikes/embed_programs/casonadams-rho.main.rs`, the local `rho` bindings in
`spikes/embed_programs/matthewyjiang-rho-sdk.main.rs`, and the `rho leg:` line label in the
committed captures, each of whose header already names its subject.

**What would keep it fixed.** A `docs_check` step that fails a bare `rho` in a
claim-bearing sentence in `docs/adr/*.md` and `docs/README.md` is filed as
`workspace-8jk.12` and was deliberately not built here: a naming lint with false positives
on looprs' own identifiers gets disabled inside a week, which is worse than no lint. The
rule it would enforce is written once, in the blockquote above the decision-record table in
`docs/README.md`.

---

## How to re-litigate this in one command

```sh
# the boundary: spawn, round trip, RSS, one real streamed pass, and the corpus denominator
python3 spikes/pi_rpc_boundary_cost.py            | tee spikes/results/pi-rpc-boundary-cost.log
# what looprs' own parser charges for the JSONL
LOOPRS_MEASURE_CORPUS=~/.pi/agent/sessions/--Users-…-looprs-- \
  cargo run -q --release --example spike_rpc_serde_cost | tee spikes/results/rpc-serde-cost.log
# the alternative: cold build of a consumer of the library named by the id, its
# supply chain, and one embedded turn (~6 min). workspace-8jk.1 made the identity
# a parameter: `--list` prints the targets, and the header of the capture names
# the owner/repo and every crate@version the run measured. matthewyjiang/rho
# (rho-sdk) is the library this ADR is about; casonadams/rho is what §3 as filed
# priced, and is retained at results/rho-surface-cost.log.
LOOPRS_SPIKE_RHO_ID=matthewyjiang python3 spikes/rho_surface_cost.py \
  | tee spikes/results/matthewyjiang-surface-cost.log
# the control arm: typed bd tools, bash+bd vs typed, on the real model
python3 spikes/bd_tools_cost.py                   | tee spikes/results/bd-tools-cost.log
# trigger 3 alone — the maintenance test, both libraries that answer to the name
# `rho`, scored against the written definitions in seconds: no build, no model
# call, and every number printed with the public record it was read from
python3 spikes/dependency_maintenance_metrics.py  | tee spikes/results/dependency-maintenance-metrics.log
```

Each prints `not measured` with the reason when its prerequisite is missing — no
model endpoint, no corpus, no network for the supply-chain leg — so a partial
environment never silently becomes a passing answer.
