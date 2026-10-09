//! The wire format: every typed envelope that crosses the session/UI boundary.
//!
//! This is pi's RPC protocol (json.md / rpc.md), deserialized directly with
//! serde, plus the UI's own two envelopes: [`Msg`] (everything that can change
//! the UI) and [`UiCommand`] (everything the UI asks of a session). What an event
//! *does* to a transcript is the App's job — see
//! [`apply_pi`](crate::app::apply_pi) — this module only says what can arrive.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see [`parse`]).
//!
//! ## The named value sets
//!
//! Any field whose legal values are strings has a type of its own here —
//! [`EntryRole`], [`CompactionReason`], [`StopReason`] — never a `String` with
//! the values listed in a comment behind it. A `String` in a field position is
//! the hole a silent misread goes to hide: the code cannot answer "how many of
//! these are there, and what happens on one nobody has seen", so the answer has
//! to come from a second source (pi's own docs), and the ellipsis after the
//! third example is where the drift starts.
//!
//! Every one of those types has the same shape: the values this crate names, plus
//! an explicit `Unknown(String)` arm that keeps the spelling it could not name,
//! so an unrecognised value is a case in a `match` rather than a string that
//! flows through and renders as nothing. See [`WireValue`].
//!
//! [`WIRE_INVENTORY`] is the same set written out with what the harness *does*
//! with each value, and `docs/guide/wire-protocol.md` is generated from it by
//! `scripts/docs_check.py` — the one-home, generated-out-of-it contract
//! `CHORD_TABLE` already keeps (looprs-00u.7). The tests in
//! [`crate::app::tests`](crate::app::tests) run that claim rather than trusting
//! it: for every row, `parse` produces the variant the row names, the variant's
//! own [`Outcome`] agrees with the row, and — for the roles a `message_end`
//! actually carries — the app is made to do the thing the row says.

use crate::session::{ActiveBead, ByteStream, ExitReason, SessionId, SessionStatus, TerminalType};
use crossterm::event::Event;
use serde::Deserialize;
use serde_json::Value;

pub use crate::session::BeadStep;

/// Everything that can change the UI.
///
/// ADR-0002 Q2, the part that matters: **every message that came from a session is
/// tagged with the `SessionId` that produced it.** The App must never infer
/// provenance from `self.input.mode` — the mode is what the user last Tabbed to,
/// and it changes independently of what is running. Provenance-in-the-envelope is
/// what makes looprs-msj unrepeatable.
///
/// The rule in one line: **input mode is authoritative for intent (where my
/// keystrokes go); the envelope is authoritative for origin (who made this).**
///
/// Construct these through `session::router::wrap`, never by hand: `wrap` is the
/// only place that has a `SessionId` to attach.
#[derive(Debug)]
pub enum Msg {
    /// Raw terminal input. The only message with no origin, because it *is* the user.
    Term(Event),
    /// A pi protocol event, from the session named in `session`.
    Agent {
        /// Provenance. Read, and the only allowed basis for deciding which view an
        /// event belongs to (ADR-0002 Q2).
        session: SessionId,
        event: PiEvent,
    },
    /// Shell output (ADR-0001). `stream` is `Merged` for a pty; `chunk` is a read
    /// buffer, not a line — do not re-split it.
    BashOutput {
        session: SessionId,
        /// Kept because the envelope has to be able to say *which* pipe a byte came
        /// from even though today's only producer (a pty) cannot tell them apart.
        /// looprs-guh's row does **not** mark stderr, and cannot: the pty hands
        /// one merged stream, and `ByteStream::Merged` is what honestly says so.
        /// Marking a stream the backend cannot attribute would be inventing a
        /// distinction that does not exist — worse than not having one. Kept
        /// because the day a split (non-pty) backend lands, this is the field that
        /// makes the row able to say which pipe a line came from.
        ///
        /// Not dead in the meantime: the bus reads it on every coalesce
        /// ([`mergeable`](crate::bus::mergeable)) so that two streams are never
        /// merged into one another's transcript.
        stream: ByteStream,
        chunk: String,
    },
    /// The beads machine moved. Rendered, never re-derived.
    BeadStep {
        session: SessionId,
        step: BeadStep,
    },
    /// A session's liveness changed (looprs-guh). Mirrored, and nothing else: the
    /// status row answers from it, and the only consequence is the one the view
    /// derives — a session that is not working has its keyboard back, and a dead
    /// one certainly does (see [`SessionView::accepts_input`]).
    ///
    /// This is not `App::chat_state`, and the two must not be conflated: `chat`
    /// says *the live region has text arriving*, this says *a child exists and what
    /// it is doing*. A bash shell running `sleep 10` is the second and not the
    /// first, which is exactly why the row needs its own edge.
    SessionStatus {
        session: SessionId,
        status: SessionStatus,
    },
    /// The ticket the beads loop holds right now, `None` when it holds none
    /// (looprs-w7q). Rendered, never re-derived — see
    /// [`SessionEvent::ActiveBead`](crate::session::SessionEvent::ActiveBead).
    ActiveBead {
        session: SessionId,
        bead: Option<ActiveBead>,
    },
    /// A session's child is gone. Guaranteed exactly once per session, so the
    /// receiver can always seal that session's transcript.
    SessionDown {
        session: SessionId,
        reason: ExitReason,
    },
    /// A full-screen program took the real terminal over, or gave it back
    /// (ADR-0001 Q2 rule 2 — the screen-buffer path).
    ///
    /// While it is held *and* the holder is the mode on screen: nothing is drawn
    /// and nothing is flushed, and that session's `BashOutput` chunks go to the
    /// real terminal verbatim. The child's own cursor addressing *is* the
    /// rendering; building one is the thing the ADR rejected.
    ScreenHeld {
        session: SessionId,
        active: bool,
    },
    /// A failure the human needs to see (spawn failure, `bd` failure, ...).
    /// `session: None` means it is harness-level (router/spawn), not a session's.
    Error {
        session: Option<SessionId>,
        text: String,
    },
    /// A status line ("working looprs-1", "board empty, awaiting input").
    System {
        session: Option<SessionId>,
        text: String,
    },
    /// Text the user's input box should take back, from the session that was holding
    /// it. Produced by Pi's interactive `Esc`: the queued steering/follow-up text
    /// is pulled out of the child before the abort and handed back here rather than
    /// being spent on a turn the user just cancelled.
    ///
    /// Tagged with the session that made it, like everything else: a Pi cancel must
    /// not be able to type into the Beads box.
    RestoreInput {
        session: SessionId,
        text: String,
    },
    Tick,
}

