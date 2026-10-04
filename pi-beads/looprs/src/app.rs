//! Typed wire format for pi's session events (json.md / rpc.md), deserialized directly with serde,
//! plus the one place that turns them into transcript changes.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see `parse`).

use crate::components::input::{InputAction, InputState};
use crate::session::view::SessionView;
use crate::session::{ByteStream, ChatState, ExitReason, SessionId, SessionStatus, TerminalType};
use crate::state::state::MessageKind;
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
        stream: ByteStream,
        chunk: String,
    },
    /// The beads machine moved. Rendered, never re-derived.
    BeadStep {
        session: SessionId,
        step: BeadStep,
    },
    /// A session's child is gone. Guaranteed exactly once per session, so the
    /// receiver can always seal that session's transcript.
    SessionDown {
        session: SessionId,
        reason: ExitReason,
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
    /// Legacy: the App asks the beads loop to advance. Goes away with looprs-msj,
    /// which moves that transition inside `BeadsSession` where it belongs.
    BeadsNext,
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
    ToolExecutionUpdate {
        tool_call_id: String,
        partial_result: ToolOutput,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        result: ToolOutput,
        is_error: bool,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        #[serde(default)]
        final_error: Option<String>,
    },
    CompactionStart {
        reason: String,
    },
    CompactionEnd {
        #[serde(default)]
        aborted: bool,
        #[serde(default)]
        error_message: Option<String>,
    },
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
#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    }, // serialized (partial) argument JSON
    ToolcallEnd {
        content_index: usize,
        tool_call: Value,
    },
    Done {
        reason: String,
    },
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
            Msg::BashOutput {
                session,
                stream,
                chunk,
            } => {
                // Routed, deliberately not rendered yet: looprs-553 owns the Bash
                // display path, and that path must be a raw passthrough. Pushing
                // these bytes in as `Answer` would run the markdown renderer over a
                // shell's output, which Q5 rejects outright — so the bytes are
                // dropped in one obvious place rather than rendered wrongly here.
                let _ = (session, stream, chunk);
            }
            Msg::BeadStep { session, step } => {
                self.dirty = true;
                self.view_mut(session).set_step(step);
            }
            Msg::SessionDown { session, reason } => {
                // Q5 rule 3: death must seal. Unconditional, because the pump
                // promises exactly one of these per session ever created.
                self.dirty = true;
                let view = self.view_mut(session);
                view.seal();
                view.set_status(SessionStatus::Dead);
                view.push_note(MessageKind::System, format!("{session} ended ({reason:?})"));
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
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if let Some(action) = self.input.handle_key(k) {
            match action {
                InputAction::Submit { text, mode } => {
                    // Every mode is addressed to a session now; whether it can
                    // answer is the backend's business (Pi/Bash still refuse,
                    // loudly, and the refusal comes back as `Msg::Error` against
                    // the same view the echo is in).
                    self.echo_local(mode, text.clone());
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

    /// A pi protocol event, applied to the view of the session that made it.
    ///
    /// `session` decides *where it lands* — never `self.input.mode`, and never
    /// `self.active` (ADR-0002 Q2). The `session.mode` test at `AgentSettled` is a
    /// statement about the event's *producer*, not about where the user happens to
    /// be looking; that is the difference between routing and guessing, and it is
    /// the whole of looprs-msj's bug class. (`msj` still owns deleting the
    /// App-driven advance altogether.)
    pub fn on_pi(&mut self, session: SessionId, ev: PiEvent) {
        self.dirty = true;
        let advance = apply_pi(self.view_mut(session), session, ev);
        if let Some(cmd) = advance {
            let _ = self.cmd_tx.try_send(cmd);
        }
    }
}

/// Apply one pi event to one session's view, and report the follow-up command (if
/// any) that the App still owes.
///
/// A free function on purpose: it cannot reach `App`'s globals, so "which view does
/// this touch" is answered by the signature rather than by the current mode.
fn apply_pi(view: &mut SessionView, session: SessionId, ev: PiEvent) -> Option<UiCommand> {
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
            None
        }
        // user messages are already echoed locally on submit; ignore pi's copy
        PiEvent::MessageEnd { message } if message.role == "assistant" => {
            view.transcript.finish_last();
            None
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
            None
        }
        // PiEvent::ToolExecutionUpdate { .. } => stream partial output into the row if you want it
        PiEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
        } => {
            view.transcript
                .finish_tool(tool_call_id, result.text(), is_error);
            None
        }
        PiEvent::AgentSettled => {
            view.chat = ChatState::Stopped;
            // Legacy: see `App::on_pi`. Keyed on the *producer's* mode.
            (session.mode == TerminalType::Beeds).then_some(UiCommand::BeadsNext)
        }
        // AutoRetryStart / CompactionStart: show a status note if you want one
        _ => None,
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
        assert!(text_of(&app, TerminalType::Beeds).contains("pi answer") == false);
        assert!(text_of(&app, TerminalType::Pi).contains("pi answer"));
        assert!(text_of(&app, TerminalType::Pi).contains("beads answer") == false);
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

    /// **A Pi run must never move the beads machine** (looprs-ctn, re-verifying
    /// looprs-msj). The two are separate sessions with separate processes, and the
    /// only thing that decides whether a settle means "advance the loop" is who
    /// produced it.
    #[tokio::test]
    async fn a_pi_run_never_moves_the_beads_machine() {
        let (mut app, mut rx) = app_with(TerminalType::Pi);

        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta {
                    content_index: 0,
                    delta: "a chat answer".into(),
                },
            },
        });
        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::AgentSettled,
        });

        assert!(
            rx.try_recv().is_err(),
            "a Pi settle must not ask anyone to advance a beads pass"
        );
        assert!(
            app.view(TerminalType::Beeds).is_none(),
            "and must not create one"
        );
        assert!(
            text_of(&app, TerminalType::Beeds).chars().count() == 0,
            "nothing of the Pi chat belongs in the beads view"
        );

        // The contrast, so it is clear the rule is about the *producer* and not an
        // accident of ordering: the same settle from the beads session does advance.
        app.update(Msg::Agent {
            session: beads_id(),
            event: PiEvent::AgentSettled,
        });
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::BeadsNext)),
            "only the beads session's settle is a step in the beads machine"
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
}
