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
| `npm run audit` | compare `models.json` against the provider's `/health` and print the config changes it suggests (see "Startup: the provider comparison") |
| `npm start` | `node dist/main.js` |
| `npm run clean` | remove `dist/` |

## Layout

```
src/main.ts          entry point: reads the environment, calls runApp. Nothing else.
src/health.ts        the provider probe: derive `<base>/health` from the API base URL, GET it once, never hang
src/audit.ts         the startup comparison — pure, no I/O. Rules over the health report and the resolved model config; findings as data, suggestions that patch `models.json`
src/startup.ts       the interpreter for both: read `models.json`, resolve the target, probe, compare, print, optionally write the suggested config
src/audit-cli.ts     `npm run audit` — the same comparison without starting the loop
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
src/monitor.ts      the backend monitor: read-only polling of /health, /metrics, /cache,
                    /v1/models, with a tolerant field reader and a panel. GET-only, and
                    no bd/session handle exists in it, so it cannot touch the run
src/kanban.ts       the mini kanban: ready / in progress / done, read through an injected
                    `read()` closure — no BdClient in its signature, so nothing in the
                    module could move a ticket. Two layouts, backoff, and three honest
                    states per column (see ADR-004)
src/notify.ts       the completion notice: what a finished bead says, in what order,
                    and when to stop trying to say it. Wording only — no sockets
src/mail.ts         the ONLY module that opens a socket to a mail server: RFC 822
                    rendering, quoted-printable, folded headers, a hand-rolled SMTP
                    client, and a mailer whose every failure path returns a delivery
                    instead of throwing (see ADR-005)
src/format.ts       one-line plain-log summaries — NOT the renderer (see ADR-001)
src/gitlock.ts      the ONLY module that spawns git, and the only kill policy:
                    SIGTERM first, SIGKILL as escalation, because a killed git
                    strands .git/index.lock (see ADR-006)
docs/               ADR-001: transport + rendering decision
                    ADR-002: comparing the provider's /health report with models.json at startup
                    ADR-003: the backend monitor — where it is drawn, and why not at the top
                    ADR-004: the mini kanban — read-only by shape, and what "unread" must not render as
                    ADR-005: the completion notice — an effect the machine asks for,
                             a report that can never break the run
                    ADR-006: the index lock — wait it out, and never be the thing
                             that strands it
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

And [`docs/ADR-002-startup-provider-comparison.md`](docs/ADR-002-startup-provider-comparison.md):
`models.json` is a list of claims about a machine that is not in this repo, so
before the first ticket is claimed the loop asks that machine what it is and
prints the disagreements. See "Startup: the provider comparison" below.

And [`docs/ADR-003-backend-monitor.md`](docs/ADR-003-backend-monitor.md):
while a ticket is being worked the loop also watches what that machine is
*doing* — KV pressure, queue depth, drafter, throughput — and draws it in the
fixed chrome, because a component appended to the transcript scrolls off
instead of staying at the top. See "The monitor" below.

And [`docs/ADR-004-mini-kanban.md`](docs/ADR-004-mini-kanban.md):
the loop is a walk over a kanban — ready → in progress → done — and nothing on
the screen showed it. The board is drawn beside the monitor, on the same terms:
read-only *by shape* (it takes a `read()` closure, not a client), never
load-bearing, and honest about the difference between a queue that is empty and a
queue it could not read. See "The board" below.

And [`docs/ADR-005-completion-notice.md`](docs/ADR-005-completion-notice.md):
when a bead closes, the machine asks — as data — for a notice to be sent, and the
interpreter decides who hears about it. Mail is a *report* about the run and
never a dependency of it: a relay that is down costs a warning, a streak limit
and a transcript line, and nothing else. See "The completion notice" below.

And [`docs/ADR-006-index-lock.md`](docs/ADR-006-index-lock.md):
`.git/index.lock` was killing runs — and, worse, keeping them dead. Two measured
facts drove the fix: a git stopped with `SIGKILL` **strands** the lock, so a
single timed-out `git add` used to break every later write in the repo until a
human intervened; and `git status` takes the same lock to refresh the stat cache,
so contention with an IDE or a second run is normal, not exceptional. The loop
now stops git with `SIGTERM` first, retries *only* lock contention, and ages a
lock before calling it stale — removing one is opt-in, because pulling a live
process's lock out is how you corrupt an index. See "The index lock" below.

Env knobs read by the current entry point: `PI_PROVIDER`, `PI_MODEL`, `PI_THEME`,
`LOOP_WIDTH`, the per-pass thinking levels `LOOP_WORK_THINKING` /
`LOOP_SPLIT_THINKING` — one of `off`, `minimal`, `low`, `medium`, `high`,
`xhigh`, `max` — the budget knobs `LOOP_WORK_TIMEOUT_MS`, `LOOP_WRAP_UP_MS` and
`LOOP_RETRY_UNFIT_WORK` (see "Two clocks" below), the startup audit knobs
`LOOP_AUDIT`, `LOOP_AUDIT_STRICT`, `LOOP_AUDIT_VERBOSE`, `LOOP_AUDIT_WRITE` and
`LOOP_HEALTH_URL` (see "Startup: the provider comparison" below), the monitor
knobs `LOOP_MONITOR*` (see "The monitor" below), the board knobs
`LOOP_KANBAN*` (see "The board" below), the completion-notice knobs
`LOOP_NOTIFY_*` / `LOOP_MAIL_*` (see "The completion notice" below), and the
git-lock knobs `LOOP_GIT_LOCK_WAIT_MS` / `LOOP_GIT_KILL_GRACE_MS` /
`LOOP_GIT_STALE_LOCK_AFTER_MS` / `LOOP_GIT_STALE_LOCK` (see "The index lock"
below). Neither knob
set is not the same as either set to `low`: unset falls through to the user's
configured default and then pi's own, so a ticket is never run at a level nobody
chose. A value pi
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

## Startup: the provider comparison

Read [`docs/ADR-002-startup-provider-comparison.md`](docs/ADR-002-startup-provider-comparison.md).

`models.json` is a list of assertions about a machine that is not in this repo,
and the server is the only witness worth checking. Before the loop reads the
board it does one `GET` on the provider's health report and diffs it against the
config the run is about to use. The report lives at the **base** of the URL, not
under the version prefix: pi sends requests to `http://host:8081/v1` and the
report is at `http://host:8081/health` — `healthUrlFor()` in `src/health.ts` is
the only place that transformation is encoded.

