# ADR-001 spikes — throwaway prototypes

Not part of the shipped app. These exist to turn the three questions in
[ADR-001](../docs/ADR-001-transport-and-rendering.md) into measured facts instead
of opinions. They are kept (and their output committed) as the evidence trail for
that decision, and spike 1 doubles as a regression harness when pi is upgraded.

Self-contained: its own `package.json` so the spike cannot contaminate the app's
dependency set.

## Running

```bash
cd pi-beads/loop/spikes
npm install

npx tsx 1-render-highlight.ts     # offline, deterministic, no model needed
FORCE_COLOR=3 npx tsx 2-fresh-session.ts   # live model, 3 iterations
npx tsx 3-rpc-comparison.ts      # subprocess RPC path, for the rejected alternative
```

Each exits non-zero if its assertions fail. `FORCE_COLOR=3` is needed for spike 2
only if you want the captured ANSI to survive being piped to a file.

Requires the model endpoint configured in `~/.pi/agent/models.json`. Override with
`SPIKE_PROVIDER` / `SPIKE_MODEL` (falls back to `PI_PROVIDER` / `PI_MODEL`).

## What each spike proves

| Spike | Question | Result |
|---|---|---|
| `1-render-highlight.ts` | Can pi's theme + `Markdown` + `AssistantMessageComponent` produce pi.dev syntax highlighting without running `InteractiveMode`? | 11/11 assertions. Keyword / string / comment get three different colours; code inside markdown keeps its colour; components render and re-render outside interactive mode. Also: `src/format.ts` has 0 exports, so the hand-rolled renderer is unreachable today. |
| `2-fresh-session.ts` | Does a new `AgentSession` per iteration really give a clean context slate? | 9/9 assertions. Session create 1–3 ms; `messages` empty at start of each of 3 iterations in one process; input tokens stay flat (105 / 108 / 125 context tokens — not the 213 a retained transcript would carry); iteration 3 answers `UNKNOWN` when asked what the earlier session was told. |
| `3-rpc-comparison.ts` | What does the RPC subprocess path actually cost — and does the scaffold's version even work? | Correct invocation: 36 JSONL events / 9 event types per trivial reply, plus framing + child-process handling. The scaffold's `[" --mode", "rpc"]` argv: 0 bytes of stdout, never entered RPC mode, killed at timeout. |
| `4-idle-app.ts` / `4-idle-pty.ts` | Does pi's own TUI give us input, a status line and live theming without `InteractiveMode`? | Yes. Real `TuiMainScreen` + `CustomEditor` under a pty; theme colours applied; `/exit`, double Ctrl+C and Ctrl+D-on-empty all leave the terminal clean (`out/4-idle-pty.txt`). |
| `5-split-live.ts` | Does the splitter work against a real model writing into a real board? | Yes: verbatim request on the epic, two children with acceptance criteria, and the model's `#0` position bound to a real `live-*.1` id (`out/5-split-live.txt`). |
| `6-loop-live.ts` | Does the composed program walk `idle → SPLIT → work → FINALIZE → idle` with a real model, real `git` and a real `bd`? | Yes (`out/6-loop-live.txt`): two children worked and closed, one commit each carrying the issue id and a `Loop-Handoff:` trailer, both handoff memories recallable, run ends `done`, and no later prompt contains any earlier session's answer. |

Captured output lives in [`out/`](out) — raw ANSI (`.raw.txt`), stripped
(`.plain.txt`), JSON summaries and the run transcripts.

## Cleanup

Safe to delete once the decision is implemented and no longer contested:

```bash
rm -rf pi-beads/loop/spikes
```

Nothing in the app imports from here.
