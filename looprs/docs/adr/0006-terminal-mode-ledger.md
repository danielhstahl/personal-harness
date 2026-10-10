# ADR-0006: The terminal mode ledger — every mode we switch on gets switched off, exactly once

- **ID:** looprs-pdl.3
- **Status:** Accepted — 2026-10-06
- **Epic:** looprs-pdl (Full-screen TUI: own the screen, the scrollback, the selection, the clipboard)
- **Decides for:** looprs-pdl.4 (the frame migration onto the alternate screen), looprs-pdl.8
  (mouse scroll), looprs-pdl.11 (bracketed paste), looprs-pdl.12 (full-screen children
  handed over and taken back), and every future `ESC[?…h` this app ever writes
- **Widens:** ADR-0005's "the presentation is resolved by exactly one component" from the
  bytes coming *in* to the modes going *out*: exactly one component switches a terminal mode
  off, and it is [`teardown::Ledger`]
- **Extends:** the exit contract of looprs-ecr (`teardown.rs`), which until now covered raw
  mode, the live pane and one newline

---

## Context

Entering the alternate screen is one escape sequence. Not leaving the user in it is the work.

looprs-ecr gave the app one teardown with a good rule — *the exit path never asks the
terminal a question* — and one good mechanism — an `AtomicBool` swap so that the run loop,
`main` and a panic hook can all claim to be the one that restores the terminal without
restoring it twice. That was the right shape for **raw mode and a pane**. It was already the
wrong shape for everything this epic is about:

* a leaked `?1002` is a terminal that reports drags forever;
* a leaked `?1049` is a scrollback the user cannot get back — the one loss in this program
  they cannot undo;
* a leaked `?25` is a cursor they have to `reset` to find;
* and a mode switched off **twice** is not harmless either: `?1049l` restores the screen
  contents and cursor the terminal *saved*, so a second one is the user's screen replaced
  with whatever the terminal happened to keep.

Meanwhile the app already had an unowned mode switch. ratatui hides the cursor on every frame
that reports no cursor position, and gives it back in `Terminal::drop` — a destructor that
switched a terminal mode *after* the teardown, outside any ledger, in an order nobody
controlled. That is where the `\x1b[?25h` used to land after the closing newline, which the
shutdown spike had to forgive. It also left two real holes:

1. **No signal handling at all.** `SIGTERM` and `SIGHUP` took the process down with the
   default disposition: raw mode on, cursor hidden, pane painted. Measured against the
   pre-ticket binary — after `SIGTERM` the tty is still raw (`spikes/results/shutdown-e2e-pdl3-control.log`).
2. **An early `?` out of setup skipped the teardown.** `main` built the live view *before*
   installing the net under itself, so a failing `LiveView::new` — which queries the cursor
   and can time out, and is therefore exactly the call that fails — returned with raw mode on
   and nothing left to turn it off.

Alt screen, mouse capture and bracketed paste all want *mode queries* (`DECRQM`) and all
three change what the terminal does with the next byte. The rule from looprs-ecr survives
this ticket unchanged and gets harder to hold.

## Decision

**`src/teardown.rs` gains a ledger: `Mode` says what a mode is and what its two byte strings
are; `Ledger` records what this process switched on and is the only thing allowed to switch
any of it off.** `Teardown` is now anchor + ledger + closing newline, with the same
idempotent, total, non-propagating contract it already had.

Four rules, and they are the whole design:

1. **It only unsets what it set.** A leave sequence for a mode we never entered is not free
   (see the `?1049l` above). "Every mode we switch on gets switched off" is not a licence
   to emit every off sequence we know.
2. **It never asks the terminal what state it is in.** Every mode here *can* be queried —
   `CSI ? <mode> $ p` for the DEC private modes — and none of them are. The reason is
   looprs-ecr's: a query at exit is a round trip on a stdin whose async reader is being
   torn down, and the exit path does not get to be uncertain. The ledger's own record **is**
   the state. This is why `restore` no longer calls `is_raw_mode_enabled()` either: it is a
   `tcgetattr`, and the record answers it better.
3. **Exactly one leave per mode we hold, whatever else has touched it.** The off bytes go out
   unconditionally. A mode the terminal already dropped is switched off again — one byte
   string, cannot be wrong. A mode skipped because "it was probably already off" is a leak.