What it checks, in one line each (25 rules, `src/audit.ts`):

- **context window** — declared vs served, taking the tightest of `context`,
  `slot_ctx` and `kv_pool_positions`. Undershooting is the expensive one: the
  wall in `src/context.ts` is the *declared* number, so a ticket that would have
  fitted in the window you pay for stops short and reports a false thing about
  the work.
- **output ceiling** — over the server's cap is a 400; under the thinking budget
  plus answer room is a truncated `report_done`. `token_budget_covers_reasoning`
  decides whether that check applies at all.
- **the thinking channel** — does a requested level actually reach the server?
  Under `thinkingFormat: "chat-template"` pi sends only what
  `chatTemplateKwargs` lists, so a config that lists just `preserve_thinking`
  leaves every level in the harness choosing something nobody receives and the
  server running *its* default. Same for `enable_thinking`, which is what makes
  `off` mean off.
- **effort names** — pi bills `xhigh` and `max` at the same budget but sends the
  name it was given; a name the server has no value for comes with a suggested
  `thinkingLevelMap` translation.
- **tools** — no tool support means no `report_done` means nothing can finish.
  `constrained_decoding: false` means pi's `strict` schemas cannot be honoured.
  `forced_call_disables_thinking` is why `report_done` is asked for, never
  coerced.
- **vision** — an image-capable `input` over a server with no vision tower turns
  every `read` of a PNG into a refused request mid-ticket.
- **sampling** — implemented vs accepted-but-ignored vs mirrored-from-the-server-
  defaults (which is a freeze, not a no-op: a field you send always wins).
- **stream usage** — the context guard eats `usage` every turn; without it the
  guard is blind and the wall clock is the only clock left.
- **the harness's own tool schemas** — against the server's enforced /
  accepted-not-enforced / refused keyword lists.
- **cache and capacity** — a warm cache that is not bit-identical (a re-run is
  not a repro), and a server already queueing at startup, which the work budget
  is charged for like any other time.
- **what it could not check** — compat keys the report never spoke to are listed
  as unverified rather than counted as passing.