/// UI -> session layer. Every variant says which terminal state it is *for*.
///
/// Note what is **not** here: any way to advance the beads loop. There was one
/// (`BeadsNext`), and it was looprs-msj's bug made expressible — the App could ask
/// "someone take the next bead" on the strength of a settle it had seen, while the
/// thing that owned the step, the worker and the parked flag was a session it could
/// not see inside. The beads loop now moves off its own worker's stream, inside
/// [`BeadsSession`](crate::session::BeadsSession); the App cannot drive it because
/// it has nothing to say.
#[derive(Debug)]
pub enum UiCommand {
    /// Enter in the input box. `mode` is where the text was typed: declared
    /// intent, and legitimate routing input. (Contrast with `Msg`, where the tag
    /// is origin and the input mode must not be consulted.)
    Submit { mode: TerminalType, text: String },
    /// Tab. Drives the per-mode switch-away policy (ADR-0002 Q3).
    SwitchMode {
        from: TerminalType,
        to: TerminalType,
    },
    /// Esc. Routed by the router to the *active* session only.
    Cancel,
    /// Raw keystrokes for a full-screen child that currently owns the terminal.
    ///
    /// A separate command from `Submit` because the bytes are the point: `Esc`
    /// must arrive as `0x1b`, and `:wq!` + Enter must not arrive with a newline
    /// added on the way. This is the half of the screen problem that the
    /// "`Esc` means `0x03`" mapping cannot be separated from (looprs-4hv): one
    /// without the other leaves vim reading interrupts where it expects keys.
    Keys { mode: TerminalType, bytes: Vec<u8> },
    /// The real terminal changed shape.
    ///
    /// Not user intent — an environment fact — but the Router is the only thing
    /// that can reach a session, and ADR-0001 rule 6 says the shell's pty is sized
    /// to the window it is actually shown in rather than a virtual 80x24. Without
    /// this the child wraps for a terminal that does not exist and every table it
    /// prints is wrong forever, in the transcript as well as on screen.
    Resize { rows: u16, cols: u16 },
    /// The app is leaving; take every session down with it (ADR-0002 Q3, "on
    /// quit").
    ///
    /// A command rather than "just drop the `App`", which is how the exit used to
    /// be signalled. Dropping does work — the Router shuts everything down when
    /// the channel closes — but it also throws away the only thing that can *read*
    /// what the sessions say on the way out: the last lines of a streamed answer,
    /// and the `SessionDown` that seals each transcript. The exit path sends this,
    /// then stays alive to drain those messages into the scrollback before the
    /// live pane is erased.
    Quit,
}

pub(crate) fn print_json_value_to_string(v: &Value) -> String {
    let mut s = "".to_string();
    if let Some(map) = v.as_object() {
        for (key, value) in map {
            s += &format!("{}: {}", key, value);
        }
    }
    s
}

//UI only
// `ChatState` now lives in `session::view` — it describes one session's live
// region, not the app's. It is imported above rather than re-declared here.
/// Only fields you consume are declared; everything else in the record is skipped.
///
/// Where a field is parsed but **nothing renders it yet**, it carries its own
/// `#[allow(dead_code)]` with the reason it stays (looprs-6ol's warning-gate rule:
///
/// **What looprs-guh (the status row) took from this enum: nothing.** That is the
/// honest label, not an oversight. The row is one line and its `Show` list is
/// mode / loop step / bead / liveness / last error / key hints. Thinking deltas,
/// pi's own retry ladder and the per-content-block indices are none of those,
/// and pushing them in would cost the row the things that are. What the user gets
/// instead is that a long pause is no longer *unattributed*: the row says the run
/// is live and how long it has been going, which is the question those variants
/// are usually standing in for.
///
/// Compaction went somewhere else rather than into the row: it gets a live card of
/// its own (`components::compaction`), because "the session is summarising its
/// own history" is an event with a start and an end, not a segment of a
/// one-line status.
///
/// They stay for the reason they were written down at all: this enum is the
/// harness's record of pi's RPC wire format, and a field captured here is a field
/// a later ticket cannot silently misread as absent. Each allow names the
/// surfacing that field is waiting for; none of them has a reader today.
/// a dead-code allow is allowed only when it is individually justified, never
/// blanket). The reason is almost always the same one and it is a real one: this
/// enum *is* the harness's record of pi's RPC wire format, and a field that is
/// written down here is a field a later ticket cannot silently misread as absent.
/// Deleting them would make the next protocol-facing ticket a reverse-engineering
/// exercise; keeping them is documentation that the compiler otherwise cannot see.
#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PiEvent {
    AgentStart,
    AgentEnd,     // NOT "done": retries / steering / follow-ups can continue after this
    AgentSettled, // pi has no more automatic work => this is "done"
    TurnStart,
    TurnEnd,
    MessageStart {
        /// pi says a message began; it has no text in it yet, and the transcript is
        /// built from the authoritative `message_end` record. Parsed so that
        /// "a message started" stays a distinguishable event from "a message
        /// arrived" once the live region needs it (looprs-guh's streaming cursor).
        #[allow(dead_code)]
        // unread: the whole field — would surface as: a start-of-message cursor in the live region
        message: WireMessage,
    },
    MessageUpdate {
        assistant_message_event: AssistantEvent,
    },
    MessageEnd {
        message: WireMessage,
    }, // authoritative final message
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    /// A tool's partial output. Nothing renders in-place tool updates yet: the card
    /// is drawn at `ToolExecutionStart` and rewritten at `ToolExecutionEnd`.
    ///
    /// * `tool_call_id` — which card to update, needed the moment updates repaint
    ///   instead of being appended (looprs-guh's live tool preview);
    /// * `partial_result` — the streamed output itself, same content the end record
    ///   carries, so nothing is lost by not printing it here.
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: an in-place tool card repaint
    ToolExecutionUpdate {
        tool_call_id: String,
        partial_result: ToolOutput,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        result: ToolOutput,
        is_error: bool,
    },
    /// pi is retrying a failed request on its own. None of this is displayed yet,
    /// and every one of the four fields is something the user needs to know is
    /// happening rather than watching a frozen transcript (looprs-guh: "is it
    /// working or is it hung?").
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: the retry as a status line, not as silence
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// As [`PiEvent::AutoRetryStart`]: the outcome of pi's own retry ladder. The
    /// transcript currently treats a successful retry as invisible, which is fine
    /// for a run that recovers and terrible for one that does not — hence kept.
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: the retry outcome, `final_error` above all
    AutoRetryEnd {
        success: bool,
        #[serde(default)]
        final_error: Option<String>,
    },
    /// Context compaction began — pi has paused the run to summarise old messages
    /// so the conversation fits the context window.
    ///
    /// Surfaced as a live card (`⠹ compacting context · threshold`), for the
    /// reason the comment on this variant always said it needed: without it the
    /// run looks hung for however long the summarisation call takes, and the one
    /// question a frozen transcript provokes is "is it working?".
    CompactionStart {
        /// `"manual"` (`/compact`), `"threshold"` (context nearly full) or
        /// `"overflow"` (the provider rejected the prompt) — typed as
        /// [`CompactionReason`], so "what are the possible values" is answered
        /// by the type rather than by this comment. Printed on the card: "why did
        /// my run stop" has a different answer for each.
        reason: CompactionReason,
    },
    /// Compaction finished, was aborted, or failed. `aborted`/`error_message` are
    /// the two things a user must not have to guess about, so both are on the
    /// card — grey for a cancel, red with the message for a failure.
    CompactionEnd {
        /// The reason from the matching `compaction_start`, repeated on the
        /// wire; `None` means this record did not carry one. Used only when the
        /// end arrived with no card open, so that card can say what was being
        /// done rather than nothing.
        #[serde(default)]
        reason: Option<CompactionReason>,
        #[serde(default)]
        aborted: bool,
        #[serde(default)]
        error_message: Option<String>,
        /// What the compaction reported on success. `None` when it was aborted or
        /// failed — the wire says the result is absent in exactly those cases.
        #[serde(default)]
        result: Option<CompactionResult>,
    },
    /// An extension in the pi child raised. Not the harness's fault, and the
    /// path and the message are captured here so the record exists on this side
    /// of the wire; nothing puts them on screen yet (see the `extension_error`
    /// row of [`WIRE_INVENTORY`]).
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: extension errors named in the status row, not swallowed
    ExtensionError {
        extension_path: String,
        error: String,
    },
    #[serde(other)]
    Unknown, // must be last; swallows any event type you haven't modeled
}

