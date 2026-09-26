# ADR-004: A read-only mini kanban on the loop's own surfaces

- **Status:** Accepted
- **Modules:** `src/kanban.ts` (view model, layout, poller, component), `src/beads.ts`
  (`listClosed`), `src/app.ts` (composition, the read closure, refresh-on-write),
  `src/render.ts` (the row in the work surface's chrome), `src/idle.ts` (the grid
  above the prompt), `src/main.ts` (env knobs)

## Context

ADR-003 put a window into the inference server on the screen, because the question
"why is this pass slow" had an answer that was not anywhere visible. There is a
second question this loop asks constantly and never shows:

**What is on the board, and what will it pick next?**

The loop knows. `check_work` reads the ready list on every iteration, `pick` takes
the head of it, `work` holds one `in_progress` ticket, `finalize` closes it. The
whole machine is a walk over a kanban — ready → in progress → done — and the
screen shows none of it. When the loop is idle the operator sees
`ready 3 · in progress 1`: two numbers, no picture. Not whether the next pick is
the ticket they meant to hand it, whether someone else's ticket is stuck in
progress, or whether the thing they asked for an hour ago actually finished.

Meanwhile the repo already ships a full web kanban (`bdui`, in `harness.sh`). That
is the right tool for browsing a board and the wrong tool for this moment: it is
another window, opened before the run and closed after it, while the thing the
operator is watching — the session, the monitor, the prompt — is in the terminal.
The question is asked *here*, so the answer belongs *here*, in the four lines
above the keyboard.

Five constraints shaped the module.

**1. A read here is a child process.** The monitor's polls are `GET`s against a
socket; every poll of the board is a `bd` spawn. The cadence cannot be the
monitor's one-second tick, and a board that cannot be read must not become a
process-per-second loop.

**2. A display that can move a ticket is not a display.** The rule ADR-003 states
for the monitor matters more here. The loop claims and closes tickets; a component
that could too is a second driver with different incentives and no lock. So
`src/kanban.ts` takes no `BdClient`: the composition root hands it a `read()`
closure returning plain data, and there is nothing in the module to misuse.

**3. Absent must not render as zero.** A board that paints an unread queue as
empty is worse than no board: it is a confident statement that there is nothing
to do, made at exactly the moment the reader cannot check. Every column therefore
carries whether it answered at all.

**4. The screen is already spoken for.** ADR-001 §6 put the transcript in
scrollback and ADR-003 §2 established that the only place a HUD stays put is the
fixed chrome. The board joins that region; it does not become content.

**5. `bd` reports dependencies in two shapes.** `bd list` emits an edge
(`{issue_id, depends_on_id, type}`) and `bd show` emits the other issue inlined
with `dependency_type`. A card that shows "no blockers" for a ticket that has one
is a fail-open lie, so blockers are counted through `normaliseDependencies`, the
same reader the loop's own picker uses.

## Decision

**Add `src/kanban.ts`: a pure view model over three columns — `ready`,
`in progress`, `done` — with two layouts (a one-line `row` and a bordered
`board` grid), driven by a poller that reads through an injected closure, drawn in
the fixed chrome of both surfaces.**

### 1. Three columns, and three honest states per column

Each column is `live` (answered, possibly with an empty list), `absent` (not part
of the read) or `unknown` (asked, failed). The rendering follows:

| State | Count shows | Empty column shows |
| --- | --- | --- |
| `live` | the number | `—` |
| `absent` | `—` | `—` |
| `unknown` | `?` | `unread` |

The closed read is **bounded** (`bd list --status closed --limit N`, N =
3 × `doneLimit`, floor 30) and the view knows the window it was read with: if
the read came back full, the count is rendered as a floor — `done 5+` — because
we know how far we looked and do not know what lies beyond. An exact count and a
truncated one are different claims and must not look the same.

`Promise.allSettled` is what the read closure returns into: one column failing
does not black out the two that answered. Before the first read lands the board
draws nothing. After a *failed* first read it draws `ready ? · in progress ? ·
done ?` — silence there would be indistinguishable from `LOOP_KANBAN=0`, and an
operator who cannot tell the two will assume the queue was empty.

### 2. The order is the picker's order, or the board is lying

The `ready` column is sorted the way the loop chooses: priority first, ties broken
by the ticket that has waited longest, then id. The head of that column is the
next pick, and the `row` layout says so explicitly:

```
▦ ready 4 · in progress 1 · done 5+ · next loop-3k ship the kanban
```

If the board's order and the picker's order ever diverged, the board would be
advertising a future that would not happen — so the sort is tested against the
shape of the loop's own choice rather than against a plausible-looking one.
`in progress` leads with our own ticket; `done` is most-recently-closed first,
sorted on `closed_at` (read off the raw payload — `Issue` does not carry it)
before `updated_at`, and capped *after* sorting so the cap keeps the recent past
rather than whatever `bd` happened to return first.

Deduplication is by id with the live columns winning: a ticket read as both ready
and closed mid-poll is painted once, in the column that is being worked.

### 3. Two layouts, chosen by what fits

