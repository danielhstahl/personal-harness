# Testing `looprs`

One entry point, no network, no model calls, no real `bd` database.

```bash
cargo test              # the behaviour: 314 tests, ~20s
./scripts/check.sh      # the gate + the behaviour: fmt, clippy -D warnings, tests
cargo test -- --ignored # the two static gate checks, from inside cargo
```

Everything below explains how those two commands manage to test a stateful,
multi-process, three-backend TUI without a model, a board, or a terminal.

---

## The gate

`./scripts/check.sh` runs, in that order:

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
4. `cargo clippy --all-targets --features notify -- -D warnings` **and**
   `cargo test --features notify` — the *other* build. The shipped default carries
   no `notify` feature (`[features]` in `Cargo.toml`); without this step the ntfy
   half would be compiled only by whoever last touched it. The seam is designed not
   to change shape between the two, and running both is what makes that a checked
   claim rather than a design intention
5. `scripts/dead_audit.py --gate` — every `#[allow(dead_code)]` still covers dead
   code
6. `scripts/dep_audit.py --gate` — every `[dependencies]` entry in `Cargo.toml` is
   named in `src/`, or answered at its own line with a `dep-audit: <reason>`
   comment. The counterpart of step 5 for the other half of the tree: an
   unjustified allow in the source and an unjustified dependency in the manifest
   are the same failure, and the blanket that used to cover the second one
   (`unused_dependencies = "allow"`) is what this replaced
7. `scripts/docs_check.sh` — the docs rot gate: no dead internal link, no page
   outside the site tree, every `LOOPRS_*` the code reads is in the
   [configuration reference](guide/configuration.md), no two pages state a
   different default for one knob, the
   [keymap](guide/keymap.md) tables still match `CHORD_TABLE`, the
   [wire protocol](guide/wire-protocol.md) tables still match `WIRE_INVENTORY`
   and no `#[allow(dead_code)]` in `src/wire.rs` is missing the reason it
   carries, every spike check count written into prose matches a committed
   capture in `spikes/results/` (looprs-00u.23), and every test name a page
   quotes in backticks answers to a real `fn` of that name in the tree
   (looprs-00u.25)

The same gate is declared three ways, because each one catches a different kind of
person:

| where | what it guarantees |
| --- | --- |
| `Cargo.toml` `[lints.clippy] all = "deny"` | `cargo clippy` alone is a gate; nobody gets to "forget" `-D warnings` |
| `scripts/check.sh` | one command that means "this branch is mergeable" |
| `tests/warning_gate.rs` (`--ignored`) | reachable from `cargo test` without leaving the cargo mindset |

The docs step is new with the docs site and follows the same shape on purpose: the
half a prose review catches is not checkable, but a dead link, an undocumented knob
and a drifted table all are, and a docs rule that is not checked has a half-life of
about a month. `./scripts/docs_check.sh` runs in under a second, needs no network
and no cargo, and its five checks are described in
[ADR-0008](adr/0008-docs-site.md) and in
[the contributor guide](guide/contributing.md). The generator itself is
`./scripts/docs.sh` (`build` / `serve` / `check`).

The fourth check is the newest and answers a mistake this page made: it restated a
spike's check count in three places, the spike grew, and the prose stayed where it
was. A count in a page is now read as a claim about a *run*, and the run's home is a
committed capture — `189/189` is true of the tree while
`spikes/results/shutdown-e2e-00u24.log` is in it, and stops being true when the
next capture says something else. Take captures with
[`./scripts/capture.sh`](../scripts/capture.sh), which heads the log with the UTC
time, the tree rev and the command that produced it, and commit the fresh capture in
the same commit that changed the count.

The fifth check is the same failure one level down: a test **name** quoted in prose
that no function answers to. This page is a specification, and a specification you
cannot grep is not one — the ticket that opened this ran into a page that had
dropped the word `is` out of a test name, and a reader who followed that name into
`src/` found nothing and suspected their grep before suspecting the page. So a
backticked `snake_case` name of four or more words has to resolve to a real
`fn <name>` in `src/`, `tests/`, `examples/`, `spikes/` or `scripts/`, and a
failure prints the page, the line, the name, and the nearest real name — because
the usual cause is a renamed test with a page that was never told:

```
docs/testing.md:577: `components::compaction::tests::an_unknown_reason_leaves_no_trailing_separator`
  reads like a test name (at least 4 words) but no `fn an_unknown_reason_leaves_no_trailing_separator`
  exists in src/tests/examples/spikes/scripts — the closest name that does exist is
  `no_reason_at_all_leaves_no_trailing_separator` (src/components/compaction.rs:186)
```

Names that are deliberately *not* functions — a prose example of the naming rule,
or the name of something a page is about the absence of — are excused one at a
time in `ILLUSTRATIVE_TEST_NAMES` with the reason beside them, in the same voice
`src/wire.rs` uses for an `#[allow(dead_code)]` that has to carry its
justification. The excuse list is checked in the other direction as well: an entry
no page quotes any more fails the gate, so it cannot quietly become the dump that
the blanket allows were. `--list-test-names` prints every citation and what happened
to it.

The check found two besides the one that opened the ticket, in the same commit's own
grep: the compaction row above (`an_unknown_reason_…` for a test called
`no_reason_at_all_leaves_…`), and the scrollback row that credited the *row* cap
with a test written about the **byte** cap — not only a wrong identifier, a wrong
unit. Both now quote what the code calls them. Renaming a test to match a page is
the forbidden direction: the page is the thing that was wrong.

CI runs `scripts/check.sh` (`.github/workflows/looprs-gate.yml`, at the repo root,
path-filtered to `pi-beads/looprs/**`).

The one warning that is not ours: `nix v0.28.0` future-incompatibility, pulled in by
`portable-pty`. It is a note, not a failure. It is documented in
[ADR-0001](adr/0001-bash-terminal-state-pty.md) rather than silenced, because the
gate's claim is "everything except this one named thing is clean".