4. **The last thing changed is the first thing given back.** Modes come off in the reverse of
   [`Mode::BOOT_ORDER`], so each leave sequence lands on the screen its `h` left the
   terminal on, and raw mode — the first thing set — is the last thing dropped, which is
   what makes the closing newline still be written through a cooked tty.

### What the ledger holds

`Mode` is the whole table, spelled once:

| Mode | on | off | notes |
| --- | --- | --- | --- |
| `Raw` | `tcsetattr` | `tcsetattr` | not a byte string, so the ledger knows how to set this one itself |
| `AltScreen` | `?1049h` | `?1049l` | `?1047`/`?47` are the same fact and are tracked as one, as `screen.rs` already treats them; the app switches it as `?1049` because that is the spelling that saves the **cursor** along with the screen, which the hand-back depends on |
| `CursorHidden` | `?25l` | `?25h` | the app hides the cursor for its own frame and the user needs it back |
| `MouseReport` | `?1000h` | `?1000l` | |
| `MouseDrag` | `?1002h` | `?1002l` | its own entry, because a leaked 1002 is the "draws buttons forever" bug |
| `MouseSgr` | `?1006h` | `?1006l` | coordinate encoding |
| `BracketedPaste` | `?2004h` | `?2004l` | what looprs-pdl.11 needs on |

`Mode::DEFAULT` is `[raw, cursor_hidden]` — what this app switches on today. `LOOPRS_MODES`
**adds** to it (`LOOPRS_MODES=all` turns the whole table on), so the alternate screen and
the mouse can be proved on a real pty before looprs-pdl.4 makes them the real thing. The
list is parsed before any of it is applied, and an unknown name is an error that names the
known names: a typo in a mode list that silently means "nothing" is a proof that passed with
the thing unproven.

### The alternate screen changes the shape of the hand-back

`?1049h` parks the main screen **and the cursor with it**; `?1049l` puts both back exactly
as they were. So while the alternate screen is held there is nothing above the pane to erase
*over* — the pane is not sharing the screen with the user's text — and no line of theirs to
close afterwards; a newline there would scroll a prompt that is already where it belongs.
`restore` therefore asks its own ledger one question, `is_on(AltScreen)`, and writes the
erase and the closing newline only on the inline path. On the alternate-screen path the last
byte the app writes is the leave sequence itself.

**This is why `LiveAnchor` stays.** The ticket asked whether the anchor can come out of the
teardown contract now that the alternate screen needs none of it. It cannot, yet: the ledger
has to serve both paths, and until the frame migration (looprs-pdl.4) lands there is still an
inline pane whose top row the exit path must know in order to erase without guessing. The
deletion is a `pdl.4` change with a real owner, not a leftover of this one.

**Landed (`looprs-pdl.4`): it left.** The frame took the alternate screen, so there is no
pane and no top row to know. `Teardown::new()` takes no anchor, `restore_bytes` is gone, and
the erase branch of `restore` went with them: the hand-back is the leave. The one question the
exit path still asks its own ledger is `is_on(AltScreen)` — not to find a row to erase, but to
know that no closing newline is owed. See ADR-0004, *Landed — pdl.4*.

### The screen the app did not switch on

The ledger answers "what did **we** switch on", and that is the right question for the app's
own modes. It is the wrong question for `vim`.

A full-screen child in Bash mode is *passed through*: `src/screen.rs` tees the child's bytes
to the real terminal verbatim, so the `?1049h` that took the user's screen went out on **the
app's own stdout**, written by the app on the child's behalf. The app did not "switch on the
alternate screen" in the sense its ledger means — and yet the user is standing in a screen they
cannot get out of, and the only thing left holding a write handle to that terminal when the app
goes down is the app.

This hole survived three tickets that touched the exit path, because the leave was being
*emitted* the whole time and nobody could see that it never *arrived*. Quit with a full-screen
child up and the capture ends `?25h\r\n` with **no `?1049l` anywhere in the run** (reproduced
against the pre-ticket binary: `alt_screen=1` at the end, 0 leaves). `ScreenWatch` knows about
the debt, and `BashTask::release_screen_at_command_end` dutifully emits it at the start of
`shutdown()`. What happens next is a pile-up of individually reasonable parts:

