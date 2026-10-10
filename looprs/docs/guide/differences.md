# How it differs

"How is this different from X?" is the first question asked about looprs and the
existing docs answered none of it. This page answers it on **axes** rather than by
adjectives: nine questions the design actually had to answer, and what looprs chose
compared with what the neighbouring categories chose. Every cell cites either a file
in `src/` or a decision record. Every section ends with the line that matters more
than the comparison: **who should pick the other thing.**

External claims are about design shape, not version numbers, and were read from each
project's own documentation on **2026-10-09**. Where a claim about a third-party
tool is not checkable from that documentation, it is not made here.

## The axes

| axis | the question the axis answers | what looprs does | where that is decided |
| --- | --- | --- | --- |
| **Transcript buffer** | who holds the text you are reading | looprs does: a bounded, in-memory, re-wrap-able store per mode, with a journaled file behind it | [`state::scrollback`](../../src/state/scrollback.rs), [ADR-0004 R2](../adr/0004-fullscreen-tui.md) |
| **System of record** | what is true when the screen goes away | `bd` for work, `git` for code, the journal for prose — and explicitly *not* the transcript | [`services::journal`](../../src/services/journal.rs), [ADR-0004](../adr/0004-fullscreen-tui.md) |
| **The shell** | a real interactive shell, or `bash -c` per call | `bash -i` on a real pty, with cwd, env and jobs surviving everything but a kill | [ADR-0001](../adr/0001-bash-terminal-state-pty.md), [`session/bash.rs`](../../src/session/bash.rs) |
| **Session lifecycle** | fresh-per-task, one long conversation, or several coexisting | three coexisting states, one keystroke apart, each with its own subprocess and its own switch-away policy | [`TerminalType::switch_away_policy`](../../src/session/mod.rs), [ADR-0002 Q3](../adr/0002-session-abstraction.md) |
| **Clipboard** | who owns the selection, and how bytes reach your paste | looprs owns the selection; native helper locally, OSC 52 over SSH, chosen per run | [`services::clipboard`](../../src/services/clipboard.rs), [ADR-0004 R7/R9](../adr/0004-fullscreen-tui.md) |
| **Exit safety** | what the tty looks like after a panic or SIGTERM | a ledger: every mode switched on is switched off exactly once, from every path including the panic hook | [`teardown.rs`](../../src/teardown.rs), [ADR-0006](../adr/0006-terminal-mode-ledger.md) |
| **Mouse** | captured and ours, or left to the emulator | captured, with drag-selection and copy-on-select, and a documented single-switch-off hatch | [`state::selection`](../../src/state/selection.rs), `LOOPRS_MODES=-mouse` |
| **Evidence** | how a design claim gets believed | measured spikes, committed in-repo, with control runs against the pre-change binary | [`spikes/README.md`](../../spikes/README.md), [`docs/testing.md`](../testing.md) |
| **Runtime** | what you have to install | one Rust binary. No interpreter, no node_modules, no side services | [`Cargo.toml`](../../Cargo.toml) |

The two axes that do most of the work are the first and the last. Owning the
transcript is what makes a dozen other features possible, and "one binary" is what
made doing it in the terminal worthwhile.

## A plain agent CLI (`pi`, Claude Code, Codex CLI)

One agent, one conversation, the terminal emulator's scrollback as the transcript,
no board.

This is the honest baseline, and for a large class of work it is the better tool:
fewer layers, nothing between you and the agent's output, and the terminal's
scrollback is as good a transcript as a single-session job needs. What you give up
is the three things looprs exists to provide:

1. **A view of work that is not the transcript.** In a plain CLI, "what is left on
   the board?" costs a `bd list` in a second terminal. Here it is a band that is
   already on screen and refreshed on a schedule
   ([ADR-0007](../adr/0007-kanban-board.md)).
2. **A transcript that is a store, not a print stream.** The emulator's scrollback
   cannot be re-wrapped on resize, cannot be bounded deliberately with a marker
   naming the file that kept the rest, and cannot answer "copy the last answer" as
   one keystroke. [`state::scrollback`](../../src/state/scrollback.rs) exists to be
   that store.
3. **A shell that coexists rather than alternates.** In a plain CLI the shell is a
   tool call. Here it is a mode with a live `bash -i` that has a cwd, an environment
   and background jobs
   ([ADR-0001](../adr/0001-bash-terminal-state-pty.md)).

**Pick the plain CLI if** you are doing one thing at a time in one session and you
like the agent's own UI. looprs adds a frame around the same underlying agents; if
you do not need the frame, the frame is pure cost.

## Agent orchestrators and swarm runners

Multi-agent panels, concurrency dashboards, "N agents working in parallel" harnesses.

