# `looprs` docs

Two kinds of page here, and they answer different questions:

* **a decision record (ADR)** — *why it is this way*, what was measured, and what is
  forbidden. Read one before changing the thing it covers.
* **a page** — how to use or test something, now.

There is deliberately **one** list of the `LOOPRS_*` knobs per feature, in the page
that owns that feature. A second list of the same variables is a future argument with
the first about which one is true.

## Pages

| page | about |
| --- | --- |
| [kanban.md](kanban.md) | the beads board in the frame: what the three columns show, how tall it gets and what `+N more` means, the `LOOPRS_KANBAN` / `LOOPRS_KANBAN_ROWS` / `LOOPRS_KANBAN_POLL_MS` knobs and their defaults, what `stale` means, and how to turn it off |
| [testing.md](testing.md) | the gate (`./scripts/check.sh`), the fakes, and the scenario index — how a stateful multi-process TUI gets tested with no network, no model and no real `bd` database |
| [../spikes/README.md](../spikes/README.md) | the measurement harness: what each spike measures and what it proved |

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