impl PiEvent {
    /// The variant's own name, without the type prefix.
    ///
    /// Not for rendering — for checking. The inventory test reads this to
    /// confirm that every `event` row in [`WIRE_INVENTORY`] names the variant
    /// that pi's `type` tag actually deserialises to, so a wire spelling in the
    /// table cannot be a guess nobody verified.
    #[allow(dead_code)] // audit-only: `app::tests::wire_protocol` reads this against WIRE_INVENTORY; the binary never renders a variant name
    pub fn variant(&self) -> &'static str {
        match self {
            PiEvent::AgentStart => "AgentStart",
            PiEvent::AgentEnd => "AgentEnd",
            PiEvent::AgentSettled => "AgentSettled",
            PiEvent::TurnStart => "TurnStart",
            PiEvent::TurnEnd => "TurnEnd",
            PiEvent::MessageStart { .. } => "MessageStart",
            PiEvent::MessageUpdate { .. } => "MessageUpdate",
            PiEvent::MessageEnd { .. } => "MessageEnd",
            PiEvent::ToolExecutionStart { .. } => "ToolExecutionStart",
            PiEvent::ToolExecutionUpdate { .. } => "ToolExecutionUpdate",
            PiEvent::ToolExecutionEnd { .. } => "ToolExecutionEnd",
            PiEvent::AutoRetryStart { .. } => "AutoRetryStart",
            PiEvent::AutoRetryEnd { .. } => "AutoRetryEnd",
            PiEvent::CompactionStart { .. } => "CompactionStart",
            PiEvent::CompactionEnd { .. } => "CompactionEnd",
            PiEvent::ExtensionError { .. } => "ExtensionError",
            PiEvent::Unknown => "Unknown",
        }
    }
}

/// The `result` of a successful `compaction_end`.
///
/// Two fields are modelled because two are rendered: the card prints what the
/// compaction freed (`150k → 32k`), and that is the whole of what this harness
/// wants from the record.
///
/// What is deliberately **not** here: `summary`, `firstKeptEntryId` and the
/// summarisation call's own `usage`. The first two live in the session file and
/// have no reader on this side of the wire; and folding that `usage` into the
/// status row's window would be counting a cost pi has already charged to the
/// run it paused — the same double-count trap the per-`message_update` `usage`
/// is avoided for (see [`crate::session::view::Tokens::add`]).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    /// Context size before the summarisation, in tokens.
    #[serde(default)]
    pub tokens_before: Option<u64>,
    /// What pi estimates it is afterwards. `estimated`, not measured: pi's own
    /// word for it, kept in the field name so nobody reads it as an exact figure.
    #[serde(default)]
    pub estimated_tokens_after: Option<u64>,
}

/// What the harness does with one declared wire value.
///
/// This is the column the protocol page prints and the tests check, so "we handle
/// it" is never a claim nobody ran. The third arm is the one that makes the
/// difference between this and a comment: a value that nothing paints still
/// **says something**, on the same principle as the kanban mapping's `?` for a
/// status it cannot classify.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // docs/audit-only: the inventory's outcome column, rendered by scripts/docs_check.py and asserted by app::tests::wire_protocol; the binary renders its own arms instead
pub enum Outcome {
    /// Painted, and where it lands: the answer stream, a tool card, a
    /// compaction card.
    Renders(&'static str),
    /// Deliberately not painted, with the reason that makes it a decision and
    /// not a hole — "the user's own words came back and were already echoed".
    Silent(&'static str),
    /// Nothing renders this by name, so the transcript says so and names the
    /// value. Every `Unknown`, and every known value this harness has no
    /// renderer for, lands here. Never silence.
    SurfacesAsNote,
}

impl Outcome {
    /// The docs page's wording for this outcome.
    #[allow(dead_code)] // audit-only: asserted by name in app::tests::wire_protocol, so the page's wording is a checked claim rather than a phrase nobody re-runs
    pub fn label(&self) -> String {
        match self {
            Outcome::Renders(where_it) => format!("renders: {where_it}"),
            Outcome::Silent(why) => format!("not painted — {why}"),
            Outcome::SurfacesAsNote => "surfaces as a transcript note naming the value".to_string(),
        }
    }
}

/// The shape every named wire string has: the values we name, or the string.
///
/// [`EntryRole`], [`CompactionReason`] and [`StopReason`] implement this, and
/// they all owe the same four things:
///
/// * [`Self::known`] — the values with a fixed spelling;
/// * [`Self::as_str`] — that spelling, so `parse(v.as_str()) == v` round-trips
///   for every value of the type, `Unknown` included;
/// * [`Self::variant`] — the Rust variant's name, which is how the docs table
///   gets a reader from a wire value to the `match` arm that catches it;
/// * [`Self::outcome`] — what the harness does with it, written as an
///   **exhaustive `match`** per type. That last one is the load-bearing part:
///   adding a variant to one of these enums without deciding what happens to it
///   stops compiling, and [`WIRE_INVENTORY`] is checked against the decision.
///
/// `parse` never fails and never drops a value. Naming is best-effort; what is
/// not named stays visible.
pub trait WireValue: Sized + Clone + PartialEq {
    /// Every value of this type that has a fixed wire spelling, in the order the
    /// protocol lists them.
    ///
    /// Hand-listed, and the honest caveat goes with it: the compiler cannot
    /// enumerate a type's variants, so this list is not what forces the coverage
    /// — [`Self::outcome`]'s exhaustive `match` is. A value missing from this
    /// list parses as `Unknown` and therefore *shows up* rather than being
    /// misread as one of the known ones, which is the safe direction to fail.
    fn known() -> Vec<Self>;

    /// The wire spelling of this value; for `Unknown`, the value itself.
    fn as_str(&self) -> &str;

    /// The Rust variant's name, without the type prefix.
    #[allow(dead_code)] // audit-only: the inventory test compares a parsed value's `variant()` with the row's
    fn variant(&self) -> &'static str;

    /// The catch-all arm, built from the spelling we could not name.
    fn unknown(raw: &str) -> Self;

    /// What this harness does with it. Exhaustive per type; see the trait doc.
    #[allow(dead_code)] // audit-only: WIRE_INVENTORY rows are asserted equal to what each variant's `outcome()` returns
    fn outcome(&self) -> Outcome;

    /// Name it if it is one of ours, keep it as `Unknown` if it is not.
    fn parse(raw: &str) -> Self {
        Self::known()
            .into_iter()
            .find(|v| v.as_str() == raw)
            .unwrap_or_else(|| Self::unknown(raw))
    }

    /// Whether this value is one of the named ones (`false` only for `Unknown`).
    #[allow(dead_code)] // audit-only: the inventory test uses it to keep the `*anything else*` row honest
    fn is_known(&self) -> bool {
        Self::known().contains(self)
    }
}

/// The `role` of a message record on the wire.
///
/// The whole set pi's `message-types.md` declares, not the subset this harness
/// paints. That distinction is the ticket: `user` / `assistant` / `toolResult`
/// are the three that reach the transcript, and the other five — plus anything
/// an augmented host invents — are what the old `String` field turned into
/// nothing at all, silently, with no arm to notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryRole {
    /// The prompt and tool declarations. Lives in the session file; the RPC
    /// stream this harness reads does not normally carry it.
    System,
    /// The human's own words.
    User,
    /// The model's answer.
    Assistant,
    /// A tool's result record — the wire's copy of what `tool_execution_end`
    /// already reported.
    ToolResult,
    /// A shell command run *inside* pi (the RPC `bash` command). Not this
    /// harness's shell, and not an LLM tool result.
    BashExecution,
    /// A context message sent by an extension in the child.
    Custom,
    /// A summary of a branch of the session tree.
    BranchSummary,
    /// The summary a compaction produced.
    CompactionSummary,
    /// A role this type has no name for — a custom role from a host that merged
    /// extra ones into the union, which pi explicitly tells consumers to
    /// tolerate, or one invented after this file was last read.
    Unknown(String),
}

impl WireValue for EntryRole {
    fn known() -> Vec<Self> {
        vec![
            EntryRole::System,
            EntryRole::User,
            EntryRole::Assistant,
            EntryRole::ToolResult,
            EntryRole::BashExecution,
            EntryRole::Custom,
            EntryRole::BranchSummary,
            EntryRole::CompactionSummary,
        ]
    }

