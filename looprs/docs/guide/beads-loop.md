# Driving the Beads loop

*Page 2 of 3 in the **"use it effectively"** track. Previous:
[Your first session](first-session.md) · Next:
[Living with a transcript](transcript.md).*

This is the page about the mode that costs money. It covers what a pass is, what
happens to a ticket when a session dies halfway through one, what happens when an
agent finishes a pass without closing its ticket, what the board is showing you while
one runs, what `Tab` and `Esc` do mid-pass, how to tell finished from wedged, and the
one thing you cannot do by accident.

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

## When a session dies mid-ticket

A worker that crashes, gets `kill`ed, runs out of money or is cancelled by `Esc`
leaves its ticket `in_progress` and its claim held. That is the state a restart has
to read correctly, and the trap is that `bd ready` **cannot show it** — `ready`
excludes in-progress work, so the abandoned ticket is not merely unclaimed, it is
invisible, and a loop that reads only `ready` takes the next ticket and never looks
back.

So the first thing a pass does is ask the board what is *in progress*, and finish
that before touching anything new:

```text
pass boundary
   │  bd list --status in_progress   (what `bd ready` cannot show)
   ├─ resumable → claim it → worker told "look before you write"
   ├─ someone else holds it → said out loud, walked past
   ├─ this loop already paid for it → refused, same guard as a re-offered ticket,
   │    unless it is owed an automatic re-buy (see below)
   ▼  nothing in progress
   bd ready → the rest of the machine as above
```

What that means for you, in practice:

* **A restart continues the interrupted ticket.** The transcript says
  `resuming <id>` rather than starting something new, and the worker is told to read
  `bd show <id>`, `git status`/`git diff` and the recent log before writing, so the
  part the last session already shipped is not redone.
* **A live claim is not up for grabs.** `bd` itself decides: a claim it refuses means
  somebody else holds that ticket, and the loop leaves it alone, names it, and takes
  fresh work. It does not run `bd reclaim` over the board — reverting other actors'
  leases is a supervisor's call, not a worker's.
* **`Esc` still means "do not spend on that one again".** An aborted or half-finished
  pass leaves the ticket `in_progress`, and re-entering the mode does not resume it:
  the re-work guard answers with the same refusal it uses for a re-offered ticket,
  plus `bd close <id>` / type-an-instruction as its two ways out. `Esc` is a decision
  about spending, so it earns no automatic re-buy — see the next section.
* **A resumed pass is an ordinary pass.** Claimed, guarded, published to the status
  row and checked against the board afterwards like any other, so a resumed ticket
  that never closes stops the loop once its automatic re-buys are spent instead of
  becoming a second way to spin.

The tests are listed in [docs/testing.md](../testing.md) under "Crash recovery";
the rule lives in
[`BeadsLoop::recover_in_progress`](../../src/session/beads/machine.rs).

## When an agent completes without closing its ticket

The other way a pass ends is the ordinary one and the most confusing: the worker
simply stops talking. `agent_settled` says "the run is over", and it says exactly
the same thing whether the agent shipped the ticket, hit a provider error mid
sentence, or did the whole job and never typed `bd close`. The board is the only
thing that can tell those apart, so the loop asks it — and if the ticket is still
open, it does **not** park and wait for you.

```text
worker settles
   │  bd show <id>  → not closed, and nobody pressed Esc
   ├─ say so: "worked <id> but `bd` says it is `in_progress`, not closed"
   ├─ owe that ticket an automatic re-buy (2 per ticket, per instruction)
   ▼  next part of the loop, immediately
   pass boundary → the same ticket, resumed, with "do not start over" in its prompt
```

Two numbers and one exception are the whole design:

* **Two re-buys per ticket, then the guard.** `MAX_AUTO_RESUME` is the knob (two, at
  time of writing). A transient failure gets ridden out; a ticket that genuinely
  cannot be closed costs three passes and then stops, with the same refusal and the
  same two ways out as any other stop. Unbounded would be the runaway the re-work
  guard exists to stop, and every pass is billed.
* **The re-buy names a ticket.** It is owed to *the* ticket whose pass just ended, so
  it cannot be spent on some other ticket this loop also worked, and it is retired
  the moment the loop buys a pass on anything else.
* **`Esc` is not covered.** A cancelled pass earns nothing, so the ticket you just
  stopped stays stopped even if you `Tab` out and straight back in. That is the
  difference between "the agent stopped by itself" and "a human stopped it", and it
  is the reason the guard was kept rather than removed.

You will see it as two lines, in this order:

```text
beads: worked looprs-26r but `bd` says it is `open`, not closed. The loop is not
stopping: the same ticket gets an automatic re-buy, and when those run out the guard
will stop it.
beads: automatic re-buy 1 of 2 on looprs-26r — its last pass stopped without closing
it, so the loop is finishing what it already paid for rather than parking on it.
```

The count is the part worth reading: `1 of 2` is a repair the loop chose, and `2 of
2` is the last one before a human is needed again.

