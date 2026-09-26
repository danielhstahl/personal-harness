# ADR-006: The index lock — wait it out, and never be the thing that strands it

- **Status:** Accepted
- **Modules:** `src/gitlock.ts` (the only process spawner in `src/`, and the
  kill policy), `src/vcs.ts` (the writer: lock-aware staging and committing),
  `src/repo.ts` (the reader, which also takes the lock and so also obeys the
  policy), `src/app.ts` and `src/main.ts` (composition and env knobs)

## Context

The report: **every so often the loop dies with `.git/index.lock … File exists`,
and it keeps happening after that.**

That second half is the important half. A single failed commit is a bad minute.
A repository that fails *every* commit from then on, including this one, until a
human deletes a file, is an outage — and the loop caused it to itself.

Before this ADR, two facts about git were unknown to this codebase. Both were
measured in a throwaway repo with git 2.47 rather than assumed, and both are
still asserted by tests, because "we read that somewhere" is how the wrong
signal got picked the first time.

**1. `SIGKILL` strands `.git/index.lock`. `SIGTERM` does not.**

git removes its own lock on every exit path it gets to run. `SIGKILL` gives it
none of them. The loop's git spawners used `execFile(..., { timeout, killSignal:
"SIGKILL" })` — so a `git add` that overran its 30-second deadline (a big file,
a slow disk, a hook that hung) did not merely fail that commit. It left the lock
file in the repository permanently, and every later write — this loop's or
anybody else's — failed with the message in the report. A timeout was a
self-inflicted outage with a two-second trigger.

```text
  git add -- huge.bin        (200 MB, ~1.2s here; the lock is held throughout)
  kill -TERM during the add  ->  lock removed by git, nothing left behind
  kill -KILL during the add  ->  LOCK STRANDED. Every later write fails.