Findings are ranked `error` / `warn` / `info` / `ok`, each carrying its own
patch: a scope, a dotted field, a value and a reason. `applySuggestions()`
resolves a model with a raw `models` entry to `providers.<p>.models[i].<field>`
and one without to `providers.<p>.modelOverrides.<id>.<field>`; two rules never
claim the same key. The suggested `$var` and budget values are pinned by drift
tests to pi's own schema and source, so the audit cannot suggest a key this
version of pi would reject.

It is a warning by default, never a crash, and it never rewrites the live config
unasked:

| Knob | Effect |
| --- | --- |
| `LOOP_AUDIT=0` | skip the request entirely (an endpoint with no health page) |
| `LOOP_AUDIT_STRICT=1` | an `error`-severity finding stops the run (`provider-audit`, exit 2) |
| `LOOP_AUDIT_VERBOSE=1` | also print the checks that passed |
| `LOOP_AUDIT_WRITE=1` | write `models.json.proposed` beside the real file |
| `LOOP_AUDIT_WRITE=inplace` | patch `models.json` and keep `models.json.bak` |
| `LOOP_HEALTH_URL=…` | use this URL instead of the derived one |
| `LOOP_AUDIT_TIMEOUT_MS` | the probe's deadline (default 2000) |

```sh
npm run audit                    # the same comparison, without starting the loop
LOOP_AUDIT_WRITE=1 npm run audit # and leave a models.json.proposed to review
```

Against a real report and this repo's config (template values filled in), the
warnings it prints are the real ones:

```
provider audit — llamacpp/halogen-qwen3.8-flash-next against http://inference.test:8081/health [ok]
  WARN context-window-undersized: 6.1K of window is unreachable (256.0K declared, 262.1K served)
         → set providers.llamacpp.models[0].contextWindow = 262144
  WARN output-starves-the-answer: at `xhigh` the ceiling leaves 0 for the answer (16384 total, 16384 of thinking)
         → set providers.llamacpp.models[0].maxTokens = 18432
  WARN thinking-effort-never-sent: `thinkingFormat: "chat-template"` with no effort kwarg: the requested level is never sent
         → set providers.llamacpp.compat.chatTemplateKwargs.reasoning_effort = {"$var":"thinking.effort"}
  WARN thinking-enable-never-sent: nothing in the request can switch thinking off
         → set providers.llamacpp.compat.chatTemplateKwargs.enable_thinking = {"$var":"thinking.enabled"}
  WARN strict-mode-without-decoding: pi sends `strict` tool schemas and this server has no constrained decoding
         → set providers.llamacpp.compat.supportsStrictMode = false
0 error, 5 warn, 8 info, 12 ok (25 check(s), 6 suggested change(s))
```

Leave those six alone and every pass runs at the server's `xhigh` whatever the
harness asks for, with no room left for the verdict it has to hand back.

Printed output is secret-redacted (`«redacted»` for a literal key, `$ENVVAR`
references left intact). The file the audit writes is *not* redacted: it lands
beside the file that already holds the credential, and a redacted key there would
just be a broken config.

## The monitor: what the server is doing right now

ADR-002 answers what the server *is configured as*, before the first ticket. The
monitor answers what it is *doing*, while a ticket is being worked — the question
that arrives every time a pass is slow and the answer is sitting in `/metrics`:

```
● 1s · t/s 132 · kv 64% 168k/262k · slots 1/4 · queued 0
ctx 262k · out 66k · draft mtp · cache 89% hit · req 2.0/s (415 served)
```

Top line is the live one (throughput, KV pressure, who else is here). Second
line is the settled one (window, output ceiling, drafter, prompt cache, request
rate, loaded model). `● 1s` is the age of the freshest reading, so a stalled
server keeps its last good numbers without pretending to be current; if nothing
ever answered the line reads `✕ unreachable: …` instead of a reading.

**Where it is drawn.** Not at the top of the transcript — a component appended
there scrolls off within a screenful (ADR-003 has the measurement). While a
session runs it lives in the fixed chrome, the band immediately above the
footer, which is the only region that is redrawn in the same place every frame.
When the loop is idle there is no transcript, so the monitor is the top line
proper. When the session is released the band stops being drawn: those lines
describe what the server is doing *now*, and after a handoff “now” is no longer
describing the work you are looking at. A piped log never sees the panel at all.

