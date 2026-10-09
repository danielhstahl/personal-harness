# The wire protocol

Everything that can cross the boundary between a session and the UI, **generated
from the types that carry it**. The tables below are rendered out of
[`WIRE_INVENTORY`](../../src/wire.rs) by `scripts/docs_check.py` — the same
one-home contract the [keymap](keymap.md) keeps with `CHORD_TABLE` — so this
page cannot disagree with the binary it describes, and a value added to the
protocol without a row here fails the gate instead of quietly going
undocumented.

Why that matters here more than anywhere else in the docs: until looprs-00u.19
this file's own source carried `role: String` with
`"user" | "assistant" | "toolResult" …` behind it in a comment. "What crosses
the wire?" could not be answered from the repo, so the answer would have had to
come from pi's protocol docs — a second source of truth, with an ellipsis in
the middle of it. That is exactly the kind of thing this site is not allowed to
be a copy of.

Three things to know before the tables:

1. **Every value set has an `Unknown` arm.** A value this harness has not named
   is a case in a `match`, not a string that flows through and renders as
   nothing. For roles that means a transcript line that prints the value; for
   event *types* it means silence, which is a decision, not an oversight — see
   the `*anything else*` rows.
2. **"Not painted" is written down as loudly as "painted".** Each row says
   which of the three outcomes applies and why. `not painted — the box already
   echoed it` is a decision. A blank cell would be a hole.
3. **`reader: nothing today` always comes with what it is waiting for.** A
   field that is parsed but unread stays in the tree only while someone has
   written down what would make it read. That is the same rule
   `#[allow(dead_code)]` carries elsewhere, and the reason
   `scripts/dead_audit.py` prints a reason next to every allow.

---

## The message roles

`message.role` on `message_start` / `message_end`. The whole set pi declares,
not the three this harness paints — which is the point of typing it. The role
is what `App::apply_pi` switches on, and `app::tests::wire_protocol` drives
**every row of this table through the real `apply_pi`** and checks the
transcript against the "what looprs does" column.

<!-- BEGIN GENERATED:wire:role -->
| on the wire | the variant that catches it | what looprs does with it | who reads it | note |
| --- | --- | --- | --- | --- |
| `user` | `User` | not painted — the box already echoed it on submit | app::apply_pi — the arm that drops pi's copy | painting pi's copy as well as the box's prints every prompt twice, which is worse than printing neither |
| `assistant` | `Assistant` | renders: the answer stream, and seals the live region | app::apply_pi → SessionView::finish_stream, Tokens::add | the only role whose `message_end` is authoritative — for the streamed entry and for the token window |
| `toolResult` | `ToolResult` | not painted — its card came off tool_execution_start/end | app::apply_pi — the arm that leaves the card alone | the same record `tool_execution_end` already carried; a second copy here is a second card for one call |
| `system` | `System` | surfaces as a transcript note naming the value | app::apply_pi — the unrendered-role note | the prompt and tool declarations normally live in the session file; one reaching the stream is not something this harness can draw, but it is something that happened |
| `bashExecution` | `BashExecution` | surfaces as a transcript note naming the value | app::apply_pi — the unrendered-role note | a shell command run *inside* pi (the RPC `bash` command), not this harness's shell — different shell, different pty, and nothing here renders the other one's output |
| `custom` | `Custom` | surfaces as a transcript note naming the value | app::apply_pi — the unrendered-role note | a context message an extension pushed into the child; whose it was and what it said are the extension's business, that it arrived is worth a line |
| `branchSummary` | `BranchSummary` | surfaces as a transcript note naming the value | app::apply_pi — the unrendered-role note | pi's own summary of a branch of the session tree; the summary text is not shown, so without this a run rewrites its own history unseen |
| `compactionSummary` | `CompactionSummary` | surfaces as a transcript note naming the value | app::apply_pi — the unrendered-role note | what a compaction summarised into, arriving after the fact; the card the user saw for that work came off `compaction_start`/`compaction_end` |
| `*anything else*` | `Unknown` | surfaces as a transcript note naming the value | app::apply_pi — the unrendered-role note | pi tells consumers to tolerate custom roles an augmented host merges into the union; this arm carries the string rather than being a unit variant precisely so the value can be printed as itself |
<!-- END GENERATED:wire:role -->

An augmented host can merge custom roles into pi's message union, and pi's own
docs say consumers must tolerate them. That is the `Unknown` row, and it is why
the arm carries the string rather than being a unit variant: `unknownFrobnicate`
arrives in the transcript as `unknownFrobnicate`.

## The session events