    fn as_str(&self) -> &str {
        match self {
            EntryRole::System => "system",
            EntryRole::User => "user",
            EntryRole::Assistant => "assistant",
            EntryRole::ToolResult => "toolResult",
            EntryRole::BashExecution => "bashExecution",
            EntryRole::Custom => "custom",
            EntryRole::BranchSummary => "branchSummary",
            EntryRole::CompactionSummary => "compactionSummary",
            EntryRole::Unknown(raw) => raw,
        }
    }

    fn variant(&self) -> &'static str {
        match self {
            EntryRole::System => "System",
            EntryRole::User => "User",
            EntryRole::Assistant => "Assistant",
            EntryRole::ToolResult => "ToolResult",
            EntryRole::BashExecution => "BashExecution",
            EntryRole::Custom => "Custom",
            EntryRole::BranchSummary => "BranchSummary",
            EntryRole::CompactionSummary => "CompactionSummary",
            EntryRole::Unknown(_) => "Unknown",
        }
    }

    fn unknown(raw: &str) -> Self {
        EntryRole::Unknown(raw.to_string())
    }

    /// What `App::apply_pi` does with a `message_end` of this role. Exhaustive
    /// on purpose — see the trait doc: a new role has to be classified here
    /// before it can compile, and the inventory row has to agree.
    fn outcome(&self) -> Outcome {
        match self {
            EntryRole::Assistant => {
                Outcome::Renders("the answer stream, and seals the live region")
            }
            EntryRole::User => Outcome::Silent("the box already echoed it on submit"),
            EntryRole::ToolResult => Outcome::Silent("its card came off tool_execution_start/end"),
            // Everything else is a record this harness has no picture for. That
            // is a fact the operator is allowed to see.
            EntryRole::System
            | EntryRole::BashExecution
            | EntryRole::Custom
            | EntryRole::BranchSummary
            | EntryRole::CompactionSummary
            | EntryRole::Unknown(_) => Outcome::SurfacesAsNote,
        }
    }
}

impl<'de> Deserialize<'de> for EntryRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// Why the run stopped to summarise itself (`compaction_start` / `compaction_end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionReason {
    /// `/compact`.
    Manual,
    /// Context nearly full.
    Threshold,
    /// The provider rejected the prompt as too big.
    Overflow,
    /// A reason pi added after this enum was written.
    Unknown(String),
}

impl WireValue for CompactionReason {
    fn known() -> Vec<Self> {
        vec![
            CompactionReason::Manual,
            CompactionReason::Threshold,
            CompactionReason::Overflow,
        ]
    }

    fn as_str(&self) -> &str {
        match self {
            CompactionReason::Manual => "manual",
            CompactionReason::Threshold => "threshold",
            CompactionReason::Overflow => "overflow",
            CompactionReason::Unknown(raw) => raw,
        }
    }

    fn variant(&self) -> &'static str {
        match self {
            CompactionReason::Manual => "Manual",
            CompactionReason::Threshold => "Threshold",
            CompactionReason::Overflow => "Overflow",
            CompactionReason::Unknown(_) => "Unknown",
        }
    }

    fn unknown(raw: &str) -> Self {
        CompactionReason::Unknown(raw.to_string())
    }

    /// All four are painted, because all four answer "why did my run stop":
    /// the reason is a segment of the compaction card either way, and a card
    /// that says `compacting context · quantum` is a card the user can still
    /// read.
    fn outcome(&self) -> Outcome {
        match self {
            CompactionReason::Manual
            | CompactionReason::Threshold
            | CompactionReason::Overflow
            | CompactionReason::Unknown(_) => {
                Outcome::Renders("the compaction card's reason segment")
            }
        }
    }
}

impl<'de> Deserialize<'de> for CompactionReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// Why an assistant message ended (`assistantMessageEvent.done` / `.error`).
///
/// The whole `StopReason` union, in the order pi declares it. `done` carries
/// only `stop | length | toolUse | deferred` and `error` only `aborted | error`,
/// but this type is one enum for both because the wire is the thing being
/// described, not the TypeScript: splitting it into two partial mirrors would
/// let a value pi sends on one arm fail a parse on the other, which is a worse
/// outcome than reading a reason the renderer does not expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Still streaming — the reason on a partial message, never persisted.
    Pending,
    /// Finished speaking.
    Stop,
    /// Cut off by the output token limit. The answer the user reads is
    /// incomplete and this is the only thing that says so.
    Length,
    /// Stopped to call a tool.
    ToolUse,
    /// The turn failed.
    Error,
    /// The user cancelled it.
    Aborted,
    /// The provider parked the response for later retrieval.
    Deferred,
    /// A stop reason pi added after this enum was written.
    Unknown(String),
}

impl WireValue for StopReason {
    fn known() -> Vec<Self> {
        vec![
            StopReason::Pending,
            StopReason::Stop,
            StopReason::Length,
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::Deferred,
        ]
    }

    fn as_str(&self) -> &str {
        match self {
            StopReason::Pending => "pending",
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "toolUse",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
            StopReason::Deferred => "deferred",
            StopReason::Unknown(raw) => raw,
        }
    }

