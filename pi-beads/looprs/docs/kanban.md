# The kanban band

A three-column board — **To-do / In progress / Complete** — drawn inside the app, directly
above the status row, refreshed on its own schedule. It exists to turn "what is the state of
the board right now?" into a glance instead of a transcript read or a second terminal running
`bd list`.

* **Where it is.** The fifth band of the full-screen frame, immediately above the status row
  (`viewport::frame_areas`).
* **When it is there.** Only while the **Beads** mode is what is on screen. In Bash and in Pi
  the band is **zero rows** — not hidden, not blank, *zero* — so the frame in those modes is
  exactly the frame it was before the board existed.
* **It is read-only.** Nothing on this path changes a bead. The read is issued as
  `bd --readonly list --all --limit 0 --json`, so "the board cannot change the board" is a
  property of the command line rather than of the widget not having a `&mut`.
* **It has its own task.** The board is polled in a long-lived tokio task that outlives every
  session: it keeps working while the beads loop is idle, parked on another tab, or being
  respawned, and it never sits on the session's hot path.

The decision behind all of this — the status mapping and its rationale, the freshness states,
why one read per tick, what the read costs measured — is
[ADR-0007](adr/0007-kanban-board.md). This page is the manual; the ADR is the argument.

---

## What the three columns show

One rule covers the whole board: **every bead in one read is counted exactly once, in exactly
one visible place.** Nothing is dropped and nothing is counted twice.