1. the leave is queued into the session's unbounded event channel;
2. the Bash task then blocks for the whole 2 s `KILL_WAIT` on a shell whose foreground is
   `vim` — `std::thread::sleep`, inside an async task, which parks a runtime worker;
3. the router's shutdown grace expires alongside it and `abort()`s the pump
   (`router: "Bash#3 did not report its exit within 2s; cutting its pump"`);
4. the bytes die in the queue.

**Decision: the debt is recorded from the bytes that reached the real terminal, and paid by the
teardown.** `screen::ScreenDebt` sits on the tee. An alt-screen `h` going *out* makes the app
answerable for the matching `l`; seeing the child's own `l` go out through the same pipe
discharges it. `Teardown::restore` reads it first, before the erase, and writes the leave
itself if it is still owed — first, because every byte addressed to the user's own screen is
meaningless while the app is still standing inside somebody else's. Then it does the ordinary
inline hand-back: erase the pane from the published anchor, unwind the ledger, one closing
newline. Coming back from a child's screen ends in exactly the hand-back an ordinary inline
quit ends in, which is the property the spike checks as "the tail after the leave".

Three things this keeps on purpose:

* **The tee is the report.** `note_tee` is called on the same bytes as the write, not from the
  session's `ScreenHeld` event, because "did the real terminal get switched?" and "who gets
  the next frame?" are different questions. Bytes that were never teed — the user tabbed away,
  so the paint went to the transcript — switched nothing and must not be paid for.
* **The code that went in is the code that comes out.** `?1047h` is not undone by `?1049l`, so
  the debt remembers which of 1049 / 1047 / 47 was switched and answers it in kind
  (`alt_leave`).
* **Only an explicit switch counts.** A takeover the watcher *inferred* (cursor addressing with
  no linefeeds) switched no mode at all, so it owes no leave. Manufacturing a `?1049l` for it
  is exactly the class of byte this exit path is forbidden to emit.

It changes nothing about the ledger's three rules; it adds a second, smaller class of promise
next to them: *a mode we passed through and never saw left is ours, because we are the only one
left who can leave it.*

And one thing it deliberately does **not** do, so that it is written down instead of
rediscovered: a real `vim` also leaves `?2004h` (bracketed paste) and, with a mouse-tracking
vim, `?1000h`/`?1002h`/`?1006h` switched on in the user's terminal. Those are **not** paid
back. The asymmetry is not laziness: the alternate screen has a *defined* thing to return to
(the main screen, cursor included), while `?1002h` has to return to whatever the user's
terminal was doing before looprs started — which the exit path does not know and is forbidden
to ask. The honest shape of that fix is to read those modes at **startup**, where one
`DECRQM` round trip is paid at boot instead of on the way out through a stdin that is being
torn down, and restore what was read. Bigger than this ticket; named in `docs/testing.md` and
printed by the spike's `child` scenario as `tee'd modes still on at exit` so it stays visible
rather than theoretical.

### Three supporting decisions

**The live view is held in a `ManuallyDrop` (`main.rs`).** ratatui's `Terminal::drop` writes
`?25h`. That is a mode switch outside the ledger — after the teardown, unordered, and
impossible to make once-only against the ledger's own write. So the view's destructor is never
run: the cursor comes back from the ledger instead, in the ledger's order, exactly once, on
every path including the panic. What is not freed is the view's two cell buffers, in a
process that is on its way out. The alternative — letting a destructor keep a veto over when
the user's cursor returns — is the thing this ticket is about.

**A signal is answered with the same call the quit key makes.** `signals.rs` installs
handlers for `SIGTERM`, `SIGHUP` and `SIGINT` before the run loop starts, and the run loop
selects on them into the same `should_quit` that Ctrl-Q sets — one exit path, six steps, one
set of promises. There is deliberately no second shutdown path written for signals, because a
second shutdown path is a second set of promises about the terminal and those two disagree.
The handlers are tokio's, delivered through the run loop's own `select!` rather than a
`sigwait` thread: it keeps the answer inside the runtime that owns the app's state, and the
loop never blocks on anything unbounded. A process wedged inside a synchronous write is not
saved by a signal handler either.

**The fault injector is debug-only.** `LOOPRS_PANIC=draw` makes the frame panic on purpose,
because "a forced panic inside the draw" is in the proof list and a panic nobody can cause is
a proof nobody can run. `#[cfg(debug_assertions)]` keeps a shipped binary from turning an
environment variable into a panic.