**It is read-only, by shape.** No `bd` handle, no issue id, no `POST`. Every
failure path returns a value rather than throwing. A field it cannot find is
`—`, never a guess: if nothing exposes the KV capacity, the panel says
`kv —` rather than dividing by an assumed pool.

Rates are slopes and refuse to be computed when they would lie: a counter that
went down is a restart, not negative throughput; a renamed counter is not
differenced at all. Cache hit rate is reported as an interval with its delta
(`cache 89% hit +21/-2`), so the first read shows `—` rather than a
fabricated lifetime average that stays flattering long after the cache stopped
helping.

| Knob | Effect |
| --- | --- |
| `LOOP_MONITOR=0` | no monitor at all (a null source that renders and requests nothing) |
| `LOOP_MONITOR_MS` | poll interval (default 1000) |
| `LOOP_MONITOR_TIMEOUT_MS` | per-request deadline (default 1500) |
| `LOOP_MONITOR_MODELS_MS` | model-list interval (default 30000; the list rarely changes) |
| `LOOP_MONITOR_URL=…` | the backend base, over the provider config and the audit's URL |
| `LOOP_MONITOR_LINES=n` | panel lines, 1–3. 3 adds the per-KV-pool breakdown |
| `LOOP_MONITOR_AT=band\|top` | above the footer (default) or first on the surface (which scrolls) |
| `LOOP_MONITOR_VERBOSE=1` | at startup, print each endpoint's state and the keys it exposed that no field reads |

`/health` advertises where its own other pages live, and on the first cycle it is
read before `/metrics`, `/cache` and `/models` are asked — so a server keeping
its counters at `/counters` is never once asked at `/metrics`, and a
non-standard layout needs no configuration. An endpoint that answers 404 is
dropped from the rotation with a note rather than retried forever.

It costs what it says it costs: three or four small `GET`s per second, one frame
per poll through the existing coalescing window, and never two polls at once — a
slow server produces one long wait, not a queue of them.

```sh
node tools/monitor-stub.mjs   # the panel, against a stub backend — no GPU needed
LOOP_MONITOR=0 npm start                    # work without it
LOOP_MONITOR_LINES=3 LOOP_MONITOR_VERBOSE=1 npm start   # wide panel + what the server exposed
node --test test/monitor.test.ts         # 75 tests over the reader, poller and surfaces
```

## The board: what is left, what is being worked, what will be picked next

ADR-003 answers what the server is doing. The board answers the other question an
operator has while watching this loop, and the loop is the reason it is answerable:
`check_work` reads the ready list every iteration, `pick` takes its head, `work`
holds one `in_progress` ticket, `finalize` closes it. The machine is a walk over a
kanban. Until now the screen showed none of it.

```
▦ ready 4 · in progress 1 · done 5+ · next loop-3k ship the kanban

╭ ready 4 ───────────────────────┬ in progress 1 ─────────────────┬ done 5+ ───────────────────────╮
│· loop-3k ship the kanban       │▸ loop-4k wire the board i… [1m]│✓ loop-1m add the backend … [1h]│
│· loop-7m back off when bd is u…│—                               │✓ loop-2b audit the provid… [5h]│
│· loop-2t blocked behind … ⚠1 +1│                                │✓ loop-0a bootstrap the… [3d] +2│
╰────────────────────────────────┴────────────────────────────────┴────────────────────────────────╯
```
The one-line `row` is the work surface's default — that screen already carries a
transcript, a monitor and a footer. The bordered `board` is the idle default:
nothing there is competing for the room, and "what will it pick" is exactly what is
on the operator's mind before they type. The `ready` column is ordered the way the
picker orders (priority, then longest-waiting), so its head **is** the next pick —
and the row says which one that is.

**Read-only, by shape — harder than the monitor's version.** The loop claims and
closes tickets; a component that could too would be a second driver with one
protocol between them. `src/kanban.ts` takes no `BdClient` and has no write path:
the composition root hands it a `read()` closure returning plain data. There is
nothing in the module that could move a ticket even by accident, which is what
lets it sit next to a live loop at all. (A drag-and-drop kanban was rejected here
for the same reason the loop rejects a claim from anything but its own transition.)