**No test is forgiven in advance.** A gate whose failures are pre-forgiven trains
people to ignore failures, so this page used to carry a named list of four
`session::bash` pty tests whose red meant "re-run it in isolation, it's fine".
That list is gone, and so is the reason for it: every wait in those tests now
blocks on an observable — the session's own `Sync` seam, the command's exit
marker, the byte tape — and the durations that remain are failure bounds with that
written on them. The discipline, and how to check it under the contention that
used to break it, is in
[the four pty tests are event-driven](#the-four-non-hermetic-pty-tests-are-event-driven-looprs-00u17).

### Dead-code allows

`clippy`/`rustc` report items nothing reaches as dead. This crate does not silence
that with a blanket `allow`. Each one carries its own `#[allow(dead_code)]` with the
reason it stays and the thing that will read it — the two usual answers are *"test
seam"* (`quiesce`, the `Sync` commands: without them the lifecycle tests are sleeps)
and *"looprs-guh's status row"* (fields parsed off the wire today so the row can
print them next week).

The policy is written out at the top of
[`src/session/mod.rs`](../src/session/mod.rs), where the blanket
`#![allow(dead_code)]` used to sit. An allow without a reason is a warning deleted
rather than answered.

**A reason can go stale, and that is the failure mode a lint cannot see.** An allow
that names a ticket as its reader is a promise; when that ticket lands and reads
something else, the comment keeps asserting a reader that is not there, which reads
as coverage. `./scripts/dead_audit.py` is the check for the half of that which is
mechanical:

```sh
./scripts/dead_audit.py            # every allow, its verdict, and its stated reason
./scripts/dead_audit.py --gate     # quiet; fails if any allow covers live code
```

It runs `cargo clippy --all-targets -- --force-warn=dead_code`, which reports the
dead items *through* the `allow`s, and then asks of each attribute whether the code
under it is still dead. An allow over live code is redundant and fails the gate —
run without `--gate` for the list of what is still dead and what each one claims,
which is the part a human has to keep true. `./scripts/check.sh` runs `--gate`.

looprs-2nd was that pass over the `looprs-pdl.9` promises: the drag selection
routed through `CellMap::at` / `bytes_at` / `cell_of_byte`, which made those
three live (their allows are gone), and left `snap_bytes`, `CellMap::is_empty`,
`DisplayRow::crosses` and `CharRef::anchor` promising a reader that had already
landed elsewhere — those four are deleted, comments and tests with them.

---

## The fakes

`src/testing.rs` builds real executables in a temp dir per test, on the paths
handed to `SessionConfig::pi_bin` / `bd_bin` / `LOOPRS_*_BIN`. They are real
subprocesses with real pipes on purpose: the assertions are then process-level
truth — a pid existed, a prompt reached it, the previous one was reaped — instead of
trust in a mock that never had a pid to begin with.

The programs themselves live in **[`tests/fixtures/`](../tests/fixtures)**, as bash
and python, and are embedded with `include_str!` (a missing fixture is a compile
error, not a spawn failure in whichever test got there first):

| fixture | used for |
| --- | --- |
| `tests/fixtures/fake_pi.sh` | a `pi` that answers a prompt with one canned response (started / handled / refused — the `{{REPLY}}` token) |
| `tests/fixtures/fake_pi_dies.sh` | a `pi` that exits non-zero before answering anything |
| `tests/fixtures/fake_pi_chat.py` | a stateful `pi --mode rpc`: remembers its conversation, holds a run open until the test touches `settle`, honours `abort` |
| `tests/fixtures/fake_bd.sh` | a `bd` that logs every command and answers each verb from a file the test rewrites mid-run |

The levers a test has, once the fakes are up:

| call | effect |
| --- | --- |
| `Fakes::set_board(json)` | change what `bd ready` / `bd list` print, without restarting. Writes one **journal record** too, the way a real `bd` does, so the change detector sees the change (a detector test that set the board and not the journal would be asserting against a workspace no `bd` produces) |
| `Fakes::set_board_unjournaled(json)` | move the board **without** a journal record — exactly what `bd dolt pull` / a merge does. The one way to test that a change the journal cannot see still lands, on the sweep and not before |
| `Fakes::journal_head()` / `bump_journal(n)` / `set_journal(lines)` | read and move the journal on its own: stay quiet while the board does not change, make n changes, or hand the probe a script of records verbatim |
| `Fakes::truncate_journal(floor, head)` / `disable_journal(true)` / `fail_journal(true)` | the three ways the probe itself goes bad: a watermark the retention has pruned (exit 1 + a typed `events_journal_truncated` refusal), a workspace with `events-journal` off (exit 0, empty, a note on stderr), and a `bd` that cannot answer at all. `fail_journal` breaks the **probe only** — the board reads stay healthy, which is what makes "a failed probe is never 'nothing changed'" a testable claim rather than a hope |
| `Fakes::set_show(json)` | change what `bd show <id> --json` says — i.e. "did the worker close it" |
| `Fakes::fail_bd(true)` | every subsequent `bd` exits 3, mid-pass |
| `Fakes::refuse_claim(true)` | only `bd update … --claim` fails (exit 4); reads still work |
| `Fakes::settle()` | release the chat fake's held-open run |
| `Fakes::stubborn_pi(true)` | the fake answers `abort` and then refuses to unwind |
| `Fakes::wait_for_pi_spawns(n)` / `wait_for_log_line(needle)` | poll, instead of sleeping |
| `Fakes::bd_log()` / `pi_commands()` / `pi_verbs()` | the command order, for "claim before spawn", `clear_queue` before `abort` |
| `kill_pid` / `process_alive` | take the child out from under the app, and check |

The `tests/warning_gate.rs` tests are `#[ignore]`d because they invoke cargo from
inside cargo (own `CARGO_TARGET_DIR`, cold build) — see that file's header.

---

## Scenario index

The scenarios the chore (looprs-6ol) listed, each with the tests that carry it.

### Startup with ready beads (looprs-26r)

- `session::beads::tests::startup::a_non_empty_board_self_starts_a_prompted_worker`
- `session::beads::tests::startup::an_empty_board_parks_without_spawning`
- `session::beads::tests::startup::constructing_a_loop_spawns_nothing`
- `session::beads::tests::startup::a_pi_that_dies_during_startup_is_reported`

### Settle routing (looprs-msj)

- `session::beads::tests::drive::the_loop_takes_its_next_pass_from_its_own_workers_settle`
- `session::beads::tests::drive::a_settle_from_a_pass_this_loop_does_not_own_moves_nothing`
- `session::beads::tests::startup::driving_the_loop_reaps_the_previous_worker`
- `session::router::tests::the_router_has_no_beads_advance_path_left`
- `app::tests::pi_state::no_settle_ever_turns_into_a_command_from_the_app`

### Claim / close guard (looprs-w7q)

- `session::beads::tests::claim::the_harness_claims_the_ticket_it_is_paying_for`
- `session::beads::tests::claim::a_claim_bd_refuses_buys_no_worker`
- `session::beads::tests::claim::a_ticket_that_is_never_closed_stops_the_loop_instead_of_spinning`
- `session::beads::tests::announce::the_active_ticket_is_published_when_taken_and_when_released`
- `session::beads::tests::claim::the_worker_is_told_its_ticket_not_invited_to_go_shopping`
- pure: `session::beads::tests::tables::the_claim_guard_skips_refuses_and_works_in_that_order_pure`

### Out-of-band notification (the completion edge)

Turn it on with `LOOPRS_NTFY_URL` + `LOOPRS_NTFY_TOPIC`; with either missing the
notifier is `Noop` and nothing leaves the terminal.

**One `notify` call exists in the binary**, in the `PassOutcome::Closed` arm of
`BeadsTask::worker_settled`. Not on `agent_settled`: that event says the worker
stopped talking, and a pass that closed nothing, an aborted pass and a *planner's*
pass are the same bytes. The five no-rows below are the whole reason for the
placement, and `RecordingNotifier` (`src/testing.rs`) is what makes "nothing was
announced" assertable instead of unfalsifiable.

| ending | announced |
| --- | --- |
| the board says closed | `session::beads::tests::announce::a_closed_ticket_is_announced_once_with_the_title_it_was_claimed_under` |
| settled, ticket still open | `…::a_ticket_that_was_never_closed_announces_nothing` |
| `Esc` cancelled the pass | `…::a_pass_the_user_cancelled_announces_nothing` |
| the planner's settle | `…::a_planner_settling_announces_nothing_even_though_its_plan_worked` |
| worker left it `blocked` | `…::a_ticket_its_worker_handed_to_a_human_announces_nothing` |
| `bd` unreadable after the pass | asserted inside `…::an_unreadable_board_after_a_pass_is_unverifiable_not_a_verdict` |
| a default `SessionConfig` | `session::tests::the_default_config_carries_the_silent_sink` — the line that keeps the suite off the network by construction |

The ntfy wire format itself is checked against a loopback listener, under
`--ignored` (it binds a port, and the retry test costs `RETRY_AFTER` of wall clock):
`cargo test --bin looprs -- --ignored`.

- `services::notification::tests::the_post_puts_the_message_in_the_body_and_the_chrome_in_the_headers`
  — message in the body, chrome in `Title` / `Priority` / `Tags`, topic in the path
- `services::notification::tests::a_refused_post_is_tried_a_bounded_number_of_times_and_then_stops`
  — exactly `POST_ATTEMPTS` connections, then a log line and no error upward
- `services::notification::tests::the_environment_switches_the_real_sink_on_and_off`
  — `main`'s own call, exercised

### Pi multi-turn persistence (looprs-ctn)

- `session::pi_chat::tests::two_turns_share_one_child_and_the_second_remembers_the_first`
- `session::pi_chat::tests::a_message_during_a_run_steers_instead_of_starting_another_run`
- `session::pi_chat::tests::killing_the_child_says_so_and_the_next_message_restarts_it`

### Bash echo / exit / cwd persistence (looprs-553)

- `session::bash::tests::roundtrip::echo_streams_its_output_and_reports_exit_0`
- `session::bash::tests::roundtrip::a_failing_command_is_loudly_not_zero`
- `session::bash::tests::roundtrip::cwd_persists_across_commands`
- `session::bash::tests::roundtrip::exported_variables_persist_across_commands`
- `session::bash::tests::roundtrip::stderr_and_stdout_arrive_in_the_programs_own_order`

### Esc cancellation (looprs-5g7)

- `session::cancel::tests::the_grace_is_a_warning_window_not_a_hope`
- `session::cancel::tests::arm_delivers_the_command_after_the_grace`
- `session::bash::tests::interrupt::esc_interrupts_a_running_command_without_killing_the_shell`
- `session::bash::tests::interrupt::esc_says_cancelling_before_the_command_reports_itself_done`
- `session::bash::tests::interrupt::an_esc_aimed_at_a_queued_command_takes_it_out_of_the_queue` — the
  cold-start window, held open on purpose with `/bin/cat` as the "shell" (it never prints the
  readiness marker, so the command cannot leave the queue underneath the test)
- `session::cancel::tests::the_queued_cancel_names_the_command_and_says_it_never_ran`
- `session::pi_chat::tests::esc_clears_the_queue_before_aborting_and_gives_the_words_back`
- `session::pi_chat::tests::a_pi_that_ignores_the_abort_is_killed_said_so_and_the_mode_recovers`
- `session::beads::tests::drive::a_worker_that_ignores_the_abort_is_killed_and_leaves_the_bead_named`
- `session::router::tests::cancel_reaches_the_active_session_only`
- pure: `session::beads::tests::tables::esc_is_absorbed_while_a_cancel_is_unwinding_and_only_while`
- pure: `session::beads::tests::tables::a_stall_timer_only_answers_for_the_attempt_that_armed_it`

#### A cold shell makes `Running` mean "pending", and that is not "started"

Every Bash test that wants *the command is in flight, now interrupt it* has to warm the shell
first (`session::bash::tests::warm_shell`), and this is the rule that killed a whole class of
CI flakes:

`submit` cannot write to a shell that has not printed its prompt, so a command submitted cold
sits in the session queue — and `SessionStatus::Running` covers both "queued" and "in flight".
A poll of the form *"wait until `status() == Running`, then act"* therefore cannot tell the two
apart, and on a loaded runner the start-up window is wide enough to land in most of the time.
What the test then interrupts is a queue entry: the `0x03` goes to an idle prompt, the raw keys
are typed before the command line exists, and the assertion fails against a state the test never
set up.

The two halves of the fix, both needed:

* **the product** — `interrupt` now handles the queued case instead of returning on "nothing
  outstanding" (ADR-0003: *why an Esc that arrives early still cancels*);
* **the tests** — `warm_shell` runs one cheap command to completion first, so the shell is
  ready and the next `send_text` goes straight to the child, and `in_flight` then proves the
  write happened by waiting on the session's own `Sync` seam rather than by looking at
  `status()` at all.

### The four non-hermetic pty tests are event-driven (`looprs-00u.17`)

Four tests drive a real pty and cannot be made hermetic without lying about the thing
they check: `esc_interrupts_…`, `esc_says_cancelling_…`,
`a_command_that_traps_the_interrupt_…`, `a_full_screen_program_is_handed_the_screen_…`.
They used to fail under the full suite's CPU contention, and the honest-sounding
answer was a list of names to re-run. Four rules replaced the list, all in the
shared harness `src/session/bash/tests/mod.rs`:

| rule | the helper | what it replaced |
| --- | --- | --- |
| **Wait on the observable, not on a window.** Read the stream until the event you are asserting about arrives; the timeout is there to fail, not to wait. | `until_event(rx, pred, "what")` | `collect_within(rx, 800ms, …)` + `assert!(got.contains(…))`, which is a sleep wearing an assertion's clothes: on a loaded runner the sample came up empty and the failure said *"the keystroke was not acknowledged"* about a session that had acknowledged it late |
| **Make "the command is in flight" a fact from the queue, not a poll.** `Sync` is handled by the same task after the `Submit`, and publishes the status mirror before it acks. | `in_flight(&s, cmd)` (over `warm_shell`) | `for _ in 0..200 { if status == Running break; sleep(20ms) }` — a wait whose success condition was "the mirror flipped sometime in the next four seconds" and whose failure condition was "we ran out of samples" |
| **When the precondition lives in the child, make the child declare it.** `trap '' INT` is not installed when `send_text` returns, so an `Esc` that overtakes the builtin kills the sleep and every later assertion reads a run that never had the property under test. The test runs `trap '' INT; echo looprs-trap-set` and waits for that marker. | the trap command's own `echo` | aiming `0x03` at a shell assumed to be trapping |
| **Search the bytes, not the events.** A pty read returns whatever had arrived; the line discipline echoes a typed command one or two characters at a time, so an escape and the word after it are not promised to the same event. | `Tape` — the output bytes joined in arrival order, with a map back to the event each byte came in | `position(|e| e.contains("painted") && e.contains('\u{1b}[?1049h'))`, whose needle was simply not whole on a busy runner, and which the shell's own prompt (`\u{1b}[?1034h`) could satisfy by accident |

**Where the observable genuinely is time, say so and bound it.** The escalation on
a trapped interrupt *is* a timer (`cancel::GRACE`), so the test stops pretending
otherwise: it blocks on the report and then checks the gap between the keystroke
and the report is **at least** the grace. That is a lower bound between two events,
and it is sound because the deadline is armed after the keystroke is handled — no
amount of load can make the grace elapse earlier than it started. The `collect_within(500ms)`
it replaced could not distinguish "early" from "not yet", which is every slow case.

**The remaining durations are failure bounds, labelled.** `NO_HANG` (20 s) and
`INTERRUPTED_WITHIN` (12 s), each with a comment saying *failure bound, not a
wait*. The one-second bar these tests used to assert on is gone deliberately: a
wall-clock assertion under a second measures the runner's scheduler. The
load-independent form of "the word comes before the child does" is the ordering —
`send_sigint` puts the acknowledgement out before the reply can possibly have been
read, because one task owns the pty and emits both ends — and that is what the
test now asserts.

**Verified under load, not hoped for.** Both halves of that claim are in
`scripts/stress_test.sh`, which runs the whole suite at a chosen thread count with
N `yes > /dev/null` processes over the box and prints a per-round verdict:

```sh
# 20 rounds of the full suite, 8 test threads, 12 CPU hogs, nothing skipped.
HOGS=12 LOG=/tmp/stress.log ./scripts/stress_test.sh "$(pwd)" 20 8
```

The four run concurrently with all 850-odd other tests in every one of those
rounds. The run-by-run record lives in the `looprs-00u.17` ticket notes, because a
single green run proves nothing here — which is the whole reason the ticket existed.
One discovery from re-measuring: the *old* form of `raw_keys_reach_the_child_verbatim_…`
was flaky in the same way and had simply been lucky — through one pty, two of three
runs showed the echoed `^[:wq!` before the command finished and one showed nothing
until `bash: wq!: command not found`, which is not `:wq!`. It now asks the child
for the bytes by value (`head -c 6 | od -An -tx1`) instead of reading the echo, so
it proves more and waits less.

### The shutdown hang the `--skip` used to shadow (`looprs-2ck`)

This page used to run the stress command above with
`--skip=session::bash::tests::shutdown::shutdown_leaves_no_shell_running`, and said the skip
was "a test that never returns under load", which was true and was not the end of
it. The skip is gone, and so is the hang it stood over.

`BashTask::shutdown` used to ask the shell to `exit`, sleep-poll `try_wait` for two
seconds, then `kill()` and **block in `Child::wait`** — from inside the async task
that owns the pty. That task is the only thing that drains the byte lane, and the
reader thread's only way out of a read loop is to land a buffer on it, so a shell
dying with output in flight deadlocks three parties at once:

| who | parked at | waiting on |
| --- | --- | --- |
| the session task | `Shell::kill_and_reap` → `Child::wait` → `__wait4` | the child |
| `looprs-bash-reader` | `mpsc::blocking_send` → `__psynch_cvwait` | the lane only that task drains |
| the shell itself | `exit(2)` → the tty flush | the pty buffer the reader stopped emptying |

`HOGS=12` hung 4 of 8 rounds of that one test with the pre-ticket code, `ps -o stat`
called the child `?Es` — *trying to exit* — and never reaped, and the watchdog had
to kill the round. Nothing in that stack was a tolerance problem: no event-driven test
can see past it, because the test never reaches its assertions. A quit during a long
command — Ctrl-Q with `yes` still pouring, an exit while the transcript is draining —
is a terminal the user has to go and kill by hand.

The rule in `src/session/bash/reap.rs` now is **never reap alone**:

* **The reap drains while it waits.** `reap_while_draining` takes the byte lane with
  it on every turn. That is not a courtesy to the transcript: every buffer the reader
  is holding is a buffer the dying child is still trying to write, so a `try_wait`
  loop that does not drain is a loop waiting for a death its own refusal caused.
* **Every phase is bounded.** `EXIT_ASK` (2 s) asked politely, `SIGKILL`, then
  `KILL_REAP` (1 s) of the same drained polling: 3 s of design that no child can
  stretch, because each deadline is armed before the thing it bounds.
* **What outlives the bounds goes off-task.** `reap_off_task` detaches the child into
  a `looprs-bash-reaper` thread, which owns the last blocking `wait` in the file and
  *logs what it is waiting for* — pid, how long, how many reader threads are still
  live — instead of holding the process open quietly. A thread named for waiting is a
  diagnosable stall; a session task in `wait4` is not.
* **`Drop for Shell` kills and never waits.** A blocking reap taken from a `Drop` is
  the same bug wearing a different hat, and the drop path runs from inside whatever
  task happens to let go. The residue of kill-without-wait is one zombie pid in a
  process that is leaving anyway; a task parked in `wait4` is the terminal.

Two properties hold, and both are asserted rather than assumed: **shutdown returns**,
and **the reader thread ends**. The second needed a number rather than a feeling, so
reader threads are counted — `LiveReaders`, held from before the spawn until the body
finishes by any exit including a panic, surfaced as `BashSession::readers_live()` —
and the tests watch the count reach zero. A parked reader used to be invisible; now
it is the thing a test fails on.

| new test | the shape it drives |
| --- | --- |
| `shutdown_returns_while_the_lane_is_full_and_the_reader_is_parked` | `yes` — the worst case on all three axes at once: the shell never reads the `exit` we send, the lane is full so the reader is parked in `blocking_send` rather than in `read(2)`, and the kernel's pty buffer is full of what the dying child must flush. Asserts the `down` notice inside `SHUTDOWN_RETURNED_WITHIN` and `readers_live() == 0` |
| `a_shell_that_ignores_the_exit_is_killed_within_the_bound` | `sleep 30` — the quiet half, where nothing is draining and the only thing that can go wrong is the wait itself. Asserts the shutdown took **at least** `EXIT_ASK`, a lower bound no load can bend because the deadline is armed before the ask, so the test proves it travelled the kill path instead of exiting politely by accident |

**Measured both ways.** Twenty rounds at 8 threads under 12 hogs with nothing skipped
(`looprs-2ck`): **no HANG round at all** — 18 green, 2 red, on
`esc_interrupts_…` and `beads::…the_loop_takes_its_next_pass_…`. Twenty rounds of
the **same tree with only `src/session/bash.rs` reverted**, carrying the old `--skip`
so the rounds finish: also 2 red of 20, on
`beads::…the_loop_takes_its_next_pass_…` and
`board_poller::…a_probe_that_cannot_answer_never_reads_as_a_quiet_board`. Same rate,
different names each time, and the shutdown test in neither failing list: those two
(or three) are ambient contention noise in the suite, not something this change
brought in and not this ticket's to fix. What changed is that the exit path is now one
of the things those rounds are actually testing.

### `bd` missing / failing surfaces (looprs-037)

- `services::bd::tests::a_missing_bd_is_named_not_gossiped_around`
- `services::bd::tests::a_failing_bd_is_not_an_empty_board`
- `services::bd::tests::silence_from_bd_is_malformed_never_empty`
- `session::beads::tests::startup::a_failing_bd_is_reported_and_parks`
- `session::bash::tests::roundtrip::a_missing_path_shows_its_stderr`

### The pure state machines (this chore's first bullet)

No subprocess, no fake, no channel wait — just the decision layer, enumerated.

**Mode table (Bash / Beeds / Pi × Tab)**

- `session::tests::tab_walks_the_whole_mode_table_and_back_to_where_it_started` — one
  3-cycle, one-in-one-out per mode, three Tabs = one lap
- `session::tests::every_row_of_the_mode_table_names_where_tab_goes_and_what_it_leaves_behind`
  — mode → Tab target → `SwitchAway` policy, one row per mode, and only Beads parks
- `session::tests::only_await_input_hands_the_keyboard_back_to_the_human`

**Beads step machine (`AwaitInput → CreateTickets → WorkTickets → AwaitInput`)**

- `session::beads::tests::tables::every_step_cause_lands_on_one_step_and_says_so` — the
  table, and the fact that a transition is *published* and not merely stored
- `session::beads::tests::tables::the_machine_walks_await_plan_work_and_home_again`
- `session::beads::tests::tables::the_step_table_is_a_bijection_over_the_whole_enum` — no
  orphan step, no unmapped cause, no two causes claiming one step

The machine takes a `StepCause` (`Planning` / `Working` / `Awaiting`) rather than a
`BeadStep`: there is no `set_step(anything)`, only `set_step(why)`, so the table
lives once instead of being re-implied at six call sites.

**Guards**

- `session::beads::tests::tables::the_pass_gate_has_exactly_one_open_row_in_sixteen` — the
  one gate in front of "start a pass", all 16 rows, exactly one yes
- `session::beads::tests::tables::each_flag_closes_the_pass_gate_by_itself`

### Status row (looprs-guh)

**What it proves:** the one-row band `frame_areas` reserves is filled, is a pure
function of app state, and follows the *real* state machines — not just in the
model, but as painted in a real pty.

| Layer | Where | What it pins |
| --- | --- | --- |
| the row builder | `components::status::tests` | the verb table (`the_verb_table_says_what_every_state_means`), the drop ladder (`the_widest_segment_goes_first_and_the_mode_last`), no overflow at any width (`the_row_never_overflows_its_width_at_any_width`), the 40-column floor with an error (`at_forty_columns_with_an_error_the_work_and_the_failure_survive`), a truncation that keeps its `✗` (`a_shortened_error_keeps_its_marker`) |
| the paint | `main::tests` (ratatui `TestBackend`) | `the_status_row_is_painted_into_the_band_the_layout_reserved_for_it`, `…_even_with_no_input_box`, `the_status_row_stays_on_its_own_line_at_any_terminal_height` (heights 5–20) |
| the plumbing | `app::tests` | `a_status_edge_lands_in_its_own_view_and_not_in_the_active_one`, `the_run_age_comes_from_the_app_clock_not_from_a_read_of_the_wall`, `an_idle_app_is_not_repainted_by_the_row_and_a_busy_one_is_repainted_at_eight_fps`, `the_last_error_shows_in_its_own_mode_and_does_not_follow_the_user_around` |
| the real pty | `spikes/status_e2e.py` | 20 checks, 6 scenarios, and a control run that fires on 0 of the 13 row-specific ones |

Run it:

```sh
cargo build
python3 spikes/status_e2e.py | tee spikes/results/status-e2e.log

# the control: same spike against the pre-looprs-guh binary
git worktree add --detach /tmp/looprs-ctl HEAD
(cd /tmp/looprs-ctl/pi-beads/looprs && cargo build --target-dir /tmp/ctl-target)
LOOPRS_BIN=/tmp/ctl-target/debug/looprs python3 spikes/status_e2e.py --control \
    | tee spikes/results/status-e2e-control.log
```

| Scenario | State under test | Row expected |
| --- | --- | --- |
| S1 empty board | loop waiting on a human | `awaiting input · Tab switch · ^C quit`, and the app took the alternate screen **exactly once** (since `pdl.4`; a second `?1049h` would re-save the user's own contents as their main screen) |
| S2 `bd` down | loop parked by a failing CLI | `paused · ✗ …` on the **row**, not only in scrollback |
| S3 beads working, Tab away | a paid-for pass, then focus moved (ADR-0002) | `working · <bead-id>`, then `bg: Beeds working · <id> <elapsed>` with Pi focused; never `bg: Pi` |
| S4 shell liveness | `sleep 6` start → finish | `running · … · Esc cancel`, then `idle`; a mode merely Tabbed through is not claimed warm |
| S5 40 columns, mid-run | resize while busy | app alive, row still painted and still the running row |
| S6 40 columns + long error | the truncation worst case | `paused · ✗` still reaches the band |

**Why the spike reads the wire and not the screen.** Three ways of checking a row
over a pty were tried and each produced confident nonsense:

1. *Grep the capture for the whole row.* `ratatui` renders by **diff** — a frame
   rewrites only the cells that changed, so the frame after an empty board wrote
   `●` at column 1 and `awaiting input · Tab switch ·` at column 11 and nothing
   in between.
2. *Concatenate recent output and grep.* Diff rendering plus column shifts mean the
   stream's order is not the screen's order: `Esc cancel` reached the wire as
   `Esc can` + `el`, the shared `c` cell left unwritten because the previous
   frame already had a `c` there (it was in `switch`). A needle can therefore be
   **on screen and unread**. `Driver::full_paint()` — nudge two columns out and
   back — makes the redraw non-partial and is used wherever a check needs a
   specific string.
3. *A cell-grid emulator with scroll regions.* Coherent for a while, then the
   `ESC[6n` answers the harness must feed the app drift the emulator's cursor, and
   the replay starts reading rows that were never on screen. Kept in the script
   only for the human-readable dump.

What the spike checks instead is **row-only vocabulary in a fresh window**: words
that exist nowhere else in the program's output (`Esc cancel`, `bg: `, `warm: `,
`paused · ✗`). The transcript is deliberately different at exactly those points —
it says "the loop is parked", the row says `paused`; it says "beads: working
looprs-…", the row says `bg: Beeds working`. That is what makes the match mean
the row, and it is why the control matters: run against the pre-feature binary,
**0 of 13** row-specific checks fire. The control caught two needles of mine that
were passing on transcript prose rather than on the row.

**What the spike deliberately does not claim:** line width, reflow, and the drop
order. A terminal's line model cannot be recovered from this byte stream, for the
reason above. Those live in `status.rs`'s width tests and `main.rs`'s
`TestBackend` tests, where every cell can be checked exactly.

### Token counts on the row

`↑in ↓out` on the row, plus a `cache N` detail, read straight off pi's own
`usage` record.

| claim | where |
| --- | --- |
| the wire shape parses as pi writes it — camelCase, every field independently optional, **absent ≠ zero** | `app::tests::token_window::the_wire_usage_record_parses_as_pi_writes_it` |
| the same shape through a real child's stdout, not a hand-written string | `session::pi_chat::tests::the_wire_usage_record_survives_the_real_pipes` |
| only an assistant `message_end` adds; a `user` / `toolResult` message and a usage-less message add nothing | `app::tests::token_window::every_assistant_message_adds_and_nothing_else_does` |
| the beads window opens on a claim and **survives the release** | `app::tests::token_window::the_beads_window_opens_on_a_claim_and_survives_the_release` |
| a respawned generation owes nothing for the dead one's run | `app::tests::token_window::a_new_generation_starts_the_window_at_nothing` |
| nothing reported prints nothing, rather than `↑0 ↓0` | `app::tests::token_window::the_row_prints_the_window_it_is_handed` |
| the cache detail surrenders its columns before the in/out pair does | `components::status::tests::the_cache_detail_gives_way_before_the_in_out_pair` |
| the formats stay narrow: `999`, `12.3k`, `1.24M` | `components::status::tests::token_counts_stay_compact_at_every_order_of_magnitude` |

**Why usage is read only from `message_end`.** pi reports `usage` on every
`message_update` too, and that figure is **cumulative for the message still
streaming** — the chat fixture emits it that way on purpose, with a half-built
output count on the delta and the full count at `text_end`. Folding the streaming
copies into a total double counts them, and grows the number with the length of the
stream. `message_end` lands once per API call, which is as live as the row needs
and cannot be counted twice.

**The window, per mode.** Pi chat: the whole session. Beads: the current ticket,
cleared when a new claim is published. Three deliberate non-clears — releasing a
claim (the row keeps answering "what did that ticket cost"), `Esc` (a cancelled
pass spent what it spent; a total that drops on a keystroke is a total nobody
trusts), and the planner (it holds no claim, so its spend lands in the window of
the first bead that follows it rather than being zeroed away).

**Dropped bytes are gone from the row.** They were ADR-0002's scrollback-integrity
signal for the buffer cap, and that signal still exists where the loss happens — at
the marker row at the top of the band, which reads
`⌄ scrollback trimmed: N earlier lines dropped` and names the journal file that kept
what the store dropped. The view no longer inserts a `… N bytes dropped (buffer cap) …`
*entry* into the transcript (see
[`session/view/buffer.rs`](../src/session/view/buffer.rs), `trim_open_entry`'s doc: the entry is
gone because the marker row says the same thing where the user can see it). The row
spends the room on cost instead.

That quoted sentence is pinned in both directions: `state::scrollback::tests::the_marker_says_the_exact_sentence_the_docs_quote` asserts the rendered row by *equality* (its journal half by `the_marker_names_the_journal_that_still_has_what_it_dropped`), and `the_pages_that_quote_the_marker_quote_a_row_this_code_renders` reads the pages that quote the row — this one and `docs/guide/transcript.md` — and fails if a reword on either side leaves the other behind. Rewording the marker is allowed; leaving a page quoting a row nobody renders is not.

### A compaction says so (the compaction card)

pi pauses a run to summarise its own history whenever the context crosses its
reserve. The summarisation is a separate LLM call that puts nothing on the event
stream but `compaction_start` / `compaction_end`, so without a renderer for
those two it is ten to sixty seconds of transcript that has stopped moving — the
exact shape of "is it hung?". It gets the same one-row live card a tool call
gets: `⠹ compacting context · threshold` while it runs, and once it is over a
finished row in the scrollback, `✓ context compacted · threshold · 150.0k → 32.0k`.

| claim | where |
| --- | --- |
| the wire shape parses as pi writes it: `reason` on the start, `result.tokensBefore` / `estimatedTokensAfter` on the end, and no `result` at all when it was aborted | `app::tests::token_window::the_compaction_wire_format_parses` |
| the start puts a live card on the screen, and the height policy is told to budget its row | `app::tests::token_window::a_compaction_shows_a_live_card_while_it_runs` |
| the finished card reaches the scrollback carrying what it freed | `app::tests::token_window::a_finished_compaction_reaches_the_scrollback_with_what_it_freed` |
| cancelled and failed are two different sentences, and a cancel is not painted in failure's red | `app::tests::token_window::an_aborted_compaction_says_aborted_and_a_failed_one_says_why`, `components::compaction::tests::aborted_is_grey_and_failed_is_red` |
| the card is drawn in the frame's card band, and the two endings draw differently | `main::tests::a_live_compaction_is_drawn_in_the_card_band`, `main::tests::a_cancelled_and_a_failed_compaction_are_drawn_differently` |
| an `end` whose `start` never arrived is recorded rather than swallowed | `app::tests::token_window::a_compaction_end_with_no_open_card_is_recorded_anyway` |
| closing the card releases everything that queued up behind it | `app::tests::token_window::closing_the_compaction_card_releases_what_came_behind_it` |
| a missing `reason` leaves no dangling separator on the row | `components::compaction::tests::no_reason_at_all_leaves_no_trailing_separator` |

**A card left open when the session dies is a bug with teeth.** A `!done` entry
stalls the flush cursor, so a compaction — or a tool — that was still in flight
when the child died would take the rest of that session's transcript with it:
exactly the tail the exit drain exists to collect. `SessionView::seal` now closes
open cards of either kind as `Aborted` —
`session::view::tests::flush::sealing_closes_cards_left_running_so_the_transcript_keeps_flushing`,
`state::transcript::tests::abandoning_closes_every_open_card_and_nothing_else`.
A frozen spinner in a transcript whose process is gone is the thing that was
replaced.

**Not claimed:** the token figures are pi's own and `estimated` in its own words,
printed without being checked; and a compaction in a mode that is *not* on screen
is announced nowhere but that mode's own transcript — the same limit a tool card
lives under, with the status row saying only that the mode is working.


### The terminal mode ledger, and every way out (looprs-pdl.3)

**What it proves:** every terminal mode the app switches on — raw mode, the alternate
screen, the three mouse modes, bracketed paste, the hidden cursor — is switched back off on
every path out of the app, exactly once, without the exit path asking the terminal a single
question. See [ADR-0006](adr/0006-terminal-mode-ledger.md) for why "exactly once" cuts both
ways: a leaked `?1002` reports drags forever, and a second `?1049l` replaces the user's
screen with the terminal's stale save.

| Layer | Where | What it pins |
| --- | --- | --- |
| the table | `teardown::tests` | `every_mode_names_its_own_pair_of_bytes` (the wire format, pinned rather than read out of a table), `a_mode_spec_is_parsed_into_boot_order_whatever_order_it_came_in`, `an_unknown_mode_is_an_error_that_names_the_names`, `the_startup_set_is_the_default_plus_what_was_asked_for` |
| the ledger | `teardown::tests` | `the_ledger_hands_back_in_the_reverse_of_the_order_it_went_on`, `a_mode_we_never_switched_on_is_never_left_off`, `a_switch_that_never_reached_the_terminal_is_still_a_mode_we_hold`, `enabling_a_mode_writes_it_once_and_repeating_the_enable_writes_nothing`, `releasing_a_mode_early_takes_it_off_the_ledger` |
| the hand-back | `teardown::tests` | `restore_hands_every_mode_back_once_and_nothing_after_it`, `holding_the_alternate_screen_means_no_erase_and_no_closing_newline`, `the_panic_hook_and_the_normal_exit_agree`, `a_restore_does_not_toggle_raw_mode_that_was_never_on` |
| the inherited screen | `screen::tests`, `teardown::tests` | `the_debt_follows_the_bytes_the_passthrough_wrote`, `a_switch_split_across_two_tees_is_still_one_switch`, `the_watcher_remembers_which_alt_screen_was_switched`, `the_leave_is_spelled_the_way_the_entry_was`, `an_inferred_takeover_is_not_an_alt_screen`, `a_screen_a_dying_child_left_behind_is_left_on_the_way_out`, `a_screen_the_child_already_left_is_not_left_again`, `our_own_alternate_screen_already_covers_the_childs`, `a_screen_that_was_only_guessed_at_owes_no_leave` |
| the tee reports it | `app::tests` | `a_teed_alt_screen_is_a_screen_we_owe_the_terminal_back`, `a_screen_switch_that_was_never_teed_owes_nothing` |
| the command boundary | `session::bash::tests` | `quitting_while_a_full_screen_program_holds_the_screen_leaves_the_alt_screen` |
| the signals | `signals::tests` | `the_signals_install_inside_a_runtime`, `a_signal_sent_to_this_process_is_received` |
| the real pty | `spikes/shutdown_e2e.py` | 9 scenarios, **189/189** on the tree this page documents — `spikes/results/shutdown-e2e-00u24.log`, taken by [`./scripts/capture.sh`](../scripts/capture.sh) so the log names the rev it ran on. Quote the capture and not a remembered count: the `pdl.3` run was 149/149 (`spikes/results/shutdown-e2e-pdl3.log`), and the 85/112 whose 27 failures were the ticket is `spikes/results/shutdown-e2e-pdl3-control.log`. Until `looprs-00u.23` this cell quoted a count that matched nothing in the tree, which is what the gate now refuses |

The spike keeps its **own** ledger of the wire (`ModeTrace`) instead of reading the app's:
"not in the alternate screen after exit" is not something the app may be asked at exit —
asking is a `DECRQM`, and the exit path is forbidden from asking — and there is no terminal
emulator behind a pty to answer one anyway. What was written is the whole truth, so the
driver folds the capture the way a terminal would, and counts the leaves rather than only
the final state: a mode turned off twice and one turned off once end in the same place, and
only one of those was promised.

### A leak check measures the run, not the machine (`looprs-00u.24`)

The same discipline applies to the process table, and the same mistake was living there.
Every "no child survived" check in this spike used `pgrep -f`, which answers *"is anything
on this box running a command line that looks like X?"* — not *"did the binary we just ran
leak?"*. On a machine with abandoned runs under it the two come apart:

| Run | Machine | Result |
| --- | --- | --- |
| old spike, clean session | nothing stale planted | 171/171 (`spikes/results/shutdown-e2e-00u23.log`) |
| old spike, dirtied with 3 planted stale shells | debris from *earlier* runs | **168/171** — `no bash from the generated rcfile survived`, `no shell from our rcfile outlived the app`, `no stray \`sleep 30\` from that shell` (`spikes/results/shutdown-e2e-00u24-globalpgrep-control.log`) |
| new spike, dirtied the same way | same debris, same binary | 189/189, the dirt counted at the top of the log: `stale leftovers from earlier runs: 18 orphaned process(es) killed, 3 generated rc file(s) removed` (`spikes/results/shutdown-e2e-00u24-stale.log`); replanted and run again, it cleared 6 processes and 3 rc files and passed 189/189 too (`spikes/results/shutdown-e2e-00u24-stale-again.log`) |

The old failures were not the build's. The check had no way to tell this run's children from
someone else's, so it reported the machine's history as a verdict on the binary — which also
means the reverse: on a machine that noisy, a real leak hides in the same pile. Either way
the result was not attributable to the thing under test.

The fix has three parts, and the third is the one that keeps the check honest:

1. **Scope to the run.** `RunLedger` records the pids under the app *while the app is
   alive*, because the instant it dies its children are reparented to pid 1 and no later
   scan can give them back to us. A driver records continuously (a child appears whenever a
   keystroke says so — a tab change starts a shell, that shell forks a `sleep`) and once more
   synchronously just before the quit key, the signal, or the kill.
2. **Reap what is provably orphaned, and say how much.** The generated rc file is named
   `looprs-bash-integration-<owner pid>-<seq>.sh`, so "is the app that made this still
   running?" is answerable from the argv alone. `reap_stale_leftovers` kills those shells and
   their subtrees and unlinks their rc files — **liveness of the owner pid is the only test**,
   so a running looprs, whether the user's own or another window's spike, is never touched —
   and the count lands in the log so a dirty machine is visible instead of fatal.
   `LOOPRS_NO_REAP=1` turns the reaper off.
3. **Prove the check can still fail.** A check scoped to a recorded set passes exactly as
   quietly when the recording is broken as when the run is clean, so
   `scenario_leak_is_named` makes a real leak — `SIGKILL`, the one exit no `Drop` kill,
   no reaper thread and no rc-file removal covers — and requires the scoped check to name the
   pid it recorded, the shell from the generated rc file and the busy child inside it, before
   reaping what it leaked. Plant the dirt with
   [`./scripts/plant_stale_debris.sh`](../scripts/plant_stale_debris.sh).

Run it:

```sh
cargo build
python3 spikes/shutdown_e2e.py | tee spikes/results/shutdown-e2e-pdl3.log
python3 spikes/shutdown_e2e.py alt child leak sigterm sighup panic   # one group at a time

# dirty the machine the way an abandoned run does, then run the spike over it
./scripts/plant_stale_debris.sh 3     # or: LOOPRS_NO_REAP=1 to leave the debris alone
python3 spikes/shutdown_e2e.py | tail -1   # the pass count does not move; the reap line says how much it cleared

# the control: the same spike against the pre-looprs-pdl.3 binary
git worktree add --detach /tmp/looprs-pdl3-ctrl HEAD
(cd /tmp/looprs-pdl3-ctrl/pi-beads/looprs && cargo build --target-dir /tmp/base-target)
LOOPRS_BIN=/tmp/base-target/debug/looprs python3 spikes/shutdown_e2e.py \
    | tee spikes/results/shutdown-e2e-pdl3-control.log
```

| Scenario | What is held at the moment of leaving | Expected |
| --- | --- | --- |
| Ctrl-Q mid-stream | `raw`, `alt_screen`, `cursor_hidden` | the ledger's leaves and then the leave itself; **nothing at all is written after `?1049l`** and no closing newline (that newline belonged to the inline pane's last row); no mouse/paste leave for a mode nothing switched on |
| `LOOPRS_MODES=all` + Ctrl-Q | every mode in the table | all taken, each left exactly once, and `?1049l` is the **last byte** the app writes |
| `SIGTERM` with the whole set | every mode | leaves by itself in ~0.3 s, code 0, nothing left on, tty cooked |
| `SIGHUP`, default modes | `raw`, `alt_screen`, `cursor_hidden` | same |
| `LOOPRS_PANIC=draw` | every mode, panicked inside the frame | each mode still left exactly once, exit code 101, tty cooked |
| full-screen child killed while it holds the screen | nothing: the frame hosts the alternate screen, so the child's `?1049h` is **cut** and replaced by the canvas (ADR-0001 amendment 4) | the child painted on the screen it was handed; one `?1049h` in the whole run (the app's own); **no debt to pay and no leave from the session**; exactly one `?1049l`, at exit; nothing after it; tty cooked |
| `SIGKILL` over a live shell (`leak`) | a child this run spawned, deliberately orphaned — no `Drop` kill, no reaper, no rc-file removal | **the leak check fires**: the recorded shell from the generated rc file and the busy child inside it are both named by pid; the scenario then reaps its own leak and ends clean. This is the non-vacuity half — a scoped check that never fired on a real leak would be passing on an empty list |

