//! Typed wire format for pi's session events (json.md / rpc.md), deserialized directly with serde,
//! plus the one place that turns them into transcript changes.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see `parse`).

use crate::components::compaction::{CompactionState, token_delta};
use crate::components::input::{InputAction, InputState, inner_width};
use crate::components::scrollback::RenderedRow;
use crate::session::view::SessionView;
use crate::session::{
    ActiveBead, ByteStream, ChatState, ExitReason, SessionId, SessionStatus, TerminalType,
};
use crate::state::scrollback::{DisplayRow, Scrollback};
use crate::state::selection::{BandSnapshot, Selection};
use crate::state::transcript::MessageKind;
use crate::state::wheel::WheelDir;
use crate::viewport::{self};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// The store of a mode that was never opened.
///
/// `Scrollback` is not cheap to build per call and `new_rows` / `pinned` answer by
/// reference, so the empty answer is built once. A mode with no view has said
/// nothing, is at its tail, and has nothing pending — which is what a default
/// `Scrollback` says, exactly.
fn empty_scrollback() -> &'static Scrollback {
    static EMPTY: OnceLock<Scrollback> = OnceLock::new();
    EMPTY.get_or_init(|| Scrollback::new(0))
}

pub use crate::session::BeadStep;

/// How long the exit path waits for room in the command queue to deliver `Quit`.
///
/// Bounded because *nothing* on this path may wait on a child process. The Router
/// keeps that promise everywhere else (`handle` queues and returns), so a full
/// queue here drains in microseconds and the timeout only ever fires if the
/// Router is wedged by a bug — in which case leaving is the right answer, not
/// hanging the exit on it.
const QUIT_RETRY: Duration = Duration::from_secs(1);

/// The `generation` a harness-level message uses when it has to create a view for
/// a mode that has no session yet (`"could not open Bash: …"`).
///
/// `0` is not just a spare number: the Router's generations start at 1, so a
/// harness placeholder can never collide with a real session's identity.
pub const HARNESS_GENERATION: u64 = 0;

/// The status row's animation pace: one spinner step every 125ms, i.e. 8fps.
///
/// The frame ticks at ~60fps, and painting a braille spinner at that rate spends
/// seven frames in eight on nothing — while a background `sleep 100` keeps the
/// whole pane redrawing for a row that only changes once a second. 8fps is the
/// rate a spinner still reads as spinning. This is not a second timer: the tick
/// that already exists is decimated, which is what the ticket's "driven off the
/// existing ~60fps tick, no extra timers" asks for.
const ROW_ANIM: Duration = Duration::from_millis(125);

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
        // wire-format record; would surface as: "stderr" on the row, if a split backend ever lands
        #[allow(dead_code)]
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