**Absent is not zero.** Every column is `live` / `absent` / `unknown`, and a
column that did not answer renders `?`, never `0`:

```
╭ ready 0 ───────────────────────┬ in progress 1 ─────────────────┬ done ? ────────────────────────╮
│—                               │● loop-4k wire the board i… [1m]│unread                          │
```

`ready 0` is a fact about an empty queue. `?` is a fact about a `bd` that did not
answer. A board that painted an unread queue as empty would be a confident "there
is nothing to do", issued at exactly the moment nobody can check — the worst
possible thing for this screen to say. When the *first* read fails, the board draws
`▦ ready ? · in progress ? · done ?` rather than nothing, because silence here is
indistinguishable from `LOOP_KANBAN=0` and an operator who cannot tell the two
will assume the queue was empty.

**Counts are bounded, and say so.** The closed list is read with a limit, so a read
that came back exactly as long as the window is rendered as a floor — `done 5+` —
not as a count. Exact and truncated are different claims.

**It costs what it says it costs.** Every poll here is a `bd` child process, not a
socket read, so the cadence is 5s rather than the monitor's 1s, and consecutive
failures back off exponentially to a 60s ceiling: a `bd` missing from `PATH` costs
one notice and a slowing trickle, not a spawn per second. Cycles never overlap — a
`bd` that takes three seconds is one long wait, not a queue of them — and one read
is one frame, through the same 33ms coalescing window as everything else. A write
by this run (create, status change, close) triggers a refresh immediately, so the
picture agrees with the ticket you just finished without waiting an interval. A
refresh that fails after a successful write is the board's problem, never the
write's: it shows `?` and backs off.

| Knob | Effect |
| --- | --- |
| `LOOP_KANBAN=0` \| `off` | no board at all (a null source that draws nothing and reads nothing) |
| `LOOP_KANBAN=row\|board` | pick the shape; anything else, including unset, leaves each surface on its own default |
| `LOOP_KANBAN_MS` | read interval (default 5000, floor 500 — each poll is a process spawn) |
| `LOOP_KANBAN_LINES=n` | rows the grid may take, borders included |
| `LOOP_KANBAN_DONE=n` | how many closed tickets the `done` column keeps (default 12) |
| `LOOP_KANBAN_AT=band\|top` | above the footer (default) or above the transcript, independently of the monitor |
| `LOOP_KANBAN_VERBOSE=1` | at startup, print each column's state, its totals, and the interval and backoff ceiling |

The two HUDs are placed independently of each other, and the surface's child order
is the *same rule* the frame assembly uses, stated once. That is not pedantry:
with them stated twice, `LOOP_KANBAN_AT=top` combined with a banded monitor dropped
the board out of every captured frame while it was still on screen — a bug that
looks correct in the terminal and correct in the tests, and is only wrong in the
capture.

```sh
node tools/kanban-demo.mjs          # every shape, no board needed
node tools/kanban-demo.mjs --live   # the same renderer over a real bd, read-only
LOOP_KANBAN=board npm start         # the grid on the work surface too
LOOP_KANBAN=0 npm start             # no board
node --test test/kanban.test.ts     # 84 tests over model, layout, poller and surfaces
```

There is a full web kanban in this repo's `harness.sh` (`bdui`). It stays, and it
is the right tool for browsing the board. This is for the other moment: the two
seconds before you type, when the answer has to be above the keyboard.

## The completion notice: telling somebody when a bead closes

Set one variable and every bead the loop closes sends an email:

```sh
export LOOP_NOTIFY_EMAIL="dev@example.com"
node src/main.ts
```

The subject carries the bead id — that is what the mail gets searched by — and the
body is an index into the real record rather than a paraphrase of it:

```
Subject: [pi-beads] tst.42 completed: Added colour handling to the tokenizer

  tst.42 — Add the colour mode to the parser

  Bead      tst.42
  Title     Add the colour mode to the parser
  Status    closed
  When      2025-09-26 11:03:07 UTC
  Work      done, 3m 11s, iteration 3

  What it did
      Added colour handling to the tokenizer and thread it through the parser.

  Changes
    Commit    deadbeefcafebabe0123456789abcdef01234567
    Files     2 changed
      - src/colour.ts
      - src/parser.ts

  Still to do
      1. docs still need the colour section

  Where to read more
    Bead      bd show tst.42
    Handoff   bd recall loop:handoff:tst.42 --json
    Diff      git show deadbeefcafebabe0123456789abcdef01234567
    Closed as Done: Added colour handling to the tokenizer…
    Repo      /work/project

  — sent by pi-beads-loop on buildhost from /work/project to dev@example.com (…)
    A delivery failure never stops the loop: the work this notice describes is
    committed and closed whatever the mail relay does next.
```