**Rewritten by `pdl.4`, not silenced.** `flash_e2e.py` used to be this file's named
exception — 0/3, 25 reshapes against a ≤19.5 budget, worst hole ~3.6 ms — and it stayed that
way through `pdl.3` because the thing it measured was the inline pane's erase/paint split.
The frame has no such split: it diffs the whole screen and writes once per frame, so the
partial erase whose gap was the measurement is now something the binary is required **not**
to emit. It passes **4/4** here (0 partial erases; no `ESC[2J` that needed timing; two
non-vacuity checks proving the stream actually ran). The control inverted with it: against the
pre-pdl.4 binary the same script fails with **26 partial erases, 1.83–3.58 ms**
(`spikes/results/flash-e2e-pdl4-control.log`), which is the number the old failure was
trying to catch. `fullscreen_e2e.py`, 18/21 on the vim-keystroke needles through `pdl.3`, is
**70/70** with the frame hosting the alternate screen — the child paints on a canvas it is
handed instead of switching a screen under us, which is what those needles needed all along.
The pre-pdl.3 numbers stay above as the ledger of what regressed when; they describe binaries
these spikes were not written for.

**Known gap this ticket names instead of fixing:** a real full-screen child also leaves its
*own* switches on in the user's terminal — `?2004h` (bracketed paste) always, and
`?1000`/`?1002`/`?1006` with a mouse-tracking vim. The alt screen is paid back because it has
a defined thing to return to (the main screen, cursor included); those have to return to
whatever the user's terminal was doing before looprs started, which the exit path does not know
and is forbidden to ask. The honest shape is to read them at **startup** with `DECRQM` — one
round trip at boot, where a round trip is affordable — and restore that on the way out. The
`child` scenario prints what is left behind (`tee'd modes still on at exit: [...]`) so the gap
stays measured; see [ADR-0006](adr/0006-terminal-mode-ledger.md).