fn print_json_value_to_string(v: &Value) -> String {
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

/// The UI: one [`SessionView`] per terminal state, plus the keyboard.
///
/// Note what is **not** here:
///
/// * no session handles — the Router owns those, and this type must stay cheap
///   enough to draw every frame (ADR-0002 Q4);
/// * no shared transcript — each session has its own (Q5);
/// * no global `need_input` / `chat_state` — both are *derived from the active
///   view*, which is what the ADR asks for and what stops a beads pass running
///   off-screen from hiding the Pi input box.
///
/// `active` mirrors the Router's active mode rather than owning it: a mode only
/// changes when the user presses Tab, and that keystroke goes through the Router,
/// so the two cannot diverge. Crucially, `active` is used for **rendering only**.
/// Event routing never consults it — the envelope names the owner.
pub struct App {
    pub input: InputState,
    pub views: HashMap<TerminalType, SessionView>,
    pub active: TerminalType,
    pub spinner: usize,
    pub width: u16,
    /// The window's height, tracked alongside [`Self::width`] for one reason: the
    /// scrollback needs a page, and a page is the transcript band, and the band
    /// is a function of the whole window (`viewport::frame_areas`).
    ///
    /// Nothing in the frame's geometry is *decided* here — the frame still reads
    /// its own area at draw time. This is the same number one frame earlier, which
    /// is what a keystroke needs and what a stale `ESC[6n` used to be invented
    /// for. It is not a second opinion about the screen: every write of it comes
    /// from a resize event or from the startup size, the same two sources the
    /// frame's own area comes from.
    pub height: u16,
    pub dirty: bool,
    pub should_quit: bool,
    /// The live drag selection over the transcript band (looprs-pdl.9).
    ///
    /// One per App, not one per view, because the ticket's clear list settles it:
    /// a mode switch clears it. There is therefore never a selection that is
    /// live in a mode the user is not looking at, and no question of which
    /// transcript a highlighted range belongs to.
    ///
    /// The state machine itself lives in [`Selection`]; this field is only the
    /// handle the mouse handler, the Esc rule and the frame all read.
    selection: Selection,
    /// Where the **last drawn frame** put the transcript band's rows.
    ///
    /// A cell, written by the render path (`view` calls [`App::record_band`])
    /// and read by the mouse handler, because a pointer position is a claim
    /// about the pixels and the only honest answer to it is the one made by the
    /// code that painted them. Re-deriving the layout at click time would mean
    /// either re-rendering the live tail per motion event or guessing how many
    /// live rows were in the frame the pointer is over — and a guess one row off
    /// selects the wrong paragraph while drawing the box where the user aimed.
    ///
    /// `Cell` rather than a lock or a `RefCell` because it is `Copy` and small:
    /// no borrow exists across a call that could re-enter and write it.
    band: Cell<Option<BandSnapshot>>,
    /// The session whose child currently owns the real terminal screen, if any
    /// (ADR-0001 Q2). `Some` for as long as a full-screen program holds it.
    ///
    /// Tracked here rather than asked of the session because drawing is this type's
    /// job, and the run loop has to be able to ask "am I allowed to draw?" without
    /// reaching into a backend.
    screen: Option<SessionId>,
    /// The alternate screen this app *passed through* to a full-screen child and
    /// has not seen given back.
    ///
    /// Not the same fact as [`Self::screen`]: that says who gets the next frame;
    /// this says who owes the terminal a leave sequence. It is tracked from the
    /// bytes the passthrough wrote, because that is the only record of what the
    /// real terminal was actually told, and it is read once, by
    /// [`crate::teardown::Teardown::restore`], when nothing else is left that can
    /// write. A child killed while holding the screen never pays this; the exit
    /// path does.
    screen_debt: crate::screen::ScreenDebt,
    /// The modes this process holds and a full-screen child is allowed to have
    /// switched off behind our back — written out again the moment the screen comes
    /// back, before anything is drawn.
    ///
    /// Set from [`crate::teardown::Teardown::reassert_bytes`] at startup, so the
    /// list is the ledger's own held set and not a second guess at it. Empty means
    /// "nothing to put back" (no modes on), which is why a default-constructed
    /// `App` behaves exactly as it did before this existed.
    reassert_bytes: Vec<u8>,
    /// Are *we* the ones living in the alternate screen? (ADR-0004 rule 1.)
    ///
    /// The App needs this to know whether taking the screen back includes clearing
    /// the canvas: inside our own alternate screen the child's last frame is our
    /// garbage and the whole display is ours to erase; in the inline pane it is the
    /// user's scrollback and the only thing we may erase is the pane itself.
    alt_screen_hosted: bool,
    /// The screen came back from a full-screen child and must be repainted from
    /// scratch before anything else is seen. Set on release, consumed by the run
    /// loop in `main.rs`, which is the only place holding the frame.
    ///
    /// The reason is not the geometry, it is the *diff*: our back buffer still
    /// describes the screen as it was before the child painted over it, so the
    /// next frame would compare two things that agree and write nothing, leaving
    /// a dead vim's `~` filler where our frame used to be. ADR-0001's "screen is
    /// garbled after exiting vim", one deletion of the inline viewport away.
    pub repaint_all: bool,
    /// The row's wall clock, advanced only by `Msg::Tick` (see [`Self::on_tick`]).
    ///
    /// Held here rather than read at draw time so the render path reads no clock, as
    /// the frame's purity contract requires — and so a test can hand the row any
    /// age it wants without waiting for one.
    clock: Instant,
    /// When the row last animated, and the frame of the spinner it shows.
    ///
    /// Separate from [`Self::spinner`] because that one belongs to the live text
    /// preview and only turns while text is streaming; the row has to animate for a
    /// session that is busy with no text at all (a shell command, a beads pass
    /// between deltas), and must not make the preview look like it started again.
    row_phase: Instant,
    row_spinner: usize,
    /// The wheel's clock: the throttle that turns a burst of scroll reports into
    /// a bounded number of rows, and the record of what the last gesture looked
    /// like (looprs-pdl.8).
    ///
    /// Its own type rather than a couple of fields on `App`, because the rule it
    /// holds is the interesting part and it is testable without an App: the
    /// whole of "a flick must not scroll forty rows" is a statement about
    /// arrival times, and [`crate::state::wheel`] proves it there.
    wheel: crate::state::wheel::WheelCadence,
    cmd_tx: mpsc::Sender<UiCommand>, // UI -> Router
}

impl App {
    pub fn new(
        cmd_tx: mpsc::Sender<UiCommand>,
        mut input: InputState,
        active: TerminalType,
        width: u16,
        height: u16,
    ) -> Self {
        // The box and the view must open on the same mode: they are the same fact
        // seen from two sides, and a mismatch here would route keystrokes to a mode
        // the user is not looking at.
        input.mode = active;
        let now = Instant::now();
        Self {
            input,
            views: HashMap::new(),
            active,
            width,
            height,
            dirty: true,
            should_quit: false,
            selection: Selection::None,
            band: Cell::new(None),
            spinner: 0,
            screen: None,
            screen_debt: crate::screen::ScreenDebt::new(),
            reassert_bytes: Vec::new(),
            alt_screen_hosted: false,
            repaint_all: false,
            clock: now,
            row_phase: now,
            row_spinner: 0,
            wheel: crate::state::wheel::WheelCadence::default(),
            cmd_tx,
        }
    }

    /// The view for one session, created on first sight.
    ///
    /// Adopting a *new generation* of a mode seals the previous incarnation's open
    /// entry first. That session can never close it — its pump is gone — and an
    /// unsealed entry stalls the flusher forever. This is the second half of
    /// "always seal"; the first is `Msg::SessionDown`. Together they keep the
    /// transcript correct whether or not the dying session got to say goodbye.
    pub fn view_mut(&mut self, id: SessionId) -> &mut SessionView {
        let v = self
            .views
            .entry(id.mode)
            .or_insert_with(|| SessionView::new(id));
        if v.session != id {
            v.seal();
            v.session = id;
            v.status = SessionStatus::NotStarted;
            // ...and is not halfway through a run whose age the row would carry
            // over from a process that no longer exists.
            v.run_started = None;
            // A new incarnation of a mode is not holding the previous one's
            // ticket. Leaving a stale claim on the row would have the UI naming a
            // bead no live process owns.
            v.active_bead = None;
            // Nor is it holding the previous one's bill. A respawned child is a new
            // session, and `↑ in / ↓ out` means "this session", so it starts at
            // nothing — carrying a dead child's total over would make a plain
            // respawn read like a runaway.
            v.tokens = Default::default();
        }
        v
    }

    /// A specific mode's view — and the way the status row sees the modes it is
    /// **not** showing, which is the whole reason ADR-0002 keeps them warm and
    /// invisible (looprs-guh).
    pub fn view(&self, mode: TerminalType) -> Option<&SessionView> {
        self.views.get(&mode)
    }

    pub fn active_view(&self) -> Option<&SessionView> {
        self.views.get(&self.active)
    }

    /// Does the **active** session want typed input? (Was: a global `need_input`.)
    /// No view yet means yes: a mode nobody has opened is idle by definition.
    ///
    /// This only picks *which* view to ask. The rule — Bash always, the agentic
    /// modes only while they are not working — is [`SessionView::accepts_input`],
    /// and keeping it there is what stops the box and the view disagreeing about
    /// whose keyboard this is.
    pub fn need_input(&self) -> bool {
        self.active_view()
            .map(|v| v.accepts_input())
            .unwrap_or(true)
    }

    /// What the live region of the **active** session shows.
    pub fn chat_state(&self) -> ChatState {
        self.active_view().map(|v| v.chat).unwrap_or_default()
    }

    /// The per-frame flush, for the active view only (Q5 rule 4: inactive views
    /// buffer, and their backlog goes out as one burst when they become active).
    ///
    /// Returns the rows that just became final; they are already in the store
    /// ([`Self::scrollback`]) by the time this returns, so the return value is
    /// for the caller that wants to know what arrived, not for the display.
    pub fn flush_active(&mut self, width: u16) -> Vec<RenderedRow> {
        match self.views.get_mut(&self.active) {
            Some(v) => {
                let rows = v.flush(width);
                self.sync_selection_to_trims();
                rows
            }
            None => Vec::new(),
        }
    }

    /// Re-base the drag selection against the trims the active view applied.
    ///
    /// The buffer cap evicts entries, the store renumbers its rows by entry,
    /// and the selection speaks those same addresses. Without this the two
    /// drift the first time the cap bites: the entries under a standing
    /// selection shift down and the highlight starts pointing at the message
    /// *after* the one the user dragged across, which is worse than losing the
    /// selection. `Selection::entries_evicted` is the same mapping the store
    /// was given, fed the same two numbers from the same place
    /// (`SessionView::take_trims`), and it clears the selection when the
    /// eviction ate the anchor — the ticket's "a trim that ate the anchor",
    /// wired rather than promised.
    ///
    /// Called from [`Self::flush_active`], which the run loop calls inside the
    /// draw branch, so the selection cannot be drawn with stale addresses; and
    /// again at the end of `App::update`, so a consumer that reads it without
    /// a frame in between (select-to-copy) sees the same truth.
    ///
    /// Only the active view's trims are applied: a selection cannot exist on a
    /// view that is not on screen, and the mode switch that changes which view
    /// that is clears the selection anyway. Inactive views' queues drain when
    /// they become active, onto a selection that has just been cleared, which
    /// is a no-op — so nothing accumulates and nothing is misapplied.
    fn sync_selection_to_trims(&mut self) {
        let Some(v) = self.views.get_mut(&self.active) else {
            return;
        };
        for (removed, notice_at) in v.take_trims() {
            self.selection.entries_evicted(removed, notice_at);
        }
    }

    /// The active view's live (not-yet-final) tail, taken once per frame.
    ///
    /// The frame needs it twice — as the *height* the live region should be, and as
    /// the lines to draw — so it is computed here and handed to both. That is also
    /// why it goes through the same one-viewport-one-flusher door as
    /// [`Self::flush_active`]: `SessionView::preview` is the only way in, so the
    /// count that sized the pane cannot describe a different view than the pixels
    /// that fill it (ADR-0002 Q5).
    pub fn preview_active(&self, width: u16) -> Vec<Line<'static>> {
        self.active_view()
            .map(|v| v.preview(width))
            .unwrap_or_default()
    }

    /// The active session's settled transcript, bottom-of-the-list-last.
    ///
    /// This is the content of the frame's transcript band: every line the session
    /// finished saying, already rendered at the width it was rendered at, in the
    /// store the transcript now is. The live tail is *not* in here; that comes
    /// from [`Self::preview_active`], and the band is the two of them joined in
    /// that order.
    pub fn scrollback(&self) -> &Scrollback {
        match self.active_view() {
            Some(v) => v.scrollback(),
            None => empty_scrollback(),
        }
    }

    /// The rows to draw in a transcript band `visible` rows tall.
    ///
    /// This is the whole of the scroll offset's effect on the screen: the frame
    /// asks for a window, the store answers with the rows the offset says, and
    /// the band draws them. When pinned that is the tail; when not, it is the
    /// stretch of history the user stopped on.
    pub fn transcript_window(&self, visible: usize) -> &[DisplayRow] {
        self.active_view()
            .map(|v| v.scrollback().window(visible))
            .unwrap_or(&[])
    }

    /// Rows that arrived while the user was off the tail — the "N new"
    /// affordance's whole data source, and zero whenever the view is pinned.
    pub fn new_rows(&self) -> usize {
        self.scrollback().pending()
    }

    /// Is the active view following the tail?
    pub fn pinned(&self) -> bool {
        self.scrollback().is_pinned()
    }

    /// Move the active view: positive toward the tail, negative into history.
    pub fn scroll_active(&mut self, delta: isize) {
        let visible = self.transcript_band_rows();
        if let Some(v) = self.views.get_mut(&self.active) {
            v.scrollback_mut().scroll_by(delta, visible);
        }
    }

    /// Snap the active view back to the tail — the one action the "N new"
    /// affordance names.
    pub fn tail_active(&mut self) {
        if let Some(v) = self.views.get_mut(&self.active) {
            v.scrollback_mut().scroll_to_tail();
        }
    }

    /// The top of the transcript.
    pub fn top_active(&mut self) {
        let visible = self.transcript_band_rows();
        if let Some(v) = self.views.get_mut(&self.active) {
            v.scrollback_mut().scroll_to_top(visible);
        }
    }

    // ───────────────────── drag selection (looprs-pdl.9) ─────────────────────

    /// The live drag selection, as the frame sees it.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// What the live selection would copy, as characters (ADR-0004 R14, R15).
    ///
    /// Empty when nothing is selected, and empty for a selection of blanks —
    /// which is R13's "a selection whose value is empty after trimming copies
    /// nothing and says nothing" with half its work already done: the value is
    /// resolved from the store's characters, not from the rows it was drawn
    /// over. The transport, the count and the toast are looprs-pdl.10's; the
    /// *shape* is here first so that "what was selected" has one definition in
    /// the tree and the copy ticket cannot invent a second one under pressure.
    #[allow(dead_code)] // consumer: select-to-copy (looprs-pdl.10)
    pub fn selection_paste(&self) -> String {
        self.selection.paste(self.scrollback().rows())
    }

    /// Publish where this frame drew the transcript band's rows.
    ///
    /// Called from [`crate::view`] with the same [`BandLayout`] the band drew
    /// itself from, which is the point: the hit-test and the pixels come from
    /// one computation, so they cannot disagree about which row the pointer is
    /// on. See [`App::band`].
    pub fn record_band(&self, snap: BandSnapshot) {
        self.band.set(Some(snap));
    }

    /// The character under a screen position, in the transcript.
    ///
    /// `None` for anything that is not settled transcript text: the chrome
    /// bands (not in the store, so not hittable at all), the padding above a
    /// short band, and **the live tail** — which is not in the store yet, and
    /// so is not selectable. That last one is a decision rather than an
    /// omission: the live line is being rewritten as it arrives, and a selection
    /// addressed into text that has no final form yet cannot promise what it
    /// would copy. It becomes selectable the frame after it is flushed, which
    /// is the frame after it stops changing.
    fn hit(&self, screen_row: u16, screen_col: u16) -> Option<crate::state::selection::CharRef> {
        let snap = self.band.get()?;
        let idx = snap.row_index(self.scrollback(), screen_row)?;
        let cell = snap.cell_of(screen_col);
        // Through a blank line the pointer rests at the end of the line above
        // rather than freezing (`selection::hit_resting`).
        crate::state::selection::hit_resting(self.scrollback().rows(), idx, cell)
    }

    /// A mouse report arrived, and which gesture it is comes out of `m.kind`.
    ///
    /// Three things read this one stream: the left button drives the drag
    /// selection (looprs-pdl.9), the wheel drives the transcript (looprs-pdl.8,
    /// [`Self::on_wheel`]), and every other button is **reported as unbound**
    /// rather than swallowed — with capture on, a middle-click that finds no
    /// binding here is a paste that silently does not happen anywhere, and the
    /// ticket that binds it is looprs-pdl.11. `now` is the report's arrival
    /// time, which the wheel's rate is measured against; see
    /// [`crate::state::wheel`].
    pub fn on_mouse(&mut self, m: crossterm::event::MouseEvent, now: Instant) {
        // A child holding the real screen owns the pointer with it. The pixels
        // under the cursor are the child's, so a hit against the last frame
        // *we* drew would be a fiction, and a selection band painted over a
        // full-screen program is the bug the draw gate in `main` exists to
        // stop — reached by a different input this time.
        if self.passthrough() {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A press that cannot hit transcript starts nothing, and
                // leaves whatever selection was standing alone: a click on
                // chrome is not a statement about the transcript.
                if let Some(at) = self.hit(m.row, m.column) {
                    self.selection.press(at);
                    self.dirty = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.selection.is_dragging() {
                    // A motion report with no press of ours behind it. Not our
                    // gesture; do not start one.
                    return;
                }
                let Some(snap) = self.band.get() else {
                    return;
                };
                // Scroll **before** re-reading the pointer. The content under
                // the cursor is what the band shows *after* the auto-scroll,
                // so the focus has to be looked up against the scrolled view;
                // doing it the other way round freezes the selection one row
                // short and leaves the box chasing the mouse.
                let step = self.selection.auto_scroll(snap.edge_at(m.row), self.clock);
                if step != 0 {
                    self.scroll_active(step);
                    // …and re-publish where the band's rows are now. The view
                    // moved and the screen has not been repainted yet, so
                    // without this the next motion event maps the pointer to
                    // the row it was on *before* the scroll, the focus lands
                    // back where it already was, and the selection never
                    // grows past the edge. This is the same relationship the
                    // frame keeps — one layout, one truth about where row N is
                    // — just advanced by the scroll instead of by a draw.
                    let win = self.transcript_window(snap.rows);
                    // Only re-publish while the drawn height is unchanged. If
                    // the scroll ran out of content and the band now has fewer
                    // rows, they would be bottom-pinned at a different `y`, and
                    // re-deriving that here would be the frame's job, not the
                    // mouse handler's — so the snapshot stays as it is. The
                    // view cannot scroll further in that direction either, so
                    // nothing is being asked of the stale mapping.
                    if win.len() == snap.rows {
                        let moved = BandSnapshot::new(
                            snap.area,
                            snap.first_row_y,
                            snap.rows,
                            win.first().map(|r| r.anchor()),
                        );
                        self.record_band(moved);
                    }
                }
                if let Some(at) = self.hit(m.row, m.column)
                    && self.selection.drag(at)
                {
                    self.dirty = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if !self.selection.is_dragging() {
                    return;
                }
                // Either it committed or it was a click and cleared. Both
                // change what is on the screen, so `dirty` is deliberately not
                // conditional on there being a range.
                self.selection.release();
                self.dirty = true;
            }
            // **The wheel** (looprs-pdl.8): the transcript is what scrolls.
            // Both directions arrive here, and the row they landed on decides
            // whether the transcript is allowed to move at all.
            MouseEventKind::ScrollUp => {
                self.on_wheel(WheelDir::Up, m.row, now);
            }
            MouseEventKind::ScrollDown => {
                self.on_wheel(WheelDir::Down, m.row, now);
            }
            // **Not our buttons.** An unbound button is not the same thing as a
            // swallowed one: with `?1000/?1002/?1006` held, the terminal has
            // already given up doing anything with these clicks, so a middle-click
            // that finds no binding here is a paste that silently does not happen
            // (X11-style middle-click paste is the terminal's, and the terminal
            // was just told to ask us first). Nothing in this ticket binds them,
            // so the least honest thing available is to *say* so, on every one,
            // with the coordinates it arrived at: the ticket that gives the wheel
            // its neighbours back is looprs-pdl.11, and this is the line that
            // will be read when that gets written.
            MouseEventKind::Down(btn @ (MouseButton::Right | MouseButton::Middle))
            | MouseEventKind::Up(btn @ (MouseButton::Right | MouseButton::Middle)) => {
                tracing::info!(
                    button = ?btn,
                    row = m.row,
                    col = m.column,
                    "mouse button is captured with no binding: it will do nothing. \
                     looprs-pdl.11 owns middle-click paste and the right-click menu."
                );
            }
            _ => {}
        }
    }

    /// The wheel or trackpad moved, and `row` is where the cursor was.
    ///
    /// Three doors, in this order, and each one is a different ticket's rule:
    ///
    /// 1. **a child holding the screen owns the pointer** — same rule as the
    ///    drag, same early return, so a wheel over a full-screen `vim` never
    ///    touches our scroll state;
    /// 2. **chrome owns the cursor over chrome** — [`BandSnapshot::contains_row`]
    ///    is the frame's own answer about where the transcript band is, so the
    ///    input box, the status row and the card band keep whatever their own
    ///    wheel behaviour turns out to be and are never scrolled from here;
    /// 3. **the cadence decides how far** — [`WheelCadence::step`] answers with
    ///    the whole-number row delta this report buys, which is `0` for most of
    ///    the reports in a trackpad burst and is the whole reason a flick does
    ///    not read forty rows.
    ///
    /// What happens after that is the store's: [`Self::scroll_active`] is the
    /// same door `PageUp` goes through, so unpinning on the way up and re-
    /// pinning on the way back down are one rule and not a wheel-shaped
    /// imitation of one.
    ///
    /// Returns whether the view actually moved, which is also the only condition
    /// under which this marks the frame dirty: a wheel at the end of the
    /// transcript, or a throttled report inside a burst, must cost a
    /// comparison and nothing else — no repaint, no frame, no bytes on the
    /// wire.
    pub fn on_wheel(&mut self, dir: WheelDir, row: u16, now: Instant) -> bool {
        if self.passthrough() {
            return false;
        }
        let Some(snap) = self.band.get() else {
            return false;
        };
        if !snap.contains_row(row) {
            tracing::trace!(
                row,
                ?dir,
                "wheel over chrome: the widget under the cursor owns the report"
            );
            return false;
        }
        let delta = self.wheel.step(dir, now);
        if delta == 0 {
            return false;
        }
        let before = self.scrollback().offset();
        self.scroll_active(delta);
        let moved = self.scrollback().offset() != before;
        if moved {
            self.dirty = true;
        }
        moved
    }

    /// The record of the wheel gesture that ended most recently, if any.
    ///
    /// The measurement looprs-pdl.2 #4b left unclaimed — reports per gesture,
    /// how long it ran, how many rows it moved — readable by anything that has
    /// the App, and logged at `debug` by the cadence itself for whoever is
    /// reading `looprs.log` with a trackpad in hand.
    #[allow(dead_code)] // diagnostic/test seam: nothing in the frame reads a gesture's shape, which is exactly why the shape has to be readable from outside the draw path; a real flick's numbers come out of `looprs.log` (see `state::wheel`'s measurements)
    pub fn wheel_gesture(&self) -> Option<crate::state::wheel::Gesture> {
        self.wheel.last_gesture()
    }

    /// The transcript band's height at the current window size: a "page" of
    /// scrollback.
    ///
    /// Read out of [`viewport::frame_areas`] rather than re-derived, so the page
    /// a scroll key moves is the band the frame actually lays out — the same
    /// reason `input_rows` and `preview_active` are taken once and handed down.
    pub fn transcript_band_rows(&self) -> usize {
        let [text, ..] = viewport::frame_areas(
            Rect::new(0, 0, self.width, self.height),
            self.live_card_rows(),
            self.input_band(self.width),
        );
        text.height as usize
    }

    /// The real window changed shape (`Event::Resize`).
    ///
    /// Three things follow, and only three: our wrapping width changes, the
    /// children must be told because a pty sized 80x24 while the window is
    /// 180x50 wraps every program's output for a terminal that is not there
    /// (ADR-0001 rule 6), and the frame must be redrawn. The frame's *own* idea
    /// of its area is not one of them — `draw` re-reads the window itself, and
    /// for a full-screen viewport that is an `ioctl` and a clear, not a cursor
    /// query the key stream has to get out of the way for.
    ///
    /// The one exception is a resize taken while a child holds the screen: the
    /// frame we will draw when it comes back is a full repaint anyway (the child
    /// painted over everything, at whatever size the window had at the time), so
    /// the flag is set here rather than trusting a later diff.
    pub fn set_window(&mut self, cols: u16, rows: u16) {
        self.width = cols;
        self.height = rows;
        self.forward_resize(rows, cols);
        self.dirty = true;
        if self.passthrough() {
            self.repaint_all = true;
        }
    }

    /// How many live cards the live region is carrying right now — open tool calls
    /// and any open compaction.
    ///
    /// The frame paints at most [`crate::viewport::MAX_TOOL_ROWS`] of them and the
    /// height policy budgets the same cap, so a wall of concurrent calls cannot
    /// take the live text's rows — or the input box's — away (looprs-afw).
    ///
    /// Compaction shares that budget rather than getting a row of its own, which is
    /// safe for the one case where it could be squeezed out: pi compacts in
    /// `prepareNextTurn`, after a tool batch has finished and reported, so a
    /// compaction is not competing with four live tools for the fifth row. If that
    /// ever stops being true, the fix is the cap's ordering, not a second band.
    pub fn live_card_rows(&self) -> u16 {
        self.active_view()
            .map(|v| v.transcript.open_cards().count() as u16)
            .unwrap_or(0)
    }

    /// How many rows the input box wants this frame, at `width`.
    ///
    /// It grows with the text instead of cutting it off at one line, which is the
    /// difference between typing into a box and typing into a slot. The count comes
    /// from the *same* wrapping the box draws with
    /// ([`InputState::display_lines`]), so the height policy and the pixels cannot
    /// disagree — the same reason [`Self::preview_active`] is the only way to the
    /// live text. Capped by [`viewport::MAX_INPUT_ROWS`]: the box and the live
    /// preview want the same rows, and past the cap the box scrolls to the caret
    /// rather than winning the argument.
    pub fn input_rows(&self, width: u16) -> u16 {
        let inner = inner_width(width);
        viewport::input_rows(self.input.display_lines(inner).len() as u16)
    }

    /// How many rows the input band gets this frame: the box's own height, or
    /// [`crate::viewport::NO_INPUT_ROWS`] when the active session is not taking
    /// input.
    ///
    /// This is the one place the two decisions the frame makes about the box —
    /// "is it showing?" and "how tall is it?" — are folded into a single number,
    /// because they are asked by two different callers: the height policy budgets
    /// these rows, and `main::view` draws the box into them. If they were two
    /// separate questions, a mode that hid the box could keep its three reserved
    /// rows (the status row hanging above a band of blank screen) or, worse, a
    /// hidden box's rows could be handed out twice.
    ///
    /// A hidden box is granted *nothing*: `frame_areas` then puts the status row on
    /// the bottom edge of the live region, which is where a status row belongs when
    /// there is nothing under it.
    pub fn input_band(&self, width: u16) -> u16 {
        if self.need_input() {
            self.input_rows(width)
        } else {
            crate::viewport::NO_INPUT_ROWS
        }
    }

    /// The status row for this frame (looprs-guh).
    ///
    /// `App`'s job here is to *gather*, not to decide: every fact comes off a view
    /// mirror a session published, and the layout, the truncation and the priority
    /// order all live in [`components::status`](crate::components::status), which
    /// is a pure function of what it is handed. Nothing here asks a session,
    /// a `bd`, or the clock.
    pub fn status_line(&self, width: u16) -> Line<'static> {
        crate::components::status::Status {
            active: self.sess(self.active),
            background: self.busy_background(),
            warm: self.warm_modes(),
            tokens: self.view(self.active).map(|v| v.tokens).unwrap_or_default(),
            spinner: self.row_spinner,
        }
        .line(width)
    }

    /// One mode's row input, read off its view.
    ///
    /// A mode with no view at all is `NotStarted` rather than absent: "never
    /// entered" is a state the row renders (`○ Beeds · not started`), not a hole
    /// in it.
    fn sess(&self, mode: TerminalType) -> crate::components::status::Sess<'_> {
        let Some(v) = self.view(mode) else {
            return crate::components::status::Sess {
                mode,
                status: SessionStatus::NotStarted,
                step: None,
                bead: None,
                elapsed: None,
                error: None,
            };
        };
        crate::components::status::Sess {
            mode: v.session.mode,
            status: v.status,
            step: v.step,
            bead: v.active_bead.as_ref(),
            elapsed: v.run_elapsed(self.clock),
            error: v.last_error.as_deref(),
        }
    }

    /// The modes that are not on screen and are **busy** — the sentence ADR-0002
    /// says this row exists to make visible: "beads is still working while I am
    /// chatting in Pi".
    fn busy_background(&self) -> Vec<crate::components::status::Sess<'_>> {
        TerminalType::ALL
            .iter()
            .filter(|m| **m != self.active)
            .map(|m| self.sess(*m))
            .filter(|s| s.busy())
            .collect()
    }

    /// The modes with a warm child: alive, idle, resident, and invisible without
    /// this. Costs memory and nothing else, so it is one segment rather than one
    /// per mode — the honest content is "a process is being kept for you".
    fn warm_modes(&self) -> Vec<TerminalType> {
        TerminalType::ALL
            .iter()
            .filter(|m| **m != self.active)
            .filter(|m| {
                self.view(**m)
                    .is_some_and(|v| v.status.is_alive() && !v.status.is_busy())
            })
            .copied()
            .collect()
    }

    /// Is **any** session busy, on screen or not?
    fn any_busy(&self) -> bool {
        self.views.values().any(|v| v.status.is_busy())
    }

    /// Advance the row's clock and animation from the frame tick, and say nothing
    /// back — the effect is on `dirty`.
    ///
    /// The row carries two time-shaped things: a run's age, to the second, and a
    /// spinner while anything is busy. Neither needs 60fps, and repainting the pane
    /// sixty times a second for a row that changes eight of them is the difference
    /// between "animated" and "the whole UI is doing something". When nothing is
    /// busy the row is static and no repaint is requested at all, which is what
    /// keeps an idle app from redrawing forever.
    ///
    /// Public and taking `now` as an argument so a test can advance the clock
    /// without sleeping.
    pub fn on_tick(&mut self, now: Instant) {
        self.clock = now;
        if !self.any_busy() {
            return;
        }
        if now.saturating_duration_since(self.row_phase) < ROW_ANIM {
            return;
        }
        self.row_phase = now;
        self.row_spinner = self.row_spinner.wrapping_add(1);
        self.dirty = true;
    }

    /// Echo the user's own line into the mode it was typed into.
    ///
    /// It targets the view that *exists* for that mode rather than a synthetic
    /// session id, so a local echo can never change which generation a view is
    /// tracking — that would seal the real session's entry mid-stream.
    pub fn echo_local(&mut self, mode: TerminalType, text: String) {
        let id = self
            .views
            .get(&mode)
            .map(|v| v.session)
            .unwrap_or_else(|| SessionId::new(mode, HARNESS_GENERATION));
        self.view_mut(id).push_note(MessageKind::User, text);
    }

    /// Where a harness-level message (`session: None`) goes: the view the user is
    /// looking at, because that is the only one they can see.
    fn target_view(&mut self, session: Option<SessionId>) -> &mut SessionView {
        match session {
            Some(id) => self.view_mut(id),
            None => {
                let mode = self.active;
                self.view_mut(SessionId::new(mode, HARNESS_GENERATION))
            }
        }
    }

    /// The one door every state change comes through.
    ///
    /// The body is [`Self::update_inner`]; what this adds on top is the trim
    /// re-base, and it sits outside that body on purpose. Any arm of that
    /// match can push output, pushing output can trip the buffer cap, and the
    /// cap moves the entries out from under a standing drag selection (see
    /// [`Self::sync_selection_to_trims`). Putting the re-base after the match
    /// instead of in each arm means the arm that returns early is not also
    /// the arm that forgot.
    pub fn update(&mut self, msg: Msg) {
        self.update_inner(msg);
        self.sync_selection_to_trims();
    }

    fn update_inner(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => {
                if self.chat_state().is_streaming() {
                    self.spinner = self.spinner.wrapping_add(1);
                    self.dirty = true;
                }
                self.on_tick(Instant::now());
            }
            Msg::Term(Event::Resize(w, h)) => {
                // Routed through [`Self::set_window`] rather than assigning the
                // two fields inline (looprs-pdl.15). Adopting a window is one
                // operation and it now has exactly three callers — the run
                // loop's event arm, the run loop's size poll, and this — all
                // of which go through the same door. An inline assignment here
                // was a fourth way that quietly forgot two things the door
                // remembers: the children must be told (ADR-0001 rule 6) and a
                // passthrough in progress owes itself a full repaint.
                self.set_window(w, h);
            }
            Msg::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                self.dirty = true;
                self.on_key(k);
            }
            // Mouse reports (looprs-pdl.9, .8). The three mouse modes are on
            // the ledger and these are the bytes they produce; nothing else
            // reads them. The clock is handed in rather than read at need so
            // the wheel's rate can be driven from a test at whatever cadence
            // the test wants — the same seam `Selection::auto_scroll` uses.
            Msg::Term(Event::Mouse(m)) => self.on_mouse(m, Instant::now()),
            Msg::Term(_) => {}
            Msg::Agent { session, event } => {
                self.dirty = true;
                self.on_pi(session, event);
            }
            Msg::BashOutput { session, chunk, .. } => {
                // `stream` is not consulted: a pty hands us one merged byte
                // stream and `ByteStream::Merged` is what it says.
                if self.teed(session) {
                    // The child owns this screen: its bytes go to the real
                    // terminal verbatim and **not** into the transcript. The frame
                    // is already on screen, and rendering a second copy of the
                    // same paint above the viewport is how a full-screen program
                    // ends up smeared through scrollback. For an alt-screen child
                    // this is also precisely what a real terminal does — the
                    // alternate screen is discarded on exit, not recalled.
                    crate::screen::tee(chunk.as_bytes());
                    // Recorded in the same breath as the write: the debt is a
                    // fact about these bytes reaching the real terminal, and the
                    // one thing it may not be is a guess made later about whether
                    // they got there.
                    self.screen_debt.note_tee(chunk.as_bytes());
                } else {
                    // Line-oriented output: `push_bash` resolves the presentation
                    // into styles and in-line edits, and never re-wraps and never
                    // reaches markdown (ADR-0001 rule 1, ADR-0005). The width
                    // handed over is the width the pty was given, so a `\r` in
                    // the stream lands on the row the child thought it had.
                    self.dirty = true;
                    let width = self.width;
                    self.view_mut(session).push_bash(&chunk, width);
                }
            }
            Msg::BeadStep { session, step } => {
                self.dirty = true;
                self.view_mut(session).set_step(step);
            }
            Msg::SessionStatus { session, status } => {
                // Mirrored, and nothing else. The row reads `status`; no decision
                // here may be made on the basis of it, which is the same rule
                // `BeadStep` and `ActiveBead` live by: the session owns the state,
                // the UI renders it.
                self.dirty = true;
                let now = self.clock;
                self.view_mut(session).set_status(status, now);
            }
            Msg::ActiveBead { session, bead } => {
                // A mirror of the loop's claim, recorded rather than interpreted:
                // the status row (looprs-guh) reads this field, and nothing here
                // decides what to *do* with it.
                self.dirty = true;
                let v = self.view_mut(session);
                // Taking a claim opens a fresh token window: from here the row
                // answers "what is *this ticket* costing". Releasing one (None)
                // deliberately leaves the total alone, so the number survives the
                // pass it describes.
                if bead.is_some() {
                    v.tokens = Default::default();
                }
                v.active_bead = bead;
            }
            Msg::SessionDown { session, reason } => {
                // Q5 rule 3: death must seal. Unconditional, because the pump
                // promises exactly one of these per session ever created.
                self.dirty = true;
                // A child that died holding the screen still gave it up — by dying.
                // Nobody else can say so: the release normally comes from the
                // child's own bytes, and there are not going to be any more of
                // those.
                if self.screen == Some(session) {
                    self.screen = None;
                    self.repaint_all = true;
                }
                let now = self.clock;
                let view = self.view_mut(session);
                view.seal();
                view.set_status(SessionStatus::Dead, now);
                view.push_note(MessageKind::System, format!("{session} ended ({reason:?})"));
            }
            Msg::ScreenHeld { session, active } => {
                if active {
                    self.screen = Some(session);
                    // Nothing to draw while the child holds the screen, and the
                    // frames we would have queued come back on release.
                    self.dirty = false;
                } else if self.screen == Some(session) {
                    self.screen = None;
                    // The modes the child may have switched off come back on
                    // first: the child's last bytes are already on the wire, this
                    // process still owns the screen, and the frame that follows is
                    // drawn against the mode set the ledger says we are running
                    // with. `?2004` and `?1000/2/6` are the ones a real vim
                    // takes away; they are not ours to lose.
                    let modes = self.take_back_screen();
                    if !modes.is_empty() {
                        crate::screen::tee(&modes);
                    }
                    // The real terminal is not showing what ratatui's diff thinks
                    // it is showing: the child drew over it (or switched it, in
                    // the alt-screen case, where switching back restores the main
                    // screen but not our cursor). Re-anchor, then repaint from
                    // scratch — trusting the diff here is the "screen is garbled
                    // after exiting vim" bug ADR-0001 names.
                    self.repaint_all = true;
                    self.dirty = true;
                }
            }
            Msg::Error { session, text } => {
                self.dirty = true;
                self.target_view(session).push_error(text);
            }
            Msg::System { session, text } => {
                self.dirty = true;
                self.target_view(session)
                    .push_note(MessageKind::System, text);
            }
            Msg::RestoreInput { session, text } => {
                self.dirty = true;
                self.restore_input(session, text);
            }
        }
    }

    /// Wire this App's passthrough to the teardown's alternate-screen debt.
    ///
    /// Handed in rather than owned here because the debt belongs to the exit path:
    /// the App never hands the terminal back, it can only report what it wrote to
    /// it — which is what [`Self::update`] does for every teed chunk.
    pub fn set_screen_debt(&mut self, debt: crate::screen::ScreenDebt) {
        self.screen_debt = debt;
    }

    /// Tell this App which modes to put back on when a full-screen child hands the
    /// screen over. See [`Self::reassert_bytes`].
    pub fn set_reassert_bytes(&mut self, bytes: Vec<u8>) {
        self.reassert_bytes = bytes;
    }

    /// Tell this App that its frames live in the alternate screen.
    pub fn set_alt_screen_hosted(&mut self, hosted: bool) {
        self.alt_screen_hosted = hosted;
    }

    /// Take the screen back from a full-screen child.
    ///
    /// Returns the bytes that have to go to the real terminal *before* anything
    /// else is drawn: the modes the ledger still holds and the child was free to
    /// switch off (mouse capture, bracketed paste, the hidden cursor). Not
    /// re-enabling them is the failure looprs-pdl.2 measured — after a
    /// `mouse=a` vim the app's mouse is dead and a pasted line is N submits,
    /// with nothing on screen to say why.
    ///
    /// Returns a clone rather than consuming the list, because this is not a
    /// one-shot: every child in the session takes the modes with it on the way
    /// out, so every return has to put them back. The ledger's leave at the exit
    /// is still exactly one per mode — writing the `h` bytes again is not a second
    /// `enable` and books nothing.
    ///
    /// Split out so a test can read the bytes without a unit test writing to the
    /// real stdout, and called with the tee itself from `update` so the ordering
    /// (child's last bytes, our modes, our frame) is not something each caller
    /// has to remember.
    pub fn take_back_screen(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.alt_screen_hosted {
            // The child's last frame is painted on the screen we still own. Leave
            // it there and the user keeps looking at a dead vim's `~` filler above
            // our pane; in a plain terminal those cells vanished the moment the
            // program left the alternate screen. The equivalent here is to hand
            // ourselves a blank canvas before the repaint.
            out.extend_from_slice(crate::screen::alt_canvas());
        }
        out.extend_from_slice(&self.reassert_bytes);
        out
    }

    /// The alternate-screen debt this App has run up, as the exit path sees it.
    ///
    /// Test-only: the real consumer is the teardown, which holds its own handle to
    /// the same debt and never asks the App about it. This is the window the tests
    /// look through to see what the passthrough booked.
    #[cfg(test)]
    pub fn screen_debt(&self) -> &crate::screen::ScreenDebt {
        &self.screen_debt
    }

    /// Is the **active** mode the one whose child currently owns the real screen?
    ///
    /// This is the gate the run loop asks before drawing anything. Answering it for
    /// the *active* mode rather than for the holder in general matters: a Bash
    /// child can hold the screen while the user looks at another mode, and teeing
    /// its paint over that mode would be worse than not showing it.
    pub fn passthrough(&self) -> bool {
        self.screen.is_some_and(|s| s.mode == self.active)
    }

    /// Should *this* session's bytes go out to the real terminal? Both halves are
    /// needed: the session is the one holding the screen, **and** that session is
    /// the mode actually on it. Either half alone is a bug — the first ignored is
    /// "Bash paints over the Pi view", the second is "a dead generation's bytes
    /// overwrite what the current one owns".
    fn teed(&self, session: SessionId) -> bool {
        self.screen == Some(session) && self.passthrough()
    }

    /// Tell every live session the real terminal changed shape (ADR-0001 rule 6).
    ///
    /// Best-effort on purpose: the command channel is small and a resize that cannot
    /// be queued is superseded by the next one. Blocking the input loop to deliver a
    /// window size would be worse than delivering a stale one.
    pub fn forward_resize(&self, rows: u16, cols: u16) {
        let _ = self.cmd_tx.try_send(UiCommand::Resize { rows, cols });
    }

    /// Tell the Router to shut every session down (exit step 2).
    ///
    /// A command and not a `drop`: the caller has work left to do with this App
    /// afterwards — reading the messages the sessions emit as they go — and
    /// dropping takes the reader with it.
    ///
    /// Best-effort with one bounded retry. A *full* queue is a wait on the queue
    /// and nothing else: `Router::handle` never awaits a child, so it drains in
    /// microseconds. A *closed* channel means the Router is already gone, which
    /// is not something to retry or to complain about.
    pub async fn request_shutdown(&self) {
        match self.cmd_tx.try_send(UiCommand::Quit) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("the router is already gone; nothing left to shut down");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if tokio::time::timeout(QUIT_RETRY, self.cmd_tx.send(UiCommand::Quit))
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        "the quit command never reached the router; \
                         the sessions go with the process"
                    );
                }
            }
        }
    }

    /// Hand text back to the input box (Esc's queued-message restore).
    ///
    /// Two guards, both about not losing what the user has:
    ///
    /// * it goes to the box only while that session's mode is the one on screen —
    ///   keyed on `active` rather than `input.mode` so event handling still never
    ///   reads the input mode (ADR-0002 Q2), and the two are moved together by
    ///   the one Tab handler anyway;
    /// * it never overwrites text the user typed in the meantime. The restore is
    ///   asynchronous; their keystrokes are newer than it is. Those words are not
    ///   thrown away either — they go to the transcript where they can be
    ///   re-typed, because "silently dropped" is the failure this whole recipe
    ///   exists to prevent.
    pub fn restore_input(&mut self, session: SessionId, text: String) {
        if session.mode == self.active && self.input.text().trim().is_empty() {
            self.input.set_text(text);
            return;
        }
        self.view_mut(session).push_note(
            MessageKind::System,
            format!("not restored to the input box: {text}"),
        );
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) {
        self.dirty = true;
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            match k.code {
                // ADR-0001 Q3: in the Bash view Ctrl-C belongs to the shell, not
                // to looprs. It goes down the same road Esc takes — the router hands
                // it to the Bash session, which writes `0x03` to the pty master
                // and lets the line discipline SIGINT the foreground process
                // group. Quitting on Ctrl-C here would make `sleep 30` unstoppable
                // and `vim` unreachable, which is the entire reason Bash mode has a
                // pty. Other modes keep their present meaning until looprs-5g7
                // gives them a Cancel worth the name.
                KeyCode::Char('c') => {
                    if self.active == TerminalType::Bash {
                        let _ = self.cmd_tx.try_send(UiCommand::Cancel);
                    } else {
                        self.should_quit = true;
                    }
                    return;
                }
                // The chord we do own in every mode, Bash included: quit without
                // touching the shell (the child is killed on the way out).
                KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                _ => {}
            }
        }

        // The input half of ADR-0001 Q2: a program that owns the screen owns the
        // keyboard for as long as it does. The keystroke goes back out as the bytes
        // the terminal sent for it, which is what makes `Esc` be `Esc` (vim: leave
        // insert mode) instead of the `0x03` that a line command needs Esc to be.
        // Ctrl-C and Ctrl-Q are dealt with above, so the chords this path cannot
        // take back are exactly the two that were never the child's to take.
        if self.passthrough() {
            if let Some(bytes) = crate::screen::key_bytes(k) {
                let _ = self.cmd_tx.try_send(UiCommand::Keys {
                    mode: self.active,
                    bytes,
                });
            }
            return;
        }

        // **The Esc ordering** (looprs-pdl.9): if a selection is live, the
        // first Esc clears the selection and does nothing else; the next Esc
        // is the cancel the mode table already describes (ADR-0003).
        //
        // Above every cancel below, and above the scroll keys, because the
        // failure this prevents is the loud one: the user drags a paragraph,
        // decides they did not mean it, presses Esc, and the app cancels a
        // running model call instead of unselecting. That is exactly the class
        // of surprise ADR-0003 exists to prevent, which is why the ticket
        // refuses to leave it implicit and why it sits here, where every cancel
        // has to go past it.
        //
        // Note it is *after* the passthrough block above: while a child holds
        // the screen it holds Esc too, and that is a settled decision of its
        // own (looprs-4hv) that a selection must not reach over.
        if k.code == KeyCode::Esc && self.selection.clear_if_live() {
            self.dirty = true;
            return;
        }

        // The scrollback keys (looprs-pdl.6). Chosen because nothing else in
        // this app claims them: `InputState::handle_key` ignores all four, so
        // taking them here moves no keystroke off the box, and a program that
        // holds the screen already got them back above. `End` is the single
        // action the "N new" affordance names, and `Home` is its mirror.
        //
        // The *semantics* — one row up unpins, the bottom re-pins, the view
        // holds while new output arrives — are the store's, not here: this is
        // the plumbing from a keystroke to `Scrollback`. The chord table,
        // including whatever `Home`/`End` should mean once there is a Ctrl-C
        // nobody has stolen, is looprs-pdl.13's to settle.
        let page = self.transcript_band_rows().max(1) as isize;
        match k.code {
            KeyCode::PageUp => {
                self.scroll_active(-page);
                return;
            }
            KeyCode::PageDown => {
                self.scroll_active(page);
                return;
            }
            KeyCode::Home => {
                self.top_active();
                return;
            }
            KeyCode::End => {
                self.tail_active();
                return;
            }
            _ => {}
        }

        // The box needs the width it is going to be drawn at: a wrapped row is the
        // only "line" a message has below the one the user is on, so `Up` and
        // `Home` are meaningless without the wrapping. Same width the height policy
        // measures with, from the same function.
        let inner = inner_width(self.width);
        if let Some(action) = self.input.handle_key(k, inner) {
            match action {
                InputAction::Submit { text, mode } => {
                    // The shell echoes what it reads — through the pty, into our
                    // transcript — so echoing it here too would show the line
                    // twice. Every other mode needs the local echo because nothing
                    // else will show what was typed.
                    if mode != TerminalType::Bash {
                        self.echo_local(mode, text.clone());
                    }
                    let _ = self.cmd_tx.try_send(UiCommand::Submit { mode, text });
                }
                InputAction::SwitchMode { from, to } => {
                    let _ = self.cmd_tx.try_send(UiCommand::SwitchMode { from, to });
                    // A selection does not cross a mode boundary. It is
                    // addressed into one view's transcript, and the next frame
                    // would be another mode's rows with a box still painted on
                    // them — so the ticket's clear list says gone, and gone it
                    // is before anything else about the switch happens.
                    self.selection.clear();
                    // The wheel's rate memory goes with it, for the same
                    // reason in miniature: a gesture that started over one
                    // mode's transcript is not the same gesture as the one
                    // still arriving over the other's, and a throttle that
                    // carries across the boundary would be rate-limiting a
                    // scroll against a view that has not had one yet.
                    self.wheel.reset_throttle();
                    // Move the render pointer optimistically, so the frame right
                    // after the keystroke is already the new mode. Both halves are
                    // driven by this one command, so they cannot diverge.
                    self.active = to;
                }
                InputAction::Cancel => {
                    let _ = self.cmd_tx.try_send(UiCommand::Cancel);
                }
            }
        }
    }

    /// A pi protocol event, applied to the view of the session that made it — and
    /// **rendered only**.
    ///
    /// `session` decides where it lands (ADR-0002 Q2); nothing else about the event
    /// is consulted, and nothing is decided. In particular this function no longer
    /// answers "does this settle mean take the next bead?": that question needs the
    /// step, the worker and the parked flag, all of which live inside
    /// [`BeadsSession`](crate::session::BeadsSession) and none of which the App can
    /// see. It used to answer it anyway, from `session.mode == Beeds`, and that was
    /// looprs-msj — a Pi answer settling could drive the beads machine, and a beads
    /// worker settling with the box on Pi stalled the loop.
    ///
    /// So: every session's events are paint here. Who advances is nobody's business.
    pub fn on_pi(&mut self, session: SessionId, ev: PiEvent) {
        self.dirty = true;
        apply_pi(self.view_mut(session), ev);
    }
}

