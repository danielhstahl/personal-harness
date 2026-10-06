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
| the board says closed | `session::beads::tests::a_closed_ticket_is_announced_once_with_the_title_it_was_claimed_under` |
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
| S1 empty board | loop waiting on a human | `awaiting input · Tab switch · ^C quit`, nothing flushed to the alt screen |
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
| the wire shape parses as pi writes it — camelCase, every field independently optional, **absent ≠ zero** | `app::tests::the_wire_usage_record_parses_as_pi_writes_it` |
| the same shape through a real child's stdout, not a hand-written string | `session::pi_chat::tests::the_wire_usage_record_survives_the_real_pipes` |
| only an assistant `message_end` adds; a `user` / `toolResult` message and a usage-less message add nothing | `app::tests::every_assistant_message_adds_and_nothing_else_does` |
| the beads window opens on a claim and **survives the release** | `app::tests::the_beads_window_opens_on_a_claim_and_survives_the_release` |
| a respawned generation owes nothing for the dead one's run | `app::tests::a_new_generation_starts_the_window_at_nothing` |
| nothing reported prints nothing, rather than `↑0 ↓0` | `app::tests::the_row_prints_the_window_it_is_handed` |
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
signal for the buffer cap, and that signal still exists where the loss happens: the
view inserts a `… N bytes dropped (buffer cap) …` notice into the transcript
itself (`session::view::tests`). The row spends the room on cost instead.

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
| the wire shape parses as pi writes it: `reason` on the start, `result.tokensBefore` / `estimatedTokensAfter` on the end, and no `result` at all when it was aborted | `app::tests::the_compaction_wire_format_parses` |
| the start puts a live card on the screen, and the height policy is told to budget its row | `app::tests::a_compaction_shows_a_live_card_while_it_runs` |
| the finished card reaches the scrollback carrying what it freed | `app::tests::a_finished_compaction_reaches_the_scrollback_with_what_it_freed` |
| cancelled and failed are two different sentences, and a cancel is not painted in failure's red | `app::tests::an_aborted_compaction_says_aborted_and_a_failed_one_says_why`, `components::compaction::tests::aborted_is_grey_and_failed_is_red` |
| the card is drawn in the frame's card band, and the two endings draw differently | `main::tests::a_live_compaction_is_drawn_in_the_card_band`, `main::tests::a_cancelled_and_a_failed_compaction_are_drawn_differently` |
| an `end` whose `start` never arrived is recorded rather than swallowed | `app::tests::a_compaction_end_with_no_open_card_is_recorded_anyway` |
| closing the card releases everything that queued up behind it | `app::tests::closing_the_compaction_card_releases_what_came_behind_it` |
| a missing `reason` leaves no dangling separator on the row | `components::compaction::tests::an_unknown_reason_leaves_no_trailing_separator` |

**A card left open when the session dies is a bug with teeth.** A `!done` entry
stalls the flush cursor, so a compaction — or a tool — that was still in flight
when the child died would take the rest of that session's transcript with it:
exactly the tail the exit drain exists to collect. `SessionView::seal` now closes
open cards of either kind as `Aborted` —
`session::view::tests::sealing_closes_cards_left_running_so_the_transcript_keeps_flushing`,
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
| the real pty | `spikes/shutdown_e2e.py` | 8 scenarios, 149 checks, and a control run at 85/112 whose 27 failures are the ticket |

The spike keeps its **own** ledger of the wire (`ModeTrace`) instead of reading the app's:
"not in the alternate screen after exit" is not something the app may be asked at exit —
asking is a `DECRQM`, and the exit path is forbidden from asking — and there is no terminal
emulator behind a pty to answer one anyway. What was written is the whole truth, so the
driver folds the capture the way a terminal would, and counts the leaves rather than only
the final state: a mode turned off twice and one turned off once end in the same place, and
only one of those was promised.

Run it:

```sh
cargo build
python3 spikes/shutdown_e2e.py | tee spikes/results/shutdown-e2e-pdl3.log
python3 spikes/shutdown_e2e.py alt child sigterm sighup panic   # one group at a time

# the control: the same spike against the pre-looprs-pdl.3 binary
git worktree add --detach /tmp/looprs-pdl3-ctrl HEAD
(cd /tmp/looprs-pdl3-ctrl/pi-beads/looprs && cargo build --target-dir /tmp/base-target)
LOOPRS_BIN=/tmp/base-target/debug/looprs python3 spikes/shutdown_e2e.py \
    | tee spikes/results/shutdown-e2e-pdl3-control.log
```

| Scenario | What is held at the moment of leaving | Expected |
| --- | --- | --- |
| Ctrl-Q mid-stream (inline) | `raw`, `cursor_hidden` | erase at the published anchor, the ledger's leaves, **one** newline — leaves before the newline, not after it; no `?1049l`/mouse/paste leave for a mode nothing switched on |
| `LOOPRS_MODES=all` + Ctrl-Q | every mode in the table | all taken, each left exactly once, and `?1049l` is the **last byte** the app writes |
| `SIGTERM` with the whole set | every mode | leaves by itself in ~0.3 s, code 0, nothing left on, tty cooked |
| `SIGHUP`, inline defaults | `raw`, `cursor_hidden` | same |
| `LOOPRS_PANIC=draw` | every mode, panicked inside the frame | each mode still left exactly once, exit code 101, tty cooked |
| full-screen child killed while it holds the screen | the *child's* `?1049h`, passed through the tee and never left | the user is not in the alternate screen at the end; exactly one `?1049l`; the tail after that leave is the ordinary inline hand-back (erase → the ledger's own bytes → one newline); tty cooked |

**Known pre-existing failures, not this ticket's.** `flash_e2e.py` is 0/3 (25 reshapes where
≤19.5 are allowed, worst hole ~3.6 ms against a 1.5 ms budget), and `fullscreen_e2e.py` is
18/21 — the three failing checks are the vim-keystroke needles (`Esc` reaching the screen while
the program holds it, `:wq!` writing the file, the typed text landing at the cursor). Both fail
with **identical** numbers against the pre-looprs-pdl.3 binary (`flash_e2e` 25 reshapes /
3.54 ms; `fullscreen_e2e` 18/21, same three names), so they regressed before this branch and
nothing here touches the frame path or the keystroke path. Written down rather than silenced,
per the rule above about the one named exception.

**Known gap this ticket names instead of fixing:** a real full-screen child also leaves its
*own* switches on in the user's terminal — `?2004h` (bracketed paste) always, and
`?1000`/`?1002`/`?1006` with a mouse-tracking vim. The alt screen is paid back because it has
a defined thing to return to (the main screen, cursor included); those have to return to
whatever the user's terminal was doing before looprs started, which the exit path does not know
and is forbidden to ask. The honest shape is to read them at **startup** with `DECRQM` — one
round trip at boot, where a round trip is affordable — and restore that on the way out. The
`child` scenario prints what is left behind (`tee'd modes still on at exit: [...]`) so the gap
stays measured; see [ADR-0006](adr/0006-terminal-mode-ledger.md).

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