### In-app scrollback (looprs-pdl.6)

**What it proves:** the transcript is a *store* now rather than a print stream,
and the four things the rest of the epic leans on are true of it at every layer:
a display row knows which entry it came from and whether it ends in a hard
newline or a soft wrap; a pinned view follows the tail and an unpinned one
holds; the "N new" affordance says what is unseen and how to get back to it; and
a resize re-wraps against the content the view was resting on rather than the
row index it happened to be at.

| Layer | Where | What it pins |
| --- | --- | --- |
| the store | `state::scrollback::tests` | pinned by default and output follows (`pinned_is_the_default_and_new_output_follows`), one row up unpins (`scrolling_up_one_row_unpins`), off the tail it holds and counts (`unpinned_holds_its_content_and_counts_what_arrived`), the very bottom re-pins and clears the count (`reaching_the_very_bottom_re_pins_and_clears_the_count`), the top clamp (`cannot_scroll_past_the_top_of_the_content`), provenance per row (`every_row_carries_its_provenance`), the paste join (`joining_rows_follows_the_hard_soft_rule`), the cell map across CJK, combining marks and a ZWJ family (`the_cell_map_never_splits_a_cluster`, `a_combining_mark_belongs_to_its_base`), styles kept on the row and never copied (`styles_render_and_are_never_copied`), re-wrap under a pin and against a content anchor (`rewrap_keeps_a_pinned_view_pinned`, `rewrap_anchors_on_content_not_on_row_index`), a re-wrap whose anchor was trimmed away holding rather than inventing a position (`rewrap_with_lost_content_holds_rather_than_inventing_a_position`), the byte cap and eviction renumbering (`the_byte_cap_drops_oldest_entries_whole_and_says_so`, `eviction_drops_gone_entries_and_renumbers_the_rest`) |
| the flush | `session::view::tests` | the store's width follows the flush and a re-wrap *makes* the rows rather than adding to them (`a_rewrap_makes_the_rows_again_rather_than_adding_to_them`), no row outlives the entry that can re-render it (`eviction_leaves_no_row_the_transcript_cannot_re_render`), arrivals off the tail are counted and do not move the view (`rows_that_arrive_off_the_tail_count_themselves_and_do_not_move_the_view`) |
| the plumbing | `app::tests` | a page is the band the frame lays out, from the same function and not a second arithmetic (`a_page_is_the_band_the_frame_lays_out`), scrolling is neither a round trip nor the box's (`scrolling_is_not_a_round_trip_and_not_a_keystroke_anyones_else`), a transcript shorter than the band has nowhere to scroll to and cannot unpin (`with_nothing_above_the_band_there_is_nowhere_to_scroll`), a resize re-wraps without doubling content (`a_resize_rewraps_the_store_without_doubling_or_losing_content`) |
| the paint | `main::tests` (ratatui `TestBackend`) | the live tail shows only while the view follows the tail (`the_live_tail_shows_only_while_the_view_follows_the_tail`), the pill appears off the tail and only then, on the band's bottom row (`the_new_rows_pill_shows_while_off_the_tail_and_only_then`), a resize keeps the resting line on the screen (`a_resize_keeps_the_line_the_user_was_looking_at_on_screen`) |
| the real pty | `spikes/scrollback_e2e.py` | 23 checks: `ESC[5~`/`ESC[6~`/`ESC[H`/`ESC[F` decode to the four keys the app matches on through a real pty with a real bash, the pill is legible on the band's bottom row, a held band does not move by one row across two arrivals, and the app survives a resize taken with the transcript scrolled up |

