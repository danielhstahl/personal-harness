# ADR-0010: The engine stays a child — keep `pi --mode rpc`, take the typed `bd` tools, and leave both `rho` libraries where they are

> **Identity correction — read this before any number below (amendment 0010-B).**
>
> **§3's table and decision 3's blocker were measured against `casonadams/rho`** — the crates `rho`,
> `rho-engine` and `rho-harness-core`, which is also what
> [crates.io/crates/rho](https://crates.io/crates/rho) serves. **The library this assessment was
> requested against is `matthewyjiang/rho`** — the crates `rho-sdk` (what a library consumer links),
> `rho-providers` and `rho-coding-agent`. They are unrelated projects with a colliding name, written
> against different APIs, and a third unrelated pair — `rho-core` / `rho-tools`, published by
> `pyrex41/rho` — is on crates.io too and appears in this record only as the near-miss the gate
> deliberately does not ban. A bare word was used where an identity was required, and the numbers of
> one library were filed as the price of the other.
>
> Amendment **[0010-A](#amendment-0010-a--which-rho-workspace-8jk7)** put a name in every sentence
> that carries a claim. Amendment **[0010-B](#amendment-0010-b--the-re-measurement-workspace-8jk8)**
> re-measured the substance: [§3-B](#3-b-the-same-rows-measured-against-matthewyjiangrhos-rho-sdk)
> prices `rho-sdk` on §3's own rows, [decision 3](#decision) is restated on the justification the
> re-measurement actually supports, and trigger 3 below now points at definitions instead of at
> numbers. **The verdict is unchanged — neither library enters the tree — and the justification is
> not.** The re-measured candidate is *cheaper to link than looprs itself*, so the supply-chain-weight
> half of §3 does not survive being pointed at the right library; what carries the ban is the
> maintenance record, scored against written definitions rather than quoted from a paragraph.
>
> The `casonadams/rho` capture is **retained and labelled**, not deleted. It is what was measured,
> about the library it names, and it still reproduces.

- **ID:** workspace-eqn
- **Status:** Accepted — 2026-10-12
- **Amended:** twice on 2026-10-12, in place, with the accepted date above untouched because the
  decision was made then and both amendments say what changed against it:
  * **0010-A** — [workspace-8jk.7](#amendment-0010-a--which-rho-workspace-8jk7), **naming only**.
    Every mention of `rho` in this record that carries a claim now says *which* library it carries it
    about. No number, verdict or prohibition changed. The durable process fix that came out of the
    mis-identification — dependency assessments name `owner/repo` plus the artifact, and captures
    self-identify what they measured — is
    [contributing → assessing a third-party dependency](../guide/contributing.md#assessing-a-third-party-dependency)
    (workspace-8jk.10).
  * **0010-B** — [workspace-8jk.8](#amendment-0010-b--the-re-measurement-workspace-8jk8),
    **substance**. §3's rows and decision 3's blocker re-derived against the library they were meant
    to price, the six-row inventory re-scored row by row against it, decision 3 restated (the licence
    objection survives in a different shape, the dependency-weight objection does not survive at all),
    and trigger 3 rewritten to name definitions rather than to print a `Today:` line. This amendment
    **does not fire a trigger and does not re-open the decision**; it corrects the record's citations
    and the evidence the ban rests on.
- **Epic:** workspace-eqn (Assessment: keep `pi --mode rpc` child, or embed the rho Rust SDK as
  looprs' engine — the epic's own wording, and the SDK it meant is `matthewyjiang/rho`'s `rho-sdk`)
- **Subjects (pinned, because this record is the page that got them confused):** three unrelated
  projects publish crates whose names begin `rho`, and two of them matter here:
  * **`matthewyjiang/rho`** — crates `rho-sdk` (the embeddable runtime a library consumer links),
    `rho-providers`, `rho-coding-agent` (the CLI), `rho-agent-tools`, `rho-tui-pty`. **The library
    this assessment was *requested* against**, and therefore what "the candidate" means below
    **except where a section pins a different one** — §3 as filed prices `casonadams/rho`, and says so
    at its own heading; §3-B is `rho-sdk`'s own run.
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
  → [`spikes/results/bd-tools-cost.log`](../../spikes/results/bd-tools-cost.log);
  added by 0010-B:
  [`spikes/rho_inventory_rescore.py`](../../spikes/rho_inventory_rescore.py)
  → [`spikes/results/matthewyjiang-inventory-rescore.log`](../../spikes/results/matthewyjiang-inventory-rescore.log)
  for the six-row inventory re-scored against `rho-sdk`,
  [`spikes/dependency_maintenance_metrics.py`](../../spikes/dependency_maintenance_metrics.py)
  → [`spikes/results/dependency-maintenance-metrics.log`](../../spikes/results/dependency-maintenance-metrics.log)
  for trigger 3's four definitions scored against **both** libraries,
  [`spikes/rho_footprint_cost.py`](../../spikes/rho_footprint_cost.py)
  → [`spikes/results/matthewyjiang-footprint-cost.log`](../../spikes/results/matthewyjiang-footprint-cost.log)
  for the embedded startup/memory shape and the N-concurrent extrapolation trigger 4 left unpriced
- **Reproduced by:** the commands at the bottom. Every number below is from
  those captures, on one Linux box (8 cpus), against the local
  `llama.cpp`/`halogen-qwen3.8-flash-next` endpoint and the real beads corpus
  (87 passes of this project's own session files). Rule 4 of ADR-0009 applies
  here without amendment: do not retype these numbers anywhere else. The corpus
  is a live directory, not a frozen fixture: workspace-8jk.11 re-derived the pass
  denominator from it at a later hour and got a larger pass count and a smaller
  median, and that capture quotes its figure *beside* this one rather than
  smoothing the two into a number neither run took.

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
| Wire spellings behaviour is keyed on | **51 `WIRE_INVENTORY` rows — 46 concrete spellings plus 5 catch-alls** — in `src/wire.rs` (`agent_start`, `agent_settled`, `message_update`, `tool_execution_start`, … plus the role and stop-reason sets). **Corrected by 0010-B:** the first version of this row said **40**, which is not a count of the table but what `rho_surface_cost.py`'s `wire: "([a-z_]+)"` scrape could see — it cannot read a camelCase spelling or the catch-all marker, so it silently dropped `bashExecution`, `branchSummary`, `compactionSummary`, `toolResult` (four roles, three with a reader), `toolUse`, and the row that records looprs' forward-compatibility policy. Of the 51 rows, 25 have a named reader and 26 are declared-but-unread — the difference between a behaviour port and a documentation row. Recounted by [`rho_inventory_rescore.py`](../../spikes/rho_inventory_rescore.py) leg 1 |
| Protocol semantics | `data.disposition` (`"handled"` ⇒ no run and no settle ever arrives), `agent_settled` as the *only* end-of-pass signal, `success: false` as a refusal rather than an error, correlation by client-assigned `id` |
| Process semantics | stdin-EOF-as-shutdown (ADR-0002 Q1), `kill_on_drop` so a failed pass cannot orphan a child, stderr never wired to the user's terminal, `--tools read,bash` gating for the planner |
| Test seam | `LOOPRS_PI_BIN` and the fakes in `tests/fixtures/` — every process-level test, plus the beads e2e spikes, runs against a fake child and costs no model call |
| Corpus coupling | `src/measure.rs` and three spikes replay pi **session files**; the scrollback, rewrap, highlighter and live-preview numbers are all pi-format-specific |

Any engine has to answer all six rows, not the first one. `casonadams/rho` answers the first two
differently (below), has no equivalent of the third's `disposition`, and makes the
fifth seam meaningless — an in-process engine has no binary to point a fake at, so
each fake becomes a Rust trait impl compiled into the test binary, and the
`--tools`/argv-level behaviours those fakes prove stop existing as things to prove.

#### The re-scored inventory: what `rho-sdk` answers, row by row

The list above is engine-agnostic — it describes pi and looprs, and it is the checklist every
candidate gets scored against. What was missing was the *scoring*: the version of this table filed
`casonadams/rho`'s answers against rows 1 and 2 and stopped. It is now scored row by row against
`matthewyjiang/rho`'s `rho-sdk` by
[`spikes/rho_inventory_rescore.py`](../../spikes/rho_inventory_rescore.py), whose
decision is
[`spikes/results/matthewyjiang-inventory-rescore.log`](../../spikes/results/matthewyjiang-inventory-rescore.log);
the verdict words below are that capture's leg-5 gap table, one row per row, and every `rho-sdk`
identifier it names is grepped out of the published artifact by the same run, so a verdict that ages
into a falsehood fails the run instead of ageing quietly.

| inventory row | `rho-sdk`'s verdict | first-order cost | the shape of it |
| --- | --- | --- | --- |
| RPC commands | different-but-expressible | a shim, plus one redesign | `prompt`→`Session::start`, `steer`→`Run::steer`, `abort`→`Run::cancel` map cleanly; `clear_queue` changes **kind**, not spelling — there is no queue to empty, only per-id steering retraction with a lost-race outcome, so the Esc path is redesigned rather than renamed |
| Wire spellings | different-but-expressible for most, no answer for a minority | redesign | Most rows are a renamed or narrowed `RunEvent` arm reachable with a shim; what has no answer is what pi gets from being a *process* or a typed union with a serde escape hatch — the unknown-value catch-alls, the message/step and block boundary rows, and the `length` stop reason, which `rho-sdk`'s model layer has no field for at all |
| Protocol semantics | 3 of 4 different-but-expressible, one of them **better**, 1 no answer | redesign | `disposition` disappears because a busy session is a typed refusal instead — the one row where the alternative is the better design. The no-answer half is the settle signal this loop advances on: terminal events arrive **while someone is still reading**, and answer nothing when it stops reading |
| Process semantics | 1 trivial, 2 design changes, 1 impossible | redesign, and one item not expressible | `kill_on_drop` is the impossible one: a tool's children survive cancel, survive `drop(run)`, and survive a `SIGKILL` of the host, and a host-provided tool is outside every SDK gate. stderr-never-to-the-terminal becomes a panic hook plus an ADR-0006 decision. Per-pass `--tools` gating exists as per-request tool visibility, installed as shared mutable state on a long-lived runtime instead of argv |
| Test seam | different-but-expressible for the run half, no answer for the process half | shim for new arms, redesign for the process-level ones | Its scripted provider is a *better* scripted arm than the shell fake — deterministic, no model call, records what it advertised. What it cannot prove is every process fact the fake proves, and the code the fake exists to test (framing, correlation, drain) does not exist on this side, so the seam loses its **subject**, not just its coverage |
| Corpus coupling | no answer | redesign — the largest row, and the hidden cost of the whole comparison | Persistence is opt-**in** and host-implemented: a snapshot value plus a store trait with one in-memory implementation, and nothing written to disk. So the corpus does not get a new format, it gets no format, and looprs becomes the author of one — or every corpus-justified constant gets re-baselined against captures of a corpus that does not exist yet |

Read together, the six rows say the thing §3's price rows could not: the candidate is cheap to
*link* and expensive to *become*. The row counts, the test-surface counts and the named API per row are
in the capture, and this page does not retype them (forbids 7).


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

> **Which library this table prices — pinned by workspace-8jk.7, re-measured by 0010-B.** Every
> number in this table is **`casonadams/rho`** at 0.12.0 — crates `rho` + `rho-engine` +
> `rho-harness-core` — because that is the crate set the ticket was *filed* against, and it is **not**
> the library this decision was requested against. `matthewyjiang/rho`'s own four-leg run is
> [`spikes/results/matthewyjiang-surface-cost.log`](../../spikes/results/matthewyjiang-surface-cost.log),
> indexed beside this one in [spikes/README.md](../../spikes/README.md), and its rows are read out in
> [§3-B](#3-b-the-same-rows-measured-against-matthewyjiangrhos-rho-sdk) below. That re-derivation has
> now landed, so this table is retained for what it is: **`casonadams/rho`'s measurement**, still the
> answer to "what would *that* library cost", and still forbidden as a citation for `rho-sdk`. No row
> here may be quoted as `rho-sdk`'s price, and no row here may be quoted as "`rho`'s price" without
> saying which of the two libraries it is a price for.

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
| event vocabulary overlap with looprs' wire spellings | **3 shared spellings** (`turn_start`, `turn_end`, `error`), out of 53 variants `casonadams/rho`'s protocol can name. *Corrected by 0010-B:* this row's denominator was the scraper's, not the table's — see the inventory row above for what that means | — |

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

The overlap row is the cost that the ticket did not price: of the wire spellings `src/wire.rs` keys
behaviour on, `casonadams/rho`'s protocol can name **3** — and against the right library the string
intersection is **0**, which is a worse-looking number for the same uninformative measurement: it
compares spellings, and the cost is in meanings (see the
[re-scored inventory](#the-re-scored-inventory-what-rho-sdk-answers-row-by-row)). Not the
spelling — the meaning. Every row of `WIRE_INVENTORY`, the generated
`docs/guide/wire-protocol.md`, `app::apply_pi`, the token window, and the tests that
make each row state what the harness does with the value get re-derived by hand.
Against a saving of 308 ms per pass, that is the wrong trade even before the
maintainer question is asked.

### 3-B. The same rows, measured against `matthewyjiang/rho`'s `rho-sdk`

Every row §3 filed, re-measured against the library this decision is about, by
[`spikes/rho_surface_cost.py`](../../spikes/rho_surface_cost.py) pointed at
`matthewyjiang` — four legs, one machine, three cold `--release` builds and eight EMBED arms, each of
them `rc=0`, in
[`spikes/results/matthewyjiang-surface-cost.log`](../../spikes/results/matthewyjiang-surface-cost.log)
and indexed beside §3 in [spikes/README.md](../../spikes/README.md). **This table deliberately
carries no numbers**: forbids 7 applies to an amendment before it applies to anyone else, so each row
says what the claim turns out to be and names the leg it comes from. The rows do not agree with §3's,
and they do not agree in §3's *direction*:

| §3's row | what `rho-sdk`'s own run measures | leg |
| --- | --- | --- |
| cold build, crates compiled | **cheaper than looprs' own cold build**, in crates compiled and in wall seconds — same method, same box, own target dir | 1 |
| packages in the shipped graph | **fewer than looprs ships today**, and fewer again against the `--features notify` build `docker/` and the release job actually produce. Resolved **per crate**, so a closure that contains the CLI's TUI can never be quoted as the library's price; and note that the run's own consumer graph also carries the spike program's deps, so the library-only figure is the smaller one | 1 |
| packages it would add that looprs does not ship | single-digit against the shipped (notify) build — i.e. the overlap with what looprs already links is nearly the whole graph | 1 |
| artifact size | **smaller than looprs' own binary** under one profile on both sides, and about half the shipped artifact. Recorded because the first version of this leg compared a *stripped* scratch binary against an *unstripped* looprs one, and flattering the candidate by accident is the same defect as flattering it by mistake | 1 |
| source a consumer links | the library crate on its own, which is a small part of the all-crates total the raw listing prints — and its `unsafe` count is bench code: the library carries `#![forbid(unsafe_code)]` in its own `src`, so the word in the artifact is not a word in what gets compiled | 3 |
| **licence** | **the same gap, in a nicer repo.** The repo has a root `LICENSE`, GitHub's detector reads MIT and the manifest says MIT — and the published artifact of the crate a consumer links ships no licence file, nor does `rho-providers`'; the one artifact in the workspace that does ship a `LICENSE` is the CLI bin crate, and a sibling crate's file does not arrive inside another crate's artifact. Declaration, not grant, with better manners | 3, and trigger 3's licence row |
| maintenance | a bigger and busier project than §3 filed — more releases, more downloads, more stars, a repo roughly twice as old — and still **one human** carrying it, on either denominator, with the second-named human a long way below the bar. Both denominators, the URLs and the earliest date a row can move are the scorer's business, not this page's | [trigger 3's memo](0010-trigger-3-dependency-maintenance-metrics.md) |
| exact pins | the library crate declares **no** `=` pins and no optional dependencies at all; the three exact pins in that workspace belong to the CLI crate a library consumer does not link. Upstream's `[profile.release]` size tuning is a property of their workspace root — cargo ignores a dependency's profile — so it travels with none of it | 3 |
| event vocabulary overlap | **0** shared spellings — and 0 measures spelling, not meaning. Scored by meaning, row by row, most rows are reachable through `RunEvent` with a shim and the expensive ones are exactly the rows pi gets from being a *process*: see the inventory table above | 3 + the rescore |

The EMBED leg was rewritten to run `rho-sdk`'s API rather than describe it
([`spikes/embed_programs/matthewyjiang-rho-sdk.main.rs`](../../spikes/embed_programs/matthewyjiang-rho-sdk.main.rs)),
one arm per row of the inventory list, and it came back with four findings that §3's two do not
describe — all of them about what a host has to *build*, none about what it has to pay:

* **A settle event exists, and the library disclaims delivering it.** `Completed`, `Failed` and
  `Cancelled` are real, published, prose-documented events and they arrive on the host's stream in
  every arm that was still reading. They do not arrive when nobody is reading: a dropped `Run` produced
  no terminal event in any trial, a panic inside a tool closed the stream with none, and a host that
  stops draining wedges the run against the bounded event channel while the session still reports
  `Running`. A live approval handler nobody answers holds a run open with no timeout at all. That is
  not "no settle signal" — it is a *best-effort* one, which is a different objection and a weaker
  library, and looprs' loop advance is precisely the consumer that needs the guarantee.
  The five sub-tests that separate "a name exists" from "a loop can be keyed on it" are
  [trigger 3's memo](0010-trigger-3-dependency-maintenance-metrics.md), and the
  [footprint capture](../../spikes/results/matthewyjiang-footprint-cost.log) adds the sixth thing §3
  never thought to ask: even when the terminal event *does* arrive, a host that advances its next
  pass on the event gets a typed "session busy" refusal some fraction of the time, while a host that
  drains the stream to its end is refused in none of its trials. pi's loop has to wait for the stream
  to close for a different reason; this one has to wait for the same reason, and did not know it.
* **Gating exists, and a host-provided tool stands outside it.** Policy, approval handler, pre-tool
  hook and per-request visibility are four real gates and all four hang off the same `authorize`
  call — which a tool the host supplies never makes, and the SDK says so in its own source. That is
  the shape of every tool looprs would register. The SDK's defaults fail closed (an unattended host
  gets a denial, not a hang), which is the right default and the opposite of looprs' argv-shaped,
  per-pass, per-child allowlist.
* **The blast wall has to be built by hand, and one item of it cannot be built.** A tool's child
  process survives cancel, survives `drop(run)`, and survives a `SIGKILL` of the host process —
  reparented to init — where the same child spawned with `kill_on_drop` is reaped. A panic inside a
  tool is contained by the runtime's own task so the host lives, but the panic text goes to this
  process' stderr, which in looprs is the user's terminal, mid-frame: the ADR-0006 hazard the process
  boundary was also buying against, arrived at from the other side.
* **Nothing is written to disk, which is a cost and not a saving.** Persistence is a snapshot value
  plus a host-implemented store trait with one in-memory implementation: nothing to opt out of, so
  nothing for the corpus to measure, which is inventory row 6 above.

One caveat is load-bearing and belongs in the record: **`rho-sdk` ships no model client at all.** Its
dependencies are serde, serde_json, thiserror, tokio, tracing, url and uuid, and its only shipped
provider is a test double; the model clients live in `rho-providers`. So "embed `rho`" is a crate
*composition*, not a dependency line, and the embedded rows in the footprint capture measure a runtime
plus a provider this spike wrote — deliberately left inside the numbers rather than subtracted out —
with no transport or token accounting from either candidate's shipped code.

**What it costs to run, in the shape looprs would actually use** — the case §3 left unpriced and
trigger 4 named — is
[`spikes/rho_footprint_cost.py`](../../spikes/rho_footprint_cost.py) →
[`spikes/results/matthewyjiang-footprint-cost.log`](../../spikes/results/matthewyjiang-footprint-cost.log):
the memory case for embedding is **real, and bigger than §3's one-line MiB comparison suggested**, once
every row is written as *host plus added* instead of one process' resident set against another's; boot
is smaller than the child's by two orders of magnitude, and both sit at or below a hundredth of one
percent of a median pass, so **trigger 1's latency leg stays dead on the right library's numbers
too**; N concurrent passes do not multiply memory the way N children do, and the ceiling the
extrapolation reaches is a wall the child shape hits and the embedded shape does not approach. Where it
is **not** free is history: a long pass grows in place, and a per-pass child that dies at the end of the
pass pays none of that. Rows, both RSS instruments reconciled, and the extrapolation's ceiling are in
the capture.

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
whose only job was to fail it (workspace-8jk.2). **0010-B did not change this paragraph's scope:** the
ban was made two families by the gate fix, and it stays two families.

**The bar, and what it now says.** This was never a performance objection — that number is known, and
irrelevant. It is a maintenance objection, and after 0010-B it is a **scored** one rather than a
paragraph: four definitions — a licence text inside the published artifact of the crate a consumer
links, more than one human carrying the repo, twelve months on the youngest of three clocks, and a
settle signal a loop advance can actually be keyed on — written down once in
[trigger 3's memo](0010-trigger-3-dependency-maintenance-metrics.md) and run per library by
[`spikes/dependency_maintenance_metrics.py`](../../spikes/dependency_maintenance_metrics.py).
Both libraries are scored every time. **Trigger 3 is NOT MET by either of them, on none of the four
definitions**, and the ban therefore stands — which is a *different justification* from the one this
section carried when it was filed, and it is written here as the new one instead of being glossed over
the old one.

**What the re-measurement took away**, said plainly, because a correction that only adds is not a
correction:

* **Dependency weight: withdrawn.** §3 filed a consumer whose shipped graph was several times
  looprs' own and whose artifact was over three times looprs' binary. Measured against `rho-sdk`, the
  direction is inverted — the library is *smaller than the tree that would link it*, on every axis the
  build leg measures, and nearly all of its graph is code looprs already ships (§3-B). Nobody should
  have to re-derive that in a year, and nobody should have to re-derive the reason the bloat objection
  was ever available: it was another library's number.
* **`unsafe` and raw LOC: withdrawn as written.** The figure §3 quoted counted all three crates of the
  wrong library, tests and benches included; the crate a consumer links forbids `unsafe` in its own
  source, and the `unsafe` strings its artifact does contain are in code a consumer never compiles.
* **The licence objection, in the form it was written: replaced, not withdrawn.** As filed it read
  *"no licence file anywhere, and GitHub's detector sees NONE"* — which is `casonadams/rho`'s shape.
  `matthewyjiang/rho` has a repo-root `LICENSE`, GitHub reads the repo as MIT and the manifest says
  MIT — **and the published artifact of the crate a consumer links contains no licence text**, exactly
  like the other library's. This is the one row the re-measurement was expected to *clear* and did
  not, and it is stated flatly here because the memo that ran it disagreed with the ticket that asked
  for it: a declared-but-unreceived licence is the same gap in a nicer repo, and it is a one-file fix
  upstream has not made. What a consumer receives is what is inside the artifact they receive.
* **The settle objection, in the form it was written: also replaced, and this one moves the other
  way.** `rho-sdk` has a named, published, prose-documented terminal event set and passes all but one
  of the memo's sub-tests for it, where the library §3 was filed against passes barely half — and
  the sub-test it fails is failed by the library's **own contract text**, which disclaims delivering
  exactly one terminal event for every run. That is a hole upstream can close in a release, which makes
  it a different kind of objection from a property of a library's shape. It is still a hole in exactly
  the place looprs' loop advance is: a best-effort settle event is the one thing a loop that runs
  unattended for hours cannot be driven by.

**What stands**, under those definitions and on the intended library's own numbers: one human carries
the repo on either denominator, with the second-named human far below the bar; the published artifact
is a fraction of the way to the twelve months the definition asks for, on the youngest of the three
clocks; the licence text is not in the artifact; and the settle signal is best-effort where this loop
needs a guarantee — including the case a process boundary hands over for free, *a child died*, which
has no in-process analogue and would have to be rebuilt as deadlines and a watchdog. Those four are the
ban's justification now. `casonadams/rho`'s rows are in the same capture and are no better, so the
decision covers both libraries and — unlike the version this section was filed in — no longer depends
on one paragraph's numbers having been the right library's.

**And the price, even if the bar were cleared.** Meeting trigger 3 re-opens this question; it does not
answer it. The bill is the
[six-row inventory above](#the-re-scored-inventory-what-rho-sdk-answers-row-by-row): one RPC command
whose *guarantee* rather than spelling disappears, a set of rows that need re-derivation rather than
renaming, one item (`kill_on_drop`) that is not expressible in-process at all, a test seam that loses
its subject rather than shrinking, and a corpus looprs would become the author of. The memory case is
real and is not the candidate's own chart's row (§3-B's last paragraph). If the bar is ever met, the
follow-on is `workspace-bt8` re-opened — the epic that already costs this out — and not a new ticket;
see forbids 1.

**The sentence as filed, retained.** For the record, and labelled as what it is: the blocker this
section was originally written with was *"95.1% of 865 commits by one person, a repo ~7 weeks old, 7
stars, 1 subscriber, 480 downloads, 120,799 LOC across three crates, and no licence file in the
published artifact of any of them while the manifest declares `MIT OR Apache-2.0`."* Every clause of
that is true of **`casonadams/rho`** and was written as though it had been about
**`matthewyjiang/rho`**. It is kept because this record is the page that made that mistake and the
honest place to say so: a harness that runs unattended for hours against a user's repo does not take
that in, and a declared-but-unshipped licence is not something to resolve later — a sentence that
survived being pointed at the right library.

### 4. The triggers that re-open this, written as numbers rather than as an opinion

Re-litigating this ADR requires re-running the commands at the bottom **and** meeting one of the four
below. (0010-B leaves triggers 1 and 4 as numbers and rewrote trigger 3 as definitions — the same rule
applied honestly: a number inside a trigger is a number that gets retuned, where a definition gets
evaluated.)

1. **The denominator moves.** The boundary exceeds **1% of pass wall** — i.e. above
   ~31 s of a 3095 s pass, **~100×** today's 308 ms. Realistic route: a much faster
   local model makes token time stop dominating, so re-measure the *ratio*; the ms
   will barely move.
2. **The seam stops being honoured.** pi's RPC drops or changes one of the six rows
   in the inventory table (4 commands, the wire-spelling rows enumerated there and
   recounted since, `disposition`, `agent_settled`, EOF-shutdown, `--tools`), or the
   compatibility budget for `src/wire.rs` stops
   being absorbable. At that point the engine question is forced, and it is forced
   by pi, not by either candidate.
3. **A `rho` becomes adoptable as a dependency** — the sentence as filed said
   **"rho becomes adoptable as a dependency"**, singular, and it took an epic to learn that there
   are two libraries that answer to that name and that the trigger has to be run against each of
   them. It is a maintenance test and not a benchmark. **As amended, this trigger is a pointer and a
   rule: it carries no number, no date and no verdict of its own.**

   *What it asks* is defined once, in
   [ADR-0010 trigger 3 as testable metrics](0010-trigger-3-dependency-maintenance-metrics.md) — four
   definitions, each with the convention that makes it falsifiable rather than arguable: the
   denominator behind "share of commits", whether a drive-by commit counts toward the ≥2, which clock
   the twelve months run on, and the five sub-tests that make "a documented settle signal" mean *a
   loop advance can be keyed on it* rather than *the spelling exists*.

   *What the answer is* is printed, per library, by
   [`spikes/dependency_maintenance_metrics.py`](../../spikes/dependency_maintenance_metrics.py) →
   [`spikes/results/dependency-maintenance-metrics.log`](../../spikes/results/dependency-maintenance-metrics.log):
   one verdict per definition per candidate, each with the URL its number was read from, plus the
   earliest date a failed row can next be worth re-running. Seconds to run, no build, no model call,
   no endpoint. **Evaluate it; do not re-derive it, and do not argue with it in prose** — the argument
   this trigger used to invite was about definitions, and the definitions are now written down.

   The rules that make it a test:

   * **A conjunction.** All four definitions must PASS, for the library named in the proposal, on the
     date the capture printed. One passing row is not a trigger, and a row that passes for one library
     says nothing about the other.
   * **`not measured` is never a pass.** A leg that could not read its input prints that and can never
     print PASS; a run with no network exits non-zero, so silence is never read as a verdict.
   * **No `Today: …` line is ever added back to this page.** The four this record used to print were
     measured against the wrong library, and a dated verdict in prose is a sentence with an expiry
     date nobody set. Rule 7 below is why they are not simply replaced with four new ones.
   * **A row moves in the capture or not at all.** A licence file that exists upstream but is not in
     the published artifact is not a pass; a terminal event that exists but whose delivery the
     library's own contract disclaims is not a pass; a second maintainer has to show up in the public
     attribution record, not in a sentence in a README.
   * **Meeting it re-opens the decision; it does not answer it.** The price against looprs' own six
     inventory rows is still owed — see the table above, and forbids 1 about which ticket gets
     written next.

   The fourth definition is five sub-tests rather than one for a reason this repo paid for twice: a
   settle signal can be named, published and prose-documented and still not arrive on the paths a loop
   needs — and, as the footprint capture showed on the *right* library, can arrive and still be the
   wrong thing to advance on. S4 (is it on the stream the host reads) and S5 (does it arrive on every
   path a run can end) exist for exactly that gap; a proposal that satisfies the first three sub-tests
   has not met this trigger.
4. **Passes become concurrent.** The boundary is priced per pass because looprs spawns
   one child per pass. If the loop ever runs N children at once on a constrained
   machine, re-measure: the cost is 299 ms × N of wall and 122 MiB × N of RSS, and the
   second is the one that bites.

   (`casonadams/rho` in process is 84 MiB per §3's embed arm — that library's number, for that
   library. **The `rho-sdk` half of this row is measured now, not extrapolated**:
   [`spikes/rho_footprint_cost.py`](../../spikes/rho_footprint_cost.py) →
   [`spikes/results/matthewyjiang-footprint-cost.log`](../../spikes/results/matthewyjiang-footprint-cost.log)
   prices it in the shape looprs would use — one child-process baseline, one host-side baseline, N
   children against N sessions against N runtimes, and the box's own memory ceiling as the
   denominator. Its memory rows are written as *host plus added*, which is the only honest pairing
   when one number is a separate process' resident set and the other is bytes joining a process that
   was already resident. It also prices the term §3 never did: memory a long pass grows into and does
   not give back, which a per-pass child that dies never pays, and it closes the boot question — an
   embedded boot is not free, it is ~free, and it is the same order of a pass as the child's, so the
   latency leg of this decision does not need a third hearing whoever wins the engine argument.
   A concurrency proposal gets compared against that capture, not against the 84 MiB, and not against
   the candidate's own startup/RSS chart, which measures a CLI process and not the shape looprs would
   run.)

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
| an in-tree fake engine plus a transport contract suite | the shell fake is the seam every process-level test stands on; rule 5 says a second engine would have to *add* a fake, not ask for this one's removal. The disposition pass records the added half as **cheap** for `matthewyjiang/rho`, which ships a scripted provider as public API — and *cheap-and-additive* is a different sentence from *redundant*, which is the one this row exists to refuse; rule 5 is unchanged by it |
| engine selection: `LOOPRS_ENGINE`, status band, docker image, docs | a knob with one valid value is a knob to be miscorrected, and it would be documented in the same pages that today say the engine is the pi child |
| re-baselining every corpus-justified constant and generated page | the largest ticket in the set, and it is pure cost unless an engine actually changes — this repo's constants are justified by committed logs, and moving engines invalidates the logs. **Re-priced by the disposition pass:** the switch would not even supply the logs — `rho-sdk` persists nothing unless a host writes the store — so this ticket is now a persistence-format decision *and* a re-baseline, and larger than it was filed. The finding is in the ticket's own text, with the capture |
| shipping the typed `bd` tool surface | **the exception: already shipped**, by decision 2 above (`extensions/bd_tools.ts`), which is the proof that this ticket never needed an engine at all |

**Disposition pass (`workspace-8jk.9`, 2026-10-12).** After amendment 0010-B every parked ticket was
re-read against this record as amended, and each now carries a one-paragraph disposition in its own `bd`
notes — still declined, declined-and-re-priced, or re-filed. The verdict is six declined (three of them
re-priced) and one shipped long ago from outside the epic, and **nothing was re-filed, because re-filing
hinged on decision 3 flipping and it did not**: trigger 3 is NOT MET by either library on any of the four
definitions, and that scored bar is now the epic's stated blocker in place of the licence-and-bus-factor
sentence that was `casonadams/rho`'s. Three of those dispositions are written down here because they are
the ones a sympathetic re-reader would otherwise re-derive wrongly:

* **`.1` and `.2` — the one-implementation `EngineRuntime` trait, and a feature-gated backend for either
  `rho` — stay declined on their own merits whichever library is under discussion.** A one-impl trait is
  the unbuilt door ADR-0009 rule 2 names, and no re-pricing of a candidate changes that argument. This is
  stated in advance on purpose: a corrected library is exactly the moment someone reaches for "just the
  trait seam, nothing else", and that compromise is the failure, not a smaller migration.
* **Two tickets moved in price without moving in verdict, in opposite directions.** Porting the beads
  machine got *dearer*: the re-scored inventory's rows 1, 3 and 4 make its control logic a redesign
  rather than a rename. The scripted fake-engine arm got *cheaper*, because `rho-sdk` ships a scripted
  provider as public API — and that is the sentence the fake-child seam does **not** get to lose: forbid
  5 is re-stated as unchanged, not re-litigated.
* **The re-baselining ticket's text carried an estimate the persistence finding voids** — an embedded
  `rho-sdk` writes no session file, so there is no corpus to capture — and its description now says so,
  with the re-priced surface and the capture that measured it, instead of leaving the next reader to work
  from a stale figure.

This pass opened no implementation ticket, which is the whole point of it: forbid 1 is about not opening
migration work without a met trigger and a fresh capture, and a disposition is neither.

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
workspace-8jk.8 (§3's rows, the six-row inventory, decision 3 and trigger 3, re-derived). A rule that lives only in a
decision record is re-litigated by the next person with a benchmark; one that fails a gate is not — provided the gate
names the right crate, which is what `dep_audit.py --self-test` now checks on every run
of this step.

Re-opening this work costs more than a wish: a §4 trigger named in the ticket, a fresh
capture beside it, an amendment to this ADR, and then `bd show workspace-bt8.1` …
`.7`, the seven parked tickets, which are closed rather than deleted and still carry
the full text of the costing — and, since `workspace-8jk.9`, a disposition paragraph each
saying what the amended version of this record did and did not change about it.

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
* Whether either library is *adoptable* is a command too, and a cheap one:
  `spikes/dependency_maintenance_metrics.py` re-scores both against the written definitions in
  seconds, and `spikes/rho_inventory_rescore.py` re-scores the six inventory rows against the
  published artifact — so both halves of the ban can be checked for staleness without a build, a
  model call or an opinion.

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
   the capture is two clicks away. **0010-B leaves this rule untouched, on purpose:** it is the one
   claim in this record that was engine-agnostic from the start, and re-measuring it against the right
   library confirmed it rather than denting it — an embedded boot is two orders of magnitude smaller
   than the child's, and the child's own is a hundredth of one percent of a pass, so "faster" is not
   an argument either candidate has.
4. **Do not let the typed `bd` tools grow a looprs-side vocabulary.** No derived
   statuses, no re-interpreted board states. The schema mirrors `bd` argv and returns
   what `bd` said — `bd`'s own read stays the one honest read (ADR-0007).
5. **Do not remove or narrow the fake-child seam** to accommodate a hypothetical
   in-process engine. The seam is the reason the process contract is testable at all;
   if an engine switch is ever proposed, it has to *add* a fake seam, not ask for
   this one's removal.
6. **Do not treat the licence gap as a formality — and say *which* library's gap you mean, because
   they are two different gaps.** For **`casonadams/rho`**: no `LICENSE` at the repo root, no licence
   file in any of the three published artifacts, and GitHub's detector reads NONE while `Cargo.toml`
   declares `MIT OR Apache-2.0`; its three open issues are a release bot and two feature suggestions,
   none of them about this. For **`matthewyjiang/rho`**: the repo *does* have a root `LICENSE`,
   GitHub reads MIT and the manifest says MIT — and the published artifact of `rho-sdk` (and of
   `rho-providers`) contains no licence text; the only artifact in that workspace that ships a
   `LICENSE` is the CLI bin crate, and a sibling crate's file does not travel into another crate's
   artifact. Two shapes, one defect: **the licence a consumer receives is the one inside the artifact
   of the crate they link**, and a manifest string is a declaration, not a grant. "Fix" means *the
   scored row moves in the capture*, not that a page remembers it, and an upstream fix here would not
   move the other three rows of §4's trigger 3 either
   ([trigger 3 as testable metrics](0010-trigger-3-dependency-maintenance-metrics.md)).
7. **Do not retype these numbers into another page.** Link here, or link the capture.
   `docs_check.sh` catches contradictory knob defaults; it cannot catch a number
   quoted twice and retuned once. This amendment obeys its own rule: §3-B and the re-scored inventory
   carry claims and links, not copied measurements.
8. **Do not re-litigate a dependency with a command that cannot be pointed at it.** The defect that
   put this record's §3 numbers on the wrong library was not only a misreading; it was a re-runnable
   script whose subject was a literal in the source, so re-running it reproduced the wrong answer
   *with the authority of a fresh measurement*, header line included. A re-litigation command whose
   subject is hardcoded is not a re-litigation command. The fix is not optional in either direction:
   the identity of the thing being priced is a **parameter** (`--id` / `LOOPRS_SPIKE_RHO_ID`, `--list`
   to print the candidates), and the identity is **printed in the capture header** from what the run
   actually queried — `owner/repo` and every `crate@version` it measured — not typed into the source
   (workspace-8jk.1). And a prohibition is only as good as the check that enforces it: the deny-list in
   `scripts/dep_audit.py --gate` has to name the crate the decision is about, which it did not for a
   whole epic, so `rho-sdk = "5"` walked through the gate whose only job was to fail it while the
   check printed the clause forbidding it (workspace-8jk.2; `--self-test` now asserts the *reason
   string* per banned name per dependency form).

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
* **Memory under concurrency — measured since, by 0010-B.** One child per pass is looprs' shape, so
  the 122 MiB-per-child figure had no multiplier in the original record. It has one now:
  [`spikes/rho_footprint_cost.py`](../../spikes/rho_footprint_cost.py) measures N children against N
  sessions against N runtimes and extrapolates to the box's own ceiling. Trigger 4 points at it. What
  remains unmeasured here is looprs *with* concurrency, not either engine's per-unit cost.
* **The transport half of an embedded engine.** `rho-sdk` ships no model client at all — its providers
  are a test double, and the real clients are a separate crate — so every embedded row in every capture
  here measures a runtime plus a provider written by a spike, and carries **no token accounting and no
  provider-transport cost from either library's shipped code**. "Embed `rho`" is a crate composition;
  the composition is not priced, because the ticket was about whether the runtime could be a
  dependency, and it is the first thing a real proposal would owe.
* **The artifact a user would actually download.** Upstream's own release profile (`opt-level="s"`,
  fat LTO, one codegen unit, stripped) was OOM-killed twice on the 8 GiB box even at one build job,
  so the CLI rows in the footprint capture come from a thin-LTO build, named in the leg that used it.
  The shipped-profile binary is a different artifact and is not measured here.
* **A macOS leg.** The repo's earlier captures were taken on Darwin; this one was
  taken on the Linux container the loop actually runs in, and says so in its header.
  Absolute ms differ per kernel; the *ratio* is the finding and it is four orders of
  magnitude, which is not kernel-sensitive.
* **Long-run stability of a shared engine.** An embedded engine that leaks or wedges
  after N passes is a different class of risk from a child that does, because there is
  no `kill_on_drop` and no blast wall. Part of that is measured now — the footprint capture's history
  arm grows one session across dozens of runs and shows what a live host keeps, and the surface
  capture's wedged-reader and unanswered-approval arms are real wedges — but the original claim, about
  a *pass count* an embedded engine survives without intervention, is still argued from ADR-0002 and
  ADR-0006 rather than measured, and it is the argument most likely to be re-litigated.

---

## Amendment 0010-A — which `rho` (workspace-8jk.7)

- **Date:** 2026-10-12
- **Ticket:** workspace-8jk.7, under the epic that found the mis-identification
  (workspace-8jk). The separate, larger job — re-deriving §3's rows and decision 3 against `rho-sdk` —
  was workspace-8jk.8's, and it landed as [amendment 0010-B](#amendment-0010-b--the-re-measurement-workspace-8jk8)
  below; this section is the naming sweep only.
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

**Added when that process rule landed (workspace-8jk.10).** It landed, and it is not duplicated
here either: the durable fix is
[contributing → assessing a third-party dependency](../guide/contributing.md#assessing-a-third-party-dependency)
— name the `owner/repo` and the published artifact in the first line of anything that prices a
dependency; let a capture print what it actually queried rather than a literal typed into the
source; give every supply-chain number its denominator. That section is where it is written once;
it also added the **Subjects** line to
[ADR-0008 §6's ADR template](0008-docs-site.md#6-the-adr-template) and the testing ladder's row for
an assessment that prices a dependency.

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
claim-bearing sentence in `docs/adr/*.md` and `docs/README.md` was filed as
`workspace-8jk.12` and is **now built**: the gate's ninth check reads exactly those two
places, asks whether the sentence carries a verdict or a measurement, and fails it unless
the page has pinned an `owner/rho` beside a crate name above it or the line carries a
`<!-- rho-named: owner/repo -->` marker. Its corpus is narrow for the reason this record
kept repeating: a naming lint with false positives on looprs' own identifiers gets
disabled inside a week, which is worse than no lint — `src/` and `spikes/` are out of
scope by construction, and `./scripts/docs_check.py --self-test` runs the planted-claim
case against in-memory text so the check cannot fail open. The rule it enforces is written
once each: the naming sentence in the blockquote above the decision-record table in
`docs/README.md`, and the checking of it — who, and when, plus what a capture has to print
about itself — in
[contributing → assessing a third-party dependency](../guide/contributing.md#assessing-a-third-party-dependency).

## Amendment 0010-B — the re-measurement (workspace-8jk.8)

- **Date:** 2026-10-12, same day as 0010-A, and deliberately kept separate from it: one amendment was
  a naming sweep and could be checked by reading, this one is a set of measurements and can only be
  checked by running.
- **Ticket:** workspace-8jk.8, the last ticket of the epic that found the mis-identification
  (workspace-8jk), and the only one of them that changes a verdict's *reason*. Its dependencies are the
  re-measurement spike (workspace-8jk.4), the EMBED leg against `rho-sdk` (workspace-8jk.3), the
  contract-fit spike (workspace-8jk.5), the metric-definition memo (workspace-8jk.6), the gate fix
  (workspace-8jk.2), the parameterised spike driver (workspace-8jk.1) and the embedded footprint
  measurement (workspace-8jk.11).
- **Amends:** §3's rows (retained, labelled, and now paired with `rho-sdk`'s own in §3-B), the six-row
  dependency inventory (re-scored row by row against `rho-sdk`), decision 3 (restated: what the
  re-measurement withdrew, what it confirmed, and what the ban now rests on), trigger 2's spelling count,
  trigger 3 (rewritten as definitions plus a rule, with no number of its own), trigger 4 (the
  `rho-sdk` half is measured), forbids 3, 6, 7 and new forbid 8, and the record's
  *Measured by* / *Reproduced by* lines. Outside this file it updates
  [`docs/README.md`](../README.md)'s one-line summary of this record,
  [`docs/testing.md`](../testing.md)'s description of the gate so it no longer describes the old
  blocker, [`spikes/README.md`](../../spikes/README.md)'s index rows for the two captures this amendment
  reads, and [the trigger-3 memo](0010-trigger-3-dependency-maintenance-metrics.md), whose
  "workspace-8jk.8 has not done it yet" note is now in the past tense.

**What was wrong, stated as a defect rather than as an apology.** This record priced, and then banned,
**the wrong library**. 0010-A fixed which words pointed where; this one fixes what was measured. The
§3 table and the blocker paragraph in decision 3 were read off `casonadams/rho`'s artifacts — crates
`rho`, `rho-engine`, `rho-harness-core`, the crate set
[crates.io/crates/rho](https://crates.io/crates/rho) serves — while the assessment had been requested
against `matthewyjiang/rho`'s `rho-sdk`, a different crate set with a different API, published by
strangers to each other. A third unrelated project, `pyrex41/rho`, publishes the `rho-core` /
`rho-tools` pair on crates.io and was in no sentence in this record until it had to be named as the
thing the gate deliberately does not ban. The mis-identification survived a re-runnable spike because
the spike's own header asserted the wrong owner too — see forbid 8.

**What the re-measurement found, including where it went the other way.** Run against the library it
was about, the candidate turned out to be **cheaper to link than looprs is** — smaller graph, smaller
binary, faster build, no exact pins, `unsafe` forbidden in its own source — so the dependency-weight
half of §3 does not survive, and this record says so rather than quietly dropping those rows. The rows
that survived did so on their own numbers and not on §3's: the licence gap is still there but has a
different shape (the repo has a licence, the *crate a consumer links* does not), the bus factor is
still one human on a bigger and busier repo, history is still nowhere near twelve months, and the
settle signal — the row that changed the most in the candidate's favour — is a real, named,
documented event set that the library's own contract disclaims delivering on every path. Trigger 3 is
**NOT MET** by either library, on none of the four definitions, and so the decision stands on a
justification this amendment had to write rather than inherit.

**What does not change, and why** — said explicitly, so that a correction is not read as a wholesale
reversal by the next person to find this page:

* **§1's boundary cost and its pass denominator.** Measured on looprs and pi, with no candidate in
  either number, so no re-measurement of a candidate can touch them. The claim "embedding the engine is
  a latency win" stays dead on exactly the evidence that killed it, and now on a second measurement
  too: the embedded boot is *two orders of magnitude smaller* than the child's boot, and the child's
  own is a hundredth of one percent of a pass, which is the wrong order of magnitude for an argument.
  Nothing in the identity correction resurrects it.
* **§2 and decision 2: the typed `bd` tools.** Shipped on the pi side, priced in prompt bytes per
  pass, and independent of engine choice in both directions — the engine buys none of the win, and the
  win costs the same whichever engine is under it. Nothing about which `rho` was which changes a
  TypeBox schema or a `--tools` allowlist.
* **The inventory list itself.** Its six rows describe pi and looprs, not a candidate; they are the
  checklist every engine gets scored against, and 0010-B *scored* them rather than editing them. One
  row's count was corrected — the spelling count this row first carried was a scrape artifact, not a
  count of the table, and the corrected figure is in the row itself rather than restated here — which
  is a fix to the list's own accuracy, not a change to what the list is for.
* **Decision 1, decision 5, and the ban.** The engine stays a child, the parked epic stays closed
  unworked, and neither `rho` family enters `Cargo.toml`. The scope of the prohibition never moved
  after the gate fix made it two families; what moved is the evidence underneath it.

**The reproducibility defect, recorded as the lesson it is.** A re-litigation command whose subject is
hardcoded in the script is not a re-litigation command: it re-runs, it prints a header asserting an
identity, and it converts a wrong citation into fresh evidence for the wrong citation. The fix is
named in forbid 8 and already landed — **identity as a parameter** and **identity printed by the run,
not typed by the author** (workspace-8jk.1, `spikes/rho_surface_cost.py`), and **the gate's deny-list
naming the library the decision is actually about**, with the ban tested on its reason string rather
than its exit code (workspace-8jk.2, `scripts/dep_audit.py --self-test`, wired into
`scripts/check.sh`). Both are checked on every CI run, so neither is a promise this page has to keep.

**And the part worth saying out loud about the process.** The defect here was **referential**: a
number filed against the wrong name. The process that produced the numbers was the right process and
is the reason this amendment was possible at all — measure the boundary instead of assuming it, test
the cheap arm before the expensive one, price the unknown supply chain instead of reading its README,
and write the triggers as tests. Two of the three claims this epic was handed died on that evidence
and are still dead; the third was answered with a real measurement of a real library that just was not
the library anyone had asked about. A record that measures everything except *what it is measuring* is
the failure mode, and the durable fixes are the naming rule
([contributing → assessing a third-party dependency](../guide/contributing.md#assessing-a-third-party-dependency)),
the parameterised driver, the tested gate and the definitions behind trigger 3 — all of which exist
now, and none of which required this page to be right the first time.

**What this amendment deliberately does not do.** It fires no trigger; a corrected citation is not new
evidence about a candidate's adoptability, and §4 says what would be. It prints no new number in §3-B
or in the inventory table, per forbids 7 — those two tables are claims plus links. It does not delete
the `casonadams/rho` capture, §3's table, or decision 3's original sentence, all of which are kept and
labelled, because the point of a decision record is that you can see what was believed when.

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
# the six-row dependency inventory, re-scored row by row against the library the
# decision is about (matthewyjiang/rho, crate `rho-sdk`) — verdict and first-order
# cost per row, every cited identifier grepped out of the published artifact so a
# stale verdict fails the run. Seconds, no build, no model call.
python3 spikes/rho_inventory_rescore.py           | tee spikes/results/matthewyjiang-inventory-rescore.log
# the memory/concurrency case trigger 4 left unpriced: the child shape, the host
# shape, the candidate as a CLI and embedded, N at once, and the box's own ceiling
# as the denominator. Needs the candidate CLI built (`LOOPRS_SPIKE_RHO`); the
# driver's own docstring names the knob every arm reads, and every row in the log
# names the instrument that produced it.
LOOPRS_SPIKE_RHO=/path/to/rho LOOPRS_SPIKE_CLI_MODEL=1 python3 spikes/rho_footprint_cost.py \
  | tee spikes/results/matthewyjiang-footprint-cost.log
```

Each prints `not measured` with the reason when its prerequisite is missing — no
model endpoint, no corpus, no network for the supply-chain leg — so a partial
environment never silently becomes a passing answer.
