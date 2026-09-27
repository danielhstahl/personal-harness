# ADR-008: Nothing to commit is a no-op, not a stop

- **Status:** Accepted. **Amends** rule 4 of the loop
  (`src/loop.ts`, workspace-5yn.9) and the finalize contract of `.8`
  (`src/finalize.ts`). It changes what happens *after* the finalizer refuses an
  empty commit; it does not soften that refusal.
- **Modules:** `src/orchestrator.ts` (the `finalize_noop` event and its
  transition), `src/finalize.ts` (the outcome → event mapping), `src/loop.ts`
  (unit write receipts, the park guard), `test/orchestrator.test.ts`,
  `test/finalize.test.ts`, `test/loop.test.ts`

## Context

A run stopped here, and stopped for good:

```
Finalize commit failed: nothing to commit for workspace-jzj.1; no commit, no
memory, no close. Nothing else was written; send retry to repeat just that stage.
```

What had actually happened: the agent finished the ticket, reported `done` with a
summary and a file list, and the working tree had nothing to stage for those
files — because the change was already in `HEAD` (landed earlier, landed by
someone else, landed by a rebase), or because the ticket turned out to need no
code at all. `planCommit` said `nothing-to-commit`, and the finalizer kept the
promise `.8` makes: no empty commit, no memory, no close.

The interpreter filed that under *blocked finalize*, which is what rule 4 says
to do with a finalize that did not end `finalized`. Consequences:

- The run ended. Every other ready bead behind it was unreachable — one
  already-satisfied ticket stopped the whole board.
- The suggested remedy was `retry`, which re-issues the same commit against an
  unchanged tree and returns the same answer. A hint that cannot help is worse
  than no hint: it is the sentence an operator acts on.
- The message was framed as a failure and the exit was framed as a failure, so a
  run that had broken nothing looked like one that had.

For real finalize failures the old rule is right, and it stays right. A commit
with no handoff note, a handoff with a note but no close, an unsafe path, a
foreign staged index: these are half-written states, and going off to look for
more work with one of them open is how beads get stranded. The mistake was putting
"there was nothing to write" in the same bucket as "writing failed". Nothing was
half-written. Nothing needs repair. There is only a bead that this run cannot turn
into a diff.

## Decision

### 1. `finalize_noop` is its own event

`nothing-to-commit` maps to `{ type: "finalize_noop", reason }`, where the
reason names each reported path and why it produced nothing:

```
no reported path produced anything to stage — src/landed.ts (tracked but
identical to HEAD — nothing to commit for it)
```

`unsafe-path`, `unrelated-staged`, `commit-failed` and `invalid-request` still
map to `finalize_failed{commit}`. The distinction is not cosmetic: the first
says *the tree has nothing for you*, the others say *something refused to be
written*, and only the second kind is worth stopping a run for.

The event is legal only while the commit stage is in flight. A no-op reported in
the `handoff` or `close` stage means the machine and the finalizer disagree about
whether a commit exists, and that is `stage-mismatch` — a rejection, not a
parking decision.

### 2. The bead is deferred, not closed

Closing seemed to follow: the ticket needs no change, so it is done. It does
not follow, because "no diff" is not evidence the ticket was *finished* — only
that this run had nothing to add. `report_done` with no edits would quietly mark
tickets closed, and the loop would be attesting to completions it cannot show.
The thing `.8` refuses to invent is a commit that says "done" over an empty
diff; inventing a *close* over an empty diff is the same lie in a different
table.

`deferred` is the honest status, and it is chosen for a mechanical reason too:
the loop picks from exactly two reads, `bd ready` and
`bd list --status in_progress`, and `deferred` is in neither. The bead stops
being worked, stays on the board and in the kanban, keeps its history, and is
one command from a retry.

The write is guarded (`--if-status in_progress`), so a status a human changed
while the finalize was running is left exactly as that human left it.

### 3. The reason goes down before the bead moves

Same invariant as `work_failed`: the `loop:failure:<id>` note is written first,
then the status write. A deferred bead nobody can explain is worse than one that
stayed put — deferred looks like shelving, and shelving without a reason reads as
the loop hiding something. The note says what was not done and how to undo it:

```
Nothing to commit for workspace-jzj.1: … No commit was made, so nothing was
remembered as done and workspace-jzj.1 was NOT closed. The loop deferred it
rather than end the run on it; it will not be picked again while it is
deferred. Set it back to open (bd update workspace-jzj.1 --status open) when
there is actually a change to make, or close it by hand if the ticket needs no
code at all.
```

### 4. The run carries on — and asks if there is nothing left to carry on with

The transition goes through the cold boundary like any other finished
iteration: drop context, re-read the board, pick the next ticket. When the
board has nothing else, the machine lands in `idle` and asks the operator what
to do next. That is the other half of what the report asked for — *"either ask
for human input or go on to the next ticket"* — arriving without a new input
protocol, because the idle surface already is one.

### 5. The park is not allowed to fail quietly

The whole reason this case continues instead of stopping is that the bead ends
up invisible to the next board read. So if the write that defers it fails, the
precondition is gone: the bead stays `in_progress`, the next read resumes it,
the work lands in the same nothing, and the loop is a wheel with the brake cut.