    fn variant(&self) -> &'static str {
        match self {
            StopReason::Pending => "Pending",
            StopReason::Stop => "Stop",
            StopReason::Length => "Length",
            StopReason::ToolUse => "ToolUse",
            StopReason::Error => "Error",
            StopReason::Aborted => "Aborted",
            StopReason::Deferred => "Deferred",
            StopReason::Unknown(_) => "Unknown",
        }
    }

    fn unknown(raw: &str) -> Self {
        StopReason::Unknown(raw.to_string())
    }

    /// Nothing reads a stop reason today, and that is stated rather than hidden:
    /// the row reports the *session's* state and a stop reason belongs to the
    /// turn. The type exists so the day the row wants "ended for `length`" is a
    /// match arm rather than a format string and a hunt through the wire docs.
    fn outcome(&self) -> Outcome {
        match self {
            StopReason::Pending
            | StopReason::Stop
            | StopReason::Length
            | StopReason::ToolUse
            | StopReason::Error
            | StopReason::Aborted
            | StopReason::Deferred
            | StopReason::Unknown(_) => Outcome::Silent(
                "unread today: the row reports the session, a stop reason belongs to the turn",
            ),
        }
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// The `message` record of `message_start` / `message_end`.
///
/// Two fields are declared and the record's `content` is deliberately **not**.
/// The transcript is built from the streamed deltas (`assistantMessageEvent`)
/// plus the accounting below, and nothing on this side of the wire has ever
/// needed the finished blocks: reading an answer out of `message_end` instead
/// of out of the stream would be a second path to the same text, and the two
/// would drift. What is *not* the case — and this is the line the old comment
/// blurred — is that the content is absent from pi's record: it is there, it
/// is typed in pi's `message-types.md`, and it is unmodelled *here*, on
/// purpose, with this paragraph as the reason.
#[derive(Debug, Deserialize)]
pub struct WireMessage {
    /// What kind of message this is. Typed, in full, in [`EntryRole`] — the
    /// roles are a set, not a string with an ellipsis after the third example.
    pub role: EntryRole,
    /// The message's token accounting (`usage` on the wire).
    ///
    /// **`None` means "not reported", never "zero spent".** A `user` message's
    /// `message_end` carries no usage, and some providers report nothing at all —
    /// reading that as zero would put a confidently wrong number on the row.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// One assistant message's token accounting, as pi reports it.
///
/// Field names mirror pi's own `Usage` (`@earendil-works/pi-ai`, `types.d.ts`);
/// the wire is camelCase. Every field is `#[serde(default)]` because providers omit
/// pieces independently, and a partial record is still worth counting for what it
/// does carry.
///
/// What is deliberately **not** modelled: `cost`, and the `cacheWrite1h` /
/// `reasoning` splits. The row shows tokens, and a field with no reader is a
/// dead-code allowance that answers nothing.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

/// The nested `assistantMessageEvent` of `message_update` (delta-only on the wire).
///
/// Every `content_index` here is the index of the content block inside the
/// assistant message. Nothing reads it yet because the transcript assumes one
/// stream per message; the moment a message has a text block *and* a tool call
/// block side by side, that index is what keeps the two from being appended into
/// each other. It is written down now, per variant, for exactly that reason — and
/// each allow below says what it is waiting for (looprs-6ol: individually justified,
/// never blanket).
#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantEvent {
    Start,
    /// Block N began. Nothing draws a per-block cursor yet (looprs-guh's live
    /// region), so the index is unread.
    #[allow(dead_code)]
    // unread: content_index — would surface as: a per-block cursor in the live region
    TextStart {
        content_index: usize,
    },
    /// The visible stream: this delta is what gets printed. The index beside it is
    /// unread for the same one-block-per-message reason as above.
    TextDelta {
        #[allow(dead_code)]
        // unread: content_index — would surface as: which block this delta belongs to
        content_index: usize,
        delta: String,
    },
    /// `content` is read (the authoritative block text, tee'd by the beads
    /// planner's "last words"); the index still isn't.
    TextEnd {
        #[allow(dead_code)]
        // unread: content_index — would surface as: which block ended, once blocks are drawn apart
        content_index: usize,
        content: String,
    },
    /// A thinking block began. **The reasoning itself is shown** — `ThinkingDelta`
    /// goes to `MessageKind::Thinking`, which `theme::styles` paints italic and
    /// dim — so this variant's own job is the part nobody does yet: saying
    /// "thinking" *before* the first delta arrives, which is the half a status
    /// row is for.
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: "thinking…" ahead of the first delta
    ThinkingStart {
        content_index: usize,
    },
    /// The reasoning stream, **printed to the transcript** in italic + dim.
    /// Unread: the block index only.
    ThinkingDelta {
        #[allow(dead_code)]
        // unread: content_index — would surface as: which block this reasoning belongs to
        content_index: usize,
        delta: String,
    },
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: the finished thinking block, if the transcript ever re-renders finished blocks
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    /// The tool call inside the assistant's own stream. The card the user sees is
    /// built from the top-level `ToolExecutionStart`, which carries the same
    /// identity; this variant is the assistant-side view of it, and is kept so the
    /// two can be correlated when the live region (looprs-guh) renders calls as
    /// they are minted rather than when they run.
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: the tool call as the model writes it, not as it runs
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    /// Partial argument JSON. Printing half-written JSON is worse than printing
    /// nothing until the call lands, so it is parsed, unread, and available.
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: streaming args, once a card can show a call filling in
    ToolcallDelta {
        content_index: usize,
        delta: String,
    }, // serialized (partial) argument JSON
    #[allow(dead_code)]
    // unread: the whole variant — would surface as: the completed call object, as the model wrote it
    ToolcallEnd {
        content_index: usize,
        tool_call: Value,
    },
    /// Why the assistant stopped. Typed as [`StopReason`] (`stop` / `length` /
    /// `toolUse` / `deferred` on this arm, in pi's types). Unread today; a run
    /// that ended for `length` looks exactly like one that finished, which is
    /// the kind of thing the status row (looprs-guh) is for. Not read yet: that
    /// row reports the *session's* state, and a stop reason belongs to the turn.
    #[allow(dead_code)] // wire-format record; the row reads session errors, not stop reasons
    Done {
        reason: StopReason,
    },
    /// The assistant-side error. Distinct from the session error the row carries:
    /// this one is about a turn, that one is about the child. Reported in the
    /// transcript, where the context for reading it lives. `reason` is
    /// `aborted` or `error` on this arm in pi's types.
    #[allow(dead_code)] // wire-format record; the transcript shows this, not the status row
    Error {
        reason: StopReason,
    },
    #[serde(other)]
    Unknown,
}

impl AssistantEvent {
    /// As [`PiEvent::variant`]: the variant name, for the inventory check.
    #[allow(dead_code)] // audit-only: `app::tests::wire_protocol` reads this against WIRE_INVENTORY; the binary never renders a variant name
    pub fn variant(&self) -> &'static str {
        match self {
            AssistantEvent::Start => "Start",
            AssistantEvent::TextStart { .. } => "TextStart",
            AssistantEvent::TextDelta { .. } => "TextDelta",
            AssistantEvent::TextEnd { .. } => "TextEnd",
            AssistantEvent::ThinkingStart { .. } => "ThinkingStart",
            AssistantEvent::ThinkingDelta { .. } => "ThinkingDelta",
            AssistantEvent::ThinkingEnd { .. } => "ThinkingEnd",
            AssistantEvent::ToolcallStart { .. } => "ToolcallStart",
            AssistantEvent::ToolcallDelta { .. } => "ToolcallDelta",
            AssistantEvent::ToolcallEnd { .. } => "ToolcallEnd",
            AssistantEvent::Done { .. } => "Done",
            AssistantEvent::Error { .. } => "Error",
            AssistantEvent::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolOutput {
    #[serde(default)]
    pub content: Vec<Block>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

impl ToolOutput {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                Block::Other => None,
            })
            .collect()
    }
}

/// Borrow the Value (no clone per token) and keep it for the error log.
pub fn parse(v: &Value) -> Option<PiEvent> {
    match PiEvent::deserialize(v) {
        Ok(ev) => Some(ev),
        Err(e) => {
            tracing::warn!(error = %e, raw = %v, "unparseable pi event");
            None
        }
    }
}

// ─────────────────────────── the protocol inventory ───────────────────────────
//
// The same values written out with what the harness *does* with each one, as
// data. `docs/guide/wire-protocol.md` is generated out of this array by
// `scripts/docs_check.py` — the contract `CHORD_TABLE` already keeps, applied
// to the wire instead of the keyboard — so the protocol page cannot drift from
// the code that implements it, and "we handle unknown roles" is a row with a
// test behind it rather than a sentence in a comment.
//
// Three things check this table from three directions:
//
// * `crate::app::tests::wire_protocol` asserts every row's `variant` is the
//   variant `parse` actually produces for that wire value, and that the row's
//   `outcome` is the one the variant's own `WireValue::outcome` returns;
// * the same module *runs* the roles through `App::apply_pi` and checks the
//   transcript against the row — painted, silent, or a note naming the value;
// * `docs_check.py` regenerates the page from here and fails if the committed
//   page disagrees.

/// One row of [`WIRE_INVENTORY`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // docs/audit-only: read by app::tests::wire_protocol and rendered by scripts/docs_check.py; the binary never walks its own protocol table
pub struct WireRow {
    /// Which value set this row belongs to. See [`WIRE_GROUPS`].
    pub group: &'static str,
    /// The literal on the wire — the `type` tag, the `role`, the `reason`. The
    /// catch-all arm's row spells this `*anything else*`, because "that is not
    /// one of ours" is itself a fact worth a row.
    pub wire: &'static str,
    /// The Rust variant that catches it, without the type prefix.
    pub variant: &'static str,
    /// What the harness does with it. For the three [`WireValue`] types this is
    /// asserted equal to the variant's own `outcome()`; the tagged enums have
    /// no `WireValue`, so their rows are the statement and the docs print them.
    pub outcome: Outcome,
    /// Who reads it. `Some` names the consumer by path; `None` is the honest
    /// version of what the `#[allow(dead_code)]` fields in this file are
    /// claiming — *no reader yet*.
    pub reader: Option<&'static str>,
    /// What it is waiting for, and the counterpart of `reader` rather than a
    /// fourth thought: every `reader: None` row has to name what would make it
    /// read, for the same reason a `#[allow(dead_code)]` has to carry a reason
    /// (looprs-6ol) — an unread value is only acceptable while somebody wrote
    /// down what would change that. A row with a reader carries `None` here;
    /// "waiting for nothing" is not a state this table can express, because a
    /// value that is read is not waiting for anything.
    pub waiting_on: Option<&'static str>,
    /// The one line a reader of the protocol page needs with this row.
    pub note: &'static str,
}

/// The groups, in the order the docs page prints them.
#[allow(dead_code)] // docs-only: the order scripts/docs_check.py renders the sections in
pub const WIRE_GROUPS: &[&str] = &[
    "role",
    "event",
    "assistant-event",
    "compaction-reason",
    "stop-reason",
];

/// The rows of one group, in the order written here.
#[allow(dead_code)] // audit-only: app::tests::wire_protocol walks the inventory through this
pub fn wire_rows(group: &'static str) -> impl Iterator<Item = &'static WireRow> {
    WIRE_INVENTORY.iter().filter(move |r| r.group == group)
}

/// Every declared value of the wire protocol, and what this harness does with it.
#[allow(dead_code)] // docs/audit-only: the protocol page is generated from this array; see the section banner above
#[rustfmt::skip]
pub const WIRE_INVENTORY: &[WireRow] = &[
    // ── `message.role` — the set pi declares whole, not the subset we paint ──
    WireRow {
        group: "role", wire: "user", variant: "User",
        outcome: Outcome::Silent("the box already echoed it on submit"),
        reader: Some("app::apply_pi — the arm that drops pi's copy"),
        waiting_on: None,
        note: "painting pi's copy as well as the box's prints every prompt twice, which is worse than printing neither",
    },
    WireRow {
        group: "role", wire: "assistant", variant: "Assistant",
        outcome: Outcome::Renders("the answer stream, and seals the live region"),
        reader: Some("app::apply_pi → SessionView::finish_stream, Tokens::add"),
        waiting_on: None,
        note: "the only role whose `message_end` is authoritative — for the streamed entry and for the token window",
    },
    WireRow {
        group: "role", wire: "toolResult", variant: "ToolResult",
        outcome: Outcome::Silent("its card came off tool_execution_start/end"),
        reader: Some("app::apply_pi — the arm that leaves the card alone"),
        waiting_on: None,
        note: "the same record `tool_execution_end` already carried; a second copy here is a second card for one call",
    },
    WireRow {
        group: "role", wire: "system", variant: "System",
        outcome: Outcome::SurfacesAsNote,
        reader: Some("app::apply_pi — the unrendered-role note"),
        waiting_on: None,
        note: "the prompt and tool declarations normally live in the session file; one reaching the stream is not something this harness can draw, but it is something that happened",
    },
    WireRow {
        group: "role", wire: "bashExecution", variant: "BashExecution",
        outcome: Outcome::SurfacesAsNote,
        reader: Some("app::apply_pi — the unrendered-role note"),
        waiting_on: None,
        note: "a shell command run *inside* pi (the RPC `bash` command), not this harness's shell — different shell, different pty, and nothing here renders the other one's output",
    },
    WireRow {
        group: "role", wire: "custom", variant: "Custom",
        outcome: Outcome::SurfacesAsNote,
        reader: Some("app::apply_pi — the unrendered-role note"),
        waiting_on: None,
        note: "a context message an extension pushed into the child; whose it was and what it said are the extension's business, that it arrived is worth a line",
    },
    WireRow {
        group: "role", wire: "branchSummary", variant: "BranchSummary",
        outcome: Outcome::SurfacesAsNote,
        reader: Some("app::apply_pi — the unrendered-role note"),
        waiting_on: None,
        note: "pi's own summary of a branch of the session tree; the summary text is not shown, so without this a run rewrites its own history unseen",
    },
    WireRow {
        group: "role", wire: "compactionSummary", variant: "CompactionSummary",
        outcome: Outcome::SurfacesAsNote,
        reader: Some("app::apply_pi — the unrendered-role note"),
        waiting_on: None,
        note: "what a compaction summarised into, arriving after the fact; the card the user saw for that work came off `compaction_start`/`compaction_end`",
    },
    WireRow {
        group: "role", wire: "*anything else*", variant: "Unknown",
        outcome: Outcome::SurfacesAsNote,
        reader: Some("app::apply_pi — the unrendered-role note"),
        waiting_on: None,
        note: "pi tells consumers to tolerate custom roles an augmented host merges into the union; this arm carries the string rather than being a unit variant precisely so the value can be printed as itself",
    },

    // ── `type` on an agent session event ──
    WireRow {
        group: "event", wire: "agent_start", variant: "AgentStart",
        outcome: Outcome::Silent("no line of its own today"),
        reader: Some("session::pi_chat — the liveness mirror"),
        waiting_on: None,
        note: "the run began; the status row learns that from the mirror, not from the transcript",
    },
    WireRow {
        group: "event", wire: "agent_end", variant: "AgentEnd",
        outcome: Outcome::Silent("and it is not `done`"),
        reader: None,
        waiting_on: Some("a draw that can tell “between turns” from “finished” — today `agent_settled` is the only event that says which"),
        note: "retries, steering and follow-ups can continue after this, which is exactly why the live region stops on `agent_settled` and never here",
    },
    WireRow {
        group: "event", wire: "agent_settled", variant: "AgentSettled",
        outcome: Outcome::Renders("the live region stops: the spinner ends"),
        reader: Some("session::pi_chat (liveness) + app::apply_pi → ChatState::Stopped"),
        waiting_on: None,
        note: "'this session has no more automatic work'. Who advances the beads loop as a result is the beads session's business and not this event's — that conflation was looprs-msj",
    },
    WireRow {
        group: "event", wire: "turn_start", variant: "TurnStart",
        outcome: Outcome::Silent("nothing paints a turn boundary yet"),
        reader: None,
        waiting_on: Some("a turn-level row: the boundary is on the wire and nothing on screen is drawn per turn"),
        note: "declared so 'a turn began' stays a distinguishable fact in the record; the turn-level row is what would read it",
    },
    WireRow {
        group: "event", wire: "turn_end", variant: "TurnEnd",
        outcome: Outcome::Silent("nothing paints a turn boundary yet"),
        reader: None,
        waiting_on: Some("the same turn-level row; pi's per-turn tool results are already reported by the tool cards"),
        note: "the other half of the same boundary; pi also carries the turn's tool results here, which the tool cards already report",
    },
    WireRow {
        group: "event", wire: "message_start", variant: "MessageStart",
        outcome: Outcome::Silent("the transcript is built from the authoritative `message_end`"),
        reader: None,
        waiting_on: Some("the live region's start-of-message cursor, which is the only thing that can use a message that has begun and has no text in it yet"),
        note: "a message began and has no text in it yet; what this waits for is the live region's start-of-message cursor",
    },
    WireRow {
        group: "event", wire: "message_update", variant: "MessageUpdate",
        outcome: Outcome::Renders("the live stream: answer and thinking deltas"),
        reader: Some("app::apply_pi → SessionView::push_delta"),
        waiting_on: None,
        note: "delta-only on the wire; the nested `assistantMessageEvent` rows say which part of which block each delta is",
    },
    WireRow {
        group: "event", wire: "message_end", variant: "MessageEnd",
        outcome: Outcome::Renders("seals the answer; the token window's one source"),
        reader: Some("app::apply_pi — per role, see the roles table above"),
        waiting_on: None,
        note: "what this event means depends on its `role`, so this row is a pointer: the eight answers live in the roles table, not here",
    },
    WireRow {
        group: "event", wire: "tool_execution_start", variant: "ToolExecutionStart",
        outcome: Outcome::Renders("a tool card, opened"),
        reader: Some("app::apply_pi → SessionView::start_tool"),
        waiting_on: None,
        note: "the card the user reads; the assistant-side `toolcall_*` events are the same call seen from the model's own stream",
    },
    WireRow {
        group: "event", wire: "tool_execution_update", variant: "ToolExecutionUpdate",
        outcome: Outcome::Silent("nothing repaints a tool card in place yet"),
        reader: None,
        waiting_on: Some("an in-place tool card repaint: the card is drawn at start and rewritten at end today, so partial output has nowhere to go"),
        note: "the card is drawn at start and rewritten at end; the partial output is the same content the end record carries, so nothing is lost by not drawing it here",
    },
    WireRow {
        group: "event", wire: "tool_execution_end", variant: "ToolExecutionEnd",
        outcome: Outcome::Renders("the tool card, filled in"),
        reader: Some("app::apply_pi → SessionView::finish_tool"),
        waiting_on: None,
        note: "the biggest single thing a beads pass writes, and the reason the card goes through the view: the cap and the journal both stand on that door",
    },
    WireRow {
        group: "event", wire: "auto_retry_start", variant: "AutoRetryStart",
        outcome: Outcome::Silent("pi's own retry ladder is invisible today"),
        reader: None,
        waiting_on: Some("a status line naming pi's retry — attempt, ceiling and delay — so a stalled transcript is at least attributed"),
        note: "attempt / max / delay / error: four fields someone watching a frozen transcript would give anything for; the row answers 'is it hung?' with liveness and elapsed time instead, which is why this is a stated gap and not an oversight",
    },
    WireRow {
        group: "event", wire: "auto_retry_end", variant: "AutoRetryEnd",
        outcome: Outcome::Silent("the ladder's outcome is invisible today"),
        reader: None,
        waiting_on: Some("that same status line, for the outcome: `final_error` is the part that matters when the ladder runs out"),
        note: "a run that recovers need not be shown; a run that does not should not be silent, which is why the pair is kept rather than just the start",
    },
    WireRow {
        group: "event", wire: "compaction_start", variant: "CompactionStart",
        outcome: Outcome::Renders("the compaction card, opened"),
        reader: Some("app::apply_pi → SessionView::start_compaction"),
        waiting_on: None,
        note: "a paid LLM call that prints nothing of its own; without the card the run looks hung for exactly as long as the summary takes",
    },
    WireRow {
        group: "event", wire: "compaction_end", variant: "CompactionEnd",
        outcome: Outcome::Renders("the compaction card, closed: freed / cancelled / failed"),
        reader: Some("app::apply_pi → SessionView::finish_compaction"),
        waiting_on: None,
        note: "three endings, three drawings — a cancel is not painted in failure's colour, because a cancel that looks like a crash teaches the wrong lesson about a key the user chose to press",
    },
    WireRow {
        group: "event", wire: "extension_error", variant: "ExtensionError",
        outcome: Outcome::Silent("an extension throwing is not said out loud yet"),
        reader: None,
        waiting_on: Some("the status row, so a fault inside the child is visible where the transcript stops"),
        note: "path and message captured; the surfacing this waits for is the status row's, because the screen is the harness's even when the fault is not",
    },
    WireRow {
        group: "event", wire: "*anything else*", variant: "Unknown",
        outcome: Outcome::Silent("deliberately: an event this harness does not know must not write into its transcript"),
        reader: Some("wire::parse, which logs what it cannot parse at all"),
        waiting_on: None,
        note: "`#[serde(other)]` is the feature that makes additive protocol changes harmless — a new pi event type lands here and changes nothing. The counterpart is that a *malformed* record is logged loudly by `parse`, because that one is a bug rather than an addition",
    },

    // ── `assistantMessageEvent.type`, inside `message_update` ──
    WireRow {
        group: "assistant-event", wire: "start", variant: "Start",
        outcome: Outcome::Silent("nothing paints the start of an assistant message"),
        reader: None,
        waiting_on: Some("the live region's per-message start marker"),
        note: "the SDK's cumulative `partial` snapshot is stripped for the wire, so this event carries nothing but its own beginning",
    },
    WireRow {
        group: "assistant-event", wire: "text_start", variant: "TextStart",
        outcome: Outcome::Silent("no per-block cursor exists yet"),
        reader: None,
        waiting_on: Some("a per-block cursor in the live region: one block per message today, so the index has nothing to disambiguate"),
        note: "the block index is what a live region that shows more than one block per message would need; today one message is one stream",
    },
    WireRow {
        group: "assistant-event", wire: "text_delta", variant: "TextDelta",
        outcome: Outcome::Renders("the answer, as it arrives"),
        reader: Some("app::apply_pi → SessionView::push_delta(Answer)"),
        waiting_on: None,
        note: "the visible stream: the only nested event that is printed as it lands",
    },
    WireRow {
        group: "assistant-event", wire: "text_end", variant: "TextEnd",
        outcome: Outcome::Silent("nothing repaints the finished block"),
        reader: Some("session::beads::machine — the planner's 'last words'"),
        waiting_on: None,
        note: "the authoritative block text, read by the beads planner for its own final message; the transcript keeps the deltas it already printed",
    },
    WireRow {
        group: "assistant-event", wire: "thinking_start", variant: "ThinkingStart",
        outcome: Outcome::Silent("nothing marks where thinking began"),
        reader: None,
        waiting_on: Some("“pi is thinking” before the first delta arrives"),
        note: "what it waits for is the live region saying 'pi is thinking' before the first delta arrives",
    },
    WireRow {
        group: "assistant-event", wire: "thinking_delta", variant: "ThinkingDelta",
        outcome: Outcome::Renders("the reasoning, italic and dimmed"),
        reader: Some("app::apply_pi → SessionView::push_delta(Thinking)"),
        waiting_on: None,
        note: "this contradicts the comment this file carried for three tickets ('thinking is parsed and deliberately not shown'), which the theme proved wrong on the first look: thinking *is* shown, in italic + dim. It is why this table is generated from the code rather than typed by hand",
    },
    WireRow {
        group: "assistant-event", wire: "thinking_end", variant: "ThinkingEnd",
        outcome: Outcome::Silent("the finished block is not reprinted"),
        reader: None,
        waiting_on: Some("a re-render of finished blocks; the deltas already painted the same text"),
        note: "the deltas are already on screen; the finished text would be a second copy of the same reasoning",
    },
    WireRow {
        group: "assistant-event", wire: "toolcall_start", variant: "ToolcallStart",
        outcome: Outcome::Silent("the card comes from `tool_execution_start`"),
        reader: None,
        waiting_on: Some("the live region drawing calls as the model mints them rather than as they run"),
        note: "the assistant-side view of the same call; kept so the two can be correlated when the live region draws calls as the model mints them rather than as they run",
    },
    WireRow {
        group: "assistant-event", wire: "toolcall_delta", variant: "ToolcallDelta",
        outcome: Outcome::Silent("half-written JSON is worse than nothing"),
        reader: None,
        waiting_on: Some("a card that can show arguments filling in without printing half-written JSON"),
        note: "streaming argument fragments are parsed, unread, and available for the day a card can show a call filling in",
    },
    WireRow {
        group: "assistant-event", wire: "toolcall_end", variant: "ToolcallEnd",
        outcome: Outcome::Silent("the completed call object is not drawn"),
        reader: None,
        waiting_on: Some("the model-side copy of a call the run's own card already reports"),
        note: "the run's card already reports the call and its result; this is the model's copy of the same object",
    },
    WireRow {
        group: "assistant-event", wire: "done", variant: "Done",
        outcome: Outcome::Silent("the row reads the session, not the turn"),
        reader: None,
        waiting_on: Some("a turn-level row that can say “ended for `length`” where the answer on screen is cut off"),
        note: "`reason` is `stop` | `length` | `toolUse` | `deferred` on this arm; `length` means the answer the user is reading was cut off, and nothing says so today — the clearest gap in this table",
    },
    WireRow {
        group: "assistant-event", wire: "error", variant: "Error",
        outcome: Outcome::Silent("the row carries the session's error, not the turn's"),
        reader: None,
        waiting_on: Some("that turn-level row, showing the turn's own error beside the session's"),
        note: "`reason` is `aborted` | `error` here; the two are different questions and the wire keeps them apart",
    },
    WireRow {
        group: "assistant-event", wire: "*anything else*", variant: "Unknown",
        outcome: Outcome::Silent("same additivity one level down"),
        reader: None,
        waiting_on: Some("a stated decision about what an unknown nested event should do: silence is the current answer, and it is a design answer rather than an oversight"),
        note: "a nested event type this file does not name lands here and changes nothing, which is what keeps a pi upgrade from writing into a transcript it knows nothing about",
    },

    // ── `compaction_start.reason` / `compaction_end.reason` ──
    WireRow {
        group: "compaction-reason", wire: "manual", variant: "Manual",
        outcome: Outcome::Renders("the compaction card's reason segment"),
        reader: Some("components::compaction::compaction_line"),
        waiting_on: None,
        note: "`/compact`: the user asked for it, and the card says so rather than leaving the pause unattributed",
    },
    WireRow {
        group: "compaction-reason", wire: "threshold", variant: "Threshold",
        outcome: Outcome::Renders("the compaction card's reason segment"),
        reader: Some("components::compaction::compaction_line"),
        waiting_on: None,
        note: "context nearly full: pi acted on its own, which reads differently to the user than being asked",
    },
    WireRow {
        group: "compaction-reason", wire: "overflow", variant: "Overflow",
        outcome: Outcome::Renders("the compaction card's reason segment"),
        reader: Some("components::compaction::compaction_line"),
        waiting_on: None,
        note: "the provider rejected the prompt as too big — the one reason that arrived as an error rather than as a plan",
    },
    WireRow {
        group: "compaction-reason", wire: "*anything else*", variant: "Unknown",
        outcome: Outcome::Renders("the compaction card's reason segment"),
        reader: Some("components::compaction::compaction_line"),
        waiting_on: None,
        note: "prints its own spelling: the card keeps saying *something* where the old empty-string fallback said nothing at all",
    },

    // ── `assistantMessageEvent.done.reason` / `.error.reason` ──
    WireRow {
        group: "stop-reason", wire: "pending", variant: "Pending",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "the reason on a partial message while it streams; pi does not persist it",
    },
    WireRow {
        group: "stop-reason", wire: "stop", variant: "Stop",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "finished speaking",
    },
    WireRow {
        group: "stop-reason", wire: "length", variant: "Length",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "cut off by the output token limit: the only value here that says the answer on screen is incomplete, and it currently says it to nobody",
    },
    WireRow {
        group: "stop-reason", wire: "toolUse", variant: "ToolUse",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "stopped to call a tool — which the tool card reports anyway, one event later",
    },
    WireRow {
        group: "stop-reason", wire: "error", variant: "Error",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "the turn failed; the assistant-side `error` event carries it with the message attached",
    },
    WireRow {
        group: "stop-reason", wire: "aborted", variant: "Aborted",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "the user cancelled it — the one reason on this list that is an answer to a keystroke rather than to a model",
    },
    WireRow {
        group: "stop-reason", wire: "deferred", variant: "Deferred",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "the provider parked the response for later retrieval; the handle lives on the message record, which this file does not model",
    },
    WireRow {
        group: "stop-reason", wire: "*anything else*", variant: "Unknown",
        outcome: Outcome::Silent("unread today: the row reports the session, a stop reason belongs to the turn"),
        reader: None,
        waiting_on: Some("a turn-level row: nothing in this harness reports per-turn facts today (see the `done` row above)"),
        note: "keeps its spelling, so a stop reason pi adds shows up as itself in the record rather than being coerced into one of the six this file knew when it was written",
    },
];
