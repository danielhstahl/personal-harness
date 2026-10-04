# Testing `looprs`

One entry point, no network, no model calls, no real `bd` database.

```bash
cargo test              # the behaviour: 208 tests, ~20s
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

The same gate is declared three ways, because each one catches a different kind of
person:

| where | what it guarantees |
| --- | --- |
| `Cargo.toml` `[lints.clippy] all = "deny"` | `cargo clippy` alone is a gate; nobody gets to "forget" `-D warnings` |
| `scripts/check.sh` | one command that means "this branch is mergeable" |
| `tests/warning_gate.rs` (`--ignored`) | reachable from `cargo test` without leaving the cargo mindset |

CI runs `scripts/check.sh` (`.github/workflows/looprs-gate.yml`, at the repo root,
path-filtered to `pi-beads/looprs/**`).

The one warning that is not ours: `nix v0.28.0` future-incompatibility, pulled in by
`portable-pty`. It is a note, not a failure. It is documented in
[ADR-0001](adr/0001-bash-terminal-state-pty.md) rather than silenced, because the
gate's claim is "everything except this one named thing is clean".

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
| `Fakes::set_board(json)` | change what `bd ready` / `bd list` print, without restarting |
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

- `session::beads::tests::a_non_empty_board_self_starts_a_prompted_worker`
- `session::beads::tests::an_empty_board_parks_without_spawning`
- `session::beads::tests::constructing_a_loop_spawns_nothing`
- `session::beads::tests::a_pi_that_dies_during_startup_is_reported`

### Settle routing (looprs-msj)

- `session::beads::tests::the_loop_takes_its_next_pass_from_its_own_workers_settle`
- `session::beads::tests::a_settle_from_a_pass_this_loop_does_not_own_moves_nothing`
- `session::beads::tests::driving_the_loop_reaps_the_previous_worker`
- `session::router::tests::the_router_has_no_beads_advance_path_left`
- `app::tests::no_settle_ever_turns_into_a_command_from_the_app`

### Claim / close guard (looprs-w7q)

- `session::beads::tests::the_harness_claims_the_ticket_it_is_paying_for`
- `session::beads::tests::a_claim_bd_refuses_buys_no_worker`
- `session::beads::tests::a_ticket_that_is_never_closed_stops_the_loop_instead_of_spinning`
- `session::beads::tests::the_active_ticket_is_published_when_taken_and_when_released`
- `session::beads::tests::the_worker_is_told_its_ticket_not_invited_to_go_shopping`
- pure: `session::beads::tests::the_claim_guard_skips_refuses_and_works_in_that_order_pure`

### Pi multi-turn persistence (looprs-ctn)

- `session::pi_chat::tests::two_turns_share_one_child_and_the_second_remembers_the_first`
- `session::pi_chat::tests::a_message_during_a_run_steers_instead_of_starting_another_run`
- `session::pi_chat::tests::killing_the_child_says_so_and_the_next_message_restarts_it`

### Bash echo / exit / cwd persistence (looprs-553)

- `session::bash::tests::echo_streams_its_output_and_reports_exit_0`
- `session::bash::tests::a_failing_command_is_loudly_not_zero`
- `session::bash::tests::cwd_persists_across_commands`
- `session::bash::tests::exported_variables_persist_across_commands`
- `session::bash::tests::stderr_and_stdout_arrive_in_the_programs_own_order`

### Esc cancellation (looprs-5g7)

- `session::cancel::tests::the_grace_is_a_warning_window_not_a_hope`
- `session::cancel::tests::arm_delivers_the_command_after_the_grace`
- `session::bash::tests::esc_interrupts_a_running_command_without_killing_the_shell`
- `session::bash::tests::esc_says_cancelling_before_the_command_reports_itself_done`
- `session::pi_chat::tests::esc_clears_the_queue_before_aborting_and_gives_the_words_back`
- `session::pi_chat::tests::a_pi_that_ignores_the_abort_is_killed_said_so_and_the_mode_recovers`
- `session::beads::tests::a_worker_that_ignores_the_abort_is_killed_and_leaves_the_bead_named`
- `session::router::tests::cancel_reaches_the_active_session_only`
- pure: `session::beads::tests::esc_is_absorbed_while_a_cancel_is_unwinding_and_only_while`
- pure: `session::beads::tests::a_stall_timer_only_answers_for_the_attempt_that_armed_it`

### `bd` missing / failing surfaces (looprs-037)

- `services::bd::tests::a_missing_bd_is_named_not_gossiped_around`
- `services::bd::tests::a_failing_bd_is_not_an_empty_board`
- `services::bd::tests::silence_from_bd_is_malformed_never_empty`
- `session::beads::tests::a_failing_bd_is_reported_and_parks`
- `session::bash::tests::a_missing_path_shows_its_stderr`

### The pure state machines (this chore's first bullet)

No subprocess, no fake, no channel wait — just the decision layer, enumerated.

**Mode table (Bash / Beeds / Pi × Tab)**

- `session::tests::tab_walks_the_whole_mode_table_and_back_to_where_it_started` — one
  3-cycle, one-in-one-out per mode, three Tabs = one lap
- `session::tests::every_row_of_the_mode_table_names_where_tab_goes_and_what_it_leaves_behind`
  — mode → Tab target → `SwitchAway` policy, one row per mode, and only Beads parks
- `session::tests::only_await_input_hands_the_keyboard_back_to_the_human`

**Beads step machine (`AwaitInput → CreateTickets → WorkTickets → AwaitInput`)**

- `session::beads::tests::every_step_cause_lands_on_one_step_and_says_so` — the
  table, and the fact that a transition is *published* and not merely stored
- `session::beads::tests::the_machine_walks_await_plan_work_and_home_again`
- `session::beads::tests::the_step_table_is_a_bijection_over_the_whole_enum` — no
  orphan step, no unmapped cause, no two causes claiming one step

The machine takes a `StepCause` (`Planning` / `Working` / `Awaiting`) rather than a
`BeadStep`: there is no `set_step(anything)`, only `set_step(why)`, so the table
lives once instead of being re-implied at six call sites.

**Guards**

- `session::beads::tests::the_pass_gate_has_exactly_one_open_row_in_sixteen` — the
  one gate in front of "start a pass", all 16 rows, exactly one yes
- `session::beads::tests::each_flag_closes_the_pass_gate_by_itself`

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