```

**2. `git status` takes the index lock too.**

A status run that has to refresh the stat cache *writes* the index, so a reader
and a writer in one repository genuinely collide. That makes lock contention an
ordinary condition of a repo with several things looking at it — an IDE's git
integration, a second run of the loop, the agent session running its own git,
this loop's own reader and writer — rather than evidence of a bug somewhere. Failing
the first time git says "File exists" treats a two-hundred-millisecond
scheduling collision as a broken build.

**3. The loop had no notion of the difference.**

Any non-zero exit from the write phase became a `VcsError` of kind `exit`, the
finalize unit reported `commit-failed`, and the run stopped. Correct for a
rejected hook; far too timid for a lock that would have cleared in 40ms.

## Decision

### 1. One spawner, one kill policy: `SIGTERM` first, `SIGKILL` only as escalation

`src/gitlock.ts` is now the only module in `src/` that spawns git (the
allowlist in `test/beads.test.ts` shrank to `beads.ts` plus `gitlock.ts`;
`repo.ts` and `vcs.ts` build argv and classify results but never hold a child
process). It runs the child with a two-stage stop:

```
t = timeoutMs            → SIGTERM   ("stop, and clean up after yourself")
t = timeoutMs + grace    → SIGKILL   ("you did not stop")
```

Node's own `timeout` + `killSignal` pair can deliver exactly one signal, which
is the whole reason the timer moved into this module. The grace period defaults
to 5s — deliberately generous, because it is the window in which git finishes
writing the index and removes its lock. Tightening it is how the stale lock comes
back.

The kill is reported, not inferred: the `ChildOutcome` carries `killedBy`, so
"timed out" is a fact this run observed rather than a guess from
`killed === true`, which node also sets for other reasons.

### 2. Lock contention is retryable. Almost nothing else is

`runWrite()` in `src/vcs.ts` retries **only** on git's index-lock error —
matched on the whole phrase `Unable to create '<path>': File exists` rather than
on `File exists` alone, because that phrase turns up in errors about other files
and a false positive there buys a retry loop behind an error that will never
clear.

Retrying on that specific error is safe by construction, and that is the
argument for doing it at all: the error means git failed *before it acquired the
lock*, so it changed nothing, so running the same command again cannot
double-apply anything.

A rejected hook, an unsafe path, a corrupt object — none of those get the loop.
Retrying a hook rejection just rejects again, slower, and buries the real message
under backoff noise. There is a test that installs a real pre-commit hook that
says no and asserts exactly one attempt was made.

Backoff is exponential with 25% jitter, capped, with a ceiling on both attempts
and total wait (defaults: 250ms base, 4s cap, 20 attempts, 30s total). The
jitter is not decoration: two processes that begin retrying simultaneously on
identical fixed schedules keep colliding on the same millisecond forever.

### 3. The failure is its own kind, and says what happened

When the wait is exhausted the writer raises `VcsError` kind `index-locked` —
not `exit`, not `timeout`. The distinction is the whole operator experience:

```
the git index has been locked for 30.2s (20 retries; the lock is 91.8s old).
Nothing was staged and nothing was committed. A lock that old usually means a
git process was killed partway through: if no git is running, remove
/path/.git/index.lock and run again — or set LOOP_GIT_STALE_LOCK=remove to
let the loop clear locks older than 60s by itself.
```

Three things that sentence has to get right: the **age** of the lock (the number
that says whether this is a collision or a corpse), the **assertion that nothing
was staged or committed** (because it is true here and it is the first thing a
reader needs to know before they decide what to do), and the **remedy**.

### 4. A stale lock is aged, never assumed — and removal is opt-in

`.git/index.lock`'s mtime is set when git creates it and never updated, so its
age *is* how long it has been held. A lock older than `staleAfterMs` (default
60s) *looks* like a crashed process rather than a running one.

Looking is not doing. `staleLockPolicy` defaults to `"report"`: the loop waits,
then fails with the message above and leaves the file alone. `"remove"` —
`LOOP_GIT_STALE_LOCK=remove` — deletes it once and retries, and logs the act:

> *If a git process really was still running, this is the line to look at —
> `LOOP_GIT_STALE_LOCK=report` is the default and would have waited instead.*

The default is not caution for its own sake. A lock held by a live process is
load-bearing, and yanking it mid-write is how you get a corrupt index — a much
worse injury than the one being cured, and one that is silent until much later.
An automated agent should not be guessing at that trade on the operator's
behalf; it should age the lock, say what it found, and let a human make the
call. Operators who would rather it made the call can turn it on in one variable,
having read this paragraph.

An unknown age is never treated as stale. "Could not stat it" is a reason to
wait, not a reason to delete.

### 5. The reader obeys the same policy

`git status` — `src/repo.ts`, the read-only snapshot builder — takes the index
lock when it refreshes the stat cache. A reader killed with `SIGKILL` therefore
leaves the *writer's* repo locked behind it, which is exactly the bug from
§1 wearing a different hat. Both the reader and the writer now stop with
`SIGTERM` + grace, from the same function, so the invariant "nothing this app
does can strand a lock" is a property of the spawner rather than of two
modules remembering to be careful.

## Consequences

**The knobs.** `LOOP_GIT_LOCK_WAIT_MS` (how long a write waits out somebody
else's lock, default 30000), `LOOP_GIT_KILL_GRACE_MS` (the SIGTERM→SIGKILL
grace window, default 5000), `LOOP_GIT_STALE_LOCK_AFTER_MS` (when a lock looks
abandoned, default 60000), `LOOP_GIT_STALE_LOCK=remove` (allow clearing a
stale one, off by default). All four are ordinary `-e` flags in the container.

**What the operator sees now, and what to do about it.** A collision with
something alive costs a pause and, if it clears inside the wait, nothing at all —
the commit lands. A wait that exhausts prints the `index-locked` message with the
lock's age. If the age is large and no git is running, `LOOP_GIT_STALE_LOCK=remove`
makes the loop handle that case itself from then on. A lock that clears on the
retry is logged with each retry and its age, so `LOOP_DEBUG` shows the whole
wait.

**What this deliberately does not do.** It does not delete a lock that looks
live, ever, under any policy. It does not retry anything except the index-lock
error. It does not treat a lock as stale when it cannot measure it. It does not
make the run hang: every wait is bounded, and the outcome of waiting is either a
committed bead or a message that says nothing was staged.

**Residual risk, accepted.** A process that holds the lock longer than
`LOOP_GIT_LOCK_WAIT_MS` still fails the run. That is by design — the
alternative is a loop that can stall indefinitely behind something it cannot
see. What the ADR buys is that the failure is *legible* (age, retries, remedy),
that it is not self-inflicted (`SIGTERM` cleans up), and that the common case —
an IDE refreshing the repo for 300ms — is now free.

**One spawn site, auditable in one file.** `src/gitlock.ts` is now the only
place in `src/` that spawns git; `test/beads.test.ts` enforces it. The signal
order exists once. If someone later changes `SIGTERM` to `SIGKILL` there, the
control test (`test/gitlock.test.ts`: *SIGKILL strands the lock*) fails on the
same pull request, which is the point of writing that test at all: the good test
is only trustworthy because the bad one proves the measurement works.