The `type` tag of an agent session event — the top-level envelope of everything
pi sends. Everything the transcript and the status row are made of arrives
through one of these.

<!-- BEGIN GENERATED:wire:event -->
| on the wire | the variant that catches it | what looprs does with it | who reads it | note |
| --- | --- | --- | --- | --- |
| `agent_start` | `AgentStart` | not painted — no line of its own today | session::pi_chat — the liveness mirror | the run began; the status row learns that from the mirror, not from the transcript |
| `agent_end` | `AgentEnd` | not painted — and it is not `done` | *nothing today — waiting on: a draw that can tell “between turns” from “finished” — today `agent_settled` is the only event that says which* | retries, steering and follow-ups can continue after this, which is exactly why the live region stops on `agent_settled` and never here |
| `agent_settled` | `AgentSettled` | renders: the live region stops: the spinner ends | session::pi_chat (liveness) + app::apply_pi → ChatState::Stopped | 'this session has no more automatic work'. Who advances the beads loop as a result is the beads session's business and not this event's — that conflation was looprs-msj |
| `turn_start` | `TurnStart` | not painted — nothing paints a turn boundary yet | *nothing today — waiting on: a turn-level row: the boundary is on the wire and nothing on screen is drawn per turn* | declared so 'a turn began' stays a distinguishable fact in the record; the turn-level row is what would read it |
| `turn_end` | `TurnEnd` | not painted — nothing paints a turn boundary yet | *nothing today — waiting on: the same turn-level row; pi's per-turn tool results are already reported by the tool cards* | the other half of the same boundary; pi also carries the turn's tool results here, which the tool cards already report |
| `message_start` | `MessageStart` | not painted — the transcript is built from the authoritative `message_end` | *nothing today — waiting on: the live region's start-of-message cursor, which is the only thing that can use a message that has begun and has no text in it yet* | a message began and has no text in it yet; what this waits for is the live region's start-of-message cursor |
| `message_update` | `MessageUpdate` | renders: the live stream: answer and thinking deltas | app::apply_pi → SessionView::push_delta | delta-only on the wire; the nested `assistantMessageEvent` rows say which part of which block each delta is |
| `message_end` | `MessageEnd` | renders: seals the answer; the token window's one source | app::apply_pi — per role, see the roles table above | what this event means depends on its `role`, so this row is a pointer: the eight answers live in the roles table, not here |
| `tool_execution_start` | `ToolExecutionStart` | renders: a tool card, opened | app::apply_pi → SessionView::start_tool | the card the user reads; the assistant-side `toolcall_*` events are the same call seen from the model's own stream |
| `tool_execution_update` | `ToolExecutionUpdate` | not painted — nothing repaints a tool card in place yet | *nothing today — waiting on: an in-place tool card repaint: the card is drawn at start and rewritten at end today, so partial output has nowhere to go* | the card is drawn at start and rewritten at end; the partial output is the same content the end record carries, so nothing is lost by not drawing it here |
| `tool_execution_end` | `ToolExecutionEnd` | renders: the tool card, filled in | app::apply_pi → SessionView::finish_tool | the biggest single thing a beads pass writes, and the reason the card goes through the view: the cap and the journal both stand on that door |
| `auto_retry_start` | `AutoRetryStart` | not painted — pi's own retry ladder is invisible today | *nothing today — waiting on: a status line naming pi's retry — attempt, ceiling and delay — so a stalled transcript is at least attributed* | attempt / max / delay / error: four fields someone watching a frozen transcript would give anything for; the row answers 'is it hung?' with liveness and elapsed time instead, which is why this is a stated gap and not an oversight |
| `auto_retry_end` | `AutoRetryEnd` | not painted — the ladder's outcome is invisible today | *nothing today — waiting on: that same status line, for the outcome: `final_error` is the part that matters when the ladder runs out* | a run that recovers need not be shown; a run that does not should not be silent, which is why the pair is kept rather than just the start |
| `compaction_start` | `CompactionStart` | renders: the compaction card, opened | app::apply_pi → SessionView::start_compaction | a paid LLM call that prints nothing of its own; without the card the run looks hung for exactly as long as the summary takes |
| `compaction_end` | `CompactionEnd` | renders: the compaction card, closed: freed / cancelled / failed | app::apply_pi → SessionView::finish_compaction | three endings, three drawings — a cancel is not painted in failure's colour, because a cancel that looks like a crash teaches the wrong lesson about a key the user chose to press |
| `extension_error` | `ExtensionError` | not painted — an extension throwing is not said out loud yet | *nothing today — waiting on: the status row, so a fault inside the child is visible where the transcript stops* | path and message captured; the surfacing this waits for is the status row's, because the screen is the harness's even when the fault is not |
| `*anything else*` | `Unknown` | not painted — deliberately: an event this harness does not know must not write into its transcript | wire::parse, which logs what it cannot parse at all | `#[serde(other)]` is the feature that makes additive protocol changes harmless — a new pi event type lands here and changes nothing. The counterpart is that a *malformed* record is logged loudly by `parse`, because that one is a bug rather than an addition |
<!-- END GENERATED:wire:event -->