```sh
cargo build
python3 spikes/scrollback_e2e.py | tee spikes/results/scrollback-e2e.log
```

**What the spike could not settle, and says so instead of skipping quietly.** Two
things. First, this harness has no reflowing emulator, so "the text the user was
looking at stayed on the screen across a resize" is *not* claimed from the wire;
it is claimed from the painted test above, where the screen model is ratatui's
own buffer. Second, and worth more than a footnote: with the app **idle**, a
resize produces **zero bytes** out of the app over 2 seconds — the spike prints
the count on every run — and the pre-pdl.6 binary at `195e3c0` measures the
same, so the scrollback work neither fixed nor broke it. A window drag on a
quiet session leaves the old frame until something else makes the app draw. The
app is not ignorant of the new size (its next drawn frame adopts it; the frame
reads the window at draw time), but the draw is gated on `app.dirty` in the run
loop. That is `looprs-pdl.15`, filed from this measurement so the next one
starts here.

**Settled by `pdl.15`, and the measurement was the thing that was wrong.**
That zero is not the app ignoring a resize: `SIGWINCH` goes to the foreground
process group of the terminal's *session*, and a spike that `Popen`s into a pty
without a `setsid`/`TIOCSCTTY` never puts the app in that group, so the signal
never arrives. The same check against an attached child repaints on the
pre-pdl.6 binary too. The app has since stopped depending on the signal at all —
see the section below — and the paragraph above stays exactly as `pdl.6` wrote it,
because it is the record of the measurement that filed the ticket.

