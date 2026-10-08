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
        // wire-format record; would surface as: live region wants start-of-message
        #[allow(dead_code)]
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
    #[allow(dead_code)] // wire-format record; would surface as: in-place tool card repaint)
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
    // wire-format record; would surface as: retry shown as a status, not silence
    #[allow(dead_code)]
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
    // wire-format record; would surface as: retry outcome, esp. `final_error`)
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
        /// `"overflow"` (the provider rejected the prompt). Printed on the card:
        /// "why did my run stop" has a different answer for each.
        reason: String,
    },
    /// Compaction finished, was aborted, or failed. `aborted`/`error_message` are
    /// the two things a user must not have to guess about, so both are on the
    /// card — grey for a cancel, red with the message for a failure.
    CompactionEnd {
        /// The reason from the matching `compaction_start`, repeated on the wire.
        /// Used only when the end arrived with no card open, so that card can say
        /// what was being done rather than nothing.
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        aborted: bool,
        #[serde(default)]
        error_message: Option<String>,
        /// What the compaction reported on success. `None` when it was aborted or
        /// failed — the wire says the result is absent in exactly those cases.
        #[serde(default)]
        result: Option<CompactionResult>,
    },
    /// An extension in the pi child raised. Not the harness's fault, but it is the
    /// harness's screen, so the path and the message are recorded now that the
    /// status row exists to put them in (looprs-guh).
    // wire-format record; would surface as: extension errors are surfaced, not swallowed
    #[allow(dead_code)]
    ExtensionError {
        extension_path: String,
        error: String,
    },
    #[serde(other)]
    Unknown, // must be last; swallows any event type you haven't modeled
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

#[derive(Debug, Deserialize)]
pub struct WireMessage {
    pub role: String, // "user" | "assistant" | "toolResult" ...; content left undeclared for now
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
    #[allow(dead_code)] // wire-format record; would surface as: per-block live region)
    TextStart {
        content_index: usize,
    },
    /// The visible stream: this delta is what gets printed. The index beside it is
    /// unread for the same one-block-per-message reason as above.
    TextDelta {
        // wire-format record; would surface as: which block this delta belongs to
        #[allow(dead_code)]
        content_index: usize,
        delta: String,
    },
    /// `content` is read (the authoritative block text, tee'd by the beads
    /// planner's "last words"); the index still isn't.
    TextEnd {
        #[allow(dead_code)] // wire-format record; would surface as: which block ended)
        content_index: usize,
        content: String,
    },
    /// Thinking is parsed and deliberately not shown. Both fields unread today:
    /// the transcript prints answers, not reasoning. Kept because "pi is thinking"
    /// is the single most useful thing a status row can say while a run is open.
    #[allow(dead_code)]
    // wire-format record; would surface as: "thinking…" while a run is open)
    ThinkingStart {
        content_index: usize,
    },
    #[allow(dead_code)] // wire-format record; would surface as: thinking stream, if ever surfaced)
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    #[allow(dead_code)] // wire-format record; would surface as: the finished thinking block)
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
    // wire-format record; would surface as: tool call as the model writes it)
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    /// Partial argument JSON. Printing half-written JSON is worse than printing
    /// nothing until the call lands, so it is parsed, unread, and available.
    #[allow(dead_code)]
    // wire-format record; would surface as: streaming args, once renderable)
    ToolcallDelta {
        content_index: usize,
        delta: String,
    }, // serialized (partial) argument JSON
    #[allow(dead_code)] // wire-format record; would surface as: the completed call object)
    ToolcallEnd {
        content_index: usize,
        tool_call: Value,
    },
    /// Why the assistant stopped (stop / end_turn / length / …). Unread today; a
    /// run that ended for `length` looks exactly like one that finished, which is
    /// the kind of thing the status row (looprs-guh) is for. Not read yet: that
    /// row reports the *session's* state, and a stop reason belongs to the turn.
    #[allow(dead_code)] // wire-format record; the row reads session errors, not stop reasons
    Done {
        reason: String,
    },
    /// The assistant-side error. Distinct from the session error the row carries:
    /// this one is about a turn, that one is about the child. Reported in the
    /// transcript, where the context for reading it lives.
    #[allow(dead_code)] // wire-format record; the transcript shows this, not the status row
    Error {
        reason: String,
    },
    #[serde(other)]
    Unknown,
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
