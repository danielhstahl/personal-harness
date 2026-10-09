# looprs

A terminal that owns its transcript.

One Rust binary with **three modes** in one window, switched with `Tab`:

| | |
| --- | --- |
| **Beads** | a loop that reads a [`bd`](https://github.com/steveyegge/beads) board, plans, claims a ticket, and pays a worker to do it |
| **Pi** | a chat session with [`pi`](https://pi.dev), one child process, many turns |
| **Bash** | a real interactive `bash -i` on a real pty — cwd, env and jobs intact |

Same transcript, same scrollback, same selection, same clipboard in all three. The
app takes the whole window: five bands, a bounded in-memory transcript that
re-wraps on resize, a journaled file behind it, and a terminal that comes back
exactly once — from a panic, a signal, or a full-screen child that never handed the
screen back.

```
● working · looprs-00u · ↑12.3k ↓4.1k
```

That row is one of the five bands, and it answers "is it doing something, what,
for how long, did anything fail" for the mode you are looking at **and the two you
are not**.

## 60 seconds

```sh
cargo build
./target/debug/looprs

#   Tab            → Pi → Bash → Beads  (a fixed 3-cycle)
#   PageUp / End   → scroll the transcript; End gets back to the tail
#   Ctrl-S ?       → list the copy chords in a toast
#   Ctrl-S a       → copy the last answer   (the one keystroke worth learning)
#   Ctrl-Q         → quit, from any mode, any state
```

`Ctrl-Q` is the exit in every mode. In Bash `Ctrl-C` goes to the shell instead —
that is the shell's key, not ours.

## The docs site

```sh
./scripts/docs.sh build    # renders docs/ into docs/_site (~0.7s, no new toolchain)
./scripts/docs.sh serve    # http://localhost:3000 with live reload
./scripts/docs.sh check    # the docs rot gate
```

Start at **[docs/index.md](docs/index.md)** — what it is, the frame drawn, the
three terminal states, who it is *not* for. A 30-minute reading order is at the
bottom of that page.

## Where to go

| I want to… | go |
| --- | --- |
| **know what it is** | [docs/index.md](docs/index.md) |
| **know how it differs** from a plain agent CLI, tmux, an IDE agent, `aider`, or its own TypeScript predecessor in [`../loop`](../loop) | [docs/guide/differences.md](docs/guide/differences.md) |
| **use it well** — first session, driving the beads loop, living with a transcript | [first session](docs/guide/first-session.md) · [the loop](docs/guide/beads-loop.md) · [the transcript](docs/guide/transcript.md) |
| **the keys** | [docs/guide/keymap.md](docs/guide/keymap.md) — generated from `CHORD_TABLE`, per mode, with a printable cheat sheet |
| **the knobs** — all 23 `LOOPRS_*` variables, defaults, combination examples | [docs/guide/configuration.md](docs/guide/configuration.md) |
| **recover something** — where the transcript went, what to attach to a bug report | [docs/guide/operator.md](docs/guide/operator.md) |
| **change the code** — the module map, the data-flow diagram, recipes | [docs/guide/contributing.md](docs/guide/contributing.md) |
| **the kanban band** specifically | [docs/kanban.md](docs/kanban.md) |
| **how any of it is tested** — no network, no model, no real `bd` database | [docs/testing.md](docs/testing.md) |
| **the measurements behind the claims** | [spikes/README.md](spikes/README.md), with committed output in [spikes/results/](spikes/results) |
| **the argument** — why each behaviour is this way and what it forbids | the [ADR index](docs/README.md#decision-records): [0001](docs/adr/0001-bash-terminal-state-pty.md) pty · [0002](docs/adr/0002-session-abstraction.md) sessions · [0003](docs/adr/0003-cancellation.md) Esc · [0004](docs/adr/0004-fullscreen-tui.md) the frame · [0005](docs/adr/0005-shell-output-content-model.md) shell output · [0006](docs/adr/0006-terminal-mode-ledger.md) the terminal ledger · [0007](docs/adr/0007-kanban-board.md) the board · [0008](docs/adr/0008-docs-site.md) this site |

## The gate

```sh
./scripts/check.sh
```

`cargo fmt` · `cargo clippy -D warnings` · `cargo test` · the dead-code allow audit
· the docs rot gate. The same file CI runs, so "passes locally" and "passes in CI"
are one statement. ~20 s, no network, no model calls.

Docs-only edits run the same gate: the rot step needs no cargo and no network, and a
dead link, an undocumented knob or a keymap table that drifted from `CHORD_TABLE`
fails the build the same way a clippy warning does.

## Not this

Not a chat UI for one model (`pi` itself is better at that). Not an editor, not a
git workflow, not a multiplexer, not a daemon, and not a parallel-agent runner.
[Here is who should pick each of those instead](docs/guide/differences.md).
