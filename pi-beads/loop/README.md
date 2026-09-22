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
`LOOP_WIDTH`.

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
