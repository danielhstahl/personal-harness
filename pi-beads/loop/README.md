# pi-beads loop

A single-purpose [pi](https://pi.dev) application that runs a beads-driven loop:
check the board → idle for human input if empty → split input into beads → work the
top ready issue in a **fresh session** → commit, record a handoff, close, restart
with no conversational context carried over.

Status: under construction. The toolchain and the ADR are in place; the loop body
arrives in `workspace-5yn.4`–`.9`.

## Requirements

- Node `>= 22.19.0`
- `bd` (beads) on `PATH`, with a project DB and `no-git-ops = true`
- A configured pi model (`~/.pi/agent/models.json`)

## Scripts

| Command | What it does |
| --- | --- |
| `npm run dev` | `node src/main.ts` — run the TypeScript sources directly, no build step |
| `npm run typecheck` | `tsc -p tsconfig.json --noEmit` (strict; must stay clean) |
| `npm test` | `node --test test/*.test.ts` — adapter tests against a fake `bd` shim |
| `npm run build` | `tsc -p tsconfig.build.json` → `dist/` |
| `npm start` | `node dist/main.js` |
| `npm run clean` | remove `dist/` |

## Layout

```
src/main.ts          entry point: reads the environment, calls runApp. Nothing else.
src/app.ts           composition root: builds the real adapters, runs the loop, and
                     owns the one-idle-surface-per-turn rule (see perTurnIdle)
src/loop.ts          the interpreter: executes the machine's effects, decides nothing
src/orchestrator.ts  the loop's decision layer: pure, effects-as-data, no I/O at all
src/agent.ts         one pi AgentSession per iteration; fresh in, disposed out, verdicts only
src/split.ts         SPLIT — a request in, a recorded epic plus ordered children
src/finalize.ts      FINALIZE — commit, then remember, then close; or nothing at all
src/vcs.ts           the ONLY module that shells out to `git` (typed, side-effect-safe)
src/beads.ts         the ONLY module that shells out to `bd` (typed, side-effect-safe)
src/idle.ts          the idle surface: pi's own TUI input, clean exits, raw text back.
                     Single-shot by contract — one surface answers once, then it is
                     torn down, so the root builds a fresh one per idle turn
src/format.ts        one-line plain-log summaries — NOT the renderer (see ADR-001)
docs/               ADR-001: transport + rendering decision
spikes/             throwaway prototypes + captured evidence backing ADR-001
test/               unit tests, plus the whole walk in test/loop.test.ts
```

## The loop machine

`src/orchestrator.ts` is a reducer: `step(state, event) -> { state, effects }`.
It decides; it never *does*. Nothing in it touches a file, socket, process, the
clock or the environment — the compiled JS for that file contains zero imports —
so the whole loop is testable with no board and no pi.

States: `init → check_work`, then `idle` / `split` / `pick` / `work` /
`finalize` / `restart` back to `check_work`; `done` and `aborted` are terminal.
Every side effect leaves as data (`beads.*`, `agent.*`, `vcs.commit`, `ui.*`,
`drop_context`), and `src/loop.ts` is the interpreter — a dispatch loop over
`OrchestratorPorts`, the only place those decisions touch the world. It owns no
phase logic of its own: `step()` is called from exactly one function, and an
effect kind with no handler is a typed stop, never a silent no-op.

`restart` is a real state that emits `drop_context`. Per ADR-001 that boundary is
what buys "no context carried between iterations" — a fresh agent session, not a
compacted one — so it shows up in every trace instead of being a back-edge a later
refactor can shortcut into a warm state.

A rejected transition is provably inert: it emits no effects and hands back the
same state object. The test suite walks the full state × event cross-product, so
"undefined behaviour" is not an option anywhere in the loop.

## Reading `bd`'s dependency shapes

bd emits dependencies in **two shapes**: `bd list` / `bd ready` return edges
(`{issue_id, depends_on_id, type}`) while `bd show` returns the other issue
inlined with `dependency_type`. Reading `depends_on_id` off a `bd show` result
yields `undefined`, which silently means "not blocked". Use
`normaliseDependencies(issue)` / `dependsOn(issue, id)` from `src/beads.ts`.

## Decisions worth knowing before you touch this

Read [`docs/ADR-001-transport-and-rendering.md`](docs/ADR-001-transport-and-rendering.md).
In one line each:

- **In-process SDK**, not a spawned `pi --mode rpc`.
- **Pi's own components** render output, so highlighting is inherited from
  highlight.js via the theme — no hand-rolled ANSI.
- **One fresh `AgentSession` per iteration** (`SessionManager.inMemory()`), which
  is how "start over with no context" is enforced structurally rather than by
  discipline.

Env knobs read by the current entry point: `PI_PROVIDER`, `PI_MODEL`, `PI_THEME`,
`LOOP_WIDTH`, the per-pass thinking levels `LOOP_WORK_THINKING` /
`LOOP_SPLIT_THINKING` — one of `off`, `minimal`, `low`, `medium`, `high`,
`xhigh`, `max` — and the budget knobs `LOOP_WORK_TIMEOUT_MS`, `LOOP_WRAP_UP_MS`
and `LOOP_RETRY_UNFIT_WORK` (see "Two clocks" below). Neither knob set is not the
same as either set to `low`: unset falls through to the user's configured default
and then pi's own, so a ticket is never run at a level nobody chose. A value pi
does not recognise stops the loop with the list of what it accepts, rather than
being quietly dropped.

### Two clocks: the budget and the context wall

`timeoutMs` (default 20 minutes) is a wall clock. It answers *did this take too
long* and nothing else — which is why it was a bad witness for "the run stalled
around 128K tokens". A provider that refuses a request for being over the window
fails in milliseconds and lands as `prompt rejected: …`; a run that expires is a
run that was still generating. `test/agent.test.ts` keeps those two paths from
looking alike.

What a too-small *declared* window really does is quieter, and worse: pi clamps
each request's answer budget to what the window still has —
`min(maxTokens, max(1, window − context − 4096))` — so the answer shrinks long
before anything errors, and near the wall the model is asked for a single token.
That is not slow work, it is finished work, and the twenty minutes spent
discovering it is the worst outcome available.

So the loop measures instead of guessing. Every assistant turn carries the
provider's own count of the request that produced it (`usage.input +
usage.cacheRead`), and `src/context.ts` works out what the *next* request would
be granted. When that grant falls to `UNWORKABLE_OUTPUT_TOKENS` (2048) or below,
the run stops immediately as `context-exhausted` — seconds since the last turn,
not the rest of the budget:

```
context exhausted after 3 assistant turn(s): 126.0K/128.0K tokens used (98%),
1 of answer budget left for the next request — stopped instead of spending the
run budget (settled after abort)
```

- `CONTEXT_SAFETY_TOKENS` mirrors pi's own reserve; a drift test pins it to
  `@earendil-works/pi-ai/api/simple-options`, because a silent change upstream
  moves the wall this measures to.
- A session that reports no model gets no opinion — never a false "full".
- **What to do with one:** raising the budget does nothing, because the budget
  was never what stopped it. Either the declared `contextWindow` is smaller than
  what the server takes, or the ticket does not fit in the window it has. The
  first is a config fix; the second is a split.

### The off-ramp: ask before cutting

A hard timeout is the least informative way a run can end. There is no verdict,
no `changed_files`, nothing the finalizer could commit if it wanted to, and a
working tree left dirty behind a session that was cut off mid-sentence. The loop
then re-queues the ticket and the next attempt starts from nothing except a note
that says *the last one ran out of time* — which is a description of the harness,
read by a model that has to decide what to do with the work.

So at `wrapUpMs` — by default the budget minus `WRAP_UP_LEAD_MS` (3 minutes) —
the runner sends `session.steer(wrapUpInstruction(kind))`: finish the edit you
are in, call `report_done` now, `done: false` with what is left is a good answer.
The steering lands after the current turn's tool calls, so nothing is interrupted
mid-write, and the same clock that was going to cut the run off instead ends it
with a verdict: a reason, a file list, and a next attempt that reads something
about the *work*.

- The message carries **no number**. A model given "you have 3m14s left" cannot
  check it and will spend turns trying; it is told what to do, which it can act
  on.
- A budget smaller than the lead gets no off-ramp at all (`wrapUpAtMs` returns
  `0`), because there would be nothing to land with. That is stated rather than
  faked: `test/agent.test.ts` asserts no steer happens.
- A run that ignores the ramp still ends at the budget. The nudge is on the
  record either way (`wrap_up` runner event, painted as `wrapping up <issue>`).

### A run that ran out of the harness's room is not retried in place

The next pass after a timeout is a *fresh session with the same budget and the
same window*, so it stops in the same place one whole budget later. With the old
defaults that meant forty minutes to rediscover that one ticket does not fit one
session — and the line `Re-queued for another pass.` printed just before the loop
stopped, which is the one sentence in that trace a reader acts on and the loop
does not honour. So:

- `timeout` and `context-exhausted` set **unfit work**, and the guard stops the
  run at the boundary instead of starting the next pass. The bead is reopened and
  the note is written first, so nothing is stranded: re-run after splitting the
  ticket and it is picked up like any other open work.
- `incomplete` is *not* unfit. "I got this far and stopped" is a verdict about
  the work, and a verdict is worth another pass — which is the whole reason the
  off-ramp is worth having.
- `retryUnfitWork` (`LOOP_RETRY_UNFIT_WORK=true`) restores retrying, up to
  `maxConsecutiveFailures`, for a timeout that could plausibly clear.
- No transition claims another pass is coming. The orchestrator says the bead is
  open on the board again, which is the only part of that sentence it knows.

### One clock per work unit

The presenter used to reset its elapsed field when the *issue* changed. Work the
same ticket twice and the second pass reported both passes added together: a
`1200000ms` budget shown as `timed out after 40:01`, which reads like a budget
that doubled rather than a ticket that failed twice. Two fixes, both pinned:

- `setContext` takes a `runId`; the composition root issues one per work,
  split or finalize unit, and a new `runId` resets the clock, the token counters
  and the expand state the way a new issue always did. (The finalize step was the
  same lie in slow motion: it showed the work pass's elapsed.)
- The `timeout` event carries `elapsedMs` and `budgetMs` from the runner, which
  is the only party that knows them. The surface's own clock is a fallback, not
  the answer.

### What the next attempt reads

`describeFailure` output is the board note (`loop:failure:<id>`) and the
`prior_attempt` section of the next prompt, so its shape is model behaviour, not
just log cosmetics. A timeout note now reads:

```
Work on workspace-7eg failed: timed out at 20:00 of a 20:00 session budget (the session closed when asked)

That is the harness's limit, not a verdict on this ticket: nothing in it says
the work was wrong, and there is no clock inside your session. ...

What the last attempt had said by the time it was cut off:
> Reading the repo. Plan: open the parser, add the mode, then the docs…
```

Headline for the human, framing and recovered prose for the model. The cut-off
reply is read out of the session *before* it is disposed — before this, the
timeout path left `assistantText` empty and threw away the only thing worth
handing forward. The warn line shows the headline only (`headlineOf`), because
parking a sentence on the end of a quote is what a plain log ends up showing.

### Chain of thought is billed to the context window

pi replays an assistant turn's thinking on the next request — that is what the
`reasoning_content` field on the replayed assistant message is. Measured against
this project's server: the same history costs **94 tokens without** the replayed
CoT and **270 with it**. Every turn, compounding, on a loop that deliberately
never compacts.

There is no client-side switch that suppresses a non-empty replay. The server can
be told, though, through the chat template:

```jsonc
"compat": {
  "thinkingFormat": "chat-template",
  "chatTemplateKwargs": { "preserve_thinking": false }
}
```

That is the whole of what `chat-template` sends here, and the same A/B drops from
**270 to 90** — thinking still streams to the terminal, it just stops being
carried back in. Do **not** reach for `thinkingFormat: "qwen-chat-template"`
instead: pi hard-codes `preserve_thinking: true` there, which is precisely the
other answer.

However, for agentic workflows it is still recommended to keep the CoT within context.  Using `"preserve_thinking": true` is prefered for these types of coding use-cases.  The server brings data back pre-parsed into thinking and non-thinking responses.  Technically having `thinkingFormat` and the `chatTemplateKwargs` are not even required, but is included to be explicit about what is sent to the server.

One consequence worth naming, because it was observed before it was understood: with
the CoT replayed, whatever the model *says to itself* about time is re-read on
every later turn. Point it at a clock and it will spend budget on the clock —
which is why nothing inside a running session carries a countdown the model
cannot check, and why the note a timed-out attempt leaves says whose limit it was
before it says anything else (see "What the next attempt reads"). The replay is
paid for on purpose; what it should be carrying is work, not anxiety about the
harness.

### Cadence: `coalesceMs` and `heartbeatMs`

pi-tui has no frame rate — it draws when something asks it to. The presenter
asks through a coalescing window, so the refresh rate is two numbers on
`AppConfig`, doing two different jobs:

```ts
buildApp({ cwd: repo, coalesceMs: 33, heartbeatMs: 500 });
```

- **`coalesceMs` (default 33 ≈ 30fps) is the streaming rate.** Every delta
  inside one window costs exactly one frame, however fast the tokens arrive.
  `16` gives ~60fps; below that the terminal is written more often than it
  paints and nothing looks smoother. This is also rule 3: 300 deltas must not
  become 300 writes.
- **`heartbeatMs` (default 500) is the clock, not the animation.** A surface
  we hold repaints while no event is arriving, so the elapsed field stays
  honest. It repaints only when a time-derived footer field actually moved —
  and that field has one-second resolution.

They are not interchangeable, and that is the diagnostic: a surface running at
about 1fps *while its clock ticks* has a missing paint request in whatever
updated the content, not a cadence to tune. That is what this was for a while —
a streamed reply painted once, when its block was created, and then only on the
beat. A stream drives its own frames now (`heartbeatMs: 0` still paints every
window), and `rule 3` / `rule 14` in `test/render.test.ts` pin both halves of
that. `presenter.stats()` reports `paints`, `coalescedTicks` and the two
cadence numbers, so the rate is measurable rather than eyeballed.

## Scratch beads DB (for live / integration checks)

Never run a live check against a real board. `bd` resolves its database from
`BEADS_DIR`, so an isolated one is a single env var away:

```sh
mkdir -p /tmp/loop-live/.beads
scratch=$(mktemp -d) && cd "$scratch" && git init -q .
BEADS_DIR=/tmp/loop-live/.beads BD_NON_INTERACTIVE=1 bd init --prefix live
BEADS_DIR=/tmp/loop-live/.beads bd list --json   # prove you are isolated
```

`src/beads.ts` forwards `BEADS_DIR` through its `env` option, which is how the
live pass exercised real bd 1.3.0 without touching `~/.beads`.