## Consequences

* Modes are now a *set with an order* owned by one object. Adding a mode is adding a variant
  to `Mode`, and it cannot be added half-heartedly: the on bytes, the off bytes, the label
  and its place in `BOOT_ORDER` all have to be written down before it can be switched on at
  all, and it is handed back on every exit path from the day it is switched on.
* `pdl.8` (mouse) and `pdl.11` (bracketed paste) inherit their teardown. They switch modes
  on through `Teardown::enable` and get "the user's terminal is theirs again" for free, in
  every path — including the ones they have not thought of yet.
* `pdl.12` gets `Ledger::release`, the mid-run hand-back: a mode can be given up before the
  exit rather than only at it. Kept and tested now, so the handover ticket does not have to
  invent it under pressure.
* `pdl.4` gets the hard part already decided: switching the app onto the alternate screen is
  `Mode::AltScreen` in the default set, and the erase + newline fall out of the same
  `is_on(AltScreen)` question.
* **`screen::tee` has a second job now: it is the report.** Anyone changing the passthrough
  has to keep `ScreenDebt::note_tee` reading the same bytes the tee writes, or the exit path
  goes blind to the screen it is being asked to hand back. The two calls sit next to each
  other in `App::update` for that reason, and
  `a_screen_switch_that_was_never_teed_owes_nothing` is the test that says why the debt lives
  with the write rather than with the session's `ScreenHeld` event.
* The startup failure mode got better: the ledger and the panic hook exist before the first
  mode is switched on, and `main`'s tail is one unconditional `restore` under everything the
  app does. An early `?` from the setup now lands on the hand-back instead of leaving raw
  mode on.

## Proof

Unit tests — `cargo test` — 36 of them added in this ticket (21 in `teardown.rs`, 8 in
`screen.rs`, 3 in the new `signals.rs`, 2 in `app.rs`, 2 in `session/bash.rs`) on top of the
ones looprs-ecr left (440 passing, 3 ignored). The interesting ones are byte-level, because
the bytes are the contract:

* `every_mode_names_its_own_pair_of_bytes` — the wire format is pinned, not trusted to a table read;
* `the_ledger_hands_back_in_the_reverse_of_the_order_it_went_on`;
* `a_mode_we_never_switched_on_is_never_left_off` — the rule that keeps `?1049l` honest;
* `a_switch_that_never_reached_the_terminal_is_still_a_mode_we_hold` — the entry goes in
  before the write, because a partially written `?1049h` still switched the screen;
* `holding_the_alternate_screen_means_no_erase_and_no_closing_newline` — and the leave is the
  last byte;
* `restore_hands_every_mode_back_once_and_nothing_after_it`, `an_unknown_mode_is_an_error_that_names_the_names`;
* `a_screen_a_dying_child_left_behind_is_left_on_the_way_out` — the inherited screen, paid by
  the teardown, with the whole tail spelled out: leave, erase, ledger, newline;
* `a_screen_the_child_already_left_is_not_left_again` and
  `our_own_alternate_screen_already_covers_the_childs` — the same exactly-once rule across both
  classes of promise, so `vim` quitting politely does not get left twice;
* `the_leave_is_spelled_the_way_the_entry_was`, `an_inferred_takeover_is_not_an_alt_screen`,
  `a_screen_that_was_only_guessed_at_owes_no_leave` — 1047 goes back as 1047, and a takeover
  nobody switched owes nothing;
* `a_teed_alt_screen_is_a_screen_we_owe_the_terminal_back` and
  `a_screen_switch_that_was_never_teed_owes_nothing` — the debt is a fact about the bytes that
  reached the terminal, not about who was drawing;
* `quitting_while_a_full_screen_program_holds_the_screen_leaves_the_alt_screen`
  (`session/bash.rs`) — the command-boundary half of the same debt, which is the half that was
  already being emitted and never verified to arrive;
* `a_signal_sent_to_this_process_is_received` — the handler is wired to what the loop awaits.

