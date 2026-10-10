# `looprs` docs

Three kinds of page here, and they answer different questions:

* **a decision record (ADR)** — *why it is this way*, what was measured, and what is
  forbidden. Read one before changing the thing it covers.
* **a page** — how to use or test something, now.
* **the site** — the same files, built into a book with a sidebar and a search-free
  30-minute reading order: [`docs/index.md`](index.md) is the front door,
  [`docs/SUMMARY.md`](SUMMARY.md) is the tree.

```sh
./scripts/docs.sh build   # render into docs/_site (about a second, no new toolchain)
./scripts/docs.sh serve   # http://localhost:3000 with live reload
./scripts/docs.sh check   # the rot gate; also a step of ./scripts/check.sh
```

Why that toolchain and not `mdbook`, and why the generated HTML is not committed, is
[ADR-0008](adr/0008-docs-site.md).

There is deliberately **one** list of the `LOOPRS_*` knobs per feature, in the page
that owns that feature. A second list of the same variables is a future argument with
the first about which one is true. The full index is
[the configuration reference](guide/configuration.md); the rule that keeps the two
from disagreeing is checked, not hoped for — `./scripts/docs_check.sh` fails a page
that states a default the reference contradicts.

## Pages

| page | about |
| --- | --- |
| [index.md](index.md) | the site's front page: what looprs is, the frame drawn with its five bands, the three terminal states as a table, the 60-second quickstart, who it is not for |
| [guide/differences.md](guide/differences.md) | how looprs differs from a plain agent CLI, an orchestrator, tmux, an IDE agent, `aider`-style wrappers, and its own TypeScript predecessor in `../loop` — nine axes, and a "pick this instead if…" line for each |
| [guide/first-session.md](guide/first-session.md) | the first ten minutes: what the status row promises, how to quit from every state including the bad ones, the four things to try first |
| [guide/beads-loop.md](guide/beads-loop.md) | driving the loop that costs money: what a pass is, what the band shows during one, what `Tab` and `Esc` do mid-pass, finished vs wedged |
| [guide/transcript.md](guide/transcript.md) | the pin-to-tail rule, the "N new" pill, the bounded store and the file behind it, selection vs chords, what does *not* re-wrap on resize |
| [guide/keymap.md](guide/keymap.md) | every keystroke per mode, generated from `CHORD_TABLE`; the copy family, `Esc` semantics, the mouse, and a one-screen cheat sheet |
| [guide/wire-protocol.md](guide/wire-protocol.md) | what crosses the session/UI boundary, generated from `WIRE_INVENTORY` in `src/wire.rs`: every role, event and reason, who reads it, and what the unread records are waiting for |
| [guide/configuration.md](guide/configuration.md) | all 23 `LOOPRS_*` variables the code reads, with defaults, values, `read in`, groups, harness-only variables, and combinations that were actually run |
| [guide/operator.md](guide/operator.md) | where every file is written, recovery recipes, the diagnostic grep index, what to attach to a bug report, and the privacy note |
| [guide/contributing.md](guide/contributing.md) | the module map (every file in `src/` accounted for), the end-to-end data-flow diagram, how-to recipes, the testing ladder, house style |
| [guide/improvement-sweep.md](guide/improvement-sweep.md) | what to do with the things you notice while writing docs: the four required elements of a finding, "file it before you fix it", no drive-by refactors on a docs branch, and `scripts/sweep_check.py` |
| [kanban.md](kanban.md) | the beads board in the frame: what the three columns show, how tall it gets and what `+N more` means, the `LOOPRS_KANBAN` / `LOOPRS_KANBAN_ROWS` / `LOOPRS_KANBAN_POLL_MS` knobs and their defaults, what `stale` means, and how to turn it off |
| [testing.md](testing.md) | the gate (`./scripts/check.sh`), the fakes, and the scenario index — how a stateful multi-process TUI gets tested with no network, no model and no real `bd` database |
| [../spikes/README.md](../spikes/README.md) | the measurement harness: what each spike measures and what it proved |

**Every page above is in the site tree, and the build fails if one is not.**
`SUMMARY.md` is the tree; an unindexed page is invisible, which the rot gate counts
as the same failure as a broken link, arriving later.

## Decision records

| ADR | decides |
| --- | --- |
| [0001 — How the Bash terminal state gets a shell](adr/0001-bash-terminal-state-pty.md) | `bash -i` on a **real pty** rather than pipes, and everything that follows from owning a terminal: resize forwarding, the full-screen child handover |
| [0002 — One subprocess + one event stream per terminal state](adr/0002-session-abstraction.md) | what a "session" is across the three modes, and what the view is allowed to assume about it |
| [0003 — Cancellation](adr/0003-cancellation.md) | what `Esc` means in each terminal state: who cancels what, and how it is announced |
| [0004 — The full screen we take, the clipboard we write](adr/0004-fullscreen-tui.md) | owning the whole window instead of an inline pane: the bands, the ladder that pays for them, what a band costs a contributor (**amended** — the frame now has five), and what a selection copies |
| [0005 — What shell output may contain](adr/0005-shell-output-content-model.md) | the content model for output looprs wraps itself: what may be re-written, what must not be, and where the bytes go |
| [0006 — The terminal mode ledger](adr/0006-terminal-mode-ledger.md) | every terminal mode the app switches on is switched off **exactly once**, including by a panic, a signal, or a child that never gave the screen back |
| [0007 — The kanban board](adr/0007-kanban-board.md) | what the board shows: the `bd` status → column mapping (one table, linked from [kanban.md](kanban.md) rather than copied into it), the freshness/stale states, one `bd` read per tick and what it costs, and the ten things the band must not do |
| [0008 — The documentation site](adr/0008-docs-site.md) | why a hand-rolled generator over `pulldown-cmark` rather than `mdbook`; why nothing moved; why the generated HTML is built and not committed; the diagram and ADR-template policy |