`agent_end` is not `done`, and the row says so because the difference has bitten
before: retries, steering and follow-ups continue after `agent_end`, so the live
region stops on `agent_settled`. The unknown-event row is deliberately silent:
a pi upgrade must not be able to write into a transcript it knows nothing
about. What is *not* silent is a record that fails to parse at all — `wire::parse`
logs that loudly, because a malformed record is a bug and a new type is an
addition.

## The nested assistant events

Inside `message_update`, the `assistantMessageEvent.type` tag: which part of
which content block a delta belongs to. Wire records are delta-only — pi strips
the SDK's cumulative `partial` snapshots so a long answer does not cost the
square of its length.

<!-- BEGIN GENERATED:wire:assistant-event -->
| on the wire | the variant that catches it | what looprs does with it | who reads it | note |
| --- | --- | --- | --- | --- |
| `start` | `Start` | not painted — nothing paints the start of an assistant message | *nothing today — waiting on: the live region's per-message start marker* | the SDK's cumulative `partial` snapshot is stripped for the wire, so this event carries nothing but its own beginning |
| `text_start` | `TextStart` | not painted — no per-block cursor exists yet | *nothing today — waiting on: a per-block cursor in the live region: one block per message today, so the index has nothing to disambiguate* | the block index is what a live region that shows more than one block per message would need; today one message is one stream |
| `text_delta` | `TextDelta` | renders: the answer, as it arrives | app::apply_pi → SessionView::push_delta(Answer) | the visible stream: the only nested event that is printed as it lands |
| `text_end` | `TextEnd` | not painted — nothing repaints the finished block | session::beads::machine — the planner's 'last words' | the authoritative block text, read by the beads planner for its own final message; the transcript keeps the deltas it already printed |
| `thinking_start` | `ThinkingStart` | not painted — nothing marks where thinking began | *nothing today — waiting on: “pi is thinking” before the first delta arrives* | what it waits for is the live region saying 'pi is thinking' before the first delta arrives |
| `thinking_delta` | `ThinkingDelta` | renders: the reasoning, italic and dimmed | app::apply_pi → SessionView::push_delta(Thinking) | this contradicts the comment this file carried for three tickets ('thinking is parsed and deliberately not shown'), which the theme proved wrong on the first look: thinking *is* shown, in italic + dim. It is why this table is generated from the code rather than typed by hand |
| `thinking_end` | `ThinkingEnd` | not painted — the finished block is not reprinted | *nothing today — waiting on: a re-render of finished blocks; the deltas already painted the same text* | the deltas are already on screen; the finished text would be a second copy of the same reasoning |
| `toolcall_start` | `ToolcallStart` | not painted — the card comes from `tool_execution_start` | *nothing today — waiting on: the live region drawing calls as the model mints them rather than as they run* | the assistant-side view of the same call; kept so the two can be correlated when the live region draws calls as the model mints them rather than as they run |
| `toolcall_delta` | `ToolcallDelta` | not painted — half-written JSON is worse than nothing | *nothing today — waiting on: a card that can show arguments filling in without printing half-written JSON* | streaming argument fragments are parsed, unread, and available for the day a card can show a call filling in |
| `toolcall_end` | `ToolcallEnd` | not painted — the completed call object is not drawn | *nothing today — waiting on: the model-side copy of a call the run's own card already reports* | the run's card already reports the call and its result; this is the model's copy of the same object |
| `done` | `Done` | not painted — the row reads the session, not the turn | *nothing today — waiting on: a turn-level row that can say “ended for `length`” where the answer on screen is cut off* | `reason` is `stop` | `length` | `toolUse` | `deferred` on this arm; `length` means the answer the user is reading was cut off, and nothing says so today — the clearest gap in this table |
| `error` | `Error` | not painted — the row carries the session's error, not the turn's | *nothing today — waiting on: that turn-level row, showing the turn's own error beside the session's* | `reason` is `aborted` | `error` here; the two are different questions and the wire keeps them apart |
| `*anything else*` | `Unknown` | not painted — same additivity one level down | *nothing today — waiting on: a stated decision about what an unknown nested event should do: silence is the current answer, and it is a design answer rather than an oversight* | a nested event type this file does not name lands here and changes nothing, which is what keeps a pi upgrade from writing into a transcript it knows nothing about |
<!-- END GENERATED:wire:assistant-event -->

