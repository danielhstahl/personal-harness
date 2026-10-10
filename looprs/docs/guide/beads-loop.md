# Driving the Beads loop

*Page 2 of 3 in the **"use it effectively"** track. Previous:
[Your first session](first-session.md) · Next:
[Living with a transcript](transcript.md).*

This is the page about the mode that costs money. It covers what a pass is, what the
board is showing you while one runs, what `Tab` and `Esc` do mid-pass, how to tell
finished from wedged, and the one thing you cannot do by accident.

The kanban band itself — the column mapping, what `⊘` and `?` mean, staleness, the
row budget, `+N more` — is documented once, in
**[docs/kanban.md](../kanban.md)**. This page links it instead of restating it,
because the second copy of that table is the one that will be wrong.

---

## What a pass is

One pass = one claim + one worker. Concretely:

```text
awaiting input            the loop is waiting for you to type something
   │  you type a request, press Enter
   ▼
planning (CreateTickets)  the planner child turns the request into beads
   │  a planner pass is not believed until the board says so
   ▼
working (WorkTickets)     claim the ticket → spawn the worker → the worker does the work
   │  the worker closes the bead through bd
   ▼
awaiting input            again, with the board changed
```

Three properties of that machine are worth knowing before you drive it, because each
one is a feature rather than an implementation detail:

**A planner pass is not believed until the board agrees.** `agent_settled` means
"the planner stopped talking", which is not "there is a plan". The loop snapshots
the open board *before* spawning the planner and diffs it afterwards
([`BeadsLoop::verify_plan`](../../src/session/beads/machine.rs)). What falls out: zero new
tickets is a loud error that quotes the planner's own last words and parks instead
of advancing; a real plan is listed as `id: title` in the transcript **before** a
worker is paid to read it; and "the board could not be read" is its own verdict and
never a synonym for "empty".

**The worker drives the loop, not the UI.** `agent_settled` off the loop's own
child routes to the task that owns the loop, tagged with the serial of the pass that
made it. The App renders the records and decides nothing. It used to be the other
way round, and the transition was keyed off whatever the input box was set to when
the event arrived — a Pi answer settling while the box was on Beads drove the
beads machine. Read
[ADR-0002](../adr/0002-session-abstraction.md) before touching that path.

**The claim is the harness's knowledge, not a copy of the bead.** `ActiveBead` is
set when `bd update … --claim` succeeded and cleared when the pass ends — exactly
the window in which the loop is accountable for that ticket. The row shows its id
and title because a number you cannot read as a sentence is not a status.

## Reading the band while a pass runs

You are looking at two independent reads of the same board: the **band** (the
poller's, on a schedule, `--readonly`) and the **row** (the loop's own, from its
own pass state). They are not the same data path and they are allowed to disagree by
up to one poll interval. What each is for:

| you want to know | look at | freshness |
| --- | --- | --- |
| "is the loop doing something, and what" | the status row: `working · looprs-00u · 12s` | live |
| "what is on the board now" | the kanban band | last good read, `bd ok · 3s ago` — and explicitly `stale` if the last read failed |
| "which of these can I take" | the band's To-do column, with `⊘`/`?` marks | same |
| "what did that pass actually say" | the transcript, or the journal file after the fact | — |

**The band cannot move the board.** Both of its reads run under `bd --readonly`, so
"looking at the board is safe" is a property of the command lines rather than of a
widget that happens not to have a `&mut`
([ADR-0007 rule 10](../adr/0007-kanban-board.md)). The loop's writes are the only
thing that changes beads, and they are all in `services::bd`.

## `Tab` during a pass: drain, then park

`Tab` away from Beads while a pass is running and:

1. the pass **already running** is allowed to finish. Killing it throws away
   paid-for work;
2. **no new pass starts** while the mode is off-screen. This loop self-advances, so
   "keeps running while nobody looks" would otherwise mean "keeps spending money on
   the board";
3. the transcript keeps accumulating in that mode's own store, so coming back shows
   the backlog rather than a hole;
4. the board poller keeps polling regardless — it is a separate task that outlives
   every session, so returning to Beads shows a board that was never out of date.

That is `SwitchAway::DrainThenPark`, and it is the *only* one of the three modes
that parks. Bash and Pi are `KeepRunning`: a shell you killed on `Tab` would lose
cwd, env and jobs, and a chat answer that got killed mid-stream is an answer you
have to pay for twice
([`switch_away_policy`](../../src/session/mod.rs)).

The practical consequence, and the one that catches people: **`Tab` is not Cancel.**
`Tab`ing away from a pass does not stop it. It finishes, and the next one does not
start. If you wanted it stopped, that is `Esc`.

## `Esc` here, `Esc` there

`Esc` is the cancel key in every mode and it cancels a different thing in each. The
whole table with its reasoning is
[ADR-0003](../adr/0003-cancellation.md); the difference you need at the moment of
pressing it:

| mode | `Esc` does |
| --- | --- |
| **Beads** | cancels the pass: `abort` to the worker, then kill it if it ignores the abort; the ticket stays claimed until the unwind finishes; nothing on the board is rolled back |
| **Pi** | clears the queued messages **first**, then `abort`s the run — a message in the queue is not "already sent" |
| **Bash** | `0x03` to the pty master: SIGINT to the shell's foreground group; the app is not involved |
| **any mode, copy chord armed** | cancels **the chord** and nothing else. `Esc` never cancels a run while `Ctrl-S` is armed — that is the reason the chord has a cancel of its own |

## Telling a finished pass from a wedged one

The row's verb is the answer, and the verbs are a small closed set
([`components::status::verb`](../../src/components/status.rs)):

| row | meaning | act |
| --- | --- | --- |
| `✓ …` / back to `awaiting input` | the pass ended and the loop is waiting for you | nothing |
| `working · <bead> · 4m` | a worker is on it | nothing; long is normal |
| `planning · …` | the planner child is turning your request into beads | nothing; this is usually fast, so a long `planning` is suspicious |
| `cancelling` | an `Esc` is unwinding | wait for the grace window, then the child gets killed |
| `paused · ✗ …` | the loop stopped because `bd` said no — the error is on the row | read the error; the loop will not spin on a board it cannot trust |
| `working · <bead>` for a very long time with a live spinner | a real run with a long tail | read the transcript / live card before assuming the worst |

The single most useful discriminator is the **glyph vs the verb**. A spinner means a
process is doing something; `●` means warm and idle; `○` means nothing there. A
verb that says "working" next to `○` is a contradiction the row tests look for. If
the row says a process is running and the transcript has not moved in a minute,
read the live card band — the [`compaction` card](../../src/components/compaction.rs)
exists precisely because "pi paused to summarise its own context" is 10–60 seconds
of dead transcript that otherwise reads as a hang.

**A ticket that is never closed stops the loop instead of spinning.** That is a
designed outcome, not a failure: `bd show <id>` answers "did the worker close it",
and a pass that settled without closing is reported rather than retried forever. If
your loop stopped with a ticket still open, that is the loop refusing to pay for the
same ticket twice — go look at the ticket.

## What gets announced, and when

If `LOOPRS_NTFY_URL` + `LOOPRS_NTFY_TOPIC` are set
([configuration reference](configuration.md#notifications)), a finished ticket is
posted to ntfy so a human who is not watching this terminal hears about it. The
precision that matters: **exactly one `notify` call exists in the binary**, on the
`PassOutcome::Closed` arm. Not on `agent_settled` — a pass that closed nothing, an
aborted pass and a *planner's* pass are the same bytes there. So:

| ending | announced |
| --- | --- |
| the board says the ticket closed | yes, with the title it was claimed under |
| settled, ticket still open | no |
| `Esc` cancelled it | no |
| the planner settled | no (it holds no claim) |
| the worker left it `blocked` | no (that is a hand-off to a human, not a completion) |
| `bd` unreadable after the pass | no — unverifiable is not a verdict |

## Not moving the board by accident

The failure mode to design against is "I was only looking at it and I changed it".
The guarantees here:

* the band's reads are `bd --readonly` — both the board read and the change probe;
* the widget is a pure function of *snapshot + area*: no clock, no environment, no
  `bd` call in the paint path, so it has no way to reach anything;
* the only writes in the whole board path are the loop's own claim/close, and they
  are in `services::bd` where you can grep for them;
* the poller runs one read at a time and leaves no orphan `bd` behind (asserted in
  the poller's tests with a fake that logs its own pids).

What *will* change the board: typing into Beads mode's input box (that is a
planner request), and running a real `bd` yourself in the Bash mode. The band will
show both within a poll interval and, if you changed it with `bd dolt pull` or a
merge, within the sweep interval rather than the poll interval
([kanban.md → the sweep](../kanban.md#how-the-band-is-refreshed)).

## Where the fakes come in

Every claim above is tested against `tests/fixtures/fake_bd.sh` and
`tests/fixtures/fake_pi_chat.py` — real subprocesses with real pipes, no network
and no model. If you want to *see* a pass without paying for one, the drivers in
`spikes/` are the pattern:

```sh
cargo build
python3 spikes/status_e2e.py            # the row across 6 scenarios
python3 spikes/cancel_e2e.py            # Esc, per mode
python3 spikes/board_poll_cost.py            # what one read of the board costs
```

The full map of what is tested where is
[docs/testing.md](../testing.md) — and the beads-specific sections there
("Claim / close guard", "Settle routing", "Esc cancellation") name the exact test
for each claim on this page.

**Something not behaving as this page said?**
[Files, logs and recovery → "Debugging a bad run"](operator.md#debugging-a-bad-run).