The pty proof extends `spikes/shutdown_e2e.py` rather than adding a file, as the ticket
asked. It ran 8 scenarios and 149 checks at `pdl.3`
(`spikes/results/shutdown-e2e-pdl3.log`); the same spike on the tree this site now
documents prints 189 (`spikes/results/shutdown-e2e-00u24.log`), and the spike **keeps its
own ledger** of the
bytes on the wire (`ModeTrace`): the app's ledger and the driver's ledger are independent,
which is the only way "we handed it all back" can be a check rather than a claim. There is
no terminal emulator behind a pty anyway, so a real `DECRQM` would go unanswered — what is
on the wire is the whole truth there is.

| Scenario | What it asks | Result |
| --- | --- | --- |
| quit mid-stream (inline, default modes) | no `?1049l`/mouse/paste leave for a mode nothing switched on; the ledger's bytes come *before* the closing newline; cursor visible at the end | pass |
| `LOOPRS_MODES=all`, Ctrl-Q | alt screen, three mouse modes, bracketed paste, hidden cursor: all taken, all left **exactly once**, and the `?1049l` is the last thing the app wrote | pass |
| `SIGTERM` with the whole set held | the app leaves by itself in 0.31 s, code 0, every mode handed back, no `ESC[6n` after the hand-back, tty cooked | pass |
| `SIGHUP` on the plain inline run | same | pass |
| `LOOPRS_PANIC=draw` | the ledger still leaves each mode exactly once through a panic; exit code 101; tty cooked | pass |
| full-screen child killed while it holds the screen | the user is **not** in the alternate screen afterwards; the `?1049l` is there exactly once; the tail after the leave is the ordinary inline hand-back (erase → ledger bytes → one newline); no `ESC[6n` after the hand-back; tty cooked | pass |

**The control.** The same spike against the pre-ticket binary
(`spikes/results/shutdown-e2e-pdl3-control.log`) scores **85/112**, and the 27 failures are
the ticket: the alternate screen is never taken (no ledger to take it), `SIGTERM` and
`SIGHUP` leave the **tty raw**, the old binary's `\x1b[?25h` still lands after the closing
newline, and — the half added last — a run that quits with a full-screen child holding the
screen ends with `alt_screen=1` and **zero `?1049l` bytes in the whole capture**, which is the
user stranded inside a dead `vim`. A passing check that a broken build also passes is worth
nothing, so the control is part of the answer, not a footnote. (The control's check count is
lower than 149 because scenarios that cannot start without `LOOPRS_MODES` bail early.)

```sh
cargo build
python3 spikes/shutdown_e2e.py | tee spikes/results/shutdown-e2e-pdl3.log

# the control
git worktree add --detach /tmp/looprs-pdl3-ctrl HEAD
(cd /tmp/looprs-pdl3-ctrl/pi-beads/looprs && cargo build --target-dir /tmp/base-target)
LOOPRS_BIN=/tmp/base-target/debug/looprs python3 spikes/shutdown_e2e.py \
    | tee spikes/results/shutdown-e2e-pdl3-control.log
```

### "No child may survive" is a claim about *this run's* children (`looprs-00u.24`)

The looprs-ecr rule quoted above — nothing the app spawned may outlive it — was checked with
`pgrep -f`, which scans every process on the box. That makes the check's subject the machine
rather than the build under test. Measured on one machine, same binary: **189/189** in a clean
session (`spikes/results/shutdown-e2e-00u24.log`) and **168/171** for the pre-`00u.24` spike
on the same box deliberately dirtied with abandoned shells, every failure on the leak checks
(`spikes/results/shutdown-e2e-00u24-globalpgrep-control.log`) — and the mirrored defect is
worse still, because a leak the build *did* cause hides in that same pile and nothing about the
result says which pile it came from.

The rule is unchanged and no check was deleted; what changed is what they look at. The spike now
records the pids under the app **while the app is alive** — after it dies its children are
reparented to pid 1 and no later scan can give them back — and asserts on those. Earlier runs'
debt gets reaped rather than judged: a shell carrying a generated `looprs-bash-integration-<pid>-<n>.sh`
marker whose owner pid is gone, plus its subtree and its rc file, is orphaned by the arithmetic
in its own name, so `reap_stale_leftovers` clears it and the run *prints the count* instead of
failing for it. Liveness of that owner pid is the only test, so a running looprs — the user's
own, or another window's spike — is never touched. And because a check scoped to a recorded set
fails silently when the recording breaks, `leak` in the spike SIGKILLs the app over a live shell
and requires the scoped check to name the pid it recorded before reaping its own mess.
Written up in [the testing guide](../testing.md).

