//! Typed wire format for pi's session events (json.md / rpc.md), deserialized directly with serde,
//! plus the one place that turns them into transcript changes.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see `parse`).

use crate::components::input::{InputAction, InputState};
use crate::session::view::SessionView;
use crate::session::{
    ActiveBead, ByteStream, ChatState, ExitReason, SessionId, SessionStatus, TerminalType,
};
use crate::state::transcript::MessageKind;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub use crate::session::BeadStep;

/// The `generation` a harness-level message uses when it has to create a view for
/// a mode that has no session yet (`"could not open Bash: …"`).
///
/// `0` is not just a spare number: the Router's generations start at 1, so a
/// harness placeholder can never collide with a real session's identity.
pub const HARNESS_GENERATION: u64 = 0;

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
        /// Consumed by looprs-guh, which wants stderr lines marked as such in the
        /// status row; until then the transcript deliberately prints one merged
        /// stream, because inventing a split the backend cannot back up is worse
        /// than not having one.
        #[allow(dead_code)] // consumer: looprs-guh (status row marks stderr distinctly)
        stream: ByteStream,
        chunk: String,
    },
    /// The beads machine moved. Rendered, never re-derived.
    BeadStep {
        session: SessionId,
        step: BeadStep,
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
        #[allow(dead_code)] // consumer: looprs-guh (live region wants start-of-message)
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
    #[allow(dead_code)] // consumer: looprs-guh (in-place tool card repaint)
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
    #[allow(dead_code)] // consumer: looprs-guh (retry shown as a status, not silence)
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// As [`PiEvent::AutoRetryStart`]: the outcome of pi's own retry ladder. The
    /// transcript currently treats a successful retry as invisible, which is fine
    /// for a run that recovers and terrible for one that does not — hence kept.
    #[allow(dead_code)] // consumer: looprs-guh (retry outcome, esp. `final_error`)
    AutoRetryEnd {
        success: bool,
        #[serde(default)]
        final_error: Option<String>,
    },
    /// Context compaction began. Until the UI can say "compacting…" (looprs-guh)
    /// this arrives as an unexplained pause, which is the bug class the ticket is
    /// about — so the reason is parsed and kept, not dropped.
    #[allow(dead_code)] // consumer: looprs-guh ("compacting: <reason>")
    CompactionStart {
        reason: String,
    },
    /// Compaction finished, was aborted, or failed. `aborted`/`error_message` are
    /// the two things a user must not have to guess about.
    #[allow(dead_code)] // consumer: looprs-guh (aborted/failed compaction is loud)
    CompactionEnd {
        #[serde(default)]
        aborted: bool,
        #[serde(default)]
        error_message: Option<String>,
    },
    /// An extension in the pi child raised. Not the harness's fault, but it is the
    /// harness's screen, so the path and the message are recorded now that the
    /// status row exists to put them in (looprs-guh).
    #[allow(dead_code)] // consumer: looprs-guh (extension errors are surfaced, not swallowed)
    ExtensionError {
        extension_path: String,
        error: String,
    },
    #[serde(other)]
    Unknown, // must be last; swallows any event type you haven't modeled
}

