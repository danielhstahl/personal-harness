//! `App`'s tests, split along the banners that already sectioned the one module
//! this came out of. Each file in here is one of those sections:
//!
//! * [`input_box`](input_box) — the box, its height policy, and the keys that
//!   stop at the box instead of reaching a session;
//! * [`token_window`](token_window) — the row's cost facts, per claim and per
//!   generation, and the compaction card's wire half;
//! * [`pi_state`](pi_state) — the Pi terminal state from the UI side, including
//!   the rule that routing never reads the input mode;
//! * [`screen_held`](screen_held) — the full-screen seam (ADR-0001 Q2): who gets
//!   the keyboard, the bytes, and the screen back;
//! * [`status_row`](status_row) — `App`'s half of the row: gather the mirrors,
//!   decide nothing;
//! * [`scrollback_band`](scrollback_band) — the store driving the band: paging,
//!   held-back output, resizing;
//! * [`drag_selection`](drag_selection) — the drag through the real frame, where
//!   a row of pixels and a row of the store have to agree;
//! * [`select_to_copy`](select_to_copy) — the copy a release makes: the count,
//!   the toast, the deadline, the clear list;
//! * [`wheel`](wheel) — the wheel and the trackpad: bands, cadence, throttling;
//! * [`user_band`](user_band) — the user's own rows: the background that says
//!   "mine", and the copy that must carry none of it;
//! * [`chord_table`](chord_table) — the `Ctrl-S` family, driven keystroke by
//!   keystroke against the table in `session::view`.
//!
//! What more than one section drives lives here: the one `App` builder, the
//! session ids the fixtures share, and the paint/geometry readers a test points
//! at. They are private to this module on purpose — each file above reaches them
//! through `use super::*;`, and nothing outside the suite should be assembling an
//! `App` with them.

use super::*;
use crate::session::view::Tokens;
use crate::session::{ActiveBead, BeadStep, ByteStream, ExitReason, SessionStatus};
use crate::wire::{CompactionResult, Usage, WireMessage, parse};
use serde_json::Value;

mod chord_table;
mod drag_selection;
mod input_box;
mod pi_state;
mod screen_held;
mod scrollback_band;
mod select_to_copy;
mod status_row;
mod token_window;
mod user_band;
mod wheel;

const W: u16 = 80;

fn app_with(active: TerminalType) -> (App, mpsc::Receiver<UiCommand>) {
    let (tx, rx) = mpsc::channel::<UiCommand>(16);
    (App::new(tx, InputState::new(), active, 80, 24), rx)
}

fn beads_id() -> SessionId {
    SessionId::new(TerminalType::Beeds, 1)
}

fn pi_id() -> SessionId {
    SessionId::new(TerminalType::Pi, 1)
}

fn bash_id() -> SessionId {
    SessionId::new(TerminalType::Bash, 1)
}

fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(crossterm::event::KeyEvent::new(code, mods))
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
        viewport::KanbanBudget::Off,
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

/// A mouse report, as the `Msg` the run loop would deliver for it.
fn mouse(kind: MouseEventKind, row: u16, col: u16) -> Msg {
    Msg::Term(Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }))
}

/// Hand the App a clipboard that records. Everything below asserts on the
/// exact string that reached the sink, which is the only assertion that says
/// "the copy was the selection" rather than "a copy happened".
fn recording_clipboard(app: &mut App) -> crate::testing::RecordingClipboard {
    let rec = crate::testing::RecordingClipboard::new();
    app.set_clipboard(Arc::new(rec.clone()));
    rec
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