/// Why Bash output never enters the live region (`ChatState`), stated once:
///
/// The shell has no `agent_settled` to stop the spinner with, and the bytes that
/// follow every command — its own prompt — look exactly like output that is still
/// arriving. Arming a live region on shell bytes therefore means a spinner that
/// spins forever over an idle prompt, which is worse than no live region at all.
/// So Bash output goes straight to the scrollback as complete lines (raw, unwrapped,
/// via `MessageKind::Bash`) and "is the shell working?" is answered by the status
/// row (`SessionStatus::Running`) instead of by a spinner.
/// Apply one pi event to one session's view.
///
/// A free function on purpose: it cannot reach `App`'s globals — no `input`, no
/// `active`, no `cmd_tx` — so "which view does this touch" is answered by the
/// signature, and "what does this make happen next" is answered by nothing. The
/// only mutations are to the transcript and the live-region state of the view it was
/// handed.
fn apply_pi(view: &mut SessionView, ev: PiEvent) {
    match ev {
        PiEvent::MessageUpdate {
            assistant_message_event: e,
        } => {
            view.chat = ChatState::Chat;
            match e {
                AssistantEvent::TextDelta { delta, .. } => {
                    view.push_delta(MessageKind::Answer, &delta)
                }
                AssistantEvent::ThinkingDelta { delta, .. } => {
                    view.push_delta(MessageKind::Thinking, &delta)
                }
                _ => {}
            }
        }
        // user messages are already echoed locally on submit; ignore pi's copy
        PiEvent::MessageEnd { message } if message.role == "assistant" => {
            view.transcript.finish_last();
            // The authoritative per-message accounting, folded into this view's
            // window. Only ever here, and never from `message_update`'s `usage`,
            // because that figure is cumulative for the message still streaming —
            // see [`Tokens::add`]. One `message_end` per API call is every bit as
            // live as the row needs, and cannot be double counted.
            if let Some(u) = message.usage {
                view.tokens.add(&u);
            }
        }
        PiEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            view.chat = ChatState::Tool;
            // upsert: fills in the args
            view.transcript
                .start_tool(tool_call_id, tool_name, print_json_value_to_string(&args));
        }
        // PiEvent::ToolExecutionUpdate { .. } => stream partial output into the row if you want it
        PiEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
        } => {
            view.transcript
                .finish_tool(tool_call_id, result.text(), is_error);
        }
        // Settled: this session has no more automatic work, so its live region stops.
        // That is *all* this event means here. Whether it is also "the beads pass
        // finished, take the next one" is decided inside the beads session by the
        // beads session, and this arm must not grow an opinion about it: the same
        // `AgentSettled` arrives from Pi chat, where there is no loop to advance.
        PiEvent::AgentSettled => view.chat = ChatState::Stopped,
        // Compaction: a card, exactly like a tool's, because it is the same shape
        // of pause — a paid-for LLM call in the middle of the run that prints
        // nothing of its own. Without the card the run looks hung for as long as
        // the summary takes, which is the one thing the pause is not.
        PiEvent::CompactionStart { reason } => {
            view.chat = ChatState::Compacting;
            view.transcript.start_compaction(reason);
        }
        // Three endings, and the card says which: freed something, was cancelled,
        // or failed. `aborted` is not painted as a failure because it was not one
        // — the user pressed Esc — and a cancel that looks like a crash teaches
        // the wrong lesson about a key they chose to press.
        PiEvent::CompactionEnd {
            reason,
            aborted,
            error_message,
            result,
        } => {
            let (state, detail) = if aborted {
                (CompactionState::Aborted, String::new())
            } else if let Some(err) = error_message {
                (CompactionState::Failed, err)
            } else {
                // pi reports the two figures on a successful compaction only when
                // it has them; with either missing the card says "compacted" and
                // stops rather than putting a made-up number on the row.
                let freed = result
                    .as_ref()
                    .and_then(|r| Some((r.tokens_before?, r.estimated_tokens_after?)))
                    .map(|(before, after)| token_delta(before, after))
                    .unwrap_or_default();
                (CompactionState::Done, freed)
            };
            // The card closes and the live region hands the row back: whatever
            // comes next is another event's business (`MessageUpdate` will set
            // `Chat` again when the run resumes). Leaving `Compacting` set would
            // keep a spinner turning over work that has finished.
            //
            // An `end` with no open card is recorded rather than swallowed — it
            // happened, and a compaction that finishes unseen is the same bug in
            // the other direction.
            if !view.transcript.finish_compaction(state, detail.clone()) {
                view.transcript.push_done(
                    MessageKind::Compaction {
                        reason: reason.unwrap_or_default(),
                        state,
                    },
                    detail,
                );
            }
            view.chat = ChatState::Stopped;
        }
        // AutoRetryStart / AutoRetryEnd: show a status note if you want one
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::view::Tokens;
    use crate::session::{BeadStep, SessionStatus};

    fn app_with(active: TerminalType) -> (App, mpsc::Receiver<UiCommand>) {
        let (tx, rx) = mpsc::channel::<UiCommand>(16);
        (App::new(tx, InputState::new(), active, 80, 24), rx)
    }

    /// The wiring between the box and the height policy: what the app asks the frame
    /// for *is* what the box's own wrapping needs, capped. If those two drift the
    /// box gets cut off, or the pane grows a band of blank space nobody can
    /// explain from a screenshot.
    #[test]
    fn the_app_asks_for_the_rows_the_box_actually_wraps_into() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        let width = 22u16; // inner width 20
        for n in 0usize..=200 {
            app.input.set_text("x".repeat(n));
            let inner = inner_width(width);
            let wrapped = app.input.display_lines(inner).len() as u16;
            assert_eq!(
                app.input_rows(width),
                viewport::input_rows(wrapped),
                "{n} characters typed"
            );
        }
        // Grows with the text, then stops at the cap.
        app.input.set_text("x".to_string());
        assert_eq!(app.input_rows(width), viewport::MIN_INPUT_ROWS);
        app.input.set_text("x".repeat(40)); // two rows of 20 cells
        assert_eq!(app.input_rows(width), viewport::MIN_INPUT_ROWS + 1);
        app.input.set_text("x".repeat(200)); // ten rows wanted, the cap wins
        assert_eq!(app.input_rows(width), viewport::MAX_INPUT_ROWS);
    }

    /// **The arrow keys belong to the box, not to a session.** A `Left` that
    /// leaked down the command channel would arrive in the shell as `ESC [ D` — a
    /// history search, or half an escape sequence handed to whatever program is
    /// running. Nothing goes out; the caret moves and the text is untouched.
    #[test]
    fn arrow_keys_are_consumed_by_the_box_and_reach_no_session() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);
        for c in "ab cd".chars() {
            app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::BackTab,
            KeyCode::Delete,
        ] {
            app.update(Msg::Term(key(code, KeyModifiers::NONE)));
        }
        assert_eq!(
            app.input.text(),
            "ab cd\n",
            "the keys edited the box (Shift-Tab broke the line) and lost nothing"
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "not one keystroke went out to a session"
        );
    }

    /// The multi-line box is still one message on the wire: a newline in the
    /// middle changes nothing about what `Enter` means.
    #[test]
    fn a_two_line_box_submits_one_command_with_both_lines_in_it() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);
        for c in "first".chars() {
            app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        app.update(Msg::Term(key(KeyCode::BackTab, KeyModifiers::NONE)));
        for c in "second".chars() {
            app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        app.update(Msg::Term(key(KeyCode::Enter, KeyModifiers::NONE)));
        match rx.try_recv() {
            Ok(UiCommand::Submit { mode, text }) => {
                assert_eq!(mode, TerminalType::Pi);
                assert_eq!(text, "first\nsecond");
            }
            other => panic!("expected exactly one Submit, got {other:?}"),
        }
    }

    /// The one place "is the box showing?" and "how tall is it?" become a single
    /// number, so the height policy and the drawn box cannot each make their own
    /// mind up. Hidden means *zero rows granted*, not a box drawn somewhere else:
    /// that is what lets the status row sit on the bottom edge of the live region
    /// instead of hanging above a band nothing is drawn into.
    #[test]
    fn the_input_band_is_the_box_when_it_shows_and_nothing_when_it_is_hidden() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.input.set_text("a question to type".to_string());
        assert!(app.need_input(), "nothing running: the box is open");
        assert_eq!(
            app.input_band(60),
            app.input_rows(60),
            "a showing box is budgeted its measured height"
        );

        app.view_mut(pi_id())
            .set_status(SessionStatus::Running, Instant::now());
        assert!(!app.need_input(), "Pi is mid-run: the box is hidden");
        assert_eq!(
            app.input_band(60),
            viewport::NO_INPUT_ROWS,
            "a hidden box must be granted nothing, not its old height"
        );

        // Bash always takes the keyboard, so its band is always the box — even
        // while a command is running (it is the human's shell).
        let (mut bash, _rx) = app_with(TerminalType::Bash);
        bash.view_mut(crate::session::SessionId::new(TerminalType::Bash, 1))
            .set_status(SessionStatus::Running, Instant::now());
        assert!(bash.need_input(), "bash is always open");
        assert_eq!(bash.input_band(60), bash.input_rows(60));
    }

    fn beads_id() -> SessionId {
        SessionId::new(TerminalType::Beeds, 1)
    }

    fn pi_id() -> SessionId {
        SessionId::new(TerminalType::Pi, 1)
    }

    fn text_of(app: &App, mode: TerminalType) -> String {
        app.view(mode)
            .map(|v| {
                v.transcript
                    .entries
                    .iter()
                    .map(|e| e.text.clone())
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default()
    }

    // ─────────────────── the token window (the row's cost facts) ───────────────────
    //
    // `↑in ↓out` is a *window*, not a global counter, and which window a mode
    // gets is the design (see `SessionView::tokens`). Every number here was handed
    // to the App by a `Msg`, so none of it depends on a child, a provider, or a
    // fixture's arithmetic.

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    fn ended(session: SessionId, role: &str, u: Option<Usage>) -> Msg {
        Msg::Agent {
            session,
            event: PiEvent::MessageEnd {
                message: WireMessage {
                    role: role.to_string(),
                    usage: u,
                },
            },
        }
    }

    fn claim(id: &str) -> Msg {
        Msg::ActiveBead {
            session: beads_id(),
            bead: Some(ActiveBead {
                id: id.to_string(),
                title: format!("ticket {id}"),
            }),
        }
    }

    fn beads_tokens(app: &App) -> Tokens {
        app.view(TerminalType::Beeds).unwrap().tokens
    }

    fn pi_tokens(app: &App) -> Tokens {
        app.view(TerminalType::Pi).unwrap().tokens
    }

    /// The window adds every assistant message's usage, and nothing else adds.
    ///
    /// The two no-rows carry the weight. A `user` message's `message_end` is not
    /// assistant work, and an assistant message that reported **nothing** is not a
    /// message that cost nothing: `None` must never be counted as zero.
    #[test]
    fn every_assistant_message_adds_and_nothing_else_does() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(ended(pi_id(), "assistant", Some(usage(100, 40, 900, 25))));
        app.update(ended(pi_id(), "assistant", Some(usage(200, 80, 1800, 50))));
        let t = pi_tokens(&app);
        assert_eq!((t.input, t.output, t.cache), (300, 120, 2775), "{t:?}");

        app.update(ended(pi_id(), "user", Some(usage(999, 999, 999, 999))));
        app.update(ended(pi_id(), "toolResult", Some(usage(9, 9, 9, 9))));
        app.update(ended(pi_id(), "assistant", None));
        assert_eq!(pi_tokens(&app), t, "not one of those moved the total");
    }

    /// A claim opens a fresh window; releasing one deliberately does not close it.
    ///
    /// "What is this ticket costing" is only answerable if the number restarts at
    /// the claim, and it is only *worth* anything after the pass if it survives the
    /// release that follows it.
    #[test]
    fn the_beads_window_opens_on_a_claim_and_survives_the_release() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        app.update(ended(beads_id(), "assistant", Some(usage(10, 5, 20, 1))));
        assert_eq!(
            beads_tokens(&app).input,
            10,
            "unclaimed spend sits in the window"
        );

        app.update(claim("A"));
        assert!(beads_tokens(&app).is_empty(), "ticket A starts at nothing");
        app.update(ended(beads_id(), "assistant", Some(usage(70, 30, 600, 10))));
        let a = beads_tokens(&app);
        assert_eq!((a.input, a.output, a.cache), (70, 30, 610), "{a:?}");

        // Released, still on show: the row answers what A cost.
        app.update(Msg::ActiveBead {
            session: beads_id(),
            bead: None,
        });
        assert_eq!(beads_tokens(&app), a, "a release is not a reset");

        // And B never inherits A's bill.
        app.update(claim("B"));
        assert!(beads_tokens(&app).is_empty(), "B is a fresh window");
    }

    /// A respawned child starts at nothing.
    ///
    /// The row means "this session", and a new generation *is* a new session —
    /// carrying the dead child's total over would make an ordinary respawn read
    /// like a runaway on the one number the user watches for that.
    #[test]
    fn a_new_generation_starts_the_window_at_nothing() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(ended(pi_id(), "assistant", Some(usage(500, 200, 4000, 90))));
        assert!(!pi_tokens(&app).is_empty());

        let gen2 = SessionId::new(TerminalType::Pi, 2);
        app.update(Msg::SessionStatus {
            session: gen2,
            status: SessionStatus::Idle,
        });
        assert!(
            pi_tokens(&app).is_empty(),
            "the new incarnation owes nothing for the old one's run"
        );
    }

    /// The wire shape, parsed from bytes shaped like pi's own.
    ///
    /// camelCase on the wire; every field independently optional. `message_update`
    /// carries a **cumulative** `usage` that this harness deliberately does not
    /// model — the test pins that it still parses, and that nothing on the
    /// streaming path can fold it into a total twice.
    #[test]
    fn the_wire_usage_record_parses_as_pi_writes_it() {
        let full: PiEvent = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":1234,"output":56,"cacheRead":9000,"cacheWrite":25}}}"#,
        )
        .expect("a full usage record parses");
        let PiEvent::MessageEnd { message } = full else {
            panic!("expected message_end");
        };
        let u = message.usage.expect("usage present");
        assert_eq!(
            (u.input, u.output, u.cache_read, u.cache_write),
            (1234, 56, 9000, 25),
            "camelCase mapped: {u:?}"
        );

        // A provider that reports only two of the four still lands; the rest are
        // zero-because-absent, which the window treats as "nothing to add".
        let part: PiEvent = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":7,"output":3}}}"#,
        )
        .expect("a partial usage record parses");
        let PiEvent::MessageEnd { message } = part else {
            panic!("expected message_end");
        };
        let u = message.usage.expect("usage present");
        assert_eq!(
            (u.input, u.output, u.cache_read, u.cache_write),
            (7, 3, 0, 0)
        );

        // No usage at all is `None` — never a zero-cost message.
        let none: PiEvent = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"user","content":"hi"}}"#,
        )
        .expect("a usage-less message_end parses");
        let PiEvent::MessageEnd { message } = none else {
            panic!("expected message_end");
        };
        assert!(
            message.usage.is_none(),
            "absent is absent: {:?}",
            message.usage
        );

        // The streaming event's cumulative `usage` is not a modelled field, and
        // must not become one silently: it parses, and lands in the same variant.
        let upd: PiEvent = serde_json::from_str(concat!(
            r#"{"type":"message_update","usage":{"input":100,"output":20,"cacheRead":900,"cacheWrite":25},"#,
            r#""assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hi"}}"#,
        ))
        .expect("message_update parses with its cumulative usage alongside");
        assert!(matches!(upd, PiEvent::MessageUpdate { .. }));
    }

    /// The row's cost facts come out of the view that owns them — and the empty
    /// window prints nothing rather than `↑0 ↓0`.
    #[test]
    fn the_row_prints_the_window_it_is_handed() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        let empty = app.status_line(120).to_string();
        assert!(
            !empty.contains('\u{2191}'),
            "nothing reported, nothing printed: {empty:?}"
        );

        app.update(claim("A"));
        app.update(ended(
            beads_id(),
            "assistant",
            Some(usage(12_345, 678, 90_000, 0)),
        ));
        let row = app.status_line(120).to_string();
        assert!(row.contains("\u{2191}12.3k \u{2193}678"), "{row:?}");
        assert!(row.contains("cache 90.0k"), "{row:?}");
    }

    /// The fix for the whole "I tabbed away and the other session wrecked this
    /// view" class: an event lands in the view that *owns* the session.
    #[test]
    fn events_land_in_the_view_that_owns_them() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);

        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "pi answer".into(),
                },
            },
        });
        app.update(Msg::Agent {
            session: beads_id(),
            event: PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "beads answer".into(),
                },
            },
        });

        assert!(text_of(&app, TerminalType::Beeds).contains("beads answer"));
        assert!(!text_of(&app, TerminalType::Beeds).contains("pi answer"));
        assert!(text_of(&app, TerminalType::Pi).contains("pi answer"));
        assert!(!text_of(&app, TerminalType::Pi).contains("beads answer"));
    }

    /// `need_input` follows the ACTIVE view, not the beads machine: a beads pass
    /// working off-screen must not hide the Pi input box, and vice versa. The box
    /// opens and closes on the *view's* derived state, never on whichever mode is
    /// making noise.
    #[test]
    fn need_input_is_derived_from_the_active_view() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        assert!(app.need_input(), "no view yet => ask the human");

        app.update(Msg::SessionStatus {
            session: beads_id(),
            status: SessionStatus::Running,
        });
        assert!(!app.need_input(), "beads is working, so the box is hidden");

        // Same beads state; look at Pi instead. The Pi box is untouched.
        app.active = TerminalType::Pi;
        assert!(
            app.need_input(),
            "a beads pass running off-screen must not hide the Pi input box"
        );

        app.active = TerminalType::Beeds;
        assert!(
            !app.need_input(),
            "and coming back must not resurrect a box the beads run already closed"
        );
    }

    /// The other half of the keyboard rule as the App sees it: no matter how many
    /// agents are working, the Bash view answers yes. This is the whole reason the
    /// user is never locked out of the harness.
    #[test]
    fn bash_takes_input_while_every_agent_is_working() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::SessionStatus {
            session: pi_id(),
            status: SessionStatus::Running,
        });
        app.update(Msg::BeadStep {
            session: beads_id(),
            step: BeadStep::WorkTickets,
        });
        app.update(Msg::SessionStatus {
            session: beads_id(),
            status: SessionStatus::Running,
        });
        app.update(Msg::BashOutput {
            session: SessionId::new(TerminalType::Bash, 1),
            stream: ByteStream::Merged,
            chunk: "$ ".into(),
        });

        assert!(!app.need_input(), "Pi is mid-run");
        app.active = TerminalType::Beeds;
        assert!(!app.need_input(), "and so is beads");
        app.active = TerminalType::Bash;
        assert!(
            app.need_input(),
            "and the shell is open regardless of what the agents are doing"
        );
    }

    /// `chat_state` (spinner / live preview) is per session too: the tick must not
    /// animate over a session that is not streaming, and one session's stream must
    /// not animate another's preview.
    #[test]
    fn chat_state_is_per_session() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        assert_eq!(app.chat_state(), ChatState::Stopped);

        app.update(Msg::Agent {
            session: beads_id(),
            event: PiEvent::ToolExecutionStart {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                args: Value::Null,
            },
        });
        assert_eq!(
            app.chat_state(),
            ChatState::Stopped,
            "a beads tool going live must not animate the Pi view"
        );
        assert_eq!(app.view(TerminalType::Beeds).unwrap().chat, ChatState::Tool);

        app.active = TerminalType::Beeds;
        assert_eq!(app.chat_state(), ChatState::Tool);
    }

    /// A compaction is a separate LLM call that pauses the run and prints nothing
    /// of its own — ten to sixty seconds of a transcript that has stopped moving,
    /// which is exactly the shape of "is it hung?". So the start event must put a
    /// row on the screen by itself.
    #[test]
    fn a_compaction_shows_a_live_card_while_it_runs() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::CompactionStart {
                reason: "threshold".into(),
            },
        });

        assert_eq!(
            app.chat_state(),
            ChatState::Compacting,
            "the live region is animating for it, not for stale prose"
        );
        assert_eq!(
            app.live_card_rows(),
            1,
            "and the height policy is told to budget its row"
        );

        let card = app
            .view(TerminalType::Pi)
            .unwrap()
            .transcript
            .open_cards()
            .last()
            .expect("the card is open");
        let line = crate::components::card::card_line(card, 0).to_string();
        assert!(line.contains("compacting context"), "{line}");
        assert!(line.contains("threshold"), "and why: {line}");
    }

    /// `compaction_start` on its own is only half the feedback. The end is the
    /// half that says whether it worked, and pi says three different things there
    /// — freed something, cancelled, failed — each of which has to reach the
    /// scrollback as its own line once the card closes.
    #[test]
    fn a_finished_compaction_reaches_the_scrollback_with_what_it_freed() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.on_pi(
            pi_id(),
            PiEvent::CompactionStart {
                reason: "threshold".into(),
            },
        );
        app.on_pi(
            pi_id(),
            PiEvent::CompactionEnd {
                reason: Some("threshold".into()),
                aborted: false,
                error_message: None,
                result: Some(CompactionResult {
                    tokens_before: Some(150_000),
                    estimated_tokens_after: Some(32_000),
                }),
            },
        );

        assert_eq!(
            app.live_card_rows(),
            0,
            "the card is closed, so it stops taking live rows"
        );
        assert_eq!(
            app.chat_state(),
            ChatState::Stopped,
            "and nothing is left spinning over work that has finished"
        );

        let flushed: String = app.flush_active(60).iter().map(|l| l.to_string()).collect();
        assert!(flushed.contains("context compacted"), "{flushed:?}");
        assert!(
            flushed.contains("150.0k → 32.0k"),
            "the numbers pi reported are the point of the row: {flushed:?}"
        );
    }

    /// Cancelled and failed are the two endings a user must not have to guess
    /// about, and they are not the same sentence: `aborted: true` means the user
    /// pressed Esc, `errorMessage` means the summarisation call broke.
    #[test]
    fn an_aborted_compaction_says_aborted_and_a_failed_one_says_why() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        app.on_pi(
            beads_id(),
            PiEvent::CompactionEnd {
                reason: Some("manual".into()),
                aborted: true,
                error_message: None,
                result: None,
            },
        );
        let aborted: String = app.flush_active(60).iter().map(|l| l.to_string()).collect();
        assert!(aborted.contains("compaction aborted"), "{aborted:?}");
        assert!(aborted.contains("manual"), "{aborted:?}");

        app.on_pi(
            beads_id(),
            PiEvent::CompactionEnd {
                reason: Some("overflow".into()),
                aborted: false,
                error_message: Some("provider refused the summary".into()),
                result: None,
            },
        );
        let failed: String = app.flush_active(60).iter().map(|l| l.to_string()).collect();
        assert!(
            failed.contains("provider refused the summary"),
            "pi's own words, not a shrug: {failed:?}"
        );
        assert!(
            !aborted.contains("provider refused"),
            "and the two endings are two separate rows"
        );
    }

    /// An `end` whose `start` never arrived (a reconnect, a dropped line) is still
    /// an event that happened. Swallowing it because there was nothing to close is
    /// the same silence this whole path exists to remove.
    #[test]
    fn a_compaction_end_with_no_open_card_is_recorded_anyway() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.on_pi(
            pi_id(),
            PiEvent::CompactionEnd {
                reason: Some("overflow".into()),
                aborted: false,
                error_message: None,
                result: None,
            },
        );
        let flushed: String = app.flush_active(60).iter().map(|l| l.to_string()).collect();
        assert!(
            flushed.contains("context compacted · overflow"),
            "the reason comes off the end record when there was no start to say it: {flushed:?}"
        );
    }

    /// The card must not hold the transcript hostage — but while it is open it
    /// legitimately owns the flush cursor, the same way a running tool does. What
    /// matters is that closing it releases everything queued behind it.
    #[test]
    fn closing_the_compaction_card_releases_what_came_behind_it() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.on_pi(
            pi_id(),
            PiEvent::CompactionStart {
                reason: "threshold".into(),
            },
        );
        app.on_pi(
            pi_id(),
            PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "text after the summary\n\n".into(),
                },
            },
        );
        assert!(
            app.flush_active(60).is_empty(),
            "the open card is the cursor, and nothing past it goes out yet"
        );

        app.on_pi(
            pi_id(),
            PiEvent::CompactionEnd {
                reason: Some("threshold".into()),
                aborted: false,
                error_message: None,
                result: None,
            },
        );
        let flushed: String = app.flush_active(60).iter().map(|l| l.to_string()).collect();
        assert!(flushed.contains("context compacted"), "{flushed:?}");
        assert!(
            flushed.contains("text after the summary"),
            "and the run's own text follows it into the scrollback: {flushed:?}"
        );
    }

    /// The wire shape, pinned: `reason` is required on the start, and optional on
    /// the end, where `result` is absent unless the compaction succeeded.
    #[test]
    fn the_compaction_wire_format_parses() {
        let s = parse(&serde_json::json!({"type":"compaction_start","reason":"overflow"})).unwrap();
        assert!(matches!(s, PiEvent::CompactionStart { reason } if reason == "overflow"));

        let e = parse(&serde_json::json!({
            "type": "compaction_end",
            "reason": "threshold",
            "result": { "summary": "...", "firstKeptEntryId": "abc", "tokensBefore": 150000, "estimatedTokensAfter": 32000 },
            "aborted": false,
            "willRetry": true
        }))
        .unwrap();
        match e {
            PiEvent::CompactionEnd {
                reason,
                aborted,
                error_message,
                result,
            } => {
                assert_eq!(reason.as_deref(), Some("threshold"));
                assert!(!aborted);
                assert!(error_message.is_none());
                let r = result.expect("the result was parsed");
                assert_eq!(r.tokens_before, Some(150_000));
                assert_eq!(r.estimated_tokens_after, Some(32_000));
            }
            other => panic!("wrong event: {other:?}"),
        }

        // Aborted: no `result` on the wire at all, and `aborted: true`.
        assert!(matches!(
            parse(&serde_json::json!({"type":"compaction_end","aborted":true})).unwrap(),
            PiEvent::CompactionEnd {
                aborted: true,
                result: None,
                ..
            }
        ));
    }

    /// Death must seal: the text a dead session left un-flushed has to reach the
    /// scrollback, not stall the cursor forever.
    #[test]
    fn session_down_seals_the_view_and_gives_the_input_box_back() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        app.update(Msg::Agent {
            session: beads_id(),
            event: PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "unterminated answer".into(),
                },
            },
        });
        assert!(
            app.flush_active(60).is_empty(),
            "an open entry is not flushed while it is still open"
        );

        app.update(Msg::SessionDown {
            session: beads_id(),
            reason: ExitReason::Crashed { code: Some(2) },
        });
        let lines = app.flush_active(60);
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("unterminated answer")),
            "sealing on death must release the tail: {lines:?}"
        );
        let v = app.view(TerminalType::Beeds).unwrap();
        assert_eq!(v.status, SessionStatus::Dead);
        assert!(
            v.accepts_input(),
            "a dead session must not hide the input box"
        );
    }

    /// A new generation of a mode cannot strand the previous one's output.
    #[test]
    fn adopting_a_new_generation_seals_the_old_one() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        let old = SessionId::new(TerminalType::Pi, 1);
        let new = SessionId::new(TerminalType::Pi, 2);

        app.update(Msg::Agent {
            session: old,
            event: PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "from the corpse".into(),
                },
            },
        });
        app.view_mut(new); // the respawn arrives without any SessionDown
        let lines = app.flush_active(60);
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("from the corpse")),
            "adoption must seal what it replaces: {lines:?}"
        );
        assert_eq!(app.view(TerminalType::Pi).unwrap().session, new);
    }

    /// Only the active view flushes; a hidden one keeps buffering.
    #[test]
    fn only_the_active_view_flushes() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        app.update(Msg::System {
            session: Some(pi_id()),
            text: "buffered while hidden".into(),
        });
        assert!(
            app.flush_active(60).is_empty(),
            "nothing has been said to the beads view"
        );
        assert!(
            text_of(&app, TerminalType::Pi).contains("buffered while hidden"),
            "the hidden view kept its text instead of dropping it"
        );

        app.active = TerminalType::Pi;
        let lines = app.flush_active(60);
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("buffered while hidden")),
            "switching in flushes the backlog as one burst: {lines:?}"
        );
    }

    /// A harness-level message (no session) goes where the user can see it.
    #[test]
    fn harness_messages_go_to_the_active_view() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::Error {
            session: None,
            text: "could not open Bash".into(),
        });
        assert_eq!(
            app.view(TerminalType::Pi).unwrap().last_error.as_deref(),
            Some("could not open Bash")
        );
        assert!(app.view(TerminalType::Beeds).is_none());
    }

    /// Switching modes: the render pointer moves immediately, and the Router is
    /// the one told to do the lifecycle. The App never touches a session itself.
    #[tokio::test]
    async fn a_tab_is_addressed_to_the_router_not_to_a_session() {
        let (mut app, mut rx) = app_with(TerminalType::Beeds);
        let tab = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        );
        app.update(Msg::Term(Event::Key(tab)));

        assert_eq!(app.active, TerminalType::Pi, "the view moved");
        match rx.recv().await.unwrap() {
            UiCommand::SwitchMode { from, to } => {
                assert_eq!(from, TerminalType::Beeds);
                assert_eq!(to, TerminalType::Pi);
            }
            other => panic!("expected SwitchMode, got {other:?}"),
        }
    }

    /// A submit is addressed to the mode it was typed into, and echoed there.
    #[tokio::test]
    async fn submitting_is_addressed_and_echoed() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);
        for c in "hi".chars() {
            app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ))));
        }
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))));

        match rx.recv().await.unwrap() {
            UiCommand::Submit { mode, text } => {
                assert_eq!(mode, TerminalType::Pi);
                assert_eq!(text, "hi");
            }
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(text_of(&app, TerminalType::Pi).contains("hi"));
    }

    /// The exit signal is a command on the same channel as every other one, not a
    /// `drop` of that channel — so the App that sent it is still alive and still
    /// holding the reader the exit drain needs. Asserting the round trip is what
    /// pins that choice down; the alternative (dropping `App`) looks the same from
    /// here and loses the tail of the answer.
    #[tokio::test]
    async fn asking_for_shutdown_puts_quit_on_the_command_channel() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);
        app.request_shutdown().await;
        assert!(
            matches!(rx.recv().await.unwrap(), UiCommand::Quit),
            "the exit instruction reached the router"
        );
        // …and the App is still a working reader afterwards.
        app.update(Msg::SessionDown {
            session: pi_id(),
            reason: ExitReason::Shutdown,
        });
        assert!(text_of(&app, TerminalType::Pi).contains("ended"));
    }

    /// **A local echo does not steal the view's session identity.** It matters
    /// because `view_mut` seals on adoption: if echoing used a placeholder id, a
    /// submit into a live session would seal that session's open entry (and flip its
    /// status) from the *user's* keystroke rather than from the session's death.
    #[test]
    fn a_local_echo_does_not_steal_the_views_session_identity() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        let before = {
            let v = app.view_mut(pi_id());
            v.push_delta(MessageKind::Answer, "streaming");
            v.status = SessionStatus::Running;
            (pi_id(), v.transcript.entries.len())
        };

        app.echo_local(TerminalType::Pi, "typed".into());

        let v = app.view(TerminalType::Pi).unwrap();
        assert_eq!(
            v.session, before.0,
            "the echo used the existing generation, not HARNESS_GENERATION"
        );
        assert_eq!(
            v.status,
            SessionStatus::Running,
            "an echo is not a lifecycle event"
        );
        assert_eq!(v.transcript.entries.len(), before.1 + 1);
        assert_eq!(v.transcript.entries[0].text, "streaming");
    }

    // ---------------------- the Pi terminal state, from the UI side ----------------------

    /// **A settle never drives the beads machine from the App** (looprs-msj).
    ///
    /// The bug as filed had two halves, and they are the same half twice: a Pi run
    /// settling while the box happened to be on Beads drove the beads state
    /// machine, and a beads worker settling while the box was on Pi left it
    /// stalled. Both came from the App deciding what a settle *meant*.
    ///
    /// So the assertion is the blunt one, run with the box in every mode and with
    /// the settle from every session: handling a settle sends **nothing** to the
    /// router. Not "the right command for the right mode" — nothing. The beads loop
    /// is driven from inside `BeadsSession` off its own worker's stream, which is
    /// pinned in `session::beads::tests`; all this side may do is paint.
    #[test]
    fn no_settle_ever_turns_into_a_command_from_the_app() {
        for ui_mode in TerminalType::ALL {
            for producer in [pi_id(), beads_id(), SessionId::new(TerminalType::Bash, 1)] {
                let (mut app, mut rx) = app_with(ui_mode);

                app.update(Msg::Agent {
                    session: producer,
                    event: PiEvent::AgentSettled,
                });

                assert!(
                    rx.try_recv().is_err(),
                    "box on {}, {} settled, and the App still sent a command",
                    ui_mode.label(),
                    producer
                );
                // It rendered, though — that part is the whole job.
                assert_eq!(
                    app.view(producer.mode)
                        .expect("the producer has a view")
                        .chat,
                    ChatState::Stopped,
                    "settling must still stop {}'s live region",
                    producer
                );
                // And no other view was touched by a session that never spoke to it.
                for other in TerminalType::ALL {
                    if other != producer.mode {
                        assert!(
                            app.view(other).is_none(),
                            "{} settling created a {} view out of the UI's guesswork",
                            producer,
                            other.label()
                        );
                    }
                }
            }
        }
    }

    /// The grep check the ticket asked for, kept as a test so it stays checked.
    ///
    /// ADR-0002 Q2: the input mode is authoritative for *intent* — where the
    /// user's keystrokes go — and never for *origin*. So here the field may only
    /// ever be *assigned* (the one line in `App::new` that pins the box to the mode
    /// the app opened in, which is the same fact the render pointer gets) and never
    /// *read* to decide anything about an event. Reads by another name are caught
    /// too, because every read of the box goes through this text.
    #[test]
    fn the_ui_never_reads_the_input_mode_to_route_an_event() {
        // Assembled rather than written out, so this test's own source cannot match
        // the pattern it is looking for.
        let mode_field = concat!("input", ".mode");
        let assigned = concat!("input", ".mode = ");
        let src = include_str!("app.rs");
        let mentions: Vec<String> = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//")) // prose is allowed to name the bug
            .filter(|l| l.contains(mode_field))
            .map(|l| l.to_string())
            .collect();
        let offenders: Vec<&String> = mentions
            .iter()
            // Assignments are not routing. `App::new` sets the box onto the mode it
            // opened in, and the tests set a scenario up; neither decides where an
            // event belongs.
            .filter(|l| !l.starts_with(assigned))
            .collect();
        assert!(
            offenders.is_empty(),
            "event handling must never consult the input mode (that is looprs-msj): {offenders:?}"
        );
        // And the checker is really looking at something: the allowed line must be
        // there, or this test would be green because the pattern rotted away.
        assert!(
            mentions.iter().any(|l| l.starts_with(assigned)),
            "no `{mode_field}` assignment found — the checker matched nothing: {mentions:?}"
        );
    }

    /// **Pi's copy of the user's own message stays out of the transcript** — the
    /// echo made on submit is the one and only copy. Two copies of the same line is
    /// worse than none, and pi sends one for every message we send.
    #[test]
    fn pi_copies_of_the_user_message_never_reach_the_transcript() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.echo_local(TerminalType::Pi, "my name is Dan".into());
        let after_echo = app.view(TerminalType::Pi).unwrap().transcript.entries.len();

        for role in ["user", "toolResult"] {
            app.update(Msg::Agent {
                session: pi_id(),
                event: PiEvent::MessageStart {
                    message: WireMessage {
                        role: role.to_string(),
                        usage: None,
                    },
                },
            });
            app.update(Msg::Agent {
                session: pi_id(),
                event: PiEvent::MessageEnd {
                    message: WireMessage {
                        role: role.to_string(),
                        usage: None,
                    },
                },
            });
        }

        let v = app.view(TerminalType::Pi).unwrap();
        assert_eq!(
            v.transcript.entries.len(),
            after_echo,
            "not one of pi's non-assistant messages added a line: {:?}",
            v.transcript
                .entries
                .iter()
                .map(|e| &e.text)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            text_of(&app, TerminalType::Pi),
            "my name is Dan",
            "the local echo is still the only copy"
        );

        // The assistant's own end-of-message is *not* suppressed: it closes the
        // streaming entry, which is what lets it flush at all.
        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "hello".into(),
                },
            },
        });
        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::MessageEnd {
                message: WireMessage {
                    role: "assistant".to_string(),
                    usage: None,
                },
            },
        });
        assert!(
            !app.flush_active(60).is_empty(),
            "a closed answer must flush"
        );
    }

    /// **Esc's restore lands in the box** — the whole point of pulling the queued
    /// text out of pi before aborting.
    #[test]
    fn esc_restore_puts_the_queued_text_back_in_the_box() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::RestoreInput {
            session: pi_id(),
            text: "and also this".into(),
        });
        assert_eq!(app.input.text(), "and also this");
    }

    /// The restore is asynchronous; the user's own typing is newer than it is, and
    /// wins. The text is not thrown away either — it goes to the transcript, where
    /// it can be re-typed, because silently dropping it is the very failure this
    /// recipe exists to prevent.
    #[test]
    fn esc_restore_never_eats_what_the_user_typed_in_the_meantime() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        for c in "no wait".chars() {
            app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ))));
        }

        app.update(Msg::RestoreInput {
            session: pi_id(),
            text: "and also this".into(),
        });

        assert_eq!(app.input.text(), "no wait", "the user's text survives");
        assert!(
            text_of(&app, TerminalType::Pi).contains("and also this"),
            "and the restored text is visible rather than lost"
        );
    }

    /// **Ctrl-C in the Bash view belongs to the shell, not to looprs**
    /// (ADR-0001 Q3). It has to arrive at the session as a cancel — the Bash
    /// session turns that into `0x03` on the pty master, and the line discipline
    /// SIGINTs the foreground process group — and it must not quit the app. A
    /// Bash pane where `sleep 30` cannot be stopped is not a shell.
    #[tokio::test]
    async fn ctrl_c_in_bash_mode_cancels_the_shell_and_does_not_quit() {
        let (mut app, mut rx) = app_with(TerminalType::Bash);
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))));

        assert!(!app.should_quit, "Ctrl-C must not quit Bash mode");
        match rx.recv().await {
            Some(UiCommand::Cancel) => {}
            other => panic!("Ctrl-C should reach the shell as Cancel, got {other:?}"),
        }
    }

    /// Outside Bash, Ctrl-C keeps the meaning it has today. Pinned rather than left
    /// implicit so that looprs-5g7 changing it is a deliberate edit to this test
    /// and not a regression nobody noticed.
    #[tokio::test]
    async fn ctrl_c_outside_bash_mode_still_quits_for_now() {
        for mode in [TerminalType::Beeds, TerminalType::Pi] {
            let (mut app, mut rx) = app_with(mode);
            app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))));
            assert!(app.should_quit, "Ctrl-C in {} mode", mode.label());
            assert!(
                rx.try_recv().is_err(),
                "quitting is not a cancel; nothing was sent"
            );
        }
    }

    /// Ctrl-Q is the chord looprs owns in **every** mode, Bash included — the
    /// way out when Ctrl-C has been handed to a shell.
    #[test]
    fn ctrl_q_quits_in_every_mode() {
        for mode in TerminalType::ALL {
            let (mut app, _rx) = app_with(mode);
            app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))));
            assert!(app.should_quit, "Ctrl-Q in {} mode", mode.label());
        }
    }

    /// **A `Msg::BashOutput` lands in the Bash view and never in the mode the input
    /// box happens to be in** — and the child keeps its own framing: a line the
    /// child ended is stored as one line, and a line it did not end stays live and
    /// unfinished until the stream does (ADR-0001 rule 1, ADR-0005).
    #[test]
    fn bash_output_lands_in_its_own_view_verbatim() {
        let mut app = {
            let (tx, _rx) = mpsc::channel::<UiCommand>(16);
            App::new(tx, InputState::new(), TerminalType::Beeds, 80, 24)
        };
        let bash = SessionId::new(TerminalType::Bash, 1);
        let chunk = "first line\nsecond line, with no trailing newline";
        app.update(Msg::BashOutput {
            session: bash,
            stream: ByteStream::Merged,
            chunk: chunk.into(),
        });

        // The finished line is in the store. The unterminated tail is *not*: ADR-0005
        // makes the store a list of complete lines so the line still being written
        // can be overwritten by the next `\r`, and the tail lives in the view's
        // resolver until the stream ends. Both halves are checked here, because
        // "arrived whole" is the same promise and it now has two addresses.
        assert_eq!(
            text_of(&app, TerminalType::Bash),
            "first line\n",
            "the line the child ended is in the store, as the child wrote it"
        );
        let live: String = app
            .view(TerminalType::Bash)
            .map(|v| v.preview(80).iter().map(|l| l.to_string()).collect())
            .unwrap_or_default();
        assert!(
            live.contains("second line, with no trailing newline"),
            "the line it did not end is live, not lost: {live:?}"
        );
        assert!(
            app.view(TerminalType::Beeds).is_none(),
            "and it went nowhere near the mode the box was in"
        );

        // And ending the stream lands the tail rather than dropping it.
        let bash = SessionId::new(TerminalType::Bash, 1);
        app.view_mut(bash).seal();
        assert_eq!(
            text_of(&app, TerminalType::Bash),
            format!("{chunk}\n"),
            "the seal collected the live line; nothing was lost with the stream"
        );
    }

    /// A Pi cancel cannot type into the Beads box: the restore is tagged with the
    /// session that made it, and only the mode on screen gets the keystroke.
    #[test]
    fn a_pi_cancel_cannot_type_into_another_modes_box() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        app.update(Msg::RestoreInput {
            session: pi_id(),
            text: "queued from pi".into(),
        });
        assert!(
            app.input.text().is_empty(),
            "the beads box is untouched by a Pi Esc"
        );
        assert!(
            text_of(&app, TerminalType::Pi).contains("queued from pi"),
            "and the text is still accounted for, in the session that owns it"
        );
    }

    // ---------------- the full-screen (ADR-0001 Q2) seam ----------------

    fn bash_id() -> SessionId {
        SessionId::new(TerminalType::Bash, 1)
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, mods))
    }

    /// **While a full-screen child holds the screen, the transcript is not the
    /// display path.** The bytes go to the real terminal instead; keeping a copy
    /// in the transcript as well is the same frame twice — once as the child drew
    /// it, once re-rendered by us above the viewport.
    #[test]
    fn a_held_screen_goes_to_the_terminal_and_not_to_the_transcript() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        assert!(app.passthrough(), "the active Bash session owns the screen");

        app.update(Msg::BashOutput {
            session: bash_id(),
            stream: ByteStream::Merged,
            chunk: "\u{1b}[?1049h\u{1b}[24;1H--INSERT--".into(),
        });

        let held_in_transcript = app
            .view(TerminalType::Bash)
            .map(|v| v.transcript.entries.len())
            .unwrap_or(0);
        assert_eq!(
            held_in_transcript, 0,
            "a program's screen is not scrollback material"
        );
    }

    /// The holder is Bash, but the **user is looking at Pi**. Teeing Bash's paint
    /// over the mode on screen would be worse than not showing it, so the bytes go
    /// to the Bash view and are kept rather than smeared.
    #[test]
    fn a_held_screen_is_never_painted_over_the_mode_that_is_showing() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        assert!(
            !app.passthrough(),
            "Bash owns the screen but is not the mode on it"
        );

        app.update(Msg::BashOutput {
            session: bash_id(),
            stream: ByteStream::Merged,
            chunk: "painted while hidden".into(),
        });
        let live: String = app
            .view(TerminalType::Bash)
            .map(|v| v.preview(80).iter().map(|l| l.to_string()).collect())
            .unwrap_or_default();
        assert!(
            live.contains("painted while hidden"),
            "kept in its own view instead of overwriting Pi"
        );
        assert_eq!(
            text_of(&app, TerminalType::Bash),
            "",
            "and it is still the live line it was: nothing was ended, so the              scrollback got nothing — while hidden, the bytes are held, not smeared"
        );
        assert!(app.view(TerminalType::Pi).is_none(), "Pi was not touched");
    }

    /// **The tee is where the alternate-screen debt gets booked**, because the
    /// tee is the only thing this app uses to tell the real terminal anything.
    /// A child that switched the screen through us and died there leaves us
    /// holding it, and the exit path reads that off this handle (`looprs-pdl.3`).
    #[test]
    fn a_teed_alt_screen_is_a_screen_we_owe_the_terminal_back() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        assert_eq!(
            app.screen_debt().outstanding(),
            None,
            "nothing was switched on yet"
        );

        app.update(Msg::BashOutput {
            session: bash_id(),
            stream: ByteStream::Merged,
            chunk: "\u{1b}[?1049h--INSERT--".into(),
        });
        assert_eq!(
            app.screen_debt().outstanding(),
            Some(1049),
            "that screen is now ours to give back"
        );

        app.update(Msg::BashOutput {
            session: bash_id(),
            stream: ByteStream::Merged,
            chunk: ":wq\r\n\u{1b}[?1049l".into(),
        });
        assert_eq!(
            app.screen_debt().outstanding(),
            None,
            "the child paid its own leave, so nobody owes it twice"
        );
    }

    /// The other half: bytes that never reached the terminal cannot have switched
    /// anything on it. With the user looking at Pi, Bash's paint is transcripted,
    /// and a transcript of `?1049h` is text, not a mode switch.
    #[test]
    fn a_screen_switch_that_was_never_teed_owes_nothing() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        app.update(Msg::BashOutput {
            session: bash_id(),
            stream: ByteStream::Merged,
            chunk: "\u{1b}[?1049hpaint that stayed in the transcript".into(),
        });
        assert_eq!(
            app.screen_debt().outstanding(),
            None,
            "the real terminal was never told anything, so it is owed nothing"
        );
    }

    /// **Acceptance: Esc reaches a full-screen program as Esc.** In vim `Esc` is
    /// the key that leaves insert mode; `0x03` is an interrupt, and sending that
    /// instead is exactly why vim was unusable. The keystroke has to go down as
    /// raw bytes and must not type into the input box.
    #[tokio::test]
    async fn esc_is_esc_inside_a_full_screen_program() {
        let (mut app, mut rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));

        match rx.recv().await {
            Some(UiCommand::Keys { mode, bytes }) => {
                assert_eq!(mode, TerminalType::Bash);
                assert_eq!(bytes, vec![0x1b], "one byte: Esc");
            }
            other => panic!(
                "Esc inside a held screen must reach the child as a keystroke, got {other:?}"
            ),
        }
        assert!(
            app.input.text().is_empty(),
            "and it did not go into the input box"
        );
    }

    /// …and with no program holding the screen, Esc is still the interrupt a line
    /// command needs. Both halves of the acceptance line, in the same app type,
    /// separated by exactly one thing: who owns the screen.
    #[tokio::test]
    async fn esc_is_still_cancel_at_a_line_prompt() {
        let (mut app, mut rx) = app_with(TerminalType::Bash);
        app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
        match rx.recv().await {
            Some(UiCommand::Cancel) => {}
            other => panic!("Esc at a prompt should still cancel, got {other:?}"),
        }
    }

    /// **Acceptance: Ctrl-C interrupts in both cases.** Inside a held screen it
    /// still routes as Cancel — which the Bash session writes to the master as
    /// `0x03`, and the line discipline does the rest. It still must not quit.
    #[tokio::test]
    async fn ctrl_c_still_interrupts_inside_a_full_screen_program() {
        let (mut app, mut rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        app.update(Msg::Term(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));

        assert!(!app.should_quit, "Ctrl-C must not quit while vim is up");
        match rx.recv().await {
            Some(UiCommand::Cancel) => {}
            other => panic!("Ctrl-C should still reach the shell as Cancel, got {other:?}"),
        }
    }

    /// The way out of a program that took the keyboard: Ctrl-Q is ours even while
    /// the screen is handed over. Without it a grabbed keyboard is a locked app.
    #[test]
    fn ctrl_q_quits_even_while_the_screen_is_held() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        app.update(Msg::Term(key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        assert!(app.should_quit, "Ctrl-Q stays ours");
    }

    /// A release asks for the viewport to be re-anchored; a takeover must not, or
    /// the run loop would resize our viewport onto the child's screen.
    #[test]
    fn a_release_asks_to_re_anchor_and_a_takeover_does_not() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        assert!(
            !app.repaint_all,
            "taking the screen over is not a reason to resize"
        );
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: false,
        });
        assert!(
            app.repaint_all,
            "getting it back is: ratatui's diff no longer describes the screen"
        );
    }

    /// A child that dies holding the screen still gives it up. Nothing can wait for
    /// the release event here — the death is what cancelled the program that would
    /// have sent it — so the death itself has to close the seam.
    #[test]
    fn a_child_dying_while_it_holds_the_screen_lets_go_of_it() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        assert!(app.passthrough());

        app.update(Msg::SessionDown {
            session: bash_id(),
            reason: ExitReason::Crashed { code: Some(137) },
        });
        assert!(
            !app.passthrough(),
            "a dead child cannot hold a screen; not releasing here wedges the UI dark"
        );
        assert!(app.repaint_all, "and the viewport has to be rebuilt");
    }

    /// A release from some *other* session must not disturb a held screen. The
    /// pairing is by session id, not by "somebody said inactive".
    #[test]
    fn a_release_from_another_session_does_not_take_the_screen_back() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        let other = SessionId::new(TerminalType::Bash, 2);
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: true,
        });
        app.update(Msg::ScreenHeld {
            session: other,
            active: false,
        });
        assert!(
            app.passthrough(),
            "a generation that never held the screen cannot release it"
        );
        assert!(!app.repaint_all);
    }

    // ─────────────────────── the status row (looprs-guh) ───────────────────────
    //
    // `App`'s half of the row: gather the mirrors, no more. The layout, the
    // truncation and the priority ladder are tested in `components::status`; what
    // is tested here is that the right thing reaches the row at all — which is the
    // half that can go wrong while every individual piece still looks correct.

    fn row(app: &App, w: u16) -> String {
        app.status_line(w).to_string()
    }

    /// ADR-0002's whole reason for this row, end to end through the envelope: the
    /// beads loop is running, the user is looking at Pi, and the row says so.
    #[test]
    fn the_row_names_a_busy_mode_you_are_not_looking_at() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::BeadStep {
            session: beads_id(),
            step: BeadStep::WorkTickets,
        });
        app.update(Msg::ActiveBead {
            session: beads_id(),
            bead: Some(ActiveBead {
                id: "looprs-guh".into(),
                title: "Status row is allocated but empty".into(),
            }),
        });
        app.update(Msg::SessionStatus {
            session: beads_id(),
            status: SessionStatus::Running,
        });
        let txt = row(&app, 100);
        assert!(txt.contains("Pi"), "{txt:?}");
        assert!(txt.contains("bg: Beeds working"), "{txt:?}");
        assert!(txt.contains("looprs-guh"), "{txt:?}");
        assert!(
            !txt.contains("warm: Beeds"),
            "a running mode is not a warm one: {txt:?}"
        );
    }

    /// The same mirrors are per-session, not global (ADR-0002 Q2 / Q5). A beads
    /// edge must not make the Pi pane look busy, and vice versa.
    #[test]
    fn a_status_edge_lands_in_its_own_view_and_not_in_the_active_one() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::SessionStatus {
            session: beads_id(),
            status: SessionStatus::Running,
        });
        assert_eq!(
            app.view(TerminalType::Beeds).unwrap().status,
            SessionStatus::Running
        );
        assert_eq!(
            app.view(TerminalType::Pi).map(|v| v.status),
            None,
            "a beads edge must not have created or touched the Pi view's liveness"
        );

        // Now the Pi view exists, and it is idle while beads runs.
        app.update(Msg::SessionStatus {
            session: pi_id(),
            status: SessionStatus::Idle,
        });
        let txt = row(&app, 60);
        assert!(txt.contains("Pi") && txt.contains("idle"), "{txt:?}");
        assert_eq!(
            app.view(TerminalType::Pi).unwrap().status,
            SessionStatus::Idle
        );
        assert_eq!(
            app.view(TerminalType::Beeds).unwrap().status,
            SessionStatus::Running,
            "and beads is still running, unchanged by the Pi edge"
        );
    }

    /// A warm child (alive, idle, resident) is the cost ADR-0002 takes on for
    /// instant mode switches. It shows up on the row, and only for modes that are
    /// actually alive.
    #[test]
    fn warm_children_are_named_only_for_modes_with_a_live_child() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        assert!(
            !row(&app, 100).contains("warm"),
            "nothing is alive yet: {:?}",
            row(&app, 100)
        );
        app.update(Msg::SessionStatus {
            session: pi_id(),
            status: SessionStatus::Idle,
        });
        assert!(row(&app, 100).contains("warm: Pi"), "{}", row(&app, 100));

        // Dead is not warm: the child is gone, and claiming otherwise would hide
        // the one thing the user would want to know.
        app.update(Msg::SessionStatus {
            session: pi_id(),
            status: SessionStatus::Dead,
        });
        assert!(
            !row(&app, 100).contains("warm"),
            "a dead child is not a warm one: {}",
            row(&app, 100)
        );
    }

    /// The active mode is never listed as background or warm. The row answers
    /// "what is *this* pane doing", and the other lists are about the other panes.
    #[test]
    fn the_active_mode_is_never_listed_against_itself() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        app.update(Msg::SessionStatus {
            session: pi_id(),
            status: SessionStatus::Idle,
        });
        let txt = row(&app, 100);
        assert!(!txt.contains("bg: Pi"), "{txt:?}");
        assert!(!txt.contains("warm: Pi"), "{txt:?}");
    }

    /// The row is a pure function of state, and the state includes *when* it was
    /// sampled. Advancing the app's clock advances the age the row shows; nothing
    /// in the render path reads a clock of its own.
    #[test]
    fn the_run_age_comes_from_the_app_clock_not_from_a_read_of_the_wall() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        let t0 = Instant::now();
        app.on_tick(t0);
        app.update(Msg::SessionStatus {
            session: bash_id(),
            status: SessionStatus::Running,
        });
        assert!(row(&app, 100).contains("0s"), "{}", row(&app, 100));

        app.on_tick(t0 + Duration::from_secs(45));
        let txt = row(&app, 100);
        assert!(txt.contains("45s"), "{txt:?}");

        app.on_tick(t0 + Duration::from_secs(3600));
        assert!(row(&app, 100).contains("1h00m"), "{}", row(&app, 100));
    }

    /// The age is *this run's*, not this session's: a second run restarts it.
    #[test]
    fn a_second_run_starts_a_new_clock() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        let t0 = Instant::now();
        app.on_tick(t0);
        app.update(Msg::SessionStatus {
            session: bash_id(),
            status: SessionStatus::Running,
        });
        app.on_tick(t0 + Duration::from_secs(100));
        assert!(row(&app, 100).contains("1m40s"), "{}", row(&app, 100));

        app.update(Msg::SessionStatus {
            session: bash_id(),
            status: SessionStatus::Idle,
        });
        // The idle row has no age at all...
        assert!(
            !row(&app, 100).contains("1m40s"),
            "an idle session must not keep showing the last run's age: {}",
            row(&app, 100)
        );
        // ...and the next run starts from zero, not from the old one.
        app.update(Msg::SessionStatus {
            session: bash_id(),
            status: SessionStatus::Running,
        });
        app.on_tick(t0 + Duration::from_secs(105));
        let txt = row(&app, 100);
        assert!(txt.contains("5s"), "{txt:?}");
    }

    /// A new generation of a mode is a different process. Its row must not carry
    /// the previous one's age or its claim.
    #[test]
    fn a_new_generation_carries_no_age_and_no_claim() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        let t0 = Instant::now();
        app.on_tick(t0);
        app.update(Msg::ActiveBead {
            session: beads_id(),
            bead: Some(ActiveBead {
                id: "looprs-old".into(),
                title: "old".into(),
            }),
        });
        app.update(Msg::SessionStatus {
            session: beads_id(),
            status: SessionStatus::Running,
        });
        app.on_tick(t0 + Duration::from_secs(60));
        assert!(row(&app, 100).contains("looprs-old"));

        let newer = SessionId::new(TerminalType::Beeds, 2);
        app.view_mut(newer);
        let txt = row(&app, 100);
        assert!(!txt.contains("looprs-old"), "{txt:?}");
        assert!(!txt.contains("1m00s"), "stale run age survived: {txt:?}");
    }

    /// `on_tick` decides when the row repaints. With nothing busy it must not mark
    /// the app dirty, or an idle app redraws at 60fps for no reason at all; with
    /// something busy it repaints at the row's own pace.
    #[test]
    fn an_idle_app_is_not_repainted_by_the_row_and_a_busy_one_is_repainted_at_eight_fps() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        let t0 = Instant::now();

        app.dirty = false;
        app.on_tick(t0 + Duration::from_millis(16));
        assert!(
            !app.dirty,
            "nothing is busy; the row must not ask for a frame"
        );

        app.update(Msg::SessionStatus {
            session: bash_id(),
            status: SessionStatus::Running,
        });
        app.dirty = false;
        // Under the animation interval: nothing to show yet that changed.
        app.on_tick(t0 + Duration::from_millis(60));
        assert!(!app.dirty, "still inside the same animation frame");
        // Past it: the spinner moved, so the row needs painting.
        app.on_tick(t0 + Duration::from_millis(200));
        assert!(
            app.dirty,
            "the spinner advanced and the row was not repainted"
        );
    }

    /// A mode with no view at all still gets a row. `App` is created before any
    /// session exists, and the very first frame is exactly the frame where "what is
    /// happening?" has no answer but the honest one.
    #[test]
    fn a_fresh_app_with_no_sessions_at_all_renders_a_sane_row() {
        let (tx, _rx) = mpsc::channel::<UiCommand>(1);
        let app = App::new(tx, InputState::new(), TerminalType::Beeds, 40, 24);
        let txt = row(&app, 40);
        assert!(txt.contains("Beeds"), "{txt:?}");
        assert!(txt.contains("not started"), "{txt:?}");
        assert!(!txt.contains('{') && !txt.contains('}'), "{txt:?}");
    }

    /// The last error must not need scrolling to see — that is the ticket. It also
    /// has to stay put when the mode is switched away from and back.
    #[test]
    fn the_last_error_shows_in_its_own_mode_and_does_not_follow_the_user_around() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        app.update(Msg::Error {
            session: Some(beads_id()),
            text: "bd: database is locked".into(),
        });
        let txt = row(&app, 100);
        assert!(txt.contains("✗"), "{txt:?}");
        assert!(txt.contains("database is locked"), "{txt:?}");

        // The Pi pane's row is not showing the beads pane's failure.
        let other = App {
            active: TerminalType::Pi,
            ..app
        };
        let txt = row(&other, 100);
        assert!(!txt.contains("database is locked"), "{txt:?}");
    }

    // ─────────── the scrollback store drives the band (looprs-pdl.6) ───────────

    /// `n` settled lines in the active view, flushed at `width`.
    ///
    /// Each `System` entry renders as itself plus its separator, so `n` entries
    /// are `2n` store rows — which is what makes "is there history above the
    /// band" a fact the tests can rely on rather than guess at.
    fn settle(app: &mut App, n: usize, width: u16) {
        for i in 0..n {
            app.update(Msg::System {
                session: None,
                text: format!("settled {i}"),
            });
        }
        app.flush_active(width);
    }

    fn shown(app: &App, rows: usize) -> Vec<String> {
        app.transcript_window(rows)
            .iter()
            .map(|r| r.to_string())
            .collect()
    }

    /// Pinned, the band shows the tail; a page up shows older rows and hides it.
    #[test]
    fn a_page_up_pages_the_transcript_and_leaves_the_tail() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle(&mut app, 20, 80);
        let band = app.transcript_band_rows();
        assert!(band > 1, "the band has rows to show: {band}");
        assert!(
            app.scrollback().len() > band,
            "there is history above this band to scroll into: band={band} rows={}",
            app.scrollback().len()
        );

        let tail = shown(&app, band);
        assert!(
            tail.iter().any(|l| l.contains("settled 19")),
            "the newest line is not on screen while pinned: {tail:?}"
        );
        assert!(
            !tail.iter().any(|l| l.contains("settled 0")),
            "the head of the transcript is off the top while pinned: {tail:?}"
        );

        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert!(!app.pinned(), "one page up is off the tail");
        let hist = shown(&app, band);
        assert!(
            hist.iter().any(|l| l.contains("settled 0")),
            "a page of history reached the head of the transcript: {hist:?}"
        );
        assert!(
            !hist.iter().any(|l| l.contains("settled 19")),
            "and the tail is not on screen any more: {hist:?}"
        );
    }

    /// A page up with nothing above the band cannot scroll into blank space, so
    /// it cannot unpin either: "pinned" is not a mode the app can be in without
    /// the content agreeing.
    #[test]
    fn with_nothing_above_the_band_there_is_nowhere_to_scroll() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle(&mut app, 2, 80);
        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert!(
            app.pinned(),
            "a transcript shorter than the band cannot be scrolled up"
        );
        assert_eq!(app.scrollback().offset(), 0);
    }

    /// The half of the contract that is not about the keystroke at all: while the
    /// user is off the tail, new output must not move what they are reading, and
    /// must be *counted* so the UI can say the view is not up to date.
    #[test]
    fn output_that_arrives_while_scrolled_up_is_held_back_and_counted() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle(&mut app, 12, 80);
        let band = app.transcript_band_rows();
        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        let before = shown(&app, band);

        app.update(Msg::System {
            session: None,
            text: "arrived while you were reading".into(),
        });
        app.flush_active(80);

        assert_eq!(shown(&app, band), before, "the view held");
        assert_eq!(
            app.new_rows(),
            2,
            "the entry and its separator, both unseen"
        );
        assert!(!app.pinned());
    }

    /// `End` is the one action the pill names, and it answers the count: back at
    /// the tail, nothing is pending, because the user is looking at it.
    #[test]
    fn end_returns_to_the_tail_and_answers_the_count() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle(&mut app, 12, 80);
        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        app.update(Msg::System {
            session: None,
            text: "arrived while you were reading".into(),
        });
        app.flush_active(80);
        assert_eq!(app.new_rows(), 2);

        app.update(Msg::Term(key(KeyCode::End, KeyModifiers::NONE)));
        assert!(app.pinned(), "the bottom of the content is the tail");
        assert_eq!(app.new_rows(), 0, "and nothing is unseen any more");
        let tail = shown(&app, app.transcript_band_rows());
        assert!(
            tail.iter()
                .any(|l| l.contains("arrived while you were reading")),
            "the tail is what the band shows: {tail:?}"
        );
    }

    /// Scrolling is local: no command goes to the Router for a wheel, a page or
    /// a `Home`, and nothing goes to the input box either.
    #[test]
    fn scrolling_is_not_a_round_trip_and_not_a_keystroke_anyones_else() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);
        settle(&mut app, 12, 80);
        for code in [
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            app.update(Msg::Term(key(code, KeyModifiers::NONE)));
            assert!(
                rx.try_recv().is_err(),
                "{code:?} sent a command; the scrollback is the app\'s own state"
            );
        }
        assert!(
            app.input.text().is_empty(),
            "the box took none of them either"
        );
    }

    /// A name for a command's shape. `UiCommand` is `Debug` but not `PartialEq`
    /// — two actions are not "equal" in this crate — and a shape string is all
    /// the comparison below needs.
    fn shape(cmd: &UiCommand) -> String {
        match cmd {
            UiCommand::Resize { rows, cols } => format!("resize {rows}x{cols}"),
            other => format!("{other:?}"),
        }
    }

    /// **The size poll and the `Resize` event are one adoption, not two
    /// implementations of it** (looprs-pdl.15).
    ///
    /// The run loop takes a resize from either source, so the two must leave
    /// the same state behind: same width and height, a frame asked for, and —
    /// the one worth pinning — the same `UiCommand::Resize` out to the
    /// children. A poll that set `App::width` locally would repaint the frame
    /// and leave every child pty wrapping for a window that no longer exists
    /// (ADR-0001 rule 6), which is the bug this ticket exists to remove in
    /// one place and would quietly reintroduce in another.
    #[test]
    fn the_size_poll_adopts_what_the_resize_event_adopts() {
        let (mut by_event, mut ev_rx) = app_with(TerminalType::Bash);
        let (mut by_poll, mut poll_rx) = app_with(TerminalType::Bash);

        by_event.update(Msg::Term(Event::Resize(132, 43)));
        // …what `main.rs`'s tick arm does with the poll's answer.
        by_poll.set_window(132, 43);

        assert_eq!((by_event.width, by_event.height), (132, 43));
        assert_eq!((by_poll.width, by_poll.height), (132, 43));
        assert!(by_event.dirty, "the event asks for a frame");
        assert!(by_poll.dirty, "and so does the poll");

        let from_event = ev_rx.try_recv().ok().map(|c| shape(&c));
        let from_poll = poll_rx.try_recv().ok().map(|c| shape(&c));
        assert_eq!(
            from_event.as_deref(),
            Some("resize 43x132"),
            "rows first, as ADR-0001 rule 6 spells it"
        );
        assert_eq!(
            from_event, from_poll,
            "the children were told the same thing by both paths"
        );
    }

    /// A resize re-wraps the store rather than re-adding to it: the same content,
    /// once, at the new width.
    #[test]
    fn a_resize_rewraps_the_store_without_doubling_or_losing_content() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        let id = SessionId::new(TerminalType::Pi, 1);
        app.view_mut(id)
            .push_delta(MessageKind::Answer, "MARKER-1 alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau. MARKER-2 the quick brown fox jumps over the lazy dog again and again until it needs a second row at eighty columns and a third at forty.

");
        app.flush_active(80);
        let wide = app.scrollback().len();
        assert!(wide > 1, "the answer rendered: {wide} rows");

        app.set_window(40, 24);
        app.flush_active(40);
        let narrow = app.scrollback().len();
        assert_eq!(app.scrollback().width(), 40, "the store is wrapped for 40");
        assert!(
            narrow > wide,
            "narrower window, more rows: {wide} -> {narrow}"
        );

        let all: String = app
            .scrollback()
            .rows()
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(all.matches("MARKER-1").count(), 1, "{all:?}");
        assert_eq!(all.matches("MARKER-2").count(), 1, "{all:?}");
    }

    /// The page a scroll key moves is the band the frame lays out — the same
    /// number, from the same function, rather than two arithmetic that can drift.
    #[test]
    fn a_page_is_the_band_the_frame_lays_out() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle(&mut app, 40, 80);
        let band = app.transcript_band_rows();
        let max = app.scrollback().max_scroll(band);
        assert!(
            max > band,
            "enough history that a page is not the whole way"
        );

        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert_eq!(
            app.scrollback().offset(),
            band.min(max),
            "one page is exactly one band of transcript"
        );

        // Walked up page by page, the head of the content is where it stops.
        for _ in 0..(max / band.max(1) + 2) {
            app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        }
        assert_eq!(app.scrollback().offset(), max, "and no page goes past it");

        app.update(Msg::Term(key(KeyCode::End, KeyModifiers::NONE)));
        assert_eq!(
            app.scrollback().offset(),
            0,
            "and `End` is all the way back"
        );
        assert!(app.pinned());
    }

    // ────────────── the drag selection, through the real frame (pdl.9) ──────────────

    /// A mouse report, as the `Msg` the run loop would deliver for it.
    fn mouse(kind: MouseEventKind, row: u16, col: u16) -> Msg {
        Msg::Term(Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    const W: u16 = 80;

    /// Paint one frame through the real `crate::view`.
    ///
    /// The App learns where the transcript band's rows are only from the draw
    /// (`App::record_band`), because a pointer position is a claim about the
    /// pixels. So a test that drives the mouse has to go through the same door
    /// the run loop goes through — otherwise it is testing a mapping the app
    /// never built.
    fn paint(app: &App, height: u16, preview: &[Line<'static>]) {
        let backend = ratatui::backend::TestBackend::new(W, height);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| crate::view(app, f, preview, viewport::MIN_INPUT_ROWS))
            .unwrap();
    }

    /// The band's geometry for the frame `paint` just drew, as plain values, so
    /// the caller can then take `&mut App`.
    ///
    /// Returns `(band x, y of the first drawn row, the drawn rows' text)`.
    fn geom(app: &App, height: u16) -> (u16, u16, Vec<String>) {
        let [text, ..] = viewport::frame_areas(
            Rect::new(0, 0, W, height),
            app.live_card_rows(),
            app.input_band(W),
        );
        let win = app.transcript_window(text.height as usize);
        let lay = crate::components::text_stream::band_layout(text, win.len(), 0);
        (
            text.x,
            lay.settled_y,
            win[lay.skip..].iter().map(|r| r.to_string()).collect(),
        )
    }

    /// `n` numbered answer entries, flushed. Two store rows each: the prose and
    /// its blank separator.
    fn settle_answers(app: &mut App, n: usize) {
        let id = SessionId::new(app.active, 1);
        for i in 0..n {
            app.view_mut(id)
                .push_note(MessageKind::Answer, format!("LINE{i:02} aaaaaaaaaa"));
        }
        app.flush_active(W);
    }

    fn drag(app: &mut App, from: (u16, u16), to: (u16, u16)) {
        app.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        app.update(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        app.update(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1));
    }

    /// The whole gesture end to end: a press and a release around a drag put
    /// the characters under the box into `selection_paste`, across a row
    /// boundary, with the blank separator row the flusher adds counted as the
    /// blank line the user saw.
    #[test]
    fn a_drag_across_two_rows_selects_the_characters_under_the_box() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 4);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn
            .iter()
            .position(|t| t.contains("LINE01"))
            .expect("setup: LINE01 is on screen");
        let b = drawn
            .iter()
            .position(|t| t.contains("LINE02"))
            .expect("setup: LINE02 is on screen");

        drag(&mut app, (y0 + a as u16, bx + 5), (y0 + b as u16, bx + 9));
        assert!(app.selection().is_live());
        // Cell 5 of "LINE01 aaaaaaaaaa" is the `1`, cell 9 of the LINE02 row
        // the third `a`, and the blank separator row the flusher adds lands in
        // between as the one newline that keeps the two messages apart: the
        // paste is the characters, with the seam the user saw between them.
        assert_eq!(
            app.selection_paste(),
            "1 aaaaaaaaaa\nLINE02 aaa",
            "the selection is the characters under the box, across the row seam"
        );
    }

    /// **A click clears.** Press and release with nothing between them is not a
    /// selection, and it takes away the one that was standing.
    #[test]
    fn a_click_selects_nothing_and_clears_what_was_selected() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();

        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
        assert!(!app.selection_paste().is_empty(), "setup: a selection");

        app.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            y0 + a as u16,
            bx,
        ));
        app.update(mouse(
            MouseEventKind::Up(MouseButton::Left),
            y0 + a as u16,
            bx,
        ));
        assert!(
            !app.selection().is_live(),
            "the click cleared the standing selection"
        );
        assert_eq!(app.selection_paste(), "");
    }

    /// **The Esc ordering, wired.** A live selection takes the first Esc and
    /// nothing reaches the session; the next Esc is the cancel the mode table
    /// already describes.
    #[test]
    fn esc_clears_the_selection_first_and_only_then_cancels() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
        assert!(app.selection().is_live());

        app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!app.selection().is_live(), "the first Esc unselected");
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "and sent no cancel: cancelling a run because the user wanted to \
             unselect is the surprise ADR-0003 exists to prevent"
        );

        app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::Cancel)),
            "and the next Esc is the cancel the mode table already describes"
        );
    }

    /// **A mode switch clears.** The selection is addressed into one view's
    /// transcript; the next frame is another mode's rows.
    #[test]
    fn a_mode_switch_clears_the_selection() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
        assert!(app.selection().is_live());

        app.update(Msg::Term(key(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(
            !app.selection().is_live(),
            "the selection does not cross the mode boundary"
        );
    }

    /// **Auto-scroll, wired.** Dragging off the top edge keeps extending the
    /// selection by scrolling, at the throttled rate — and the selection
    /// reaches rows that were never on screen at all.
    #[test]
    fn dragging_off_the_top_edge_scrolls_and_keeps_extending() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 40);
        let h = 24u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let start = drawn.iter().position(|t| t.contains("LINE35")).unwrap();
        let t0 = Instant::now();
        app.on_tick(t0);
        app.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            y0 + start as u16,
            bx,
        ));

        let offset_before = app.scrollback().offset();
        for i in 0..5u64 {
            app.on_tick(t0 + Duration::from_millis(150 * (i + 1)));
            app.update(mouse(MouseEventKind::Drag(MouseButton::Left), 0, bx));
        }
        assert_eq!(
            app.scrollback().offset(),
            offset_before + 5,
            "five edge drags, five rows up — one per interval, no more"
        );
        let text = app.selection_paste();
        assert!(
            text.contains("LINE34") && text.contains("LINE28"),
            "the selection kept growing as the view scrolled, reaching rows that \
             were never on screen: {text:?}"
        );
        assert!(
            !text.contains("LINE36"),
            "and never reached below where the drag started: {text:?}"
        );
    }

    /// The throttle in the wiring, not just in the model: 50 edge drags inside
    /// one interval move nothing.
    #[test]
    fn a_fast_burst_of_edge_drags_does_not_outrun_the_pointer() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 40);
        let h = 24u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let start = drawn.iter().position(|t| t.contains("LINE35")).unwrap();
        let t0 = Instant::now();
        app.on_tick(t0);
        app.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            y0 + start as u16,
            bx + 2,
        ));
        let offset = app.scrollback().offset();
        for i in 1..50u64 {
            app.on_tick(t0 + Duration::from_millis(i));
            app.update(mouse(MouseEventKind::Drag(MouseButton::Left), 0, bx + 2));
        }
        assert_eq!(
            app.scrollback().offset(),
            offset + 1,
            "one scroll for the whole burst; the rest were inside the interval"
        );
    }

    /// **The live tail is not selectable.** It has no final form to address:
    /// it is still arriving. The frame that flushes it is the frame that makes
    /// it selectable, and that is the frame after it stops changing.
    #[test]
    fn the_live_tail_cannot_be_selected() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        app.view_mut(SessionId::new(TerminalType::Pi, 1)).chat = ChatState::Chat;
        let h = 20u16;
        let preview = vec![Line::from("STILL-ARRIVING".to_string())];
        paint(&app, h, &preview);
        let [text, ..] = viewport::frame_areas(
            Rect::new(0, 0, W, h),
            app.live_card_rows(),
            app.input_band(W),
        );
        // The live line is the band's last row.
        let live_y = text.bottom() - 1;
        app.update(mouse(MouseEventKind::Down(MouseButton::Left), live_y, 2));
        app.update(mouse(MouseEventKind::Drag(MouseButton::Left), live_y, 8));
        app.update(mouse(MouseEventKind::Up(MouseButton::Left), live_y, 8));
        assert!(
            !app.selection().is_live(),
            "the live tail is not in the store, so it is not selectable"
        );

        // …and once flushed it is: the settled line is a store row like any other.
        app.view_mut(SessionId::new(TerminalType::Pi, 1))
            .push_note(MessageKind::Answer, "STILLED".to_string());
        app.flush_active(W);
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let row = drawn.iter().position(|t| t.contains("STILLED")).unwrap();
        drag(&mut app, (y0 + row as u16, bx), (y0 + row as u16, bx + 3));
        assert_eq!(app.selection_paste(), "STIL");
    }

    /// **Chrome cannot be selected, and touching it does not disturb the
    /// selection.** The status row is not in the store; there is nothing there
    /// to hit.
    #[test]
    fn chrome_cannot_be_selected_and_leaves_the_selection_alone() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let [_text, _, status, input] = viewport::frame_areas(
            Rect::new(0, 0, W, h),
            app.live_card_rows(),
            app.input_band(W),
        );

        // A press on the status row, then on the input box: nothing starts.
        app.update(mouse(MouseEventKind::Down(MouseButton::Left), status.y, 2));
        app.update(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            input.y + 1,
            8,
        ));
        assert!(
            !app.selection().is_live(),
            "a drag that starts on chrome selects nothing"
        );

        // Now a real selection, then a press on chrome: it stands.
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
        let selected = app.selection_paste();
        assert!(!selected.is_empty(), "setup: a selection stands");

        app.update(mouse(MouseEventKind::Down(MouseButton::Left), status.y, 2));
        app.update(mouse(MouseEventKind::Up(MouseButton::Left), status.y, 2));
        assert_eq!(
            app.selection_paste(),
            selected,
            "a press on chrome is not a statement about the transcript: it \
             neither selects nor clears"
        );
    }

    /// **The resize property, through the App.** The selection is addressed by
    /// content, so a resize moves the rows and leaves the selection's own
    /// coordinates alone; the highlight re-derives onto wherever those
    /// characters ended up.
    ///
    /// The *range* is the invariant, deliberately, rather than the pasted
    /// string: the store's wrap drops the space it broke a soft line on (see
    /// the note on `RowEnd::Soft`), so a selection that spans a soft seam can
    /// come back a space short of what it was. That is a defect in what the
    /// wrap keeps, not in what the selection addresses — it is on the board
    /// for the copy ticket, and asserting the range here is what keeps the two
    /// concerns from getting welded together.
    #[test]
    fn a_resize_re_derives_the_cells_and_keeps_the_range() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        let id = SessionId::new(TerminalType::Pi, 1);
        for i in 0..6 {
            app.view_mut(id).push_note(
                MessageKind::Answer,
                format!("LINE{i} {}", "word ".repeat(14)),
            );
        }
        app.flush_active(80);
        let h = 30u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE2")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE3")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 6));
        let before = app.selection().range().expect("setup: a range");

        // A narrower window: everything re-wraps, rows are added.
        app.set_window(52, h);
        app.flush_active(52);
        assert!(
            app.scrollback().len() > 12,
            "setup: the narrower wrap made more rows"
        );
        assert_eq!(
            app.selection().range(),
            Some(before),
            "the selection followed the characters, not the rows they were on"
        );

        // And the highlight re-derives: the box still lands somewhere, on the
        // rows the same content is now drawn on.
        paint(&app, h, &[]);
        let (_bx2, _y1, drawn2) = geom(&app, h);
        let runs = app.selection().cells(app.transcript_window(drawn2.len()));
        assert!(!runs.is_empty(), "the box is still drawn somewhere");

        // A selection inside one row is byte-identical across a resize: the
        // seam problem above cannot touch it.
        let same_row = drawn2
            .iter()
            .position(|t| t.to_string().split_whitespace().count() > 3)
            .unwrap();
        drag(
            &mut app,
            (y0 + same_row as u16, bx),
            (y0 + same_row as u16, bx + 8),
        );
        let pinned = app.selection_paste();
        app.set_window(46, h);
        app.flush_active(46);
        assert_eq!(
            app.selection_paste(),
            pinned,
            "same eight characters, same place"
        );
    }

    /// **Not cleared by a repaint, a tick, or new output.** The ticket's
    /// other half: a selection is a fact about content, not about the frame
    /// it happened to be made in. Repainting, a clock tick, and new output
    /// arriving *below* the selection all leave it standing with the same
    /// characters — only the four things on the clear list take it away.
    #[test]
    fn a_selection_survives_a_repaint_a_tick_and_new_output() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        drag(&mut app, (y0 + a as u16, bx + 1), (y0 + b as u16, bx + 4));
        let selected = app.selection_paste();
        assert!(!selected.is_empty(), "setup: something selected");

        // A tick: the spinner turns, the clock moves.
        app.on_tick(Instant::now() + Duration::from_millis(120));
        assert_eq!(app.selection_paste(), selected, "a tick keeps it");

        // New output, flushed into the store below the selection.
        app.view_mut(SessionId::new(TerminalType::Pi, 1))
            .push_note(MessageKind::Answer, "arrived after the drag".to_string());
        app.flush_active(W);
        assert_eq!(
            app.selection_paste(),
            selected,
            "new output does not take the selection away"
        );

        // A repaint, which re-derives the band geometry from scratch.
        paint(&app, h, &[]);
        assert_eq!(
            app.selection_paste(),
            selected,
            "and neither does drawing the frame again"
        );
        assert!(app.selection().is_live(), "still live, still selectable");
    }

    /// **A trim that ate the anchor clears it — wired, not promised.** The
    /// buffer cap evicts entries from the front of the transcript and the
    /// store renumbers its rows by entry; the selection speaks the same
    /// addresses, so it has to hear about the eviction with the same numbers
    /// or the entries shift out from under it.
    #[test]
    fn a_trim_that_eats_the_anchor_clears_the_selection() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 6);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
        assert!(app.selection_paste().contains("LINE01"), "setup");

        // Trip the cap and stream enough to evict the selected entries.
        let id = SessionId::new(TerminalType::Pi, 1);
        app.view_mut(id).set_buffer_limit(300);
        for _ in 0..30 {
            app.view_mut(id)
                .push_note(MessageKind::Answer, "z".repeat(60));
        }
        app.flush_active(W);
        assert!(
            !app.selection().is_live(),
            "the entries the selection addressed are gone, and it went with them"
        );
        assert_eq!(app.selection_paste(), "");
    }

    /// The same eviction with the selection *above* the water line: the entries
    /// survive, and the selection follows them down the renumbered store to
    /// the same text it had before. This is the case a silent drift would turn
    /// into a highlight pointing at the message after the one that was
    /// selected, which is a worse bug than losing the selection.
    #[test]
    fn a_trim_above_the_selection_renumbers_it_onto_the_same_text() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        let id = SessionId::new(TerminalType::Pi, 1);
        for i in 0..10 {
            app.view_mut(id).push_note(
                MessageKind::Answer,
                format!("LINE{i:02} {}", "y".repeat(40)),
            );
        }
        app.flush_active(W);
        let h = 30u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE08")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE09")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 12));
        let before = app.selection_paste();
        assert!(
            before.contains("LINE08") && before.contains("LINE09"),
            "setup: the tail two entries selected: {before:?}"
        );

        // A cap trip that eats the *front* of the transcript, not this selection.
        app.view_mut(id).set_buffer_limit(420);
        for i in 10..13 {
            app.view_mut(id).push_note(
                MessageKind::Answer,
                format!("LINE{i:02} {}", "y".repeat(40)),
            );
        }
        app.flush_active(W);
        assert!(
            app.scrollback()
                .rows()
                .iter()
                .any(|r| r.to_string().contains("LINE09")),
            "setup: the trim stopped short of the entries this selection wants"
        );
        assert_eq!(
            app.selection_paste(),
            before,
            "the selection followed its entries down the renumbered store"
        );
    }

    /// A motion report with no press of ours behind it is not our gesture, and
    /// a release with nothing pressed is not either.
    #[test]
    fn a_drag_without_a_press_is_nothing() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        app.update(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            y0 + a as u16,
            bx,
        ));
        app.update(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            y0 + b as u16,
            bx + 4,
        ));
        app.update(mouse(
            MouseEventKind::Up(MouseButton::Left),
            y0 + b as u16,
            bx + 4,
        ));
        assert!(!app.selection().is_live());
    }

    /// The other buttons are left alone: an unbound button must not be silently
    /// swallowed (pdl.8's rule), and the middle click is pdl.11's.
    #[test]
    fn the_other_buttons_do_not_drive_the_selection() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        for btn in [MouseButton::Right, MouseButton::Middle] {
            app.update(mouse(MouseEventKind::Down(btn), y0 + a as u16, bx));
            app.update(mouse(MouseEventKind::Drag(btn), y0 + b as u16, bx + 4));
            app.update(mouse(MouseEventKind::Up(btn), y0 + b as u16, bx + 4));
        }
        assert!(
            !app.selection().is_live(),
            "a right- or middle-drag does not select"
        );
    }

    /// A drag taken while a full-screen child holds the real terminal is a
    /// fiction: the pixels under the pointer are not ours, so nothing is
    /// selected against the last frame we painted.
    #[test]
    fn no_selection_while_a_child_holds_the_screen() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        settle_answers(&mut app, 3);
        let h = 20u16;
        paint(&app, h, &[]);
        let bash = SessionId::new(TerminalType::Bash, 1);
        app.update(Msg::ScreenHeld {
            session: bash,
            active: true,
        });
        let (bx, y0, drawn) = geom(&app, h);
        let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
        let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
        drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
        assert!(
            !app.selection().is_live(),
            "the pointer is over someone else's screen"
        );
    }

    // ─────────────── the wheel and the trackpad (looprs-pdl.8) ───────────────

    /// The frame's four bands at this height, with the transcript's own band
    /// first — the same call the draw makes, so a row classified against these
    /// rectangles is classified against the pixels.
    fn bands(app: &App, h: u16) -> [Rect; 4] {
        viewport::frame_areas(
            Rect::new(0, 0, W, h),
            app.live_card_rows(),
            app.input_band(W),
        )
    }

    /// A transcript with a head, a tail, and enough rows between them that the
    /// band is not the whole of it: a scroll has somewhere to go, so a check
    /// that something *did not* move means something.
    fn settle_deep(app: &mut App) {
        settle_answers(app, 24);
    }

    /// A wheel report handed to the real door — `App::on_mouse`, the same
    /// match arm the run loop's `Msg::Term(Event::Mouse(_))` reaches — at a
    /// chosen moment. The moment is the point: the whole flick/ notch question
    /// is a question about arrival times, and a test that cannot choose them
    /// cannot test it.
    fn wheel(app: &mut App, dir: WheelDir, row: u16, at: Instant) {
        let kind = match dir {
            WheelDir::Up => MouseEventKind::ScrollUp,
            WheelDir::Down => MouseEventKind::ScrollDown,
        };
        app.on_mouse(
            crossterm::event::MouseEvent {
                kind,
                column: 4,
                row,
                modifiers: KeyModifiers::NONE,
            },
            at,
        );
    }

    /// **A notch is three whole rows, through the real door.** The report is
    /// the `Msg` the run loop delivers; the store moves by
    /// [`crate::state::wheel::WHEEL_ROWS_PER_STEP`]; and the view is off the
    /// tail because the *store* says a view off the tail is off the tail. The
    /// wheel invents no rule of its own.
    #[test]
    fn a_wheel_notch_over_the_band_moves_the_transcript_by_three_whole_rows() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let [text, ..] = bands(&app, h);
        assert!(app.pinned(), "setup: open on the tail");
        let t = Instant::now();

        wheel(&mut app, WheelDir::Up, text.y + 1, t);
        assert_eq!(
            app.scrollback().offset(),
            crate::state::wheel::WHEEL_ROWS_PER_STEP as usize,
            "one notch, three rows, into the past"
        );
        assert!(!app.pinned(), "and the view left the tail");

        // A notch the other way is the same number with the other sign, and it
        // has to be spaced like a notch: two reports inside the throttle
        // interval are one flick, and the second of them moves nothing.
        wheel(
            &mut app,
            WheelDir::Down,
            text.y + 1,
            t + crate::state::wheel::WHEEL_STEP_INTERVAL,
        );
        assert_eq!(app.scrollback().offset(), 0);
        assert!(
            app.pinned(),
            "…and the tail re-pins, as the store always does"
        );
    }

    /// **Chrome keeps the cursor.** The status row, the input box and the card
    /// band are not transcript; a wheel report that lands on one of them
    /// belongs to the widget under the cursor and the transcript neither moves
    /// nor unpins.
    #[test]
    fn a_wheel_over_the_input_box_or_the_status_row_does_not_scroll_the_transcript() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        let h = 24u16;
        paint(&app, h, &[]);
        let [text, _cards, status, input] = bands(&app, h);
        assert!(
            input.height > 0 && status.height > 0,
            "setup: chrome rows exist"
        );

        let t = Instant::now();
        for (name, row) in [
            ("status row", status.y),
            ("input box", input.y),
            ("input box's last row", input.bottom() - 1),
        ] {
            wheel(&mut app, WheelDir::Up, row, t);
            assert_eq!(
                app.scrollback().offset(),
                0,
                "the wheel scrolled the transcript from over the {name} (row {row})"
            );
            assert!(app.pinned(), "and unpinned nothing from the {name}");
        }

        // The transcript's own rows do move, so the loop above is not passing
        // because nothing ever scrolls.
        wheel(
            &mut app,
            WheelDir::Up,
            text.y + 2,
            t + crate::state::wheel::WHEEL_STEP_INTERVAL,
        );
        assert_eq!(
            app.scrollback().offset(),
            3,
            "the band itself still answers"
        );
    }

    /// The card band is chrome with something live in it, which is the case a
    /// "the card row is part of the transcript text" mistake would go and
    /// scroll.
    #[test]
    fn the_live_card_band_is_not_transcript_and_does_not_scroll() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        app.update(Msg::Agent {
            session: SessionId::new(TerminalType::Pi, 1),
            event: PiEvent::CompactionStart {
                reason: "threshold".into(),
            },
        });
        let h = 26u16;
        paint(&app, h, &[]);
        let [_text, cards, _status, _input] = bands(&app, h);
        assert!(cards.height > 0, "setup: a card row was laid out");

        let before = app.scrollback().offset();
        let t = Instant::now();
        wheel(&mut app, WheelDir::Up, cards.y, t);
        wheel(
            &mut app,
            WheelDir::Up,
            cards.y,
            t + Duration::from_millis(500),
        );
        assert_eq!(
            app.scrollback().offset(),
            before,
            "the live card band is not transcript and does not scroll"
        );
    }

    /// **A wheel that moves nothing costs no frame.** At the end of the
    /// transcript, in the direction of the end, there is nowhere to go: the
    /// store is unchanged, the frame is not marked dirty, and the run loop
    /// therefore draws nothing and writes no bytes. That is the whole of
    /// "no frame cost when nothing is moving", and it starts here rather than
    /// in the draw because the draw cannot be blamed for a `dirty` it was told
    /// about.
    #[test]
    fn a_wheel_at_the_end_of_the_transcript_costs_no_frame() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let band_row = 2u16;
        let t = Instant::now();

        // At the tail, rolled toward the tail: forty reports, no movement.
        app.dirty = false;
        for i in 0..40u32 {
            wheel(
                &mut app,
                WheelDir::Down,
                band_row,
                t + Duration::from_millis(100 * i as u64),
            );
        }
        assert_eq!(app.scrollback().offset(), 0, "still at the tail");
        assert!(
            !app.dirty,
            "forty reports that moved nothing did not ask for one frame"
        );

        // Same at the head: past the top of the content there is nothing, and
        // nothing is not a place a view can be taken to.
        app.top_active();
        let top = app.scrollback().offset();
        assert!(top > 0, "setup: there is a head to be at");
        app.dirty = false;
        for i in 0..40u32 {
            wheel(
                &mut app,
                WheelDir::Up,
                band_row,
                t + Duration::from_millis(100 * i as u64),
            );
        }
        assert_eq!(app.scrollback().offset(), top, "clamped at the head");
        assert!(!app.dirty, "and no frame was asked for at the head either");
    }

    /// **The flick, at the App's own door.** Thirty-seven reports at 5 ms —
    /// a trackpad's opening, not a wheel's — move four steps, not thirty-
    /// seven, and the thirty-three that were throttled leave nothing behind:
    /// no queued scroll, no dirty frame, no byte.
    #[test]
    fn a_trackpad_burst_at_the_app_door_is_throttled_not_queued() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let [text, ..] = bands(&app, h);
        let band_row = text.y + 1;
        let t = Instant::now();

        let mut moved = 0usize;
        for i in 0..37u64 {
            if app.on_wheel(WheelDir::Up, band_row, t + Duration::from_millis(5 * i)) {
                moved += 1;
            }
        }
        // One step at 0 ms, then one per interval: 0, 60, 120, 180 of a
        // 180 ms burst on a 60 ms clock.
        assert_eq!(
            moved, 4,
            "thirty-seven reports, four applied steps: the interval is the rate"
        );
        assert_eq!(app.scrollback().offset(), 12);
        assert!(
            app.scrollback().offset() < 37,
            "one row per report would have been 37 rows, and a heavy multiplier \
             would have made the first notch one row; this is neither"
        );

        // The reports inside the throttle ask for no repaint. Times are picked
        // from between two steps (185 ms … 230 ms against a 180 ms last step),
        // so this is the throttle being tested and not the clock keeping up.
        app.dirty = false;
        for i in 1..11u64 {
            assert!(
                !app.on_wheel(
                    WheelDir::Up,
                    band_row,
                    t + Duration::from_millis(180 + 5 * i)
                ),
                "a report inside the throttle moved the view"
            );
        }
        assert!(
            !app.dirty,
            "a burst that is entirely inside the throttle asked for no repaint"
        );
    }

    /// The gesture record is the measurement looprs-pdl.2 #4b never produced,
    /// readable from the App after the fact: how many reports, how long, how
    /// far — and closed by a gap rather than by a hook somebody has to
    /// remember to call.
    #[test]
    fn the_app_keeps_the_shape_of_the_last_gesture() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let [text, ..] = bands(&app, h);
        let band_row = text.y + 1;
        let t = Instant::now();

        assert!(app.wheel_gesture().is_none(), "nothing scrolled yet");
        for i in 0..37u64 {
            app.on_wheel(WheelDir::Up, band_row, t + Duration::from_millis(5 * i));
        }
        assert!(
            app.wheel_gesture().is_none(),
            "still inside the gesture: nothing has closed"
        );

        // The next report after the gap closes the previous one and opens
        // itself, so the two flicks are counted as two. The last report of
        // the burst landed at 180 ms, so this is the first one more than
        // WHEEL_GESTURE_GAP after it.
        app.on_wheel(
            WheelDir::Down,
            band_row,
            t + Duration::from_millis(180) + crate::state::wheel::WHEEL_GESTURE_GAP,
        );
        let g = app.wheel_gesture().expect("the gap closed the flick");
        assert_eq!(g.reports, 37, "every report counted, throttled or not");
        assert_eq!(g.rows, -12, "and only the applied ones moved");
        assert!(
            g.rows_per_report().abs() < 1.0,
            "a flick moves well under one row per report: {g:?}"
        );
    }

    /// **A child holding the screen owns the pointer, the wheel included.**
    /// The same rule the drag has, reached from the other gesture: the pixels
    /// under the cursor are the child's, so the snapshot the last draw
    /// published is a fiction about them and our scroll state is not touched.
    #[test]
    fn no_wheel_state_changes_while_a_child_holds_the_screen() {
        let (mut app, _rx) = app_with(TerminalType::Bash);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let [text, ..] = bands(&app, h);
        let band_row = text.y + 1;

        // The wheel works before the handover, so the check below is not
        // passing because the wheel never works.
        wheel(&mut app, WheelDir::Up, band_row, Instant::now());
        assert_eq!(app.scrollback().offset(), 3);

        let bash = SessionId::new(TerminalType::Bash, 1);
        app.update(Msg::ScreenHeld {
            session: bash,
            active: true,
        });
        let held_at = app.scrollback().offset();
        app.dirty = false;
        let t = Instant::now();
        for i in 0..10u32 {
            assert!(
                !app.on_wheel(
                    WheelDir::Up,
                    band_row,
                    t + Duration::from_millis(100 * i as u64)
                ),
                "the wheel moved the transcript while a child held the screen"
            );
        }
        assert_eq!(
            app.scrollback().offset(),
            held_at,
            "no scroll-state change while a child holds the screen"
        );
        assert!(!app.dirty, "and no frame was asked for either");

        // Back with us, it is the same wheel it always was.
        app.update(Msg::ScreenHeld {
            session: bash,
            active: false,
        });
        assert!(app.on_wheel(WheelDir::Up, band_row, t + Duration::from_secs(1)));
        assert_eq!(app.scrollback().offset(), held_at + 3);
    }

    /// The wheel and the page keys are two doors on one store. What a notch
    /// costs and what a page costs differ; unpinning, holding, counting the
    /// rows that arrive while the view is away and re-pinning on the way back
    /// are one set of rules, because it is one `Scrollback`.
    #[test]
    fn the_wheel_and_the_page_keys_agree_because_they_are_one_store() {
        let (mut app, _rx) = app_with(TerminalType::Pi);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let [text, ..] = bands(&app, h);
        let band_row = text.y + 1;
        let page = app.transcript_band_rows() as isize;
        let t = Instant::now();

        assert!(app.on_wheel(WheelDir::Up, band_row, t));
        assert_eq!(app.scrollback().offset(), 3, "one notch");

        // A page further into the past, by the keyboard's door.
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        ))));
        assert_eq!(
            app.scrollback().offset(),
            (3 + page) as usize,
            "the notch and the page added up in the same store"
        );

        // Output arriving while the view is away is counted, not shown — the
        // store's rule, reached by a wheel-shaped route to it.
        let id = SessionId::new(TerminalType::Pi, 1);
        app.view_mut(id)
            .push_note(MessageKind::Answer, "ARRIVED-WHILE-AWAY".into());
        app.flush_active(W);
        assert_eq!(app.new_rows(), 2, "the line and its separator");
        assert_eq!(
            app.scrollback().offset(),
            (3 + page + 2) as usize,
            "the view held while the tail ran away"
        );

        // And the wheel's own way back to the tail is the store's rule too:
        // enough notches down and the view is pinned again, count cleared.
        let mut at = t + Duration::from_millis(1000);
        for _ in 0..(page + 5) {
            app.on_wheel(WheelDir::Down, band_row, at);
            at += Duration::from_millis(100);
        }
        assert!(app.pinned(), "the wheel got back to the tail");
        assert_eq!(app.new_rows(), 0, "and the count is answered");
    }

    /// **All three modes, one wheel.** The handler does not know which mode it
    /// is in — it scrolls the *active view*'s store and nothing else — so a
    /// notch over the band has to move the transcript in Beeds, in Pi and in
    /// Bash alike. This is the ticket's title read literally, and it is cheap to
    /// prove because there is nothing mode-specific to prove: one door, three
    /// views behind it.
    #[test]
    fn the_wheel_scrolls_the_transcript_in_every_mode() {
        for mode in TerminalType::ALL {
            let (mut app, _rx) = app_with(mode);
            settle_deep(&mut app);
            let h = 20u16;
            paint(&app, h, &[]);
            let [text, ..] = bands(&app, h);
            let t = Instant::now();

            wheel(&mut app, WheelDir::Up, text.y + 1, t);
            assert_eq!(
                app.scrollback().offset(),
                crate::state::wheel::WHEEL_ROWS_PER_STEP as usize,
                "{mode:?}: the notch did not move this mode's transcript"
            );
            assert!(
                !app.pinned(),
                "{mode:?}: and the store unpinned, the same way it does everywhere"
            );
        }
    }

    /// Paint with the input band the app is actually asking for — the value the
    /// run loop hands `crate::view` — so the snapshot the draw publishes is
    /// the geometry the test then classifies against. The `paint` helper above
    /// pins `MIN_INPUT_ROWS`, which is a different frame whenever the box is
    /// hidden or has grown, and a hit test run against a frame that was never
    /// drawn is exactly the mistake the snapshot exists to prevent.
    fn paint_real(app: &App, h: u16) {
        let backend = ratatui::backend::TestBackend::new(W, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        let rows = app.input_band(W);
        term.draw(|f| crate::view(app, f, &[], rows)).unwrap();
    }

    /// The same claim with the frame in its other shape: a beads pass that has
    /// taken the keyboard has no input box, so the band is taller and the row
    /// that *was* the box is now something else. The wheel follows the band,
    /// not the row numbers — which is the whole reason the gate reads the
    /// published snapshot instead of counting from the bottom of the screen.
    #[test]
    fn the_wheel_follows_the_band_when_the_input_box_is_gone() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        settle_deep(&mut app);
        let id = SessionId::new(TerminalType::Beeds, 1);
        app.view_mut(id)
            .set_status(SessionStatus::Running, Instant::now());
        assert_eq!(
            app.input_band(W),
            viewport::NO_INPUT_ROWS,
            "setup: the box is not taking rows"
        );

        let h = 24u16;
        paint_real(&app, h);
        let [text, _cards, status, _input] = bands(&app, h);
        assert_eq!(status.bottom(), h, "the status row is the last row now");
        assert_eq!(
            text.bottom(),
            status.y,
            "setup: the band ends where the chrome begins: {text:?} vs {status:?}"
        );

        let t = Instant::now();
        // The row that used to hold the box: chrome, whatever height it has.
        wheel(&mut app, WheelDir::Up, status.y, t);
        assert_eq!(app.scrollback().offset(), 0, "the status row is not band");

        // The band's own last row is band, and it scrolls.
        wheel(
            &mut app,
            WheelDir::Up,
            text.bottom() - 1,
            t + crate::state::wheel::WHEEL_STEP_INTERVAL,
        );
        assert_eq!(
            app.scrollback().offset(),
            3,
            "the band's last row is inside the band, at whatever height the frame put it"
        );
    }
}
