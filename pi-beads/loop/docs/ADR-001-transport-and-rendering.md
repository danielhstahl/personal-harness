# ADR-001: Transport and rendering strategy for the pi-beads loop

- **Status:** Accepted
- **Date:** 2026-09-19
- **Ticket:** workspace-5yn.1
- **Spike:** [`../spikes/`](../spikes) (three runnable prototypes + captured evidence in `../spikes/out/`)

## Context

`pi-beads/loop` is a single-purpose application that runs a beads-driven loop:

1. check the board for ready or in-progress work;
2. if the board is empty, idle and take human input like a normal `pi` session;
3. turn that input into new beads issues;
4. otherwise take the top ready issue and work it to completion;
5. commit to git, record a handoff memory, close the bead, and start over.

Two properties constrain every design choice below:

- **The output must look like `pi`.** The user reads this all day. Syntax
  highlighting, thinking-block presentation, tool-call lines and colours must match
  `pi` interactive, or the loop is worse than the tool it replaces.
- **Each iteration starts with no conversational context.** This is a feature, not
  a limitation. Durability lives in beads + git, so state crosses iterations
  through the issue payload and `bd remember` memories — never through a retained
  transcript. A loop that leaks context between beads accumulates garbage until it
  is unusable.

Both `@earendil-works/pi-coding-agent` (the SDK, v0.85.1) and
`@earendil-works/pi-tui` are available. The scaffold under test had begun the
opposite way: spawn `pi --mode rpc` and parse JSONL by hand.

## Decision

### 1. Transport: the in-process TypeScript SDK, not a spawned `pi`

Use `createAgentSession()` (with `ModelRuntime`, `SessionManager`,
`SettingsManager`) directly in our process. `AgentSession` is the same object the
interactive, print and RPC modes are built on — we become a fourth mode rather
than a subprocess of one.

```ts
const { session } = await createAgentSession({
  modelRuntime,
  model,
  sessionManager: SessionManager.inMemory(),
  resourceLoader: loopResourceLoader(),  // see Consequences
  settingsManager,
});
session.subscribe(renderEvent);
await session.prompt(buildPrompt(issue));
session.dispose();
```

### 2. Rendering: pi's own components on pi-tui, never hand-rolled ANSI

Render with `AssistantMessageComponent`, `UserMessageComponent`,
`ToolExecutionComponent`, `FooterComponent` and the live `Theme`, laid out with
`pi-tui` primitives (`TuiMainScreen` / `ProcessTerminal`, `Markdown`, `Editor`).
Pi's highlighting is `highlight.js` driven by the theme's `syntax*` colour roles,
so reusing the components means highlighting is *inherited*, not reimplemented.

`src/format.ts` (a copied-in file of hand-rolled ANSI helpers) is **demoted**: it
may be kept for one-line tool summaries used by plain-log output, but it is not
the renderer. Note that as it stands the file has **zero `export` statements**, so
it is currently unreachable from anywhere — a spike assertion now guards against
it quietly becoming the renderer by accretion.

### 3. Context reset: a brand-new `AgentSession` per iteration, in memory

Every iteration constructs `SessionManager.inMemory()` and a fresh session, and
`dispose()`s it at the end. We do **not** attempt to simulate a reset inside one
long-lived session with `navigateTree()` or `compact()`.

The issue payload, the recalled memories and the git state are the *only* inputs to
the prompt builder. If the next iteration needs to know something, it must be
written down before the previous one dies.

## Evidence

All three spikes run green against the local model (`llamacpp /
halogen-qwen3.8-flash-next`), pi 0.85.1. Full transcripts:
[`../spikes/out/`](../spikes/out).

**[`spikes/1-render-highlight.ts`](../spikes/1-render-highlight.ts)** — 11 render
assertions, no model call, deterministic:

- `highlightCode()` emits 7 distinct SGR codes for one TypeScript snippet —
  per-token, not per-block: keyword `38;5;74`, string `38;5;174`, comment
  `38;5;65`.
- A Markdown document containing a fenced TS block renders 20 lines at 78 cols
  with 12 distinct codes — code inside markdown keeps its syntax colour.
- `AssistantMessageComponent` renders outside `InteractiveMode` from a synthetic
  assistant message (thinking + text), 23 lines / 14 codes, and re-renders
  identically — which is exactly what live streaming needs.
- `src/format.ts`: 0 export statements found. Confirmed un-importable today.

**[`spikes/2-fresh-session.ts`](../spikes/2-fresh-session.ts)** — 9 context
assertions against the live model, three iterations in **one process**:

| # | session created | msgs before → after | context tokens | outcome |
|---|---|---|---|---|
| 1 | 3 ms | 0 → 2 | 105 (cache-read) | streamed Python `fib()` block |
| 2 | 1 ms | 0 → 2 | 108 (cache-read) | TypeScript `reverse()` |
| 3 | 1 ms | 0 → 2 | 125 (cold input) | asked what the earlier session was told: `UNKNOWN` |

- Session construction is 1–3 ms — no process spawn, no module cold start.
- `messagesBefore === 0` every time; three distinct `sessionId`s; `sessionFile`
  `undefined` (nothing persisted).
- Context stays flat: iteration 3 costs 125 tokens, **not** the 213 it would pay
  if iterations 1 and 2 were still in the window. Iterations 1–2 hit the provider
  prompt cache (`cacheRead` 105 / 108); iteration 3 uses a different system
  prompt, so all 125 of its tokens are cold `input`. Each iteration pays a cold
  start and inherits nothing from its predecessors.
- Iteration 3 answers `UNKNOWN` when asked what code it was previously told to
  write. This is amnesia, not truncation.