## Why a run stopped to compact itself

`compaction_start.reason` / `compaction_end.reason`. Printed on the card,
because "why did my run stop" has a different answer for each of them.

<!-- BEGIN GENERATED:wire:compaction-reason -->
| on the wire | the variant that catches it | what looprs does with it | who reads it | note |
| --- | --- | --- | --- | --- |
| `manual` | `Manual` | renders: the compaction card's reason segment | components::compaction::compaction_line | `/compact`: the user asked for it, and the card says so rather than leaving the pause unattributed |
| `threshold` | `Threshold` | renders: the compaction card's reason segment | components::compaction::compaction_line | context nearly full: pi acted on its own, which reads differently to the user than being asked |
| `overflow` | `Overflow` | renders: the compaction card's reason segment | components::compaction::compaction_line | the provider rejected the prompt as too big — the one reason that arrived as an error rather than as a plan |
| `*anything else*` | `Unknown` | renders: the compaction card's reason segment | components::compaction::compaction_line | prints its own spelling: the card keeps saying *something* where the old empty-string fallback said nothing at all |
<!-- END GENERATED:wire:compaction-reason -->

## Why an assistant message ended

`assistantMessageEvent.done.reason` and `.error.reason`. Nothing in this
harness reads these today and the table says so plainly rather than leaving the
reader to guess whether they were ever parsed. They are typed anyway — which is
the whole argument for the inventory: a value written down with a type is a
value a later ticket cannot misread as absent, and "ended for `length`" is the
one fact in this table that currently reaches nobody.

<!-- BEGIN GENERATED:wire:stop-reason -->
| on the wire | the variant that catches it | what looprs does with it | who reads it | note |
| --- | --- | --- | --- | --- |
| `pending` | `Pending` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | the reason on a partial message while it streams; pi does not persist it |
| `stop` | `Stop` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | finished speaking |
| `length` | `Length` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | cut off by the output token limit: the only value here that says the answer on screen is incomplete, and it currently says it to nobody |
| `toolUse` | `ToolUse` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | stopped to call a tool — which the tool card reports anyway, one event later |
| `error` | `Error` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | the turn failed; the assistant-side `error` event carries it with the message attached |
| `aborted` | `Aborted` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | the user cancelled it — the one reason on this list that is an answer to a keystroke rather than to a model |
| `deferred` | `Deferred` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | the provider parked the response for later retrieval; the handle lives on the message record, which this file does not model |
| `*anything else*` | `Unknown` | not painted — unread today: the row reports the session, a stop reason belongs to the turn | *nothing today — waiting on: a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)* | keeps its spelling, so a stop reason pi adds shows up as itself in the record rather than being coerced into one of the six this file knew when it was written |
<!-- END GENERATED:wire:stop-reason -->

## The records kept unread

Every `#[allow(dead_code)]` in `src/wire.rs`, with the reason written next to
it. `scripts/dead_audit.py` asks the compiler whether these are still dead;
this table asks whether anyone ever said why they are kept. A row with an empty
reason is a gate failure, because an allow with no reason is a blanket allow
wearing a disguise.

