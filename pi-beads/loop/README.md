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
| `npm run dev` | `tsx src/main.ts` — run the entry point without building |
| `npm run typecheck` | `tsc -p tsconfig.json --noEmit` (strict; must stay clean) |
| `npm run build` | `tsc -p tsconfig.build.json` → `dist/` |
| `npm start` | `node dist/main.js` |
| `npm run clean` | remove `dist/` |

## Layout

```
src/main.ts     entry point (currently a toolchain smoke run, see header comment)
src/format.ts   one-line plain-log summaries — NOT the renderer (see ADR-001)
docs/           ADR-001: transport + rendering decision
spikes/         throwaway prototypes + captured evidence backing ADR-001
```

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
