//! The UI state: one [`App`] holding one [`SessionView`] per terminal state, plus
//! the keyboard, the pointer, the copy chord and the toast.
//!
//! Three neighbours hold what used to be the rest of this file:
//!
//! * [`crate::wire`] — the typed envelopes (`Msg`, `UiCommand`, `PiEvent`, …)
//!   that cross the session boundary;
//! * [`copy`](crate::app::copy) — the copy half of `App`: the chord's state
//!   machine, the pending receipts on the clipboard and the dump sink, the toast;
//! * [`tests`](crate::app::tests) — this suite, split along the banners that were
//!   already written in it.
//!
//! What is left here is the App core: the view map and the answers derived off
//! it, the frame's geometry questions, the message routing, and the
//! keystroke/mouse dispatch whose ordering *is* the behaviour.
mod copy;

use copy::{
    COPY_CHORD_WINDOW, CopyChord, CopyKey, CopyTarget, PendingCopy, PendingDump, copy_chord_key,
    key_word,
};

use crate::components::compaction::{CompactionState, token_delta};
use crate::components::input::{InputAction, InputState, inner_width};
use crate::session::view::SessionView;
use crate::session::{ChatState, SessionId, SessionStatus, TerminalType};
use crate::state::scrollback::{DisplayRow, Scrollback};
use crate::state::selection::{BandSnapshot, Selection};
use crate::state::transcript::MessageKind;
use crate::state::wheel::WheelDir;
use crate::viewport::{self};
use crate::wire::{AssistantEvent, Msg, PiEvent, UiCommand, print_json_value_to_string};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
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
    /// One view per mode, kept warm and invisible (ADR-0002 Q2).
    ///
    /// This map *is* the "how many bounded stores does this process hold?"
    /// question: its ceiling is one entry per [`TerminalType`]
    /// ([`MAX_VIEWS`](crate::session::view::MAX_VIEWS)), and the bytes behind
    /// those entries are stated once on
    /// [`RETAINED_BYTES_WORST_CASE`](crate::session::view::RETAINED_BYTES_WORST_CASE).
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
    /// Where a copy goes (looprs-pdl.10). Injected, never called directly.
    ///
    /// `Noop` by default for exactly the reason `SessionConfig`'s is: the App is
    /// what ~200 unit tests build, and a default that reached for `pbcopy` would
    /// make every selection test a clipboard test.
    clipboard: Arc<dyn crate::services::clipboard::Clipboard>,
    /// The **automatic** copy path: does a drag release copy?
    ///
    /// `LOOPRS_COPY_ON_SELECT=0` turns this off and nothing else (ADR-0004 R17).
    /// The selection itself, the toast, and the keyboard copy are all untouched,
    /// because a user who does not want their clipboard written by a mouse
    /// gesture still wants it written when they ask.
    copy_on_select: bool,
    /// The copy we have handed to the sink and are waiting to hear about.
    ///
    /// Held rather than awaited: the UI task must never be parked on a
    /// clipboard. The receipt is polled from the tick, and this carries the
    /// timestamp that turns "still in flight" into a visible late failure at
    /// [`COPY_TIMEOUT`] rather than an empty screen.
    pending_copy: Option<PendingCopy>,
    /// The toast on screen right now (looprs-pdl.10).
    ///
    /// One, not a queue: R21 says the next toast *replaces* the previous rather
    /// than queueing behind it, because two toasts about two copies would leave
    /// the user unsure which selection the clipboard holds.
    toast: Option<crate::state::toast::Toast>,
    /// Where a whole-transcript dump goes (looprs-pdl.13). Injected, exactly like
    /// [`Self::clipboard`], and for the same reason: the App builds no file path
    /// of its own, so "where did my transcript go" has one answer per run.
    transcript_sink: Arc<dyn crate::services::transcript_file::TranscriptSink>,
    /// Where each session's transcript goes **as it finalises**
    /// (looprs-pdl.7, ADR-0004 R2).
    ///
    /// One handle shared by every view, injected from `main` like the clipboard
    /// and the dump sink. It is not the same thing as `transcript_sink`: that one
    /// answers "write me this transcript now" (`Ctrl-S t`) into a fresh
    /// timestamped file, this one is the run's own record, appended entry by
    /// entry, whether or not anyone ever asks. The App holds it so the marker row
    /// on a trimmed scrollback can name the file where the trimmed content still
    /// lives, and so `close` has one caller on the way out.
    journal: Arc<dyn crate::services::journal::Journal>,
    /// The dump we have handed to that sink and are waiting to hear about.
    pending_dump: Option<PendingDump>,
    /// The copy chord's prefix state (looprs-pdl.13).
    ///
    /// `Ctrl-S` arms it; the next key picks the target. A prefix rather than four
    /// top-level chords because the chord budget is the constraint this ticket was
    /// written around: three modes, an Esc rule, Tab switching, Ctrl-C as SIGINT,
    /// Ctrl-Q to quit and Shift-Enter for a newline already spend it, and a
    /// fourth free control key costs a shadow somewhere. Under the prefix the
    /// target keys (`a`/`o`/`s`/`t`/`?`) exist only in the armed window, so the
    /// audit for "does this shadow an existing binding" is one key, not five.
    ///
    /// See [`CHORD_TABLE`](crate::session::view::CHORD_TABLE) for the whole
    /// mode x key picture, and [`Self::on_key`] for the order these checks run
    /// in — which is the audit made executable.
    copy_chord: CopyChord,
    /// When the prefix went armed, for the window's expiry.
    /// Only meaningful while [`Self::copy_chord`] is `Armed`.
    copy_chord_at: Instant,
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
            clipboard: Arc::new(crate::services::clipboard::Noop),
            copy_on_select: true,
            pending_copy: None,
            toast: None,
            transcript_sink: Arc::new(crate::services::transcript_file::Noop),
            journal: crate::services::journal::default_journal(),
            pending_dump: None,
            copy_chord: CopyChord::Off,
            copy_chord_at: now,
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
        if !self.views.contains_key(&id.mode) {
            // A view is born knowing where its transcript goes. Injected at
            // creation rather than picked up by a later sweep, so there is no
            // window in which a view exists that journalled nothing — and no
            // early content that quietly missed the file.
            let mut fresh = SessionView::new(id);
            fresh.set_journal(self.journal.clone());
            self.views.insert(id.mode, fresh);
        }
        let v = self
            .views
            .get_mut(&id.mode)
            .expect("the view for this mode was just created");
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
    /// Returns how many rows were appended to the store's tail; the rows
    /// themselves stay in the store, which is where the band already reads them
    /// from ([`Self::transcript_window`]). Handing them back too meant cloning the
    /// whole batch every frame for a caller that did not exist.
    pub fn flush_active(&mut self, width: u16) -> usize {
        match self.views.get_mut(&self.active) {
            Some(v) => {
                let added = v.flush(width);
                self.sync_selection_to_trims();
                added
            }
            None => 0,
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
    /// was given, fed the same number from the same place
    /// (`SessionView::take_trims`), and it **clamps** rather than lies: a
    /// selection whose earlier end was trimmed starts now at the oldest thing
    /// still there, and the copy that follows reports the count of what actually
    /// went (looprs-pdl.7).
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
        for removed in v.take_trims() {
            self.selection.entries_evicted(removed);
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
        // Any button press means the user moved on from what the last toast was
        // reporting, and R21 says the next key or click puts it away. Done here,
        // ahead of the passthrough gate, so the rule does not depend on who else
        // is holding the screen.
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.dismiss_toast();
        }
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
                let committed = self.selection.release();
                self.dirty = true;
                // **Copy on release** (looprs-pdl.10, ADR-0004 R12). The redraw
                // runs at event rate and the side effect runs at *gesture*
                // rate: the wire can take a drag's motion reports at
                // ~4.5e5/s (pdl.2) and the selection band is repainted on
                // every one of them, but a clipboard write at that rate is 200
                // process spawns and 200 clipboard-manager history entries for
                // one gesture the user has not finished. `release()` is the
                // only place a gesture ends.
                //
                // The blank-selection case is not handled here at all: it is
                // `copy_text`'s R13 check, so the drag path and the keyboard
                // path cannot disagree about what "nothing to copy" means.
                if committed.is_some() && self.copy_on_select {
                    self.copy_selection();
                }
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
        // The toast's lifetime and the copy's deadline run off the tick, not off
        // the animation gate below: a copy made in an otherwise idle app still
        // has to get its confirmation on screen, and still has to come off.
        self.poll_copy(now);
        self.poll_dump(now);
        if self.copy_chord == CopyChord::Armed
            && now.saturating_duration_since(self.copy_chord_at) >= COPY_CHORD_WINDOW
        {
            // The window closes silently, which is not a hole in the legibility
            // rule: the help toast that named the window has the same TTL, so what
            // the user sees is the hint expiring, which is the window expiring.
            self.copy_chord = CopyChord::Off;
            tracing::debug!("copy chord window closed without a target key");
        }
        if let Some(toast) = &self.toast
            && toast.expired(now)
        {
            self.toast = None;
            self.dirty = true;
        }
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
        let id = self.view_id_for(mode);
        self.view_mut(id).push_note(MessageKind::User, text);
    }

    /// The id of the view this app currently tracks for a mode, or the harness
    /// id if there is none yet.
    ///
    /// Factored out of [`Self::echo_local`] for the same reason that reason
    /// applies: anything that writes into a mode's view must write into the *same*
    /// view, and inventing a generation number in a second place is how two
    /// callers end up sealing two different sessions.
    fn view_id_for(&self, mode: TerminalType) -> SessionId {
        self.views
            .get(&mode)
            .map(|v| v.session)
            .unwrap_or_else(|| SessionId::new(mode, HARNESS_GENERATION))
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
                    // A prefix armed before the handover dies with it: nothing is
                    // copied while a child holds the screen (R12), and the
                    // keyboard belongs to the child now, so the next key must go
                    // there rather than be read as a copy target.
                    self.copy_chord = CopyChord::Off;
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
        // The first thing any keystroke does is put the toast away (R21) — before
        // the chord dispatch, so "the next key dismisses it" is true for every
        // key including the ones that quit, cancel, or are swallowed by a mode
        // that does not read this one.
        self.dismiss_toast();
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
                    // The chord dies with it: a prefix that survived a Ctrl-C would
                    // make the *next* key a copy target in a session the user has
                    // just interrupted (or quit). Every chord in this block
                    // outranks a pending one in every mode.
                    self.copy_chord = CopyChord::Off;
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
                    self.copy_chord = CopyChord::Off;
                    self.should_quit = true;
                    return;
                }
                // **Ctrl-S: the copy prefix** (looprs-pdl.13). Claimed in every
                // mode, and claimed *hard*: it is never written to a child's pty.
                //
                // The reason is the pair, not the key. `0x13` is XOFF and `0x11`
                // is XON; looprs owns `Ctrl-Q` as quit in every mode, so a
                // forwarded `Ctrl-S` would stop a child's output with the one
                // chord needed to restart it already spent. A user who stopped a
                // shell that way could not un-stop it without killing the app and
                // the shell with it, which is an unrecoverable state created by a
                // keystroke that looked harmless. Taking `Ctrl-S` entirely removes
                // that state. (Our own terminal cannot XOFF either: crossterm's
                // raw mode is `cfmakeraw`, which clears `IXON`, so the byte
                // arrives here as a key rather than freezing our own stdout.)
                KeyCode::Char('s') => {
                    self.copy_prefix();
                    return;
                }
                _ => {}
            }
        }

        // **The chord's second key** (looprs-pdl.13). Only reached with the
        // prefix outstanding; the disarm happens here, on every branch, so a
        // pending chord cannot outlive the keystroke that answers it.
        //
        // It sits above the passthrough block, and the ordering is not an
        // accident: `Ctrl-S` is already swallowed above so a full-screen child
        // never sees it, which means a prefix outstanding while a child takes the
        // screen was armed *before* the handover. That prefix is cancelled here
        // rather than honoured, because R12 says nothing is copied while a child
        // holds the screen — the transcript is not what the user is looking at,
        // and a `Copied 4,182 characters` toast they cannot see, over a vim
        // frame, describing something that is not on screen, is worse than the
        // refusal.
        if self.copy_chord == CopyChord::Armed {
            self.copy_chord = CopyChord::Off;
            if self.passthrough() {
                tracing::debug!(
                    "copy chord cancelled: a full-screen child took the screen \u{2014} \u{2014} the \u{2018}?\u{2019} chord lists the family"
                );
                return;
            }
            match copy_chord_key(k) {
                CopyKey::Answer => {
                    self.copy_target(CopyTarget::Answer);
                }
                CopyKey::Output => {
                    self.copy_target(CopyTarget::CommandOutput);
                }
                CopyKey::Selection => {
                    self.copy_target(CopyTarget::Selection);
                }
                CopyKey::TranscriptFile => {
                    self.dump_transcript();
                }
                CopyKey::Help => {
                    self.show_toast(
                        &crate::session::view::copy_chord_hint(),
                        crate::state::toast::Tone::Good,
                    );
                }
                CopyKey::Cancel => {
                    // Esc while armed undoes the prefix and nothing else. It does
                    // **not** fall through to the selection clear below, and it
                    // does **not** become the mode's cancel: the most recent thing
                    // the user did was arm a chord, and ADR-0003's rule about Esc
                    // is that it takes back what they just gave us, not something
                    // older and louder. Cancelling a running pi run because the
                    // user changed their mind about a copy is the exact surprise
                    // that rule was written to stop.
                    tracing::debug!("copy chord cancelled by Esc");
                }
                CopyKey::NotAChord => {
                    // Swallowed, with the reason said. Falling through to the
                    // input box instead would be the worse guess: a keystroke
                    // typed after a prefix is *intended* as part of the chord, so
                    // "insert `x` into the command line I was about to run" and
                    // "copy the transcript" are both live readings of it, and the
                    // one we must not silently pick is the one that changes the
                    // user's shell. The toast names the key and points at `?`.
                    self.show_toast(
                        &format!(
                            "Nothing copied: {} is not a copy chord \u{2014} Ctrl-S ? lists them",
                            key_word(k)
                        ),
                        crate::state::toast::Tone::Bad,
                    );
                }
            }
            return;
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
                    } else {
                        // **The command boundary** (looprs-pdl.13). Sealed here,
                        // at the one moment the boundary is known, so that
                        // everything the shell says from now on is *this*
                        // command's entry and `Ctrl-S o` copies one command
                        // rather than the session. The submit is the boundary
                        // because the shell never reports where one command's
                        // output stopped and the next prompt started, and
                        // guessing from a prompt pattern breaks on every shell
                        // that is not bash.
                        let id = self.view_id_for(mode);
                        self.view_mut(id).seal_shell_output();
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
                    // The copy prefix goes with the selection for the same reason
                    // it goes with a mode change: it was armed over *this* mode's
                    // transcript, and `a`/`o`/`s` after a Tab would resolve
                    // against the next mode's while still looking, to the user,
                    // like the one they aimed at.
                    self.copy_chord = CopyChord::Off;
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
            view.finish_stream();
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
            // upsert: fills in the args. Through the view, not into the transcript:
            // a tool's argument blob is content like any other, and the write door
            // that has to run for it (cap, journal) is the view's.
            view.start_tool(tool_call_id, tool_name, print_json_value_to_string(&args));
        }
        // PiEvent::ToolExecutionUpdate { .. } => stream partial output into the row if you want it
        PiEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
        } => {
            // The tool's result is the single biggest thing a beads pass writes,
            // and until this went through the view it was also the one thing the
            // buffer cap never saw. See `SessionView::start_tool`.
            view.finish_tool(tool_call_id, result.text(), is_error);
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
            view.start_compaction(reason);
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
            if !view.finish_compaction(state, detail.clone()) {
                view.push_note(
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
mod tests;