### A window drag on a quiet session (looprs-pdl.15)

**What it proves:** the app adopts the real window whether or not a `SIGWINCH`
reaches it, the adoption re-wraps the transcript, and it reaches the *children*
too — the full-screen program on the other side of the passthrough gets its pty
resized, which nothing but this app can do for it.

| Layer | Where | What it pins |
| --- | --- | --- |
| the poll | `viewport::tests` | a window the app does not have is reported once, and the same window after adoption is not a change (`a_window_the_app_does_not_have_is_reported_once`); sixty ticks of an unchanged window ask for nothing (`a_steady_window_asks_for_nothing_for_sixty_ticks`); **one unreadable size retires the poll and it never touches the size source again** — the guard against crossterm's `tput` fallback forking twice a frame (`a_size_that_cannot_be_read_retires_the_poll_for_good`); `0 × 0` is refused without retiring the poll (`a_degenerate_window_is_refused_and_the_poll_survives_it`) |
| the adoption | `app::tests` | the poll and the `Resize` event leave the same state *and* send the same `UiCommand::Resize` — so the poll cannot repaint the frame while leaving a child pty wrapped for a window that no longer exists (`the_size_poll_adopts_what_the_resize_event_adopts`); `Msg::Term(Event::Resize)` now routes through `set_window` instead of assigning the two fields inline, which is what makes that comparison a statement about one door rather than two |
| the real pty | `spikes/resize_e2e.py` | **53 checks, 6 groups**, and a control whose 5 repaint claims all fail on the pre-ticket binary |