**Mail is a report, never a dependency.** By the time a notice is due the bead is
committed, handed off and closed, so nothing about the mail can change the work.
A failed send costs one warning — *"The bead is closed either way — nothing about
the work changed"* — and after three failures in a row the notice switches itself
off for the rest of the run, because a dead relay is discovered once, not once
per bead for the next six hours. A delivery that succeeds in the middle resets
the streak, and the switch-off is printed on the failure that caused it — not
quietly remembered until the next bead happens to close. The behaviour is not a
knob; the count is (`LOOP_NOTIFY_MAX_FAILURES`).

**Bad config stops the run; bad delivery never does.** A malformed address, a mail
URL that will not parse, or a password with no user stop the loop at startup with
`notify-config` and exit 2 — the same way an unknown thinking level does — so a
typo is found in the terminal it was typed in rather than as a day of missing
mail. What *cannot* fail the run is a relay refusing a message.

**Notice, not noise.** `LOOP_NOTIFY_EMAIL` unset means no notifier, no socket,
no poller. A dry run sends nothing: a dry run never reaches the close that
triggers the notice, which is the same reason it never closes a bead. A failed
bead sends nothing either — it is not a completion, and its reason is already on
the board under the failure key.

| Variable | What it does | Default |
| --- | --- | --- |
| `LOOP_NOTIFY_EMAIL` | Recipients. Comma, semicolon or space separated. **This is the switch.** | unset — no mail |
| `LOOP_NOTIFY_CC` | Extra recipients, copied on every notice | unset |
| `LOOP_NOTIFY_FROM` | The `From:` header. Set this: most relays require it to match the authenticated sender | `pi-beads-loop@<host>`, or the commit identity if one is set |
| `LOOP_NOTIFY_SUBJECT_PREFIX` | The `[pi-beads]` in the subject | `pi-beads` |
| `LOOP_NOTIFY_MAX_FAILURES` | Give up after this many failures in a row | `3` |
| `LOOP_MAIL_URL` | `smtp://user:pass@host:587` or `smtps://host:465`. Query params: `starttls`, `tls`, `timeout` | unset |
| `LOOP_MAIL_HOST` / `LOOP_MAIL_PORT` | The relay, without a URL | `127.0.0.1` / the port the security mode means |
| `LOOP_MAIL_USER` / `LOOP_MAIL_PASSWORD` | Submission credentials | unset — anonymous |
| `LOOP_MAIL_STARTTLS` | `required`, `optional`, `off` | `optional` — upgrade if offered |
| `LOOP_MAIL_TIMEOUT_MS` | Deadline per connect and per reply | `15000` |
| `LOOP_MAIL_INSECURE_AUTH` | Allow a password over a channel that is not encrypted | off — refused outright |

A few defaults are worth knowing about, because they are the ones that bite:

- **TLS is opportunistic by default** and strict when you ask. With `required`
  against a relay that does not advertise STARTTLS the send *fails* rather than
  quietly downgrading. A password on an unencrypted channel is refused unless
  `LOOP_MAIL_INSECURE_AUTH` says the operator means it, and `AUTH` lines are
  redacted in the log whatever happens.
- **Nothing unvalidated goes on the wire.** Addresses are parsed down to their
  `addr-spec` before `RCPT TO`, and header values — including the subject, which
  is an agent-written sentence — have line breaks flattened out of them, so a
  summary cannot forge a header.
- **Copies are copies, not blind copies.** `LOOP_NOTIFY_CC` lands in a real
  `Cc:` header, and the `To:` header names only the addressees. Anyone written
  into both fields gets one notice rather than two. An empty recipient list is a
  skip, not a fallback to some other list — see
  [ADR-005 §8](docs/ADR-005-completion-notice.md).
