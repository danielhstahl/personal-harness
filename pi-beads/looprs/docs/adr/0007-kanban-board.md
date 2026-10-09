# ADR-0007: What the kanban band shows — eight statuses, three columns, one honest read

- **ID:** looprs-5o4.1
- **Status:** Accepted — 2026-10-08
- **Epic:** looprs-5o4 (Kanban band: watch beads move across to-do / in-progress / done in the beads display)
- **Decides for:** looprs-5o4.2 (the poller), looprs-5o4.3 (the row budget), looprs-5o4.4 (the
  component), looprs-5o4.5 (the frame wiring), looprs-5o4.6 (the docs) — and for every future
  reader of `bd` that wants the board rather than one bead
- **Widens:** looprs-037's rule — *"never conflate 'bd is broken' with 'the board is empty'"* —
  from the beads loop's `Result` into a **rendered surface**: the band must be able to tell an
  empty board, an unread board, a stale board and a not-yet-loaded board apart *at a glance*,
  without a log open
- **Measured by:** [`spikes/board_poll_cost.py`](../../spikes/board_poll_cost.py),
  committed output
  [`spikes/results/board-poll-cost.log`](../../spikes/results/board-poll-cost.log)
- **Amended 2026-10-08 by [§7](#7-the-change-detector-asking-the-journal-instead-of-re-reading-the-board):**
  §3's "one read per tick" survives as *one consistent read per tick that reads
  the board*. Most ticks no longer read the board at all — they ask
  `bd events tail` whether anything moved. The mapping, the freshness states,
  the accounting invariant and every prohibition below are unchanged by it;
  what changes is where a tick's ~0.46 s goes, and the addition of the sweep
  that keeps the journal from being trusted beyond what it can see.

---

## Context

The user asked for three columns. `bd` has eight statuses (`BeadStatus`: `open`,
`in_progress`, `blocked`, `deferred`, `ready`, `done`, `closed`, `Unknown`), and
this board currently holds 50 issues in three of them. Three columns cannot hold
eight states without **choosing** where four of them go, and every choice either
misrepresents a bead or silently drops one. Silent drops are the failure class this
codebase already refuses — the whole of looprs-037 is "an empty board and an unread
board must not look the same" — so the mapping has to be written down and defended
before anybody draws a column.

Three things turned out to matter more than the drawing:

1. **`bd`'s status vocabulary is not `BeadStatus`'s.** Verified on `bd 1.2.2`:
   `bd list --status` accepts `open, in_progress, blocked, deferred, closed,
   pinned, hooked` and rejects `ready` and `done`. `ready` is a *derived*
   query (`bd ready`, blocker-aware), not a stored state. So a per-status read
   per column is not merely worse — it is **not expressible** for the two columns
   whose names the user's three words actually imply.
2. **`Unknown` is not hypothetical — it is here now.** `pinned`, `hooked`, and any
   repo-defined custom status (`bd config set status.custom review,verified`) all
   arrive in this build as `BeadStatus::Unknown`, because `#[serde(other)]` is
   doing its job. The "make the board look empty" bug does not need a `bd` upgrade;
   it needs one pinned ticket and a mapping that drops `Unknown`.
3. **The cost of the read is nearly flat in rows and dominated by the process.**
   Which read we pick is therefore a *consistency* decision, not a budget decision,
   and the poll interval is set against the process cost — not guessed.

## Decision

**One read, one mapping, one snapshot, honest states.** The band is a rendering of
`bd --readonly list --all --limit 0 --json` — taken as a single consistent read,
mapped by an exhaustive total function, published latest-wins, and drawn with its
freshness always visible.

### 1. The status → column mapping

Total function over `BeadStatus` (and over `BeadStatusFallback`, whose `Missing`
arm is already folded into `Unknown` by `Bead::status()`). **Every value lands in
exactly one visible place. Nothing is dropped, and nothing appears twice.**

| `BeadStatus` | Where it shows | Row marker | Rationale |
|---|---|---|---|
| `Open` | **To-do** | — | the plain case |
| `Ready` | **To-do** | — | `bd` never *stores* `ready` (it derives it); if a future `bd` or a custom status does, it is to-do. The board does **not** re-derive claimability — see §2 |
| `InProgress` | **In progress** | — | the only state that means "a worker has this" |
| `Blocked` | **To-do**, marked | `⊘` | waiting on a human or another bead — not *work the loop may take*, but the most actionable thing on the board for the person reading it, so it goes where the eye is, marked so it cannot be mistaken for pickable work |
| `Deferred` | **not a row** — footer count only | — | a human deliberately took it out of the running. A row puts it back in play visually; a count keeps it from vanishing |
| `Done` | **Complete** | — | not a stored `bd` status today; mapped anyway so a future/custom `done` is Complete, not `Unknown` |
| `Closed` | **Complete** | — | the state `bd close` actually writes |
| `Unknown` | **To-do**, marked | `?` | **must be visible.** Dropping it is the one option off the table. Reached *today* by `pinned`, `hooked`, and any custom status |
| `BeadStatusFallback::Missing` | folded into `Unknown` → **To-do**, `?` | `?` | a bead with no `status` field is "unknown", not "open" — the rule `bd.rs` already commits to, rendered |

Why `blocked` is a marked To-do row and `deferred` is not, when both are
`needs_human()`: **`needs_human()` answers "may the worker take this?", which is
not the same question as "should the human see this?".** A blocked bead is stuck
on something a reader can go and unstick; a deferred bead is waiting on a *date*
and on nothing that can be done to it. Same refusal, different visibility. The
board reuses `needs_human()` for the marker and does **not** reuse it for
exclusion — and says so here, because the next editor will be tempted to.

`blocked` and `deferred` reach the eye twice on purpose: the `⊘` rows, and the
footer's separate counts. The footer is the only place `deferred` appears.

### 2. `ready` is not re-derived; the board shows stored status only

`bd ready` is a second read with blocker-aware semantics. Computing it client-side
would mean keeping bd's claimability rules in sync inside a widget. So:

- The board renders the **stored** `status` field.
- `bd`'s ready/blocked distinction reaches the board as `blocked` (⊘) versus
  everything else — **not** as a computed "ready" marker.
- `bd ready`'s own ordering is not used. The board keeps `bd list`'s own order,
  which measured out priority-sorted (priorities came back non-decreasing across
  the whole read), so To-do reads in priority order for free. Complete is
  re-sorted by `closed_at` descending — the interesting tail of a long-lived
  board is the recent one, and `closed_at` is present on every closed row of the
  payload (39 of 39 here). If 5o4.4 needs that order *guaranteed* rather than
  observed, pass `--sort priority` explicitly instead of trusting the default.

*Consequence for the ADR's own honesty:* `Unknown` as rendered says "`?`", and
**the actual status word is lost**. `#[serde(other)]` gives a bare `Unknown` with
no payload. Naming it (`review`, `pinned`, …) needs `Unknown(String)`, which
touches `as_str()`, `Copy` and every match — recorded as
[follow-up F1](#costs-measured-and-deliberately-not-done-here) rather than
quietly accepted, because "a `bd` upgrade made the board say `?` everywhere" is
worth being able to read the answer to.

### 3. The snapshot is one read, taken with `--limit 0`

The literal command the poller runs, through `crate::services::bd::run` (which is
what sets `BD_JSON_ENVELOPE=1`, `stdin(null)`, captured stderr and `BD_TIMEOUT`):

```sh
bd --readonly list --all --limit 0 --json
```

Four flags, each load-bearing:

- **`--limit 0`** — mandatory, per looprs-037. `bd list` defaults to 50 rows and
  `bd ready` to 100; a truncated page makes the `+N more` marker wrong, and a
  wrong `+N more` is worse than no marker. Note the rule is honoured by
  `list_status_with` and **not** by `ready_with` today (`["ready", "--json"]`, no
  limit) — the board does not use `ready_with`, and F2 records the gap.
- **`--all`** — without it `bd list` hides closed (and pinned) beads, so the
  Complete column would read empty forever.
- **`--readonly`** — free, and it makes "the board cannot change the board" a
  property of the command line rather than of good intentions. Measured: a write
  under it is refused (`rc=1`, `operation 'update' is not allowed in read-only
  mode`) and the bead's status is unchanged.
- **one read per tick, not three.** Three per-status queries measured **2.9×** the
  cost of the single read *and* can disagree with each other: a bead that closes
  between query 1 and query 2 appears in two columns in one frame. Consistency is
  the reason, not speed.

**`--skip-labels` is rejected, measured.** It is the flag the ticket proposed for
economy and it does not pay: 243 KiB vs 247 KiB at current size (it *adds* a
`meta` object), no measurable time difference at any size — and it **changes the
payload shape**. With `BD_JSON_ENVELOPE=1` it answers `{"data": {"issues": […],
"meta": {…}}}`, where `BdList`'s `Envelope` arm wants a `Vec<Bead>`; to this
codebase that payload is `BdError::Malformed`. Adopting it requires touching the
parser to earn nothing.

### 4. Freshness, staleness, and failure are named visible states

The band always draws **three rows' worth of structure**: a header row, the bead
rows, and a footer row. The footer is never omitted while the band is drawn,
because "when was this read?" is always informative.

There is a fourth row — the framing rule under the header ([§6](#6-the-bands-framing)) —
and it is deliberately absent from this table. It carries no state: no count, no
marker, no freshness. A row that says nothing about the read cannot be in a table
about what the read says, and putting it here would imply it can be wrong.

| State | Header row | Column bodies | Footer row |
|---|---|---|---|
| **first poll in flight** (`never_loaded`) | three names, counts `—` | *(none)* | `reading the board…` |
| **ok, board empty** | `To-do 0` · `In progress 0` · `Complete 0` | dim `—` per column | `bd ok · Ns ago` |
| **ok** | name + **true total** per column | the mapped rows, `+N more` on overflow | `bd ok · Ns ago` (+ `⏸ N deferred` when N > 0) |
| **`Unavailable`** | names, counts `—` | *(last good, dimmed — or none)* | `bd unavailable: <reason>` |
| **`Failed`** | names, counts `—` | last good, dimmed | `bd failed (exit N): <first line of stderr>` |
| **`Malformed`** | names, counts `—` | last good, dimmed | `bd answered unreadably — see the log` |
| **`Timeout`** | names, counts `—` | last good, dimmed | `bd did not answer in 30s` |
| **stale** (last good kept after any error, or the poller has stopped) | names, counts `—` | last good, dimmed | `<the error> · stale Ns` |

Three rules that make that table hold:

- **Never a blank board.** Before the first snapshot lands the band draws the
  header + footer with `reading the board…`. A blank band reads as "no beads",
  which is the looprs-037 conflation rebuilt in a widget.
- **An error never destroys the last good snapshot.** It keeps it, dims it, marks
  it stale, and shows the error's own words. `bd: command not found` and `board
  empty` must be tellable apart from the band alone.
- **A header count is always the true total for the column**, never the number of
  rows this frame happened to draw. Truncation belongs to `+N more`, which counts
  only what that column did not draw.

**Poll interval: 5 s, env-overridable** with `LOOPRS_KANBAN_POLL_MS`, against the
measured numbers below. Not 1 s: the read is ~0.5 s of wall and ~0.3 s of CPU in
*a separate process*, so a 1 s interval runs `bd` half the time, and the measured
max read (623 ms at current size, 690 ms at 50×) already **overruns a 1 s tick**
— the poller would spend its life in `MissedTickBehavior::Delay` and contend with
the beads loop's own `bd` traffic on the same Dolt-backed board. Not 60 s: the
band's whole job is to track movement, and a minute of "To-do" after a bead was
claimed is the band lying about the thing the user is watching. At 5 s a **50×**
board leaves ~4.3 s of headroom over the read, i.e. roughly 6× slack.

**Complete is not capped in the snapshot.** Capping it there makes the displayed
count wrong, which is the one thing the ticket forbids; truncation is the frame's
job (`+N more`, looprs-5o4.4), and the count comes from the same consistent read
so it is the true total for free. The parsed snapshot is also small — `Bead` is
id + title + two enums, ≈ 120 B/bead, so 2 500 beads is ~300 KB retained, not
the 11 MiB the JSON is; the JSON is transient. What actually grows is *read time*,
and it grows slowly: fitted to the two measured points (449 ms @ 500 rows, 672 ms
@ 2 500 rows) that is **≈ 0.40 s of fixed process cost + ≈ 0.11 s per extra 1 000
rows**, which crosses 5 s at **≈ 41 k issues**. **Recorded trigger:** at that
size the read approaches the poll interval, and the fix is `--closed-after <window>` — a bounded *window* with
the window named in the header (`Complete 214 · since 2026-08-09`), never a cap
that hides beads behind an unqualified number.

### 5. Where the data comes from

**The board reads `bd` directly through `crate::services::bd`, in its own task,
with the same `LOOPRS_BD_BIN` / `bd` binary the loop uses.** It does not read the
beads session's mirror. Confirmed reading of the user's "independent of the rest
of the app":

- the session's mirror is the harness's **opinion**, not the board;
- it dies and respawns per generation, so the band would blink out on every
  respawn;
- it goes blank when the loop is parked or Tabbed away, so the band would show
  nothing exactly when the user asks "what's the state?" to decide whether to come
  back;
- and the whole point of the band is to answer the question from the **source of
  truth** the settle check is already built on.

That is why the snapshot lives behind a `watch` (latest-wins) owned outside the
session and not inside it: looprs-5o4.2's contract, and why the frame reads a
plain value and never learns that `bd` exists.

### 6. The band's framing

Three columns of ids separated by nothing but spaces read as **one long column
with line breaks in it**. The whitespace between the columns was doing no work and
pretending to, so the band draws its own framing: `│` down each gutter and a
`───┼───┼───` rule between the header and the bodies. Two rules decide it, and
both are properties of the layout rather than of taste.

**The framing is bought out of the body's surplus and is refused rather than cut a
bead row.** The rule row is granted only if the body still keeps 2 rows after
paying for it, which puts the threshold at a 5-row band: below that there is no
rule and every affordable row goes to a bead. The ranking is the reason, not the
number — *the framing is the cheapest thing on this band to lose*, so it must go
before any bead row does. A band that drew the rule by cutting the second-to-last
bead would be spending the user's content on the widget's legibility, and would
have it backwards in a way that is invisible in the diff and obvious on the screen.

**The framing is never dimmed.** Its style is fixed and does not pass through the
stale pass. This is not decoration-consistency for its own sake: on this band
`dim` is a *word*, and the word is "these rows are from the last good read".
Paint the framing with the same word and it stops meaning that anywhere — the user
is left with "some of this band is old", which is neither the freshness state nor
anything they can do about. One style, always, so the word keeps its meaning.

The two are the same decision seen from different ends: the framing costs the user
something (a row, and attention), so where it is granted and how it is coloured are
both decisions about what the band is allowed to take.

---

### 7. The change detector: asking the journal instead of re-reading the board

**Every tick asks `bd --readonly events tail --since <watermark>` what has been
mutated, and reads the whole board only when the answer says something did, when
the periodic sweep is due, or when the probe could not answer.** The rows still
come from `bd --readonly list --all --limit 0 --json`, mapped by the same total
function, published latest-wins over the same `watch`. Nothing the band *shows*
is replayed out of the journal; the journal is read for **whether**, never for
**what**.

Measured on this repo's board (50 beads, `bd 1.3.1`, macOS / Apple Silicon,
re-measured 2026-10-08 on a quiet machine, five runs each, medians):

| read | wall | CPU | what it costs scales with |
|---|---|---|---|
| `--readonly list --all --limit 0 --json` | ~0.46 s | ~0.23 s | the size of the board |
| `--readonly events tail --since <head> --json` | ~0.18 s | ~0.10 s | nothing, when quiet |

An idle band therefore stops paying for a busy one: a quiet 5 s tick costs
**about 40 % of the wall and 45 % of the CPU it used to**, and the changes a
worker actually makes in this workspace still land on the next tick. That is the whole win, and it is bought
against one new thing that can be wrong — **the journal is not a mirror of the
board** — which is why the sweep is not optional.

**Four ways a board can change without a journal record on this replica.** Each
is documented in `bd`'s own words, and each is a way a poller that trusted the
journal alone would freeze the band on rows it had no reason to re-read:

1. **`bd dolt pull` / a merge.** The journal records what *this replica*
   mutated through its write paths. Rows that arrived as data are not journaled;
   `bd` tells consumers to re-baseline after a sync.
2. **`bd sql`** and anything else that bypasses the write paths — explicitly
   unjournaled.
3. **`events-journal` switched off.** The probe answers with *nothing*, on a
   successful exit, forever. The empty answer is indistinguishable from a quiet
   board, which is exactly the trap: a quiet answer that never says anything is
   a silent lie unless somebody still goes and looks at the board now and then.
4. **Retention.** The floors prune the prefix, so a consumer's checkpoint can
   be pruned out from under it. `bd` fails that read rather than skipping
   ahead, and names `floor` and `head` — which is why the failure is a *typed*
   answer ([`JournalError::Truncated`](../../src/services/bd.rs)) and not a
   string.

So: **`LOOPRS_KANBAN_RECONCILE_MS`, default 30 s** — whatever the journal
says, the board is read in full on that interval. That is the entire cost of
trusting the journal: a change that never journalled shows up on the band up to
half a minute late instead of one tick late, and *only* for the four cases
above. Everything a loop or a human does through normal `bd` writes stays on
the 5 s tick.

**Three rules make the watermark safe** (stated in
[`decide`](../../src/services/board_poller/schedule.rs), which is pure so the policy is
tested as a table rather than raced against a subprocess):

1. **Nothing has ever been read ⇒ always read.** "Quiet since the watermark"
   is a statement relative to a watermark; with no picture on screen it would
   keep `reading the board…` up forever over a board that is perfectly well
   populated.
2. **A probe that failed is never "nothing changed".** It reads the board.
   The inverted failure — a probe that could not answer reporting a quiet
   board — is the only way this design can freeze a band on stale rows while
   the footer says `bd ok`, so it is the one thing the design refuses.
3. **The watermark adopts only what the probe saw *before* the read it
   triggers.** A read is later than the probe that caused it, so the only
   watermark that cannot outrun the rows is the probe's own. Adopting a seq
   observed after the read would let a mutation that landed mid-read be marked
   covered by rows that do not contain it: a lost change, silent forever.
   This rule costs one redundant read the next tick. That is not a trade worth
   hesitating over.
   And a **failed** read adopts nothing at all — the changes that tick was
   reacting to have not been reflected in anything, so the watermark stays
   behind and the next tick reports them again.

**While behind, drain in bounded batches.** Against a journal with history the
poller cannot know how far behind it is without reading something, so it chews
`CATCHUP_LIMIT` (512) records per tick until a batch comes back short, then
switches to asking for one (`PROBE_LIMIT`). While draining it reads the whole
board every tick — it has to, since it does not know whether the records it has
not read touched a bead — which is what makes a large journal *delay* the
savings rather than cost more than the pre-detector poller ever did. `bd` gives
no cheap `head` (there is no `--last`, and `--limit` reads the oldest N), so
this is the shape that costs nothing to be wrong about.

**`--follow` was considered and rejected.** `bd events tail --follow` streams new
records with no polling at all. It replaces a bounded request held under
`BD_TIMEOUT` with a long-lived child holding a connection into a Dolt-backed
board that the beads loop has to write to, plus its own restart, backoff,
reconnect-on-EOF and "did the stream miss anything" stories. The poller's whole
argument is that one task `.await`s one bounded read at a time; a permanent
child is strictly more ways to be wrong for a saving on top of an already cheap
probe.

**A quiet tick publishes nothing.** A `watch` wakes its reader on every send,
and a send carrying no news is a wake for nothing to do. This does not stall the
footer's freshness, and cannot: the age was never carried by the send. It is
re-derived from the last read's own `fetched_at` on the App's tick, so the last
painted frame keeps telling the truth about how old the rows are whether or not
the last tick sent anything. What a quiet tick does change is that the age can
only go **up** until the sweep resets it — bounded by `reconcile`, and
explained in `docs/kanban.md`.

---

## The accounting invariant

Everything above is one property, stated once, and it is testable at the snapshot
level rather than argued:

> **I1 — every bead in one read is counted exactly once in exactly one visible
> place.** For each column *c*: `header(c) = rows_drawn(c) + not_drawn(c)`, where
> `not_drawn` is what `+N more` carries; and `Σ header(c) + deferred_count =
> beads_in_read`. Before the first read, nothing is counted and the band says
> `reading the board…`; on error, nothing new is counted and the band says the
> error.

I1 is what makes `+N more` honest, and it is the thing a dropped `Unknown` breaks.
It is also the thing that fails silently when a column's count is derived from
what got drawn instead of from the read — so it belongs in a test on the snapshot,
not in the widget.

---

## Costs, measured, and deliberately not done here

The change detector's half, measured the same way against the same board
(`bd 1.3.1`, this repo's own 50-bead board, and the scratch board's 200-record
journal):

| Read | board | wall | CPU | payload |
|---|---|---|---|---|
| `--readonly list --all --limit 0 --json` | 50 | **~0.46 s** | ~0.23 s | 243 KiB |
| `--readonly events tail --since <head> --json` (quiet) | 50 | **~0.18 s** | ~0.10 s | empty |
| `--readonly events tail --since 0 --limit 512 --json` | 200 records | ~0.2 s | ~0.1 s | ~3 KiB / 512 recs |

The idle-board saving is the one that matters, because an idle board is what a
band spends most of its life showing: **~60 % less wall and ~55 % less CPU per
quiet tick**, and the quiet tick's cost does not move with the size of the board
at all. A busy board is unchanged in the worst case — one board read per tick,
the same as before the detector existed — because the tick, not the journal, is
what paces the poller.

The older numbers, for the read itself:

From [`spikes/results/board-poll-cost.log`](../../spikes/results/board-poll-cost.log)
(macOS / Apple Silicon, `BD_JSON_ENVELOPE=1` on every leg, medians):

| Read | board | wall (med) | CPU | peak RSS | payload |
|---|---|---|---|---|---|
| `--readonly list --all --limit 0 --json` **(chosen)** | 50 (this repo) | **510 ms** (max 623) | 0.30 s | 131 MB | 243 KiB |
| ″ | 500 (10×) | **449 ms** | 0.31 s | 134 MB | 2.1 MiB |
| ″ | 2 500 (50×) | **672 ms** | 0.65 s | 233 MB | 10.9 MiB |
| `bd ready --json` | 50 | 387 ms | 0.26 s | 122 MB | 2 KiB |
| three `--status` reads, summed | 50 | **1 478 ms = 2.9×** | ~0.95 s | — | — |
| `--skip-labels` | 50 / 500 | 476 ms / 445 ms | ~0.30 s | ~135 MB | **247 KiB / 2.2 MiB** |

The shape of that table is the finding: **rows barely matter** (10× rows ≈ 0.9×
the read; 50× rows ≈ 1.3×) because the cost is process startup, not the query.
Which is why the choice of read is made on *consistency* grounds and the interval
is made on the *fixed* ~0.5 s — and why the poll is a separate task: 131 MB of
transient peak RSS per read is someone else's memory, and it should not be paid
inside the frame.

**Deliberately not done:**

- **`--skip-labels`** — measured to buy nothing and to change the payload shape
  into one `BdList` reports as `Malformed`. Rejected on evidence.
- **No cap on Complete in the snapshot** — see §4. `--closed-after` is the
  recorded escape hatch at ~40 k issues, with the window in the header.
- **The band never writes.** Enforced by `--readonly` on the command line, not by
  the widget not having a `&mut`.
- **Unknown is not named.** F1.
- **The board does not show `pinned`'s pin, the deferred *until* date, priorities,
  labels, or blockers.** Each is a column-of-the-mind that costs a read field and a
  layout slot nobody asked for. The row is `id` + `title` + marker and that is all
  this ADR grants.

## Follow-ups this decision opens

- **F1 — `Unknown(String)`.** The marker renders `?` and the real status word is
  discarded by `#[serde(other)]`. On the first board that grows a custom status,
  the band shows `?` where it should show `review`. Changing `Unknown` to carry
  its raw string touches `as_str()`, `Copy` and every `match` in the crate; it is
  a real ticket and it is **not** a reason to leave `Unknown` off the board today.
- **F2 — `ready_with` has no `--limit 0`.** `bd ready` defaults to 100 and
  `bd.rs:ready_with` passes no limit, so the loop's own planner view silently
  truncates past 100 ready beads. The kanban board avoids the function entirely,
  which fixes nothing for the loop. looprs-037's rule should be carried across.
- **F3 — the footer's "Ns ago" needs the poller's clock in the snapshot**
  (`fetched_at`), which looprs-5o4.2 already names. Nothing here adds to it.

## What this forbids

1. **Do not drop a status.** Not `Unknown`, not `pinned`, not `deferred`, not a
   status a future `bd` invents. Every `BeadStatus` value maps to a visible
   outcome; the mapping function is total and its exhaustiveness is the test.
2. **Do not merge `deferred` into To-do.** It was taken out of the running by a
   human; a row puts it back.
3. **Do not draw a blank band.** No snapshot ⇒ `reading the board…`. An empty
   board ⇒ three zeros. They are different states and must not share a rendering.
4. **Do not run more than one `bd` read per tick to build one frame.** One read,
   one snapshot, one paint. Multi-query frames disagree with themselves and cost 3×.
5. **Do not use a truncated page.** `--limit 0` on any read that feeds a count or
   a `+N more`. A short read that looks like an empty read is the bug looprs-037
   was filed for.
6. **Do not render an error as an empty board, or clear the last good snapshot on
   error.** Errors keep the data, dim it, mark it stale, and carry their own words.
7. **Do not show a column count that is not the true total.** Truncation is the
   `+N more` marker's job and it counts only undrawn rows of that column.
8. **Do not source the board from the beads session's mirror.** `bd` is the board;
   the mirror is an opinion that dies with the generation.
9. **Do not re-derive `bd`'s ready/blocker semantics in the widget.** The board
   shows stored status; claimability stays in `bd`.
10. **Do not put `bd` in the draw path.** The frame reads a snapshot value and a
    row count; the poller, the `bd` service and the widget never meet at paint
    time.
11. **Do not dim the framing.** Dividers and rule keep one style whatever the read
    did. `dim` on this band means *"these rows are from the last good read"*;
    spending it on chrome too turns one legible fact into "some of this band is
    old", which is not a fact with a verb attached to it.
12. **Do not grant the rule row when the body cannot pay for it.** The framing is
    the first thing on the band to be given back, not the last. A band that drew a
    rule and one bead row where it could have drawn two bead rows has bought its own
    legibility with the user's content.
13. **Do not derive the framing's geometry independently of the columns.** The
    `┼` junctions come from the same layout split as the `│` they cross. Two
    functions each working out where the gutter is is how a junction ends up one
    cell off the line, which reads as a rendering bug nobody can reproduce from
    the code.
14. **Do not build the board out of journal records.** The journal is read for
    *whether*, and the rows come from one `bd --readonly list --all --limit 0
    --json`. A replayed picture would make what the band shows depend on
    retention floors, on which mutations this replica happened to see, and on
    records that arrive by merge and are not journaled at all — and it would
    break I1, because a set of replayed mutations is not one consistent read.
15. **Do not treat a probe that failed as a quiet board.** A journal that could
    not answer says nothing, and nothing is not "nothing changed". Same failure
    class as §4's "an error never renders as an empty board", one layer under
    it: the poller that swallowed the probe's error and kept its old rows would
    show `bd ok` over a board nobody had looked at.
16. **Do not advance the watermark from anything but a probe taken before a
    read that succeeded.** Both halves are load-bearing. Adopting a seq seen
    after the read loses any change that landed during it; adopting one after a
    *failed* read loses the change the read was going to bring back.
17. **Do not remove the sweep, or stretch it past a few minutes, without naming
    what it is being traded against.** The four cases in §7 are changes the band
    cannot see any other way. `LOOPRS_KANBAN_RECONCILE_MS` is allowed to be
    long; it is not allowed to be forgotten.
18. **Do not let the change detector change what the footer's age means.** It
    is the age of the **rows** — the last full read — not the age of the last
    probe. Making a probe refresh it would have the band claim its rows are
    three seconds old when they are forty.