```sh
cargo build
python3 spikes/resize_e2e.py                  | tee spikes/results/resize-e2e.log
python3 spikes/resize_e2e.py drag held        # one group at a time

# the control: the pre-looprs-pdl.15 binary, same script
git worktree add --detach /tmp/looprs-pdl15-ctl HEAD
(cd /tmp/looprs-pdl15-ctl/pi-beads/looprs && cargo build --target-dir /tmp/pdl15-target)
LOOPRS_BIN=/tmp/pdl15-target/debug/looprs python3 spikes/resize_e2e.py --control \
    | tee spikes/results/resize-e2e-control.log
```

**Why every group runs against two harnesses.** The ticket can only be settled
by the difference between them. `bare` is `Popen(stdin=slave)` — the shape of
every other spike here, and the one that filed this ticket; no `SIGWINCH` will
ever arrive. `attached` is `setsid()` + `TIOCSCTTY`, the relationship a real
terminal window has with the program inside it. In `attached` the pre-fix binary
repaints, which is the evidence that `App::set_window`, the `Event::Resize` arm
and the `dirty` gate were never the bug — so every check that group makes is
`app`-kind and is *not* counted as evidence about the poll. In `bare` a repaint
can only have come from the `ioctl`, which is what makes those checks mean
something.

| Group | What is dragged, and what has to be true after |
| --- | --- |
| `untouched` | the ticket's own case: idle, no controlling terminal, 100 → 120 columns; repaint inside one tick, the box border on the new edge, back to silence after |
| `attached` | the same drag with the signal actually arriving — the control *on the harness* |
| `rewrap` | settled prose that fits one row at 120 comes back as two at 60: the tail is no longer on the head's row, and is on a row below it |
| `drag` | six sizes in ~250 ms, because a drag is a burst and the window has to be **read**, not accumulated; the frame ends at the last size, not one from the middle |
| `scrolled` | the transcript is off the tail and idle; the drag repaints anyway, and `End` still gets back afterwards |
| `held` | a full-screen child owns the screen; the child reports its **own** `stty size` on the way out, and it says the new window |

**What the control caught, and what it taught the two checks that leaked.** The
first control run reported two `adopt`-kind needles passing on the pre-fix
binary. "`End` still reaches the tail across the resize" is true pre-fix because
`End` is a keystroke, and a keystroke always made the app draw — that is the
very mechanism the ticket says the resize had to wait behind, so it proves the
app survived, not that the resize was seen. "The app repaints its own frame when
the child hands the screen back" is true pre-fix because `repaint_all` already
existed on that path and ratatui's `autoresize` picks the new frame area up by
itself — chrome moves even when nothing wrapped does. Both are now `app`-kind,
and the control's verdict is the thing that caught them.

**What this spike cannot show, stated rather than skipped.** The `Screen` grid is
a reconstruction, not a reflowing emulator, so "the user kept the line they
were looking at" is not claimed here; it is claimed in
`a_resize_keeps_the_line_the_user_was_looking_at_on_screen` (`src/main.rs`),
where the screen model is ratatui's own buffer. And **raw shell output is not
re-wrapped at any width**: `MessageKind::Bash` is ended by the child and
ADR-0001 rule 1 / ADR-0005 forbid re-wrapping it, so a long shell line is cut
at the right edge before this ticket and after it. The `rewrap` group therefore
makes its claim on prose — the `Answer` kind, which is what
`Scrollback::rewrap` re-renders — and leaves the contrast on the record instead
of quietly testing only the case that passes.

### The kanban band (looprs-5o4)

**What it proves:** the board in the frame is a rendering of one consistent `bd`
read in which every bead is counted exactly once, it is paid for out of the
transcript's surplus and nobody else's, and it is **zero rows** — not hidden,
not blank — in every frame that is not beads mode. The user-facing half is
[`docs/kanban.md`](kanban.md); this is the half about how it gets checked
without a board, a database or a clock.

