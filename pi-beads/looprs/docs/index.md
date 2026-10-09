# What looprs is

`looprs` is one Rust binary that puts three different ways of working in one
terminal window and treats the transcript as its own property. The three are a
**Beads loop** (an agent that picks work off a [`bd`](https://github.com/steveyegge/beads)
board), a **Pi chat session**, and a plain **interactive Bash shell**. `Tab` moves
between them. Each one is its own subprocess with its own life — a command that is
running keeps running when you `Tab` away from it — and each one shares the same
transcript, the same scrollback, the same selection and the same clipboard, so
`Ctrl-S a` copies "the last answer" in all three modes and means the same kind of
thing in all three. The frame that holds them is the whole window, and the app owns
it: what you scrolled to stays where you scrolled to, the selection you dragged is
the selection that gets copied, and the bytes that scroll off the top are in a
bounded store that tells you where the file with all of them is.

That is the whole idea. Everything else in this repository is an argument about how
to pay for it.

* **Start here** if you have 5 minutes: [Your first session](guide/first-session.md).
* **Skip to the keys**: [Keymap and chords](guide/keymap.md).
* **"Why is it like this?"**: the [decision records](README.md#decision-records) —
  every behaviour on screen has one, and this page links them rather than re-arguing them.

---

## The frame, drawn

Five bands, stacked, every frame. The order is
[`viewport::frame_areas`](../src/viewport.rs), and the diagram is checked against
it by the docs gate ([`./scripts/docs.sh check`](../scripts/docs.sh)), so if this
picture is wrong the build fails.

```text
┌───────────────────────────────────────────────────────────────────────────┐
│ 1  TRANSCRIPT                                                             │
│    The scrollback. Everything this mode has said, kept in an in-memory      │
│    bounded store; PageUp/PageDown/Home/End and the wheel move it.           │
│    Min 1 row. Owned by: state::scrollback.                                 │
│                                                                           │
│    … 24 rows of answer, tool output, prompts, errors …                    │
│                                                                           │
│ 2  TOOL / COMPACTION CARDS       ⠹ running `cargo test` · 12s              │
│    0–4 rows: the live card wall — one row per in-flight tool call,         │
│    plus the compaction card. 0 rows when nothing is running.               │
│                                                                           │
│ 3  KANBAN BAND             To-do 12 │ In progress 1 │ Complete 40 ⏸ 2     │
│    Only in Beads mode. In Bash and in Pi this band is **zero rows** —     │
│    not hidden, not blank, zero — so those frames are byte-identical to     │
│    the frames from before the board existed.                               │
│                                                                           │
│ 4  STATUS ROW              ● working · looprs-00u · ↑12.3k ↓4.1k · 8fps   │
│    Exactly 1 row, always painted. Answers "is it doing something, what,   │
│    and did anything fail" for the mode on screen AND the two you are not   │
│    looking at (`bg: Pi idle`).                                            │
│                                                                           │
│ 5  INPUT BOX               ┃ type here; Enter sends, Shift-Enter is a      │
│    0, 3 or 8 rows: border (2) + text (1–6). 0 rows when a full-screen     │
│    child holds the screen.                                                │
└───────────────────────────────────────────────────────────────────────────┘
```

The bands are paid for off one budget — the window's rows — and the ladder that
spends it is documented in
[ADR-0004](adr/0004-fullscreen-tui.md) (the frame) and
[ADR-0007 §4](adr/0007-kanban-board.md) (the board's turn on that ladder). The
rule that matters when you are resizing: **the input box and the status row are
never the thing that gets cut.** The kanban band is the last thing on the ladder,
which is why a short window loses the board before it loses anything else, and why
it loses it completely rather than showing a header and a footer with nothing in
between.

## The three terminal states

| | **Beads** | **Pi** | **Bash** |
| --- | --- | --- | --- |
| What runs | a loop that reads the `bd` board, claims a ticket, spawns a worker per pass | one `pi --mode rpc` child, one conversation, many turns | `bash -i` on a **real pty** ([ADR-0001](adr/0001-bash-terminal-state-pty.md)) |
| `Tab` away means | **drain then park** — the pass in flight finishes, no new one starts ([ADR-0002 Q3](adr/0002-session-abstraction.md)) | **keep running** — the answer in flight lands in that mode's transcript | **keep running** — killing a shell loses cwd, env and jobs |
| `Esc` means | cancel this pass: `abort` the worker, then kill it if it ignores the abort ([ADR-0003](adr/0003-cancellation.md)) | clear the queued messages, then `abort` the run | `0x03` to the pty master — SIGINT to the shell's foreground group; the app stays up |
| "copy the last output" is | the last finished tool card | the last finished tool card | the last **sealed command block**: that command's echo, output and prompt, nothing before it |
| Who owns the screen | this app, always | this app, always | this app **until a full-screen program** (`vim`, `less`) takes it; then the raw keystrokes go through and the hand-back is [ledgered](adr/0006-terminal-mode-ledger.md) |
| Where the record lives | `bd` (the tickets) + the [journal file](guide/operator.md#where-things-are-written) | the journal file | the journal file — and `bd` is not involved at all |

`Tab` walks a fixed 3-cycle — Beads → Pi → Bash → Beads — and it is one keystroke
per hop. `Ctrl-Q` quits from every state. `Ctrl-C` means two different things
depending on the mode and that is deliberate; [the keymap](guide/keymap.md) is
where it is spelled out per mode, from the same
[`CHORD_TABLE`](../src/session/view.rs) the app's own rule tests read.

## The 60-second version

Verified end-to-end on this repo, with the fakes rather than a model, by
[`spikes/status_e2e.py`](../spikes/status_e2e.py) (20 checks, 6 scenarios).

```sh
# 1. build (this is a binary crate: the binary is target/debug/looprs)
cd pi-beads/looprs
cargo build

# 2. run it. `bd` and `pi` must be on PATH; a missing one is reported on the
#    status row rather than at startup.
./target/debug/looprs

# 3. you are in Beads mode. Read the row at the bottom: that is the app telling
#    you what the loop is doing. Tab twice to Bash.
#    Tab        → Pi        (a chat session with pi)
#    Tab        → Bash      (a real interactive shell)
#    ls -l⏎    → it runs in the shell; the output is in the transcript above
#    PageUp     → read the transcript. End gets back to the tail.
#    Ctrl-S ?   → list the copy chords in a toast

# 4. quit
Ctrl-Q
```

If you came here to `Ctrl-C` your way out: in Bash, `Ctrl-C` goes to the shell
(0x03, the foreground group gets SIGINT, the app stays up — that is the shell's
key, not ours). In Pi and Beads it quits the app. `Ctrl-Q` quits in every mode and
is the one that never surprises you.

## What it is not

Stated plainly, because the first question after "what is this" is "is this for
me":

* **It is not a chat UI for a model.** If you want one agent, one conversation,
  and the terminal's own scrollback, `pi` itself is better at that than looprs is:
  fewer layers, no frames between you and the thing. looprs earns its keep when
  there are *three things* to watch at once, and specifically when one of them is a
  work queue.
* **It is not an editor and has no opinion about your repo.** No auto-commit, no
  patch model, no diff review. `bd` and `git` are the record; looprs reads the
  board and gets out of the way.
* **It is not a multiplexer.** No panes, no windows, no sessions to name and
  reattach to. One window, three stacked views of one process tree.
  ["Why not just tmux?"](guide/differences.md#a-terminal-multiplexer-tmux--zellij) has the answer.
* **It is not a daemon.** It lives or dies with your terminal. The transcript
  outlives it — as a file, in `~/.local/share/looprs/transcripts/` — but nothing
  keeps running for you after you close the window.

**[How it differs →](guide/differences.md)** covers all of these properly, with
citations and a "pick this instead if…" line for each.

## Where to look for the argument behind a behaviour

Every rule on the screen has a decision record with the measurements in it. Do not
re-derive them from the code.

| behaviour | the record |
| --- | --- |
| Bash is a real interactive pty, not `bash -c` per command | [ADR-0001](adr/0001-bash-terminal-state-pty.md) |
| One subprocess + one event stream per mode; what a view may assume | [ADR-0002](adr/0002-session-abstraction.md) |
| What `Esc` means per mode, who cancels what, and the grace window | [ADR-0003](adr/0003-cancellation.md) |
| The whole window is taken; the bands; the clipboard path; the transcript is a file | [ADR-0004](adr/0004-fullscreen-tui.md) |
| What may be re-written in shell output and where the bytes go | [ADR-0005](adr/0005-shell-output-content-model.md) |
| Every terminal mode switched on is switched off exactly once, including from a panic | [ADR-0006](adr/0006-terminal-mode-ledger.md) |
| The board: the status → column mapping, staleness, one read per tick | [ADR-0007](adr/0007-kanban-board.md) |
| This site: why a generator, where the pages live, what is committed | [ADR-0008](adr/0008-docs-site.md) |

## The 30-minute route

Read in this order and you will be able to run the app, configure it, and know
where the argument for any behaviour lives:

1. **this page** — what it is, the frame, the three states (5 min)
2. **[How it differs](guide/differences.md)** — the axes, and who should pick
   something else (5 min)
3. **[Your first session](guide/first-session.md)** →
   **[Driving the Beads loop](guide/beads-loop.md)** →
   **[Living with a transcript](guide/transcript.md)** — the practical track, in
   the order it gets learned (10 min)
4. **[Keymap](guide/keymap.md)** and **[Configuration](guide/configuration.md)**
   — kept open in a second window from here on (5 min, skim)
5. **[Files, logs and recovery](guide/operator.md)** the first time something
   misbehaves, and you will thank yourself (5 min, skim)

Then [the contributor guide](guide/contributing.md), if you are about to change
something rather than only look at it.