`row` is one line — three counts and the next pick — for the work surface, which
already carries a transcript, a monitor and a footer. `board` is a bordered
three-column grid, which is what the idle surface gets because nothing there is
competing for the screen.

A mode is a preference, not a promise. Below `BOARD_MIN_COLUMN` (16) per column
three columns hold an id and nothing else, so `board` degrades to `row` rather
than crushing titles:

```
  ╭ ready 4 ───────────┬ in progress 1 ─────┬ done 5+ ───────────╮     64 cols
  │· loop-3k ship the …│● loop-4k wire… [1m]│✓ loop-1m add … [1h]│
  │· loop-7m back off …│—                   │✓ loop-2b audi… [5h]│
  │· loop-2t blo… ⚠1 +1│                    │✓ loop-0a b… [3d] +2│
  ╰────────────────────┴────────────────────┴────────────────────╯

  ▦ ready 4 · in progress 1 · done 5+                              40 cols → row
```

Two rules inside the grid are not cosmetic:

- **The id is never elided.** A truncated id cannot be typed into `bd show`,
  which makes it worse than useless. The title is what gives way.
- **Overflow is admitted, not dropped.** A column with more cards than rows puts
  `+N` on its last visible card. It does *not* reserve a row for the marker — on
  a three-row board that is a third of the column lost to bookkeeping — and it
  does not stamp a marker on every spare row, which makes one card look like a
  column of dashes.

### 4. It costs what it says it costs

Default cadence 5s (`LOOP_KANBAN_MS`), minimum 500ms, and **backoff**: N
consecutive failures wait `interval × 2^N` up to a 60s ceiling
(`maxBackoffMs`), reset by one good read. This is the whole difference between
this poller and the monitor's, and it exists because the failure mode is a child
process: a `bd` that is not on `PATH` must cost one notice and a slowing
trickle, not the machine.

Reads never overlap — `cycle()` joins the one in flight rather than starting a
second, so a `bd` that takes three seconds produces one long wait, not a queue
of them — and listeners are notified once per read, through the presenter's
existing 33ms coalescing window, so a read is one frame and never a burst.
Timers are `unref`'d; `stop()` cancels them, and a stopped source leaves nothing
on the clock.

The rendering side keeps the same rule as the monitor's component: nothing is
cached between frames and the lines are produced from a snapshot already in
memory, so a `bd` that takes a second to answer cannot delay a frame or the
prompt.

### 5. Where it is drawn

*Work surface* — the `row` in the fixed band above the footer, by default: the
transcript needs the rows. `LOOP_KANBAN=board` buys the grid there instead.

*Idle* — the `board` grid, above the board's own one-line status and the prompt.
Idle is where the room is and where the question is asked.