- **What mail can actually deliver is the relay's problem.** This sends mail; it
  does not sign it. SPF, DKIM, DMARC and whether a provider accepts the sender
  at all belong to the host and the relay, which is why `LOOP_NOTIFY_FROM` is a
  first-class knob instead of a guess at a hostname.
- **In the container these are ordinary `-e` flags:**
  `docker run -e LOOP_NOTIFY_EMAIL=dev@example.com -e LOOP_MAIL_URL=smtps://…`.

Read [`docs/ADR-005-completion-notice.md`](docs/ADR-005-completion-notice.md)
before changing any of it — in particular why the SMTP client is hand-rolled, why
the machine emits the effect without knowing whether mail exists, and why the
notice goes after the close and never before it.

```sh
node --test test/mail.test.ts    # the SMTP client against a fake relay on loopback
node --test test/notify.test.ts  # the wording, the headers, the failure streak
```

## The index lock: waiting it out, and never stranding one

`git` guards the index with `.git/index.lock`, and this loop touches that index
from two directions: the writer staging a bead's commit, and the reader
taking a `git status` snapshot — which also takes the lock, because refreshing
the stat cache means writing the index. Anything else looking at the same repo
(IDE git integration, a second run, the agent session's own `git`) joins the
same queue. Contention is normal; the interesting part is what happens next.

**A lock that clears is not a failure.** The writer retries — jittered backoff,
250ms to 4s, up to 30s of total wait — and the commit lands. Only git's
index-lock error gets that treatment; a rejected hook or an unsafe path is
reported once, because retrying those just says "no" more slowly.

**A lock that will not clear says so, with its age and the fix:**

```
the git index has been locked for 30.2s (20 retries; the lock is 91.8s old).
Nothing was staged and nothing was committed. A lock that old usually means a
git process was killed partway through: if no git is running, remove
/path/.git/index.lock and run again — or set LOOP_GIT_STALE_LOCK=remove to
let the loop clear locks older than 60s by itself.
```

**The loop will not delete a lock that looks alive, under any setting.** Removal
is opt-in (`LOOP_GIT_STALE_LOCK=remove`) and gated on the lock's *age* — a lock
held by a live process is load-bearing, and pulling it out mid-write is how a
repository gets a corrupt index, which is a far worse injury than the one being
cured and stays quiet for much longer. When the removal path runs it logs the
act, so a wrong guess is at least visible.

**Nothing this loop does can strand a lock.** That used to be false, and it is
the reason `src/gitlock.ts` exists. git cleans up its own lock on every exit it
gets to run, and `SIGKILL` gives it none: the previous behaviour — `execFile`
with `killSignal: "SIGKILL"` on timeout — meant one slow `git add` (big file,
slow disk, hung hook) left `.git/index.lock` behind **permanently**, so every
commit after it failed with the same message until somebody deleted the file.
Measured with git 2.47:

```
  git add -- huge.bin   (200 MB, ~1.2s, lock held throughout)
  kill -TERM mid-add  ->  lock removed by git
  kill -KILL mid-add  ->  LOCK STRANDED; every later write fails
```

The spawner now sends `SIGTERM` at the deadline and `SIGKILL` only if the child
is still there 5 seconds later — the window git needs to write the index and
clean up. Both the reader and the writer use it; `src/gitlock.ts` is the only
module in `src/` that spawns git at all, which the spawn-allowlist test enforces.

| Variable | What it does | Default |
| --- | --- | --- |
| `LOOP_GIT_LOCK_WAIT_MS` | How long a write waits out somebody else's lock | `30000` |
| `LOOP_GIT_KILL_GRACE_MS` | Grace after `SIGTERM` before the child is killed | `5000` |
| `LOOP_GIT_STALE_LOCK_AFTER_MS` | When a lock starts to look abandoned | `60000` |
| `LOOP_GIT_STALE_LOCK` | `remove` lets the loop clear a stale lock once; anything else reports instead | off (`report`) |

Read [`docs/ADR-006-index-lock.md`](docs/ADR-006-index-lock.md) before changing
any of it — in particular why the retry covers *only* the lock error, why an
unknown lock age is never treated as stale, and why the control test that
asserts `SIGKILL` strands the lock is as important as the one that says it
doesn't.

```sh
node --test test/gitlock.test.ts   # the policy, against real git and real signals
```

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