The authoritative `bd` status → column mapping, with a rationale per status, is the table in
[ADR-0007 §1](adr/0007-kanban-board.md#1-the-status--column-mapping). It lives in that one
file deliberately: copied into several places it would drift from the decision it records. If
the list below and that table ever disagree, the ADR is right and this page is the bug.

What you see on screen:

* **To-do** — unclaimed work: `open`, plus `ready` where a board stores it. Two kinds of row
  there arrive **marked**, because the column alone would mis-say them:
  * `⊘` — `blocked`. Not work the loop may take, but the most actionable thing on the board
    for the person reading it, so it sits where the eye goes, marked so it cannot be mistaken
    for pickable work.
  * `?` — a status this build cannot classify: `pinned`, `hooked`, or any repo-defined custom
    status (`bd config set status.custom review,verified`). Unrecognised statuses are made
    **visible** rather than dropped; a mapping that dropped them is how a board starts lying
    about being empty.
* **In progress** — `in_progress`, and only `in_progress`. The one stored status that means
  "a worker has this".
* **Complete** — `closed` (what `bd close` actually writes) and `done`.
* **`deferred` is never a row.** A human deliberately took it out of the running; putting it
  in a column would put it back in play visually. It reaches the eye as the
  `⏸ N deferred` count in the footer, which is its one visible place.

Two things follow from that and are worth knowing before you read a number:

* **A column header number is the true total for that column**, taken from the read — never the
  number of rows this frame happened to have room to draw.
* **A row is `id`, `title`, and a marker.** No priority, labels, blockers, pin or due date.
  The id is the handle you type into `bd show`; everything else is a keystroke away.

Order inside a column is `bd list`'s own order, which measured out priority-sorted, so To-do
reads in priority order. The board does **not** re-derive `bd`'s claimability rules
(`bd ready`) — claimability stays in `bd` (ADR-0007 §2).

## Height, and what `+N more` means

The band is `header (1) + rule (0 or 1) + body + footer (1)`. Body rows are shared by the
three columns, so an 8-row band is 5 bead rows *per column* (`8 − header − rule − footer`).

The **rule** — the horizontal line under the header, crossed by the two column dividers — is
the only row of the four that is conditional, and it is conditional in one direction only: it
is granted when the body would still keep 2 bead rows after paying for it, so a 3- or 4-row
band draws no rule at all. The framing is therefore the *first* thing on the band to be given
back and the *last* thing to cost you a bead. [The framing](#the-framing) has the rules.

How tall it gets is a function of the window, and the band is the **last** thing on the frame's
ladder: it is paid for out of the transcript band's *surplus* — the rows above the
transcript's floor that nothing else claimed. It can never take a row off the input box, the
tool cards, or the transcript's floor, no matter what it is showing.

* **Too short ⇒ no band.** Below the height where the rest of the chrome plus a 3-row board
  fits, the band is `0`, not a header and a footer with nothing between them.
* **Ceiling of 8 rows** (`MAX_KANBAN_ROWS`), however tall the window is. Past that, extra
  window goes to the transcript, which is the band you cannot re-read elsewhere.
* **`+N more`** at the bottom of a column counts **only the rows that column did not draw**,
  and it adds up: 3 rows drawn plus `+7 more` is a column of 10. It is a count, not a pager —
  there is no per-column scrolling. Overflow is counted, never paged.

So a short window buys fewer visible beads and *says so*, per column, rather than lying by
omission.

### A narrow terminal

The band degrades in a fixed order, so that what survives is the thing you can act on:

* the header drops the column **name** before it drops the **count** — a label that stopped
  labelling is worth less than the number nobody else on screen can tell you. Below the width
  the count itself needs, the cell is left **empty** rather than showing a cut number: a
  truncated `12…` is not a smaller number, it is a wrong one;
* a row drops the **title** before it drops the **id** — the id is the handle you type into
  `bd show`, and the title is one keystroke away from there;
* a **marker is never cut**. It is laid out before the title is truncated, precisely so a
  `blocked` bead can never end up looking exactly like pickable work in a narrow column;
* the **dividers are drawn inside the gutters and never take a column's width** — a gutter the
  layout has squeezed to nothing gets no line, and the columns keep every column they were
  given. The rule's `┼` junctions are computed from the *same* layout split as the `│` above
  and below them, so a junction cannot end up one cell away from the line it is supposed to
  cross.

### The framing

Two kinds of line, one style (`board_divider`, the same dark gray as the footer text):

* **dividers** (`│`) down the middle of each gutter, spanning the header rows and the body rows
   — not the footer, which is one sentence across the whole width and has nothing to divide;
* the **rule** (`───┼───┼───`) between the header and the body, because the header and the
   bead rows are the only two things on the band that mean different categories of thing, and
   bold on the header alone did not say so: the header read as a fourth bead row that happened
   to have no id in it.

Two rules hold it together, and both are tested rather than asserted in prose:

* **It is bought out of the body's surplus, and refused before it cuts a bead.** The framing is
  the cheapest thing on the band to lose: a band that cannot keep 2 body rows after paying for
  the rule draws none of it (`MIN_KANBAN_BODY_ROWS_WITH_RULE`), which is why 3- and 4-row bands
  have no rule and 5-row-and-up bands do.
* **The framing is never dimmed.** Its colour never changes with the read, so `dim` on this band
  keeps meaning exactly one thing — *these rows are from the last good read* — instead of
  "some of this band is old", which is not a fact anybody can act on.

## The knobs

Every `LOOPRS_*` variable the board has, with its default:

| variable | default | what it does |
| --- | --- | --- |
| `LOOPRS_KANBAN` | *(unset — the board is **on**)* | `0` / `off` / `no` / `false` takes the board off entirely: no poller task, no `bd` read, no band. Anything else — including nonsense — leaves it **on**, so a typo in a variable you did not mean to set cannot take a feature away. |
| `LOOPRS_KANBAN_ROWS` | *(unset — the height function decides)* | A fixed band height: `3`–`8`. `0` = off. `1` or `2` = off, **with a warning** (header + footer leaves no row for a bead, so there is no board to be had). Above `8` is clamped to `8`, with a warning. A pin is a **ceiling on the request**, never a guarantee: a window that cannot hold it gets no band rather than a stub. |
| `LOOPRS_KANBAN_POLL_MS` | `5000` | The read interval. Values below the `250` ms floor are clamped up with a warning; `0` is refused with a warning pointing at `LOOPRS_KANBAN=0` (a zero interval means "`bd` back-to-back against a database forever"). Unparseable falls back to 5 s with a warning. |
| `LOOPRS_BD_BIN` | `bd` | Which `bd` binary reads the board — deliberately the same binary the beads loop uses, because two binaries in one run means two boards. |

Nothing here is re-read per frame: the environment is read **once at startup**, resolved into
plain values, and logged. Every resolved choice is announced — `grep 'kanban board' looprs.log`
(in the directory `looprs` was started from) shows what binary is being read, how often, and
whether the board is on at all. Warnings on that line are the story of a knob that was not
taken literally; every fallback is loud, because a silently ignored knob is a knob you will
keep turning.

## Reading the footer

The footer row is never omitted while the band is drawn: "when was this read?" is always
informative, and it is where the states that are not rows live.

| footer | what it means | what to do |
| --- | --- | --- |
| `bd ok · 3s ago` | the last read answered, 3 s ago | nothing — this is normal. The age advances until the next read replaces it |
| `bd ok · 3s ago · ⏸ 2 deferred` | as above, plus 2 beads are `deferred` (deliberately not rows) | nothing |
| `reading the board…` | the first read has not landed yet | wait one poll interval. If it stays here, the poller never got a result — check the log |
| `bd unavailable: <reason>` | the binary could not be run at all (`command not found`, `exec` failure) | check `LOOPRS_BD_BIN` and that `bd` is on `PATH` |
| `bd failed (exit N): <first line of stderr>` | `bd` ran and exited non-zero | read `bd`'s own words — board/DB/auth problem |
| `bd answered unreadably — see the log` | `bd` exited 0 with a payload this build cannot parse | `grep board looprs.log` for the payload error; a `bd`/schema mismatch is the usual cause |
| `bd did not answer in 30s` | the read hit `BD_TIMEOUT` | the board or its database is wedged; the next tick tries again |
| `… · stale 42s` (on any of the last four) | **the rows still on screen are from the last good read**, and the most recent read did not answer. The rows are dimmed; the footer's head names the reason | fix the reason in the head of the line. Nothing needs restarting: the next successful read repaints live and the `stale` marker goes away by itself |

**`stale` never means "empty".** That conflation — "bd is broken" read as "bd has no beads" —
is the failure class this app refuses (looprs-037), and the band is where it had to be killed.
A failed read **keeps** the last good rows, dims them, marks them stale, and carries the
error's own words. An empty board, a not-yet-read board, a stale board and a board with no
band at all are four different paintings.

Two details of the same rule are worth noticing before you misread a row:

* **When the last read did not answer, the column counts read `—`, not a number.** Nothing new
  was counted, so nothing is claimed. A `0` in a header means `bd` said zero, never "we don't
  know".
* **No `stale` marker and no rows at all** means the very first read failed — there is no last
  good snapshot behind the band to be stale. The footer's head still names the reason.

## Four questions, answered from this page

**Why does the board not show in Bash (or in Pi)?**
Because the gate is the *displayed* mode, not "a beads session exists". The band is a view of
the beads board; while you are reading Bash or Pi it is granted `KanbanBudget::Off`, which is
`0` rows (`App::kanban_budget`). The poller keeps running underneath either way, so switching
back to Beads shows a board that was never out of date.

**Where did my deferred tickets go?**
Into the footer: `· ⏸ 2 deferred`. Deliberately a count and not a row — a human took them out
of the running, and a row puts them back in visually (ADR-0007 §1, forbidden-list rule 2).
`bd list --status deferred` is where the rows themselves are.

**Why does the board say `stale`?**
Because the last read failed while an earlier one succeeded. The rows you can see are that
earlier, good read, dimmed; the head of the footer says what went wrong this time. The board
is not cleared and will come back on its own — every tick retries, and the first success clears
the marker without a restart. If the head says `bd unavailable`, that is a missing/unrunnable
binary, not a board problem.

**How do I turn it off?**
`LOOPRS_KANBAN=0` — no board, no poller task, no `bd` read. `LOOPRS_KANBAN_ROWS=0` does the
same thing through the height knob. To keep it and just make it smaller, `LOOPRS_KANBAN_ROWS=3`
is the smallest band that is still a board.

## For contributors

* **The frame.** `src/viewport.rs` owns the five bands, the ladder that pays for them, and the
  "Adding a band" checklist. The board's row budget is `viewport::kanban_rows`; its floor is
  3 rows (header + one bead + footer) and its ceiling is 8.
* **The poller.** `src/services/board_poller.rs` owns the schedule, the read and the
  latest-wins publish. Its contract — three rules a later editor is most likely to break — is
  written at the top of that file.
* **The mapping.** `src/state/board.rs` owns the status → column function and the snapshot.
  The mapping itself is decided in ADR-0007 §1; this is its rendering.
* **The widget.** `src/components/kanban.rs` is a pure function of *snapshot + area*: no
  clock, no environment, no `bd` call in the paint path (ADR-0007 rule 10).
* **Purity rules that keep it testable.** The App reads no environment and no clock; knobs are
  resolved in `main` and handed in as values, so every branch above is reachable from a test
  without mutating a process or waiting on a timer.

## See also

* [ADR-0007 — What the kanban band shows](adr/0007-kanban-board.md) — the mapping table,
  the freshness state table, the measured cost of the read, and the ten things the band is
  forbidden to do.
* [docs/testing.md](testing.md) — how the board's tests run with a fake `bd` and no network.
* [ADR-0004 — Full-screen TUI](adr/0004-fullscreen-tui.md) — the frame the band sits in, and
  what a band costs.