What still parks the loop is everything that is *not* an agent completing: `bd`
unreadable after a pass, a worker whose stream ended without ever settling (crash,
`kill -9`, OOM), a planner that produced no tickets, and a ticket the worker
handed to you as `blocked`. A crash is not a completion, and auto-restarting a
worker that died is a respawn storm with a receipt.

The rule lives in [`BeadsTask::worker_settled`](../../src/session/beads/task.rs)
and [`AutoResume`](../../src/session/beads/guards.rs); the tests are indexed under
"Automatic re-buys" in [docs/testing.md](../testing.md).

## An epic is worked after its children

Ask for a subsystem and the planner writes a container first: an epic that holds the
plan, at the sharpest priority, and the actual work as children underneath it. `bd
ready` knows about *blockers*, not about containment, so it hands the epic back as
the top of the list — and the loop used to take it, spend a metered pass reading a
paragraph of intent, and leave the children untouched. Every pass after that was the
same argument again (looprs-lpw).

So a bead that another workable bead on the same ready list names as its parent goes
to the back of the queue, out loud, and becomes workable again the moment nothing is
left under it:

```text
bd ready
   ├─ needs a human (blocked/deferred) → skipped, named
   ├─ somebody's container, still → walked past, named, with the count holding it down
   ├─ this loop already paid for it → refused, both ways out, unless it is owed an
   │    automatic re-buy ([above](#when-an-agent-completes-without-closing-its-ticket))
   ▼ otherwise
   claim it and spawn the worker
```

Four edges worth knowing, because each is a board shape someone will hit:

* **A hierarchy resolves bottom-up for free.** An epic, a child, and a subtask under
  *that*: every parent defers to whatever hangs below it, so the leaf goes first and
  the list re-sorts itself as each one closes. Nothing is sorted; the deferral is
  re-derived from the ready list on every pass.
* **The epic is deferred, not benched.** Once its children are closed the container is
  the only thing left that needs a hand — the pass that decides whether the epic is
  really finished, and closes it. A rule that made parents unworkable would leave
  every epic open forever.
* **A human's ticket does not hold its parent down.** A `blocked` or `deferred` child
  is work this loop may not do, so an epic whose only children are a human's is as
  workable as an epic with none.
* **What the read cannot show, the loop does not guess.** Parentage comes from the
  child row's `parent` field, never from the shape of an id (`looprs-x.4`), and a
  parent whose children are *not* in the list — closed, held elsewhere,
  never written — is workable, exactly as an unfamiliar status is workable.

The field is the awkward part, and it is why this rule is built from the child side.
Measured on `bd` 1.3.1, an epic with one open child reports `dependent_count: 0` on
its `ready` row and `dependent_count: 1` for the same bead from `bd show`: the counts
are filled in per-verb, so a parent read off `ready` looks childless. Every child,
though, names its parent on every read. That is also why the ready read asks `bd`
for `--limit 0` (looprs-037's rule, and
[ADR-0007 F2](../adr/0007-kanban-board.md)): the loop decides "is this epic still
unfinished?" by looking for the epic's children *in this list*, so a page cut at 100
rows does not read short, it re-promotes an epic above work that is sitting on the
board.

Two things you will see, then. In the transcript, once per pass in which the rule
changes the answer:

```text
beads: working looprs-x.1 before 1 parent ticket(s) whose children are still ready —
a parent is a container and its children are the work, so it goes last:
  looprs-x (Epic: persistence) — 2 child ticket(s) still ready
```

And in the planner's instructions
([`services::prompts.rs`](../../src/services/prompts.rs)): create the epic, then
create every piece of work under it with `--parent <epic-id>`, and never leave an
epic without children — if the whole epic is one ticket's worth of work, write one
ticket instead.

The rule lives in
[`BeadsLoop::pick_bead`](../../src/session/beads/machine.rs); the tests are indexed
in [docs/testing.md](../testing.md#an-epic-comes-after-its-children-looprs-lpw).

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

Rule 1 is a claim about **both** kinds of pass — a worker's and the planner's — and
one of them used to slip through it. The gate in front of the next pass reads "a pass
is in flight, do not open another" off a flag that only the worker side set, so a
`Tab` out and back during a *plan* opened the gate, and starting that pass began by
tearing down the planner it was meant to protect: a paid-for plan thrown away by a
keystroke, and a loop that then read the board the planner had not yet written and
reported *that* as the result
(`tabbing_out_and_back_while_the_planner_runs_does_not_kill_it`, looprs-2v6). A
worker pass could not be lost this way, which is exactly why the bug read as
something the planner did rather than something a `Tab` did.

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
a pass that settled without closing gets
[`MAX_AUTO_RESUME`](../../src/session/beads/guards.rs) automatic re-buys, and when
those are spent the loop stops rather than paying for the same ticket again. If your
loop stopped with a ticket still open, it already tried twice — go look at the ticket
(`bd show <id>`, and the transcript lines that named each re-buy).

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