<!-- BEGIN GENERATED:wire:unread -->
| the record kept unread | why it stays |
| --- | --- |
| `PiEvent::message` | unread: the whole field — would surface as: a start-of-message cursor in the live region |
| `PiEvent::ToolExecutionUpdate` | unread: the whole variant — would surface as: an in-place tool card repaint |
| `PiEvent::AutoRetryStart` | unread: the whole variant — would surface as: the retry as a status line, not as silence |
| `PiEvent::AutoRetryEnd` | unread: the whole variant — would surface as: the retry outcome, `final_error` above all |
| `PiEvent::ExtensionError` | unread: the whole variant — would surface as: extension errors named in the status row, not swallowed |
| `PiEvent::variant()` | audit-only: `app::tests::wire_protocol` reads this against WIRE_INVENTORY; the binary never renders a variant name |
| `Outcome` | docs/audit-only: the inventory's outcome column, rendered by scripts/docs_check.py and asserted by app::tests::wire_protocol; the binary renders its own arms instead |
| `Outcome::label()` | audit-only: asserted by name in app::tests::wire_protocol, so the page's wording is a checked claim rather than a phrase nobody re-runs |
| `WireValue::variant()` | audit-only: the inventory test compares a parsed value's `variant()` with the row's |
| `WireValue::outcome()` | audit-only: WIRE_INVENTORY rows are asserted equal to what each variant's `outcome()` returns |
| `WireValue::is_known()` | audit-only: the inventory test uses it to keep the `*anything else*` row honest |
| `AssistantEvent::TextStart` | unread: content_index — would surface as: a per-block cursor in the live region |
| `AssistantEvent::content_index` | unread: content_index — would surface as: which block this delta belongs to |
| `AssistantEvent::content_index` | unread: content_index — would surface as: which block ended, once blocks are drawn apart |
| `AssistantEvent::ThinkingStart` | unread: the whole variant — would surface as: "thinking…" ahead of the first delta |
| `AssistantEvent::content_index` | unread: content_index — would surface as: which block this reasoning belongs to |
| `AssistantEvent::ThinkingEnd` | unread: the whole variant — would surface as: the finished thinking block, if the transcript ever re-renders finished blocks |
| `AssistantEvent::ToolcallStart` | unread: the whole variant — would surface as: the tool call as the model writes it, not as it runs |
| `AssistantEvent::ToolcallDelta` | unread: the whole variant — would surface as: streaming args, once a card can show a call filling in |
| `AssistantEvent::ToolcallEnd` | unread: the whole variant — would surface as: the completed call object, as the model wrote it |
| `AssistantEvent::Done` | wire-format record; the row reads session errors, not stop reasons |
| `AssistantEvent::Error` | wire-format record; the transcript shows this, not the status row |
| `AssistantEvent::variant()` | audit-only: `app::tests::wire_protocol` reads this against WIRE_INVENTORY; the binary never renders a variant name |
| `WireRow` | docs/audit-only: read by app::tests::wire_protocol and rendered by scripts/docs_check.py; the binary never walks its own protocol table |
| `WIRE_GROUPS` | docs-only: the order scripts/docs_check.py renders the sections in |
| `wire_rows()` | audit-only: app::tests::wire_protocol walks the inventory through this |
| `WIRE_INVENTORY` | docs/audit-only: the protocol page is generated from this array; see the section banner above |
<!-- END GENERATED:wire:unread -->

---

## What is deliberately *not* modelled

The honest list, because "absent from this page" and "absent from the wire" are
different claims and only one of them is checked by generation:

* **`content` on a message record.** It is there on the wire, typed in pi's
  `message-types.md` as text / thinking / tool-call blocks. This harness does
  not deserialize it: the transcript is built from the streamed deltas, and
  reading the finished blocks instead would be a second path to the same text
  for the two to drift apart.
* **A tool's `details`, a message's `cost`, the `cacheWrite1h` / `reasoning`
  splits.** The row shows tokens; a field with no reader is a dead-code
  allowance that answers nothing.
* **The compaction `summary`, `firstKeptEntryId` and the summarisation call's
  own `usage`.** They live in the session file, and folding that `usage` into
  the row would double-count a cost pi has already charged to the run it
  paused.
* **The harness's own two envelopes.** [`Msg`](../../src/wire.rs) (everything
  that can change the UI) and [`UiCommand`](../../src/wire.rs) (everything the
  UI asks of a session) are looprs's protocol, not pi's, and they are
  documented in [ADR-0002](../adr/0002-session-abstraction.md) rather than in
  the generated tables: every `Msg` variant carries its own `SessionId`, and
  that provenance rule is prose, not a column.

## How this page is generated, and what adding a value costs

```
./scripts/docs_check.py --fix-wire     # regenerate every table above
./scripts/docs_check.py --list-wire    # the inventory, plus every unread record
```

The gate runs as part of `./scripts/check.sh`, so a value that is in the type
but not in the page — or a page edited by hand instead of regenerated — stops
the build. Adding a protocol value takes three steps and each one is checked:

1. Add the variant to the enum in `src/wire.rs`. For the three `WireValue`
   types (`EntryRole`, `CompactionReason`, `StopReason`) that means also
   classifying it in the exhaustive `outcome()` match: **a new value that no
   one has decided the fate of does not compile.**
2. Add its row to `WIRE_INVENTORY`, with what it does, who reads it, and — if
   nothing reads it — what it is waiting for.
3. `--fix-wire`, and read the diff.

`app::tests::wire_protocol` is what makes step 2 mean something: it parses
every row's wire value and asserts it lands in the variant the row names,
asserts the row's outcome equals the variant's own `outcome()`, and runs the
roles through `App::apply_pi` to check the transcript against the page. A row
that promises a visible note and delivers silence fails there, not in a
reader's patience.