Each HUD is placed independently (`LOOP_KANBAN_AT=band|top`, same knob shape as
the monitor's), and the child order on the surface and the frame assembly in
`captureFrame()` are stated as one rule in one place: pinned `"top"` means above
the transcript, `"band"` means below it, monitor outermost. They are the same
rule because when they were two rules, `kanban=top` + `monitor=band` silently
dropped the board out of every captured frame — a bug that looks fine in the
terminal and fine in the tests, and only wrong in the capture.

A released surface stops drawing the board, exactly like the monitor: those lines
describe the queue *now*, and after a handoff "now" is no longer describing the
work you are looking at. The plain/piped path never sees it.

### 6. Our ticket is marked where we are looking

The card for the ticket this run holds gets `▸` and leads its column. The mark is
applied at draw time (`withCurrentId`) rather than baked in at read time, because
the presenter learns the issue id the moment work starts and the picture on
screen was read before that — a highlight only computed at read time is a setter
whose effect nobody can see, which is exactly what it was before it was fixed.

### 7. Colour by role, not by hand

The board paints through `MonitorTheme`, the same role-based theme object as the
monitor (accent for the next pick and our own ticket, success for closed, warning
for a blocked card and for anything unread), so it picks up pi's own theme and can
be rendered with `theme = null` for the plain path.

## Rejected alternatives

**Point at the existing web kanban instead.** Already in the harness, and kept.
It answers a different question on a different surface: browsing the board, with
full bodies and history. The moment this serves is "while I am watching this run,
what will it pick" — and the answer must be in the terminal, in the two seconds
before I type, not in another window I have to alt-tab to and refresh by hand.

**An overlay drawn over the transcript.** Rejected once already (ADR-001 §3) for
the spinner: pi-tui has no floating layer, so an overlay means fighting the diff
renderer for the terminal's idea of its own screen, plus occluding content the
user is reading.

**Make the board interactive (drag a ticket to `ready`, click to claim).** That is
the second-driver problem in its purest form, with a mouse. The loop's claims are
guarded, ordered and logged; a UI that moves tickets outside the loop's own
transitions makes two writers of a board that has one protocol for changing it.
Read-only is not a scope cut, it is the safety property.

**Poll on every render.** A frame would cost three `bd` spawns. At 30fps that is
90 processes a second against a database with a lock.

**Cache the rendered lines in the component.** Cheaper on CPU, but the picture
would then have two clocks: the read's and the cache's. `ageMs` is derived at
call time precisely so an unpainted board still ages honestly and a stale line
says how stale it is.

**Read only the counts (`bd ready | wc -l` style).** `ready 3 · in progress 1`
is what the idle status line already does, and it is what motivated this ADR:
two numbers that answer nothing about *which* ticket, *whose* ticket, or what
just finished.

**One canvas for monitor + board + footer.** Tempering them together into one
"dashboard" component sounds tidier and makes each new field a merge in the same
file, while making the width budget a single fight instead of three independent
degradations. They are separate HUDs with separate cadences and separate failure
modes; keeping them separate is what lets one go `?` without touching the others.

## Consequences

**What this buys.** The queue is visible while it is being walked. The next pick
is on screen before the loop takes it, so a wrong pick is caught in the moment it
happens instead of in a commit three hours later. Idle ceases to be a blank prompt
with a count: it shows the whole shape of the board and the recent past. All of
it in pi's own pixels, themed by the same theme object, with the same read-only
guarantee as the monitor.

**Trade-offs accepted.**

- **Chrome.** The `row` costs one line; the `board` costs up to five. Both are
  configurable, independently of the monitor, and `LOOP_KANBAN=0` removes the
  whole thing and leaves every surface working, because both surfaces talk to an
  interface and a null object satisfies it.
- **Up to one interval of staleness.** The board is a read, not a subscription;
  `bd` offers nothing to push with. Mitigated by refreshing on write: the
  composition root wraps the *real* beads client so this run's own creates,
  status changes and closes trigger a re-read immediately, which removes the
  lag on the case that matters most — the ticket you just finished.
- **Cost per poll is three `bd` processes**, every 5s, with exponential backoff
  on failure. That is real and it is why the cadence is 5s and not 1s. It is
  also why an injected fake in a test is *not* wrapped with refresh-on-write:
  the wrapper changes what the test is counting.
- **The `done` count can be a floor** (`5+`) rather than a number, because the
  read is bounded. This is deliberate: the alternative is either an unbounded
  read or a wrong number.
- **The order is a second implementation of the picker's order.** If the picker's
  rules change and this sort does not, the board will advertise a future the loop
  does not deliver. Pinned by tests over the sort itself; still a duplication a
  future refactor could remove by having the loop pass its own ordering in.

## Verification

`test/kanban.test.ts` (84 tests) over: the three honest states per column and
why an unread column never renders `0`; the floored closed count; blocker
counting through **both** bd dependency shapes (`bd list` edge and `bd show`
inlined), which is the fail-open trap; the picker's ordering in `ready`, our
ticket first in `progress`, `closed_at` over `updated_at` in `done`, cap
*after* sort; dedup with live winning; the draw-time highlight, including that
a draw-time `undefined` does not clear a view-level one; the grid's width maths
(every line the same visible width at 60/80/100/121, remainder to the
right-hand columns, refusal below the minimum); the id that survives a 20-column
cell; overflow admitted as `+N`; the marker that appears once per column and
not per spare row; the `board`→`row` degradation at 40 columns; the row's
counts, its next pick, and the next pick dropped whole rather than truncated;
staleness marking; the poller's immediate first read, four-interval continued
polling, exponential backoff with its ceiling, reset on success, joined
cycles, no-overlap under a slow read, one notify per read with a throwing
neighbour, timers gone after `stop()`, no timer left by a `refresh()` while
stopped, the `?` row after a failed first read, the 500ms interval floor, and
`describe()` in both quiet and verbose forms; the null board; the component's
`setCurrent`/`setMode`; then the surfaces — the row in the band, the grid in
the transcript's absence above the footer, our ticket marked on the work
surface, both HUDs kept when pinned to opposite ends, monitor outside board
when both go top, the board gone on release, one frame per read through the
coalescing window, nothing in a piped log, nothing drawn when none was given,
a board that fails drawn as unread rather than absent; and the composition root
reading all three lists with the closed read bounded, forwarding the label
filter, switching entirely off on one flag and *saying* so, taking an injected
board wholesale, and `refreshBoardOnWrite` refreshing on exactly the five
mutating calls and on none of the five read-only ones — with a failing refresh
proven unable to spoil the write it followed.

`node tools/kanban-demo.mjs` draws the whole thing without a board: the grid at
100 and 64 columns, the same without colour, the row, the 40-column
degradation, `ready 0` against `done ?`, the nothing-answered row, and an
empty board that must not look like a failure. Its output:

```
╭ ready 4 ───────────────────────┬ in progress 1 ─────────────────┬ done 5+ ───────────────────────╮
│· loop-3k ship the kanban       │▸ loop-4k wire the board i… [1m]│✓ loop-1m add the backend … [1h]│
│· loop-7m back off when bd is u…│—                               │✓ loop-2b audit the provid… [5h]│
│· loop-2t blocked behind … ⚠1 +1│                                │✓ loop-0a bootstrap the… [3d] +2│
╰────────────────────────────────┴────────────────────────────────┴────────────────────────────────╯
▦ ready 4 · in progress 1 · done 5+ · next loop-3k ship the kanban
```

Run against a real board with `node tools/kanban-demo.mjs --live`, which issues
the same three queries through the same client the loop uses.