That one failed write is therefore a stop — `blocked`, naming the bead, the
cause and both ways out. The check sits in the pump's read flush rather than in
the driver, which is where the re-pick would have happened: the failed park
trips the guard *before* the board is read again, so no second agent session is
started on a bead that can only repeat itself.

### 6. The interpreter's "already performed by the unit" claim now has receipts

`vcs.commit` is executed as one unit (rule 3), and the machine's follow-up
`beads.remember` / `beads.close_issue` effects are marked *already performed by
the unit* rather than run again. Until now that claim was made on trust: any
remember with the right key during a finalize unit was marked done, and any
close with the right id.

The no-op path broke that in the worst available direction. The machine asks,
during the same unit, for a note the unit never wrote — the "why this bead was
deferred" note. Marking it performed would have dropped the write while leaving
the machine believing the board held an explanation it does not. A silently
dropped effect is the failure mode rule 2 exists to prevent, and this one lands
on the exact path added to stop runs from being wasted.

The finalize unit now carries receipts per write: `owned` (the unit took
responsibility; the interpreter must never run it, least of all after the unit
died on it, which is the blind retry `.8` forbids) and `landed` (it reached the
board). Three honest states follow:

| outcome | handoff note | close |
| --- | --- | --- |
| `finalized` | owned + landed → claimed performed | owned + landed → claimed performed |
| `handoff-failed` | owned, not landed → claimed **attempted and FAILED** | never owned |
| `close-failed` | owned + landed | owned, not landed → **attempted and FAILED** |
| `nothing-to-commit` | never owned | never owned |

Any remember whose key is neither the handoff nor the failure note of the bead
in hand is still `unit-drift`, and a close the unit never owned is
`unit-drift` too. What changed is that a note the unit *didn't* own is now
written for real instead of being claimed or refused.

## What did not change

- **No empty commit, ever.** The refusal that starts all this stands.
- **No auto-retry of a write.** A failed write is a fact about the world.
  `finalize_failed` still ends the run naming the stage, the commit that exists
  and the bead that may still be open.
- **No close on a no-op.** The loop does not decide a ticket was done.
- **`retry` still means "re-issue the pending effect".** A no-op leaves the
  finalize state, so there is nothing pending to retry — which is the point:
  the new message no longer offers a remedy that cannot help.
- **The bead is not deleted, un-assigned or hidden.** Deferred is a board
  status a human owns; nothing here removes it automatically.

## Operations

What the operator sees, in order:

```
Nothing to commit for workspace-jzj.1: no reported path produced anything to
stage — src/landed.ts (tracked but identical to HEAD — nothing to commit for
it). No commit, no handoff, no close — and workspace-jzj.1 is now deferred, so
this run moves on to the next ticket instead of stopping here. To put it back:
bd update workspace-jzj.1 --status open. Reason recorded under
loop:failure:workspace-jzj.1.
```

Bringing one back:

```sh
bd update workspace-jzj.1 --status open
```

Usually worth doing *after* changing what the ticket asks. A bead that produced no
diff will produce no diff again, and the loop will defer it again — correctly.
The status is a decision point, not a junk drawer: if the answer is "this needed
no code", close it by hand, which is a claim a human can make and the loop
cannot.

## Alternatives considered

**Close it with a "already satisfied" reason.** Rejected: the loop asserting a
ticket is done because its tree was clean is exactly the invented completion
`.8` was built to prevent, and it deletes the operator's chance to notice the
ticket was miswritten.

**Reopen it (`open`) and continue.** Rejected: `open` is exactly what `bd ready`
returns. The loop picks the same bead next iteration, works it again, defers
nothing, and thrashes to the iteration limit. This is what the guard in §5
prevents even in the failure path.

**Ask the human at that instant with a menu.** Rejected: the idle surface takes
free-form text for the splitter, so a menu is a new input protocol for one
question — and the run would still stop for a bead that needn't stop it.
Deferring plus continuing plus falling through to idle gets the same attention
without the new surface.

**Keep `blocked` but word the message better.** Rejected: better words on the
same behavior still mean one already-satisfied bead ends the run, and the
operator still gets a `retry` hint that cannot change anything.

**Special-case the outcome in the interpreter without a machine event.**
Rejected: rule 1. The interpreter performs what the machine asks; a branch on
"what phase are we in" that decides the next transition is the shape this loop
was written to make impossible.

## Pinning tests

- `(d4n)` — the event applies only in the commit stage; effect order is
  *remember → park → warn → drop context → board reads*; the park write is
  `deferred` with `ifStatus: in_progress`; no `close_issue` in the batch;
  rejected as `stage-mismatch` in the handoff and close stages, `no-active-issue`
  with no bead in hand.
- finalize — the mapping is `finalize_noop`, and the reason names each skipped
  path with its own reason, so "already in HEAD" and "was never in the tree"
  stay distinguishable.
- loop (end to end, real git) — a clean bead parks itself and the next ticket
  still gets its full commit/note/close; the park note is on the board; the
  deferred bead gets **no** completion notice; exactly one commit exists, and it
  is the second ticket's.
- loop (end to end, failing park) — the run stops `blocked`, the bead is still
  `in_progress`, and exactly **one** agent session was started.