| Layer | Where | What it pins |
| --- | --- | --- |
| the mapping | `state::board::tests` | the mapping **is** ADR-0007 §1's table, asserted rather than assumed (`the_status_to_column_mapping_is_the_adrs_table`); invariant I1 — every bead in the read counted exactly once (`every_bead_in_the_read_is_counted_exactly_once`); `deferred` is a count and never a row (`deferred_is_a_count_and_never_a_row`); a bead with no `status` field is a marked `?`, not an `open` (`a_bead_with_no_status_field_is_a_marked_unknown_not_an_open_row`); empty / never-loaded / broken are three values, not one look (`loading_an_empty_board_and_a_broken_bd_are_three_different_values`); the header count is the column's true total (`the_header_count_is_the_columns_true_total`) |
| the poller | `services::board_poller::tests` | the knobs resolve leniently and **loudly**, default 5 s and floor 250 ms (`the_poll_interval_knob_resolves_leniently_and_loudly`, `the_default_is_the_adrs_five_seconds_and_a_bd_on_the_path`); only an explicit `0/off/no/false` takes the board off (`only_an_explicit_no_turns_the_board_off`); the env is read once, from one binary (`the_environment_configures_the_board_it_is_read_from_and_once`) — against `tests/fixtures/fake_bd.sh`, never a real board |
| the row budget | `viewport::tests` | zero or a whole board, never a stub (`the_band_is_zero_or_a_whole_board_never_a_stub`), the threshold and the ceiling (`the_board_appears_at_its_threshold_and_stops_at_its_ceiling`), and the failure that must be unrepresentable: turning the board on costs **only** the transcript's surplus — the box, the cards and the status row keep their rows and their places (`turning_the_board_on_only_ever_costs_the_transcript_its_surplus`) |
| the widget | `components::kanban::tests`, against a `TestBackend` | the overflow marker is honest at 1, 2, 3 and 5 rows (`the_overflow_marker_is_honest_at_one_two_three_and_five_rows`), the marker survives the width cut (`at_one_column_wide_the_marker_is_what_survives`), every read state says its own thing in the footer (`every_read_state_says_its_own_thing_in_the_footer`), and a failed read keeps and dims the last good rows (`a_failed_read_keeps_the_last_good_rows_and_says_its_own_words`) |
| the wiring | `src/main.rs` tests | the band sits directly above the status row and below the cards (`the_band_sits_directly_above_the_status_row_and_below_the_cards`), `Off` outside beads mode leaves the frame byte-identical (`the_band_is_zero_rows_outside_beads_mode_and_leaves_the_frame_untouched`), a tab out and back restores it exactly (`tab_out_of_beads_takes_the_band_off_and_tab_back_puts_it_exactly_back`), and **an unchanged poll draws nothing** while a moved board draws once (`an_unchanged_poll_draws_nothing_and_a_moved_board_draws_once`) — the repaint half of the poller's contract, enforced where the snapshot is adopted |

**No clock, no sleep, no board.** Every test above hands in its own `Instant` and
its own `Duration` (`BoardSnapshot::stamped_at` / `restamp_age`,
`KanbanBudget::from_raw(Option<&str>)`, `kanban_rows(h, tools, input, budget)`),
and the `bd` in the loop is `tests/fixtures/fake_bd.sh`. "How old is this read"
and "how tall is the band" are therefore functions of their arguments, so the
aging behaviour the footer shows is testable without waiting five seconds for it —
and the `LOOPRS_*` branches are all reachable without mutating a process the
rest of the suite shares.

## The mouse and the clipboard, measured before they are built (looprs-pdl.2)

**What it proves:** the seven claims that looprs-pdl.8/.9/.10/.12 inherit — that a
selection can be driven from outside, that the clipboard round-trips, what the OSC
52 ceiling is, what a burst costs, whether shift-drag survives, that the modes come
back, and what vim does to our mouse capture — each have a number or a yes/no
attached, taken off a wire. The per-terminal table lives in
[`spikes/results/terminal-matrix.md`](../spikes/results/terminal-matrix.md) and is
what ADR-0004 (looprs-pdl.1) lifts.

| Layer | Where | What it pins |
| --- | --- | --- |
| the wire → the library | `spikes/mouse_clipboard_e2e.py inject` | 13/13 SGR report types → one event each, kind+button correct; the wire's 1-based coordinates map to crossterm's cells at a constant **(1, 1)** with no variance; the modifier translation is measured, and it is **not** an orderly shift — wire `0x04`→SHIFT, `0x10`→CONTROL, `0x08`→ALT |
| the path's capacity | `burst` | 128 reports in one 1,536-byte write → 128 events, 0.30 ms span, worst gap 0.037 ms, ~4×10⁵/s, nothing merged or dropped |
| the hand-back | `modes` | the real binary with `LOOPRS_MODES=all`: every mode taken, each left exactly once, nothing still on, `lflags` identical to the clean `stty` baseline, cursor visible — and the one exemption measured rather than waived (`cursor_hidden`: on 1×, cleared 3×, because a frame that ends without a cursor writes `?25h` every draw) |
| the nested child | `vim` | vim `-u NONE` leaves our three mouse modes ours and on; vim with `:set mouse=a` **switches all three off on exit**; SIGKILLed vim leaves alt screen + bracketed paste + mouse modes on with nothing restored |
| our own bytes | `clipboard` | `crossterm`'s writer emits `ESC]52;c;<b64>ESC\` (ST, not BEL) and decodes back to the exact payload, including a copy written in 7 chunks |
| the emulator, in a real window | `--in-terminal` | DEC mode queries answered by the emulator itself; an OSC 52 size ladder checked against the real clipboard with a latency proxy for permission prompts; chunked reassembly; the read-back query |
| the hop | `--ssh`, `--ssh-emulator` | mouse reports injected here decode on the far side with the same mapping, and a copy issued on the remote host lands in the **local** emulator's clipboard whole |

**Why the emulator leg is a separate leg and not folded into the pty groups.** A
pty has no terminal emulator behind it. What an app writes is the whole truth about
the app and none of the truth about the user's clipboard, because the thing that
interprets OSC 52 is the window. A bare pty that never answers `ESC]52;c?` and a
terminal that refuses the copy look identical from the writer's side — so the pty
groups report *what we send* and the leg reports *what an emulator does*, and the
summary lists each separately. That is also why the leg starts with a positive
control on the query path (Primary Device Attributes, `CSI c`): without it, "this
terminal does not have the mode" cannot be told apart from "nobody is listening".
Apple Terminal answered DA1 and then answered nothing else — which is how "no OSC
52, no mouse reporting, selection is the terminal's own" became a measurement
instead of folklore.

**The controls, and what each one rules out**: `--decode-off` (same tty, same raw
mode, same `?1000h/?1002h/?1006h` on, no parser — so any "event" there came from
somewhere other than the injected sequence, and the byte count proves the tty
carried all of them); malformed SGR with the `<` dropped (no press/drag/release
chain forms, and nothing arrives as keystrokes — the failure mode worth knowing, since
keystrokes land in the input box); a +5 coordinate differential (the reported
numbers track the wire, not the constants in this script); `kill -9` on the app
(the mode detector must see residue when there is residue, or its clean verdicts
mean nothing); and the same base64 payload under **OSC 51** (a clipboard that
changed on *that* changed for a reason other than our sequence).

**What this spike deliberately does not claim.** Three cells are printed as `N/A`
in every run rather than guessed, and they are the ones a person with hands and other
terminals has to fill: the shape of one physical **trackpad flick** (no finger on
this path — `--record-flick` exists and asks for three), whether **shift-drag still
paints the terminal's own selection** under 1000+1002+1006 (nothing comes back up
the pty; the leg asks a human for a `y`/`n`), and **what the user sees while vim
holds our alternate screen** (the pty has no window). Two terminals on the ticket's
list are not installed on this machine, so their rows are empty with the one-line
command that fills them (`PDL2_LAUNCH='open -a iTerm {file}' … --launch`); the row
for the agent's own terminal (Zed) is left unfilled on purpose, because writing the
leg's escape sequences into the running session's tty would smear the UI.

Run it:

```sh
cargo build --examples
python3 spikes/mouse_clipboard_e2e.py           | tee spikes/results/mouse-clipboard-e2e.log
python3 spikes/mouse_clipboard_e2e.py --control | tee spikes/results/mouse-clipboard-e2e-control.log
python3 spikes/mouse_clipboard_e2e.py ssh       | tee spikes/results/mouse-clipboard-ssh.log
python3 spikes/mouse_clipboard_e2e.py burst shift          # one group at a time
python3 spikes/mouse_clipboard_e2e.py --in-terminal --record-flick   # inside a real window
```

---

## Why "pure" matters here, once

A condition inside an `async fn` can only be exercised by the trajectory that
reaches it, so "the guard holds when the mode is hidden" ends up meaning "this one
test did not trip it". The guards are the money-safety story of this harness — one
pass at a time, a claim before a spawn, a cancel that cannot fire twice, a stall
timer that cannot answer for a pass it did not arm. Each is a handful of boolean
operators, and each one being wrong costs money or kills work.

So the decisions are functions of plain arguments (`PassGate::allows`,
`StepCause::step`, `abort_is_redundant`, `stall_timer_is_live`,
`BeadStep::awaits_user`, `pick_bead`), the `async` code above them is an
interpreter, and the tables are in the test module where a miss says which promise
broke rather than which bit flipped.