#[derive(Debug, Deserialize)]
pub struct WireMessage {
    pub role: String, // "user" | "assistant" | "toolResult" ...; content left undeclared for now
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
    #[allow(dead_code)] // consumer: looprs-guh (per-block live region)
    TextStart {
        content_index: usize,
    },
    /// The visible stream: this delta is what gets printed. The index beside it is
    /// unread for the same one-block-per-message reason as above.
    TextDelta {
        #[allow(dead_code)] // consumer: looprs-guh (which block this delta belongs to)
        content_index: usize,
        delta: String,
    },
    /// `content` is read (the authoritative block text, tee'd by the beads
    /// planner's "last words"); the index still isn't.
    TextEnd {
        #[allow(dead_code)] // consumer: looprs-guh (which block ended)
        content_index: usize,
        content: String,
    },
    /// Thinking is parsed and deliberately not shown. Both fields unread today:
    /// the transcript prints answers, not reasoning. Kept because "pi is thinking"
    /// is the single most useful thing a status row can say while a run is open.
    #[allow(dead_code)] // consumer: looprs-guh ("thinking…" while a run is open)
    ThinkingStart {
        content_index: usize,
    },
    #[allow(dead_code)] // consumer: looprs-guh (thinking stream, if ever surfaced)
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    #[allow(dead_code)] // consumer: looprs-guh (the finished thinking block)
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    /// The tool call inside the assistant's own stream. The card the user sees is
    /// built from the top-level `ToolExecutionStart`, which carries the same
    /// identity; this variant is the assistant-side view of it, and is kept so the
    /// two can be correlated when the live region (looprs-guh) renders calls as
    /// they are minted rather than when they run.
    #[allow(dead_code)] // consumer: looprs-guh (tool call as the model writes it)
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    /// Partial argument JSON. Printing half-written JSON is worse than printing
    /// nothing until the call lands, so it is parsed, unread, and available.
    #[allow(dead_code)] // consumer: looprs-guh (streaming args, once renderable)
    ToolcallDelta {
        content_index: usize,
        delta: String,
    }, // serialized (partial) argument JSON
    #[allow(dead_code)] // consumer: looprs-guh (the completed call object)
    ToolcallEnd {
        content_index: usize,
        tool_call: Value,
    },
    /// Why the assistant stopped (stop / end_turn / length / …). Unread today; a
    /// run that ended for `length` looks exactly like one that finished, which is
    /// precisely the distinction looprs-guh exists to make.
    #[allow(dead_code)] // consumer: looprs-guh ("ended early: <reason>")
    Done {
        reason: String,
    },
    /// The assistant-side error. Surfacing is the status row's job (looprs-guh);
    /// until that row exists the harness deliberately does not half-report it.
    #[allow(dead_code)] // consumer: looprs-guh (assistant errors are shown, not hidden)
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
    pub dirty: bool,
    pub should_quit: bool,
    /// The session whose child currently owns the real terminal screen, if any
    /// (ADR-0001 Q2). `Some` for as long as a full-screen program holds it.
    ///
    /// Tracked here rather than asked of the session because drawing is this type's
    /// job, and the run loop has to be able to ask "am I allowed to draw?" without
    /// reaching into a backend.
    screen: Option<SessionId>,
    /// The screen came back and the inline viewport must be re-anchored before
    /// anything is drawn. Set on release, consumed by the run loop in `main.rs`,
    /// which is the only place that can stop the key stream, resize the
    /// `Terminal` (a re-anchor reads the cursor position back) and restart it —
    /// the same dance `main.rs` already does for a window resize, for the same
    /// reason.
    pub reanchor: bool,
    cmd_tx: mpsc::Sender<UiCommand>, // UI -> Router
}

impl App {
    pub fn new(
        cmd_tx: mpsc::Sender<UiCommand>,
        mut input: InputState,
        active: TerminalType,
        width: u16,
    ) -> Self {
        // The box and the view must open on the same mode: they are the same fact
        // seen from two sides, and a mismatch here would route keystrokes to a mode
        // the user is not looking at.
        input.mode = active;
        Self {
            input,
            views: HashMap::new(),
            active,
            width,
            dirty: true,
            should_quit: false,
            spinner: 0,
            screen: None,
            reanchor: false,
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
            // A new incarnation of a mode is not holding the previous one's
            // ticket. Leaving a stale claim on the row would have the UI naming a
            // bead no live process owns.
            v.active_bead = None;
        }
        v
    }

    /// A specific mode's view. Nothing renders through this today — the frame draws
    /// the active one — but it is how looprs-guh shows "beads is still working
    /// while you chat", which the warm-child policy makes load-bearing rather than
    /// cosmetic.
    #[allow(dead_code)] // consumer: looprs-guh's status row
    pub fn view(&self, mode: TerminalType) -> Option<&SessionView> {
        self.views.get(&mode)
    }

    pub fn active_view(&self) -> Option<&SessionView> {
        self.views.get(&self.active)
    }

    /// Does the **active** session want typed input? (Was: a global `need_input`.)
    /// No view yet means yes: a mode nobody has opened is idle by definition.
    pub fn need_input(&self) -> bool {
        self.active_view().map(|v| v.awaiting_user).unwrap_or(true)
    }

    /// What the live region of the **active** session shows.
    pub fn chat_state(&self) -> ChatState {
        self.active_view().map(|v| v.chat).unwrap_or_default()
    }