### Rest of the suite

Against the new binary: `viewport_e2e` 16/16, `status_e2e` 20/20, `cancel_e2e` 19/19,
`bash_e2e` 20/20, `cargo test` green (440 tests, 3 ignored).

`fullscreen_e2e` is **18/21**<!-- spike-count: unverified 18/21 fullscreen_e2e — this ADR's run was never captured, and the spike has since been rewritten to 70/70 (spikes/results/fullscreen-e2e.log) --> — and 18/21 against the pre-ticket binary too, the same three
vim-keystroke needles. Nothing in this ticket touches the keystroke path; the numbers are quoted
side by side rather than rounded away.

`flash_e2e` is **0/3** (`spikes/results/flash-e2e-control.log` prints 0/3; the specific run
this paragraph timed was never captured, which is why its millisecond figures cannot be checked
against a log) — 25 reshapes where ≤19.5 are allowed, worst hole ~3.6 ms against a
1.5 ms budget. That is **not this ticket**: it fails with identical numbers against the
pre-ticket binary (`LOOPRS_BIN=/tmp/base-target/debug/looprs python3 spikes/flash_e2e.py`
→ 25 reshapes, 3.54 ms), so it regressed after the flash fix and before this branch, and
this ticket changes nothing on the frame path. Named here rather than silenced, per
`docs/testing.md`'s rule about the one exception being written down.

## Costs, and what is deliberately not done here

* **A tee'd child's mouse and bracketed-paste switches are left where they landed.** The
  alternate screen is paid back; `?1000`/`?1002`/`?1006`/`?2004` that a `vim` set on the far
  side of the passthrough are not, because restoring them means knowing what the user's terminal
  was doing before this process started and the exit path is forbidden from asking. The right
  shape of that fix is a `DECRQM` sweep at **startup** — one round trip at boot, where a round
  trip is cheap — and handing those back from that record. `child` in the spike prints what is
  left behind (`tee'd modes still on at exit: [...]`) so the gap stays a number instead of an
  argument.
* **The `KILL_WAIT` block is still a block.** `BashTask::shutdown` waits up to 2 s in
  `std::thread::sleep` inside an async task, which parks a runtime worker and starves the rest
  of the app for the length of the wait — that is what made the queued leave unreachable in the
  first place. The screen debt no longer depends on that pipe, so the shutdown does not have to
  win that race; but the block itself is still there and worth unwinding on its own ticket.
* **The `ManuallyDrop` leak.** Two cell buffers of a process that is exiting, once. Taken
  deliberately: the alternative is a destructor that switches a terminal mode behind the
  ledger's back.
* **`LOOPRS_MODES` is a knob with no user-facing story.** It is the seam that let
  `pdl.4`'s modes be proved before they were the default. `pdl.4` has made the default set
  the real one (`raw`, `alt_screen`, `cursor_hidden`), so what the knob adds now is the rest
  of the set — the mouse modes and bracketed paste the shutdown spike runs under
  `LOOPRS_MODES=all`. Its reason for existing is thinner than it was and should be revisited
  rather than accreted.
* **Alt screen with the frame.** Until `pdl.4` this was the odd shape: an *inline* viewport
  drawn inside the alternate screen, no scrollback above the pane, and a height policy
  negotiating with a screen it did not own. `pdl.4` ended it — the frame is the alternate
  screen now, and the bands are sized against the window rather than against room.
* **Not covered:** `SIGKILL`/`SIGSTOP` (uncatchable — the ledger's bytes went out with the
  last flush before it, which is all there is), and `SIGQUIT` (a core-dump request; a
   program that turns it into a tidy shutdown is a program that cannot be asked for a
   backtrace).
* **Deleted by `pdl.4`:** `LiveAnchor`, `restore_bytes`, and the inline erase/newline path
  they served. The exit path's hand-back is the alternate-screen leave; `restore` no longer
  needs to know where anything was.