- The live-streamed code block renders with 9 distinct syntax codes — the real
  stream goes through the same highlighted path as the static case.
- Time-to-first-delta was 1.8 s–3.4 s and is entirely model-bound; the harness
  contributes single-digit milliseconds.

**[`spikes/3-rpc-comparison.ts`](../spikes/3-rpc-comparison.ts)** — the rejected
path, measured rather than assumed:

- A correctly spelled `pi --mode rpc` round trip for one trivial reply: 2 837 ms
  to first text delta, 3 002 ms total, **36 JSONL events across 9 event types**
  (`response`, `agent_start`, `turn_start`, `message_start`, `message_end`,
  `message_update`, `turn_end`, `agent_end`, `agent_settled`) to marshal per
  iteration — plus a child process to supervise, and the strict LF-only framing
  requirement (Node `readline` is *not* compliant: it splits on U+2028/U+2029,
  which are legal inside JSON strings).
- The scaffold's verbatim spawn — `spawn("pi", [" --mode", "rpc", ...])`, note the
  leading space — produced **0 bytes of stdout**, never entered RPC mode, and had
  to be killed at the 20 s timeout (exit 143). The scaffold as committed does not
  work, and the `stdio: "inherit"` choice in the same function means even a
  correct invocation would not be parseable by the caller.

Absolute speed is not the argument — model latency dominates both paths. The
argument is structural: RPC adds a process boundary, a hand-maintained framing
layer, and a child lifecycle, and then still hands us raw JSON that must be
re-mapped onto the very components we could have used directly.

## Rejected alternatives

**1. Spawn `pi --mode rpc` (the scaffold's direction).** Rejected as above: 9
event types over a pipe per reply, custom JSONL framing, subprocess supervision,
~hundreds of ms of process startup per iteration, and no access to the render
components. Also the worst failure mode of all — silent: a broken argument string
hangs with no output.

**2. `RpcClient` from the SDK (typed subprocess middle ground).** Better types than
hand-rolled JSON, but it keeps the process boundary, the framing and the child
supervision. It only pays off when the agent must live in a different process,
which is not our situation.

**3. `pi -p` print mode per iteration.** One-shot, no progressive rendering and no
event stream, so the "idle like `pi` then split then work" experience collapses
into blocking chunks. Also gives no hook to abort cleanly mid-bead.

**4. Hand-rolled ANSI renderer (`src/format.ts`).** Would duplicate pi's
`highlight.js` + theme-role pipeline by hand, drift upstream on every pi release,
and is currently unreachable (no exports). Kept only for optional one-line
summaries in log output.

**5. `navigateTree()` / `compact()` inside one long session to "reset" context.**
Compaction is a lossy *model-generated summary*: it retains prior turns, costs
tokens to produce, and its fidelity is not controllable enough to be a security
boundary between beads. Our loop's correctness depends on the previous bead being
unknowable. A fresh session gives a hard boundary for free.

**6. `SessionManager.create()` persistent sessions, reused across iterations.**
Deliberately persists exactly what we need to forget, and adds session-file churn
that could be mistaken for the loop's state store. If a per-bead session transcript
is later wanted for audit, write it explicitly *after* finalizing — as an
artifact, never as an input.

## Consequences

**Becomes easy**

- Highlighted streaming with no renderer to maintain; upstream theme changes arrive
  for free.
- Iteration setup costs ~1 ms, so the loop can restart aggressively.
- Direct typed access to `session.messages`, `usage`, `abort()`, `dispose()` —
  the finalize step can read exactly what happened instead of reconstructing it
  from a log.
- One process: signal handling, config, beads calls and the agent share state, so
  "abort the run" is a function call, not a `kill`.

**Becomes our problem**

- **In-process blast radius.** An unhandled throw in our code takes the agent with
  it. Needs an error boundary around each iteration and must re-queue the bead
  rather than lose it (→ workspace-5yn.12).
- **Resource-loader policy is now an explicit decision.** The spikes use a *bare*
  loader (no extensions, no skills, no `AGENTS.md`) so the proof is reproducible.
  The real loop wants repo context from `AGENTS.md`, but should not silently
  inherit the developer's ambient packages (e.g. `pi-workgraph`) into a work
  iteration. Decide per mode — idle vs work — and put it in config
  (→ workspace-5yn.11, workspace-5yn.5).
- **We own the UI shell**: editor, keybindings, resize, the idle prompt
  (→ workspace-5yn.6) and the work-mode layout (→ workspace-5yn.10).
- **These components are exported from the package root but are not documented as
  a stable API.** Pin exact versions (`0.85.1`, matching the installed `pi`) and
  re-run spike 1 on every bump — it is cheap, offline and deterministic, which
  makes it the regression harness for this decision.

**Forces a habit**

Because context is genuinely gone between iterations, anything the next bead needs
must be written down before the current one dies: the `bd remember` handoff in the
finalize step (→ workspace-5yn.8) is load-bearing, not a nicety. Spike 2's `UNKNOWN`
is the proof that nothing else will remember for us.

## Follow-ups

- Loader policy per mode: idle (interactive-ish, ambient extensions acceptable)
  vs work (locked down, reproducible) — decide in workspace-5yn.5 / .11.
- Confirm `ToolExecutionComponent` covers the beads/git tool output shapes, and
  settle the collapsed-summary format in workspace-5yn.10.
- Rendering is theme-driven from `~/.pi/agent/settings.json` via
  `initTheme(name)`; decide whether the loop follows the user theme or pins one.
- If a future requirement forces the agent out of process, revisit
  `RpcClient` — but re-read the RPC event-marshal count above first.