    /// The per-frame flush, for the active view only (Q5 rule 4: inactive views
    /// buffer, and their backlog goes out as one burst when they become active).
    pub fn flush_active(&mut self, width: u16) -> Vec<Line<'static>> {
        match self.views.get_mut(&self.active) {
            Some(v) => v.flush(width),
            None => Vec::new(),
        }
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

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => {
                if self.chat_state().is_streaming() {
                    self.spinner = self.spinner.wrapping_add(1);
                    self.dirty = true;
                }
            }
            Msg::Term(Event::Resize(w, _)) => {
                self.width = w;
                self.dirty = true;
            }
            Msg::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                self.dirty = true;
                self.on_key(k);
            }
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
                } else {
                    // Line-oriented output: `push_bash` strips the presentation
                    // but never re-wraps and never reaches markdown
                    // (ADR-0001 rule 1).
                    self.dirty = true;
                    self.view_mut(session).push_bash(&chunk);
                }
            }
            Msg::BeadStep { session, step } => {
                self.dirty = true;
                self.view_mut(session).set_step(step);
            }
            Msg::ActiveBead { session, bead } => {
                // A mirror of the loop's claim, recorded rather than interpreted:
                // the status row (looprs-guh) reads this field, and nothing here
                // decides what to *do* with it.
                self.dirty = true;
                self.view_mut(session).active_bead = bead;
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
                    self.reanchor = true;
                }
                let view = self.view_mut(session);
                view.seal();
                view.set_status(SessionStatus::Dead);
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
                    // The real terminal is not showing what ratatui's diff thinks
                    // it is showing: the child drew over it (or switched it, in
                    // the alt-screen case, where switching back restores the main
                    // screen but not our cursor). Re-anchor, then repaint from
                    // scratch — trusting the diff here is the "screen is garbled
                    // after exiting vim" bug ADR-0001 names.
                    self.reanchor = true;
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

        if let Some(action) = self.input.handle_key(k) {
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
        // AutoRetryStart / CompactionStart: show a status note if you want one
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{BeadStep, SessionStatus};

    fn app_with(active: TerminalType) -> (App, mpsc::Receiver<UiCommand>) {
        let (tx, rx) = mpsc::channel::<UiCommand>(16);
        (App::new(tx, InputState::new(), active, 80), rx)
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
    /// working off-screen must not hide the Pi input box, and vice versa.
    #[test]
    fn need_input_is_derived_from_the_active_view() {
        let (mut app, _rx) = app_with(TerminalType::Beeds);
        assert!(app.need_input(), "no view yet => ask the human");

        app.update(Msg::BeadStep {
            session: beads_id(),
            step: BeadStep::WorkTickets,
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
            "and coming back must not resurrect a box the beads machine already hid"
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
            v.awaiting_user,
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
                    },
                },
            });
            app.update(Msg::Agent {
                session: pi_id(),
                event: PiEvent::MessageEnd {
                    message: WireMessage {
                        role: role.to_string(),
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

    /// **A `Msg::BashOutput` lands in the Bash view as raw text** and never in the
    /// mode the input box happens to be in — and the chunk is kept whole rather
    /// than re-split into lines (ADR-0001 rule 1: the child owns its framing).
    #[test]
    fn bash_output_lands_in_its_own_view_verbatim() {
        let mut app = {
            let (tx, _rx) = mpsc::channel::<UiCommand>(16);
            App::new(tx, InputState::new(), TerminalType::Beeds, 80)
        };
        let bash = SessionId::new(TerminalType::Bash, 1);
        let chunk = "first line\nsecond line, with no trailing newline";
        app.update(Msg::BashOutput {
            session: bash,
            stream: ByteStream::Merged,
            chunk: chunk.into(),
        });

        assert_eq!(
            text_of(&app, TerminalType::Bash),
            chunk,
            "the read chunk arrived whole — not re-split, not re-joined"
        );
        assert!(
            app.view(TerminalType::Beeds).is_none(),
            "and it went nowhere near the mode the box was in"
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
        assert!(
            text_of(&app, TerminalType::Bash).contains("painted while hidden"),
            "kept in its own view instead of overwriting Pi"
        );
        assert!(app.view(TerminalType::Pi).is_none(), "Pi was not touched");
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
            !app.reanchor,
            "taking the screen over is not a reason to resize"
        );
        app.update(Msg::ScreenHeld {
            session: bash_id(),
            active: false,
        });
        assert!(
            app.reanchor,
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
        assert!(app.reanchor, "and the viewport has to be rebuilt");
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
        assert!(!app.reanchor);
    }
}