These answer a different question. looprs shows **one human** three views of **one
work queue**; a swarm runner shows many agents and tries to answer "what is
everybody doing". The design consequences are the opposite way round: an
orchestrator spends its budget on isolation, scheduling, log fan-out and cost
attribution. looprs spends its budget on the transcript and the frame, and
deliberately *refuses* the multi-agent axis — there is exactly one Beads loop, one
chat session and one shell per run
([`Router`](../../src/session/router.rs) owns one session per terminal state, and
`MAX_VIEWS = 3` is a constant, not a config value).

**Pick an orchestrator if** you actually want parallel agents. looprs will not do
it, and running two copies of looprs to get it is worse than the alternative.

## A terminal multiplexer (tmux / zellij)

"Why not just tmux" deserves a direct answer, because tmux is already installed
everywhere and does roughly "several things in one terminal".

tmux owns the scrollback. That is not a criticism, it is the property that makes
the comparison end there: if the multiplexer owns the scrollback, the application
inside it cannot re-wrap the transcript on resize, cannot bound it and name a file
that kept the trimmed part, cannot know what a drag selected (the emulator and tmux
fight over the mouse), cannot implement "copy the last answer" as a chord, and
cannot draw a kanban band that participates in the same scroll region as the
transcript. All four of those are the product.

What tmux does that looprs does not want: panes, windows, named sessions, detach and
reattach, a window that survives the SSH connection that made it. looprs is one
window, one process tree, and it dies with the terminal. They compose rather than
compete — the loop this app is built for runs `looprs` *inside* tmux to keep the
session alive over SSH, with `LOOPRS_MODES=-mouse` because the mouse belongs to
tmux in that arrangement.

**Pick tmux alone if** you want pane and session management and do not care about
owned-transcript features. **Pick both if** you want the transcript features and a
session that survives a dropped SSH connection. **Do not** expect looprs to
replace tmux: [the operator page](operator.md#what-looprs-does-not-do) says
what it will not do for you.

## An IDE-ish coding agent (Zed, VS Code + extensions, Cursor)

The transcript is an editor buffer; the shell is a panel; the file tree is the main
view.

That is a coherent product with a big advantage looprs will never have: the code is
on screen next to the conversation, and multi-line review of a diff is a mouse drag
rather than a scroll. looprs's counter-position is narrow and deliberate — a
terminal is the only place where a full-screen child program, an interactive shell
and an agent share the same keystroke budget and the same transcript, and looprs is
built for exactly that. A model reading a terminal's output and a model reading an
editor buffer are different things, and ADR-0005 exists because re-writing the
first one has rules the second one does not need.

**Pick the IDE** if most of your review happens on diffs in a file tree. **Pick
looprs** if most of your session happens in a terminal — including the parts where
you are watching a build, a REPL, or a full-screen program.

## Agent TUI wrappers (`aider`, OpenHands, and friends)

The closest neighbours on the "agent + shell" axis, so this one needs the most
precision. Broadly, these wrappers own the *repo relationship*: they read files,
apply edits, auto-commit, and drive the shell for specific commands. aider's
documented shape (its docs, 2026-10-09) is a chat REPL that edits your files
through the LLM and commits them itself, with shell commands available as a
slash-command.

looprs refuses that whole axis on purpose:

* **No repo editing model.** looprs does not read your files to feed them to a
  model, does not apply patches, and has no commit policy. The agents do that.
  `bd` is the record of work; `git` is the record of code
  ([`services::bd`](../../src/services/bd.rs) reads the board and only the board).
* **What it refuses to give up instead:** a real pty rather than `bash -c` per call
  ([ADR-0001](../adr/0001-bash-terminal-state-pty.md)); an owned transcript rather
  than a print stream ([ADR-0004](../adr/0004-fullscreen-tui.md)); and an exact-once
  terminal hand-back rather than hoping the child cleans up after itself
  ([ADR-0006](../adr/0006-terminal-mode-ledger.md)). Each of those three is the kind
  of thing that is invisible when it works and unfixable in a wrapper when it does
  not — which is why they are ADRs, and why the exit path has a spike of its own
  ([`spikes/shutdown_e2e.py`](../../spikes/shutdown_e2e.py)) whose totals live in
  the committed captures under [`spikes/results/`](../../spikes/results/) rather
  than in this sentence.

**Pick aider/OpenHands if** you want the tool to own the edit-and-commit loop.
**Pick looprs if** you want the terminal to be the product and the agents to stay
interchangeable — a thing you can swap the backend of, rather than a workflow you
have to move into.

---

## The one-paragraph summary

looprs is not a better agent, a better editor, or a better multiplexer. It is an
owner of the terminal: it takes the whole window, keeps the transcript in a store
instead of a scroll, puts a real shell next to the agent, and hands the terminal
back exactly once when it is done. If that is the thing you want, the ADRs explain
how it is paid for. If it is not, one of the sections above says which of these
tools you should be using instead, and that line was written for you.
