//! The full-screen frame: the five bands, and how they tile the window
//! (looprs-pdl.4; the fifth band's row budget is looprs-5o4.3).
//!
//! # What this module used to be
//!
//! Until this ticket the live region was a `Viewport::Inline(h)` — a pane parked
//! near the bottom of the terminal, sharing the screen with two neighbours it did
//! not own: the user's scrollback above it and whatever blank screen was left
//! below. Every hard thing in the old version of this file was a consequence of
//! those neighbours, and none of them survives owning the window outright:
//!
//! * **the cursor query.** Building an inline `Terminal` asks the terminal where
//!   the cursor is (`ESC[6n`) so it knows what row to anchor to. That one fact
//!   governed the whole frame loop, because the query and the async key stream
//!   read the same stdin: the stream had to be stopped and restarted around
//!   `fit` and `resize_window`, `needs_fit` existed to ask whether the rebuild
//!   was worth that, `SIGWINCH` had to be polled every tick because a resize
//!   that landed while the stream was stopped was never delivered at all, and
//!   `reanchor` covered the case where a full-screen child moved the pane out
//!   from under us. A screen we own from the first byte is never queried.
//! * **the erase-before-rebuild.** A freshly built `Terminal` diffs against a
//!   blank back buffer, so a cell that is blank in the new frame but not on the
//!   screen is last frame's text welded there forever — which is why `fit`
//!   cleared from the pane's top row down before rebuilding, and why `fit` had
//!   to be told where the pane was before it could run. The full-screen backend
//!   owns what it paints; there is nothing to reason about painting over.
//! * **the anchor.** [`crate::teardown`] used to erase the pane from a
//!   [`LiveAnchor`](crate::teardown::LiveAnchor) this type published, and
//!   `insert_before` had to follow the pane down so that the erase did not
//!   delete the lines the last insert had just written. With the alternate
//!   screen the leave *is* the hand-back — the user's main screen comes back
//!   exactly as it was, cursor included (ADR-0006) — so the anchor has no job
//!   and is gone.
//! * **the ordering dance.** `fit` → flush → `insert_before` → `draw`, in that
//!   order and for a reason at each step. A full-screen frame is `draw`.
//!
//! What is left is the part that was never about the pane's neighbours, which is
//! the part worth having: the *policy*.
//!
//! # The frame
//!
//! [`frame_areas`] tiles the whole window into five bands, top to bottom:
//!
//! 1. **the transcript** — everything this session has finished saying, plus the
//!    live tail of what it is saying now, pinned to the *bottom* of the band so
//!    the newest line sits directly above the chrome. This is the band that used
//!    to be the terminal's own scrollback; it is ours now, which is what
//!    looprs-pdl.6 (offsets, wheel and keyboard scroll) and looprs-pdl.7
//!    (bounded, journaled) get to build on.
//! 2. **the tool rows** — the live cards: open tool calls and any open
//!    compaction, capped at [`MAX_TOOL_ROWS`].
//! 3. **the kanban band** — the beads board (epic looprs-5o4): three columns
//!    sharing one header row, plus a footer row that is never omitted while the
//!    band is drawn (ADR-0007 §4, `docs/adr/0007-kanban-board.md`). How tall it
//!    gets is [`kanban_rows`], and it is **zero rows** in every frame that is
//!    not drawing it — see [`KanbanBudget::Off`], which is what the gate hands
//!    every non-beads frame (`crate::App::kanban_budget`, looprs-5o4.5).
//!    Reserving rows for a band nobody paints is a hole in the frame, not a
//!    band. What a reader sees in it, and the knobs that control it, are in
//!    `docs/kanban.md`; the column mapping itself is ADR-0007 §1 and is not
//!    repeated here.
//! 4. **the status row** — one row, [`STATUS_ROWS`], see
//!    [`crate::components::status`].
//! 5. **the input box** — [`NO_INPUT_ROWS`] when the active session is not
//!    taking the keyboard, otherwise as tall as the typed text needs, up to
//!    [`MAX_INPUT_ROWS`].
//!
//! The **priority order is the box first**: [`bands`] resolves the two variable
//! bands against the window and the tool wall gives rows back before the input
//! box ever is, because a preview row you are not typing into is worth less
//! than a row of the question you are. The transcript band is the frame's
//! `Constraint::Min` and absorbs whatever the ladder did not spend, with a
//! floor of [`MIN_TEXT_ROWS`] so a wall of tools cannot delete the transcript.
//!
//! **The board is last on that ladder** (looprs-5o4.3). It is paid out of the
//! transcript's *surplus* — the rows above [`MIN_TEXT_ROWS`] that no one else
//! claimed — and out of nothing else, so it can never take a row off the box,
//! the cards, or the transcript's floor. That is why [`kanban_rows`] runs
//! *after* [`bands`] and only ever sees the leftover: "shrink the input box so
//! the board can show three more closed tickets" is not a thing this frame can
//! be asked to do, let alone do. Its own floor is the reason it returns 0
//! rather than a stub at the bottom of a short window, and its own ceiling is
//! the reason a 200-row terminal does not turn into a board with a strip of
//! transcript under it.
//!
//! The frame is a pure function of the area it is given. There is no
//! "rows of scrollback to keep visible" and no absolute cap on the pane, because
//! there is no scrollback above the frame to keep visible and no pane below it
//! to leave room for: on a 120-row window the transcript band is 120 minus its
//! chrome, and that is the whole answer.
//!
//! # Adding a band
//!
//! Three places, and none of them is terminal-shaped:
//!
//! 1. add its row count to [`bands`] and say what it outranks when the window is
//!    too short (the ladder is now the box > the cards > the transcript's
//!    floor > the board; a band that ranks below the transcript's floor is
//!    paid the way [`kanban_rows`] pays it, from the surplus, and is not in
//!    [`bands`] at all);
//! 2. add one `Constraint::Length` to the [`Layout`] in [`frame_areas`], in the
//!    order the band sits in, leave the transcript band's `Constraint::Min`
//!    absorbing the rest, and add the matching field to [`FrameAreas`] — the
//!    named struct is the thing that stops a draw site from guessing which band
//!    it is painting into, and the compiler reads a new field at every site
//!    that has to decide what to do with it;
//! 3. draw into the new [`Rect`] in [`crate::view`], from state that the frame
//!    was already handed — a band whose contents are re-derived at draw time is
//!    a second opinion about what the frame is showing, which is the one thing
//!    [`crate::session::view`] exists to prevent.
//!
//! That is the list of *code*. There is no fourth step about where the band sits
//! on the real screen, because there is nothing above it, below it, or beside
//! it — but a band the operator can see, or turn off, owes a line in the docs:
//! its knobs go with the others that already have a home (`docs/kanban.md` for
//! the board's, and the ADR that decided it), not onto a second list that will
//! disagree with the first.

use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

/// The status row. `Constraint::Length(1)` in [`frame_areas`], and the same
/// number in the ladder in [`bands`] — the two must be one constant or the
/// frame grows a row of blank space that nobody can explain.
pub const STATUS_ROWS: u16 = 1;
/// The input box's two border rows. Its text lives between them, so a box
/// showing `n` rows of text costs the frame `n + INPUT_BORDER_ROWS` rows.
pub const INPUT_BORDER_ROWS: u16 = 2;
/// The least text the box ever shows. An empty box is still a box with a caret
/// in it, and this is also what gets reserved when the box is not drawn at all.
pub const MIN_INPUT_TEXT_ROWS: u16 = 1;
/// The most text the box is allowed to show at once.
///
/// The cap is load-bearing, not cosmetics: the input box and the transcript
/// compete for the *same* window, so an uncapped box turns a long paste into a
/// frame that is all question and no answer. Past the cap the box scrolls to
/// keep the caret visible rather than growing (see
/// [`crate::components::input::InputState::display_lines`]).
pub const MAX_INPUT_TEXT_ROWS: u16 = 6;
/// The box height for one line of text — what an empty box costs, and what a
/// policy call that is not interested in the typed text passes as `input_rows`
/// when the box is on screen.
pub const MIN_INPUT_ROWS: u16 = INPUT_BORDER_ROWS + MIN_INPUT_TEXT_ROWS;
/// No box at all: the input band is granted zero rows, so the status row comes
/// down and sits on the bottom edge of the frame.
///
/// A mode that hides the box (an agentic session that has taken the keyboard —
/// see [`crate::session::view::SessionView::accepts_input`]) passes this for
/// the same reason a mode that shows it passes its measured height: the box's
/// rows are the frame's to spend, and "how tall is the box" is one question
/// with one answer per frame. Reserving three rows for a box nobody drew left
/// the status row floating above a band of blank screen.
pub const NO_INPUT_ROWS: u16 = 0;
/// The tallest the input box can ever be.
pub const MAX_INPUT_ROWS: u16 = INPUT_BORDER_ROWS + MAX_INPUT_TEXT_ROWS;
/// Concurrent tool calls shown at once (was: `tools.take(4)` in `main::view`).
pub const MAX_TOOL_ROWS: u16 = 4;
/// The transcript band never shrinks below this.
///
/// Without a floor of its own the band is `Min(0)` and a full wall of cards on
/// a short window can take the transcript away entirely — which is the same
/// class of bug as the input box having no cap: one band winning an argument it
/// should have lost.
pub const MIN_TEXT_ROWS: u16 = 1;

/// The kanban band's header row: the three column names and their true totals,
/// **shared** across the columns (ADR-0007 §4), so three side-by-side columns
/// cost one row of header between them, not three.
///
/// It is a row of the *band*, which means the budget pays for it whether or not
/// a single bead is drawn under it.
pub const KANBAN_HEADER_ROWS: u16 = 1;
/// The kanban band's footer row: "when was this read", plus the `deferred`
/// count that has no row of its own. **Never omitted while the band is drawn**
/// (ADR-0007 §4/S1), which is what makes it a cost rather than a leftover.
pub const KANBAN_FOOTER_ROWS: u16 = 1;
/// The least body a band can have and still be a board: one row of beads
/// between the header and the footer.
///
/// This is the **budget's** floor and the only one the frame knows about. The
/// component keeps a second, stricter floor of its own
/// ([`crate::components::kanban::MIN_KANBAN_BODY_ROWS_WITH_RULE`]) for whether it
/// can afford to spend a row on the rule under the header, and refuses the rule
/// rather than cut a bead row. Deliberately not a `KANBAN_*` constant here: the
/// rule is a rendering choice, and a budget constant with its own name would read
/// as something the frame hands out, which it does not.
pub const MIN_KANBAN_BODY_ROWS: u16 = 1;
/// The smallest thing that is still a board, and therefore the band's floor:
/// header + one bead row + footer.
///
/// A band that can only afford the header and the footer is **not** a board —
/// it is two lines of chrome with nothing between them — so the budget returns
/// `0` rather than rounding down into that. The ADR's "never a blank board"
/// rule is carried by the footer's text (`reading the board…`), not by drawing
/// a degenerate one.
pub const MIN_KANBAN_ROWS: u16 = KANBAN_HEADER_ROWS + KANBAN_FOOTER_ROWS + MIN_KANBAN_BODY_ROWS;
/// The most the band ever gets, however tall the window is.
///
/// Load-bearing in both directions. Without a ceiling the band is the transcript
/// at a different font: a tall window gives the board every row above the
/// transcript's floor, and a band with nothing better to do than grow spends
/// rows the one band that cannot be re-read elsewhere never gets back. With
/// this one, five rows past [`min_frame_rows_for_board`] the ceiling is reached
/// and every row of window after that goes to the transcript, where it was
/// always going to go.
///
/// Eight rows is five per column (`8 − header − footer − the band's rule row`),
/// which on this board is the whole live work — everything after that is
/// `+N more` over a tail that `bd list` answers in one keystroke, while the
/// transcript cannot be.
pub const MAX_KANBAN_ROWS: u16 = 8;

/// What the operator asked the band for, before the frame works out what it can
/// afford.
///
/// The whole of the "or just let me pin it" route (§C of the ticket). The point
/// of it being an argument and not an [`std::env::var`] inside
/// [`kanban_rows`] is that the policy has no environment: every branch of it is
/// reachable from a test without touching the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KanbanBudget {
    /// No board, at any window height. This is what a frame that is **not**
    /// drawing the band passes.
    ///
    /// It is not the same thing as `Pinned(0)`, though both grant nothing:
    /// `Off` is a fact about the frame (not beads mode, or nothing painted yet)
    /// that no environment variable can argue with, while a pin is a user
    /// request that the frame may still turn out not to be able to afford.
    Off,
    /// The height function decides, per frame. The default when the environment
    /// says nothing.
    Auto,
    /// `LOOPRS_KANBAN_ROWS=<n>`, already clamped into
    /// `[0, MAX_KANBAN_ROWS]` by [`KanbanBudget::from_raw`]. The frame treats
    /// it as a **ceiling on the request**, never as a guarantee: it still has
    /// to fit, and it still dies at the floor rather than drawing a stub.
    Pinned(u16),
}

impl KanbanBudget {
    /// Resolve the raw `LOOPRS_KANBAN_ROWS` value into a budget.
    ///
    /// `main` reads the environment once at startup and passes the value here
    /// (`from_raw(std::env::var("LOOPRS_KANBAN_ROWS").ok().as_deref())`), so
    /// the knob is read once, logged once, and never re-parsed per frame — and
    /// so that every rule below is a function of its argument rather than of
    /// the machine's env.
    ///
    /// * unset / empty / whitespace — [`KanbanBudget::Auto`], no comment.
    /// * `0` — the band is off entirely, which is the answer for a user who
    ///   does not want it at all and does not want the height function's
    ///   opinion either.
    /// * a number above `MAX_KANBAN_ROWS` — clamped to the ceiling, with a
    ///   warning, because a silently-ignored number is a number the user will
    ///   keep setting.
    /// * `1` or `2` — off, with a warning: below [`MIN_KANBAN_ROWS`] there is
    ///   no band to be had (header + footer leaves no row for a bead), so the
    ///   honest resolution is `0`, and the log says why rather than letting the
    ///   user debug a silent `1`.
    /// * unparseable — [`KanbanBudget::Auto`] with a warning. Never a panic,
    ///   and never "off": a typo should not take the feature away, it should
    ///   leave the default in place and say so loudly.
    pub fn from_raw(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            return Self::Auto;
        };
        let Ok(rows) = raw.parse::<u16>() else {
            tracing::warn!(
                "LOOPRS_KANBAN_ROWS={raw:?} is not a number of rows; using the height function \
                 (0 = off, 1..={max} = a fixed height)",
                max = MAX_KANBAN_ROWS
            );
            return Self::Auto;
        };
        if rows > MAX_KANBAN_ROWS {
            tracing::warn!(
                "LOOPRS_KANBAN_ROWS={rows} is above the ceiling; clamped to {MAX_KANBAN_ROWS}"
            );
            return Self::Pinned(MAX_KANBAN_ROWS);
        }
        if rows > 0 && rows < MIN_KANBAN_ROWS {
            tracing::warn!(
                "LOOPRS_KANBAN_ROWS={rows} is below the smallest band that is a board \
                 ({MIN_KANBAN_ROWS} rows: header + one bead row + footer); the band is off"
            );
            return Self::Pinned(0);
        }
        Self::Pinned(rows)
    }
}

/// The rows the kanban band can afford in a frame this tall.
///
/// A pure function of `(frame_rows, tool_rows, input_rows, budget)` — no env
/// read, no clock read, no remembered window size. `tool_rows` and `input_rows`
/// are the *requested* heights, exactly as [`bands`] takes them, and are
/// resolved through [`bands`] here rather than re-derived, so the number this
/// function subtracts is the number the frame is going to spend.
///
/// The order of subtraction **is** the ladder: the box is taken at full price,
/// the cards are taken at full price, the transcript's [`MIN_TEXT_ROWS`] floor
/// is reserved, and the band gets what is left, capped at
/// [`MAX_KANBAN_ROWS`]. It never asks for anything to be given back.
///
/// Returns `0` — no band — rather than a stub whenever the leftover cannot hold
/// [`MIN_KANBAN_ROWS`], and never returns anything between `0` and that floor,
/// so the transition at the threshold is 0 → a real band and there is no frame
/// that draws half of one.
///
/// The result never adds demand to the frame: `kanban_rows(...) + tools +
/// input + STATUS_ROWS + MIN_TEXT_ROWS <= frame_rows` wherever
/// `tools + input + STATUS_ROWS + MIN_TEXT_ROWS <= frame_rows` already held,
/// which is what lets [`frame_areas`] lay the band out as a plain
/// `Constraint::Length` without risking a run past the bottom edge. Where the
/// frame was already too short for the box alone, the band is 0 rather than one
/// more claim on rows that do not exist.
pub fn kanban_rows(frame_rows: u16, tool_rows: u16, input_rows: u16, budget: KanbanBudget) -> u16 {
    if budget == KanbanBudget::Off {
        return 0;
    }
    // The higher bands are paid first, at the same prices `frame_areas` pays
    // them. Nothing here can move those numbers.
    let (tools, input) = bands(tool_rows, input_rows, frame_rows);
    let surplus = frame_rows.saturating_sub(STATUS_ROWS + tools + input + MIN_TEXT_ROWS);
    let want = match budget {
        KanbanBudget::Off => 0,
        KanbanBudget::Auto => MAX_KANBAN_ROWS,
        // A pin is clamped here too, not only in `from_raw`: the ceiling is a
        // property of the band, not of the environment variable that can set
        // it.
        KanbanBudget::Pinned(n) => n.min(MAX_KANBAN_ROWS),
    };
    if want < MIN_KANBAN_ROWS || surplus < MIN_KANBAN_ROWS {
        return 0;
    }
    want.min(surplus)
}

/// The window height below which the band is `0` for this chrome — the
/// threshold the band appears at when the window grows, and disappears below
/// when it shrinks.
///
/// `STATUS_ROWS + tools + input + MIN_TEXT_ROWS + MIN_KANBAN_ROWS`: everything
/// the higher bands keep, plus the smallest thing that is still a board. Equal
/// to [`kanban_rows`] returning non-zero for [`KanbanBudget::Auto`] (and for
/// any pin at or above [`MIN_KANBAN_ROWS`]) at every window height.
pub fn min_frame_rows_for_board(tool_rows: u16, input_rows: u16) -> u16 {
    // Resolved against a window that cannot constrain them, because this is the
    // height at which the *board* becomes affordable, not the height at which
    // the other bands get cut to make room for it — cutting them is precisely
    // what this band does not do.
    let (tools, input) = bands(tool_rows, input_rows, u16::MAX);
    STATUS_ROWS
        .saturating_add(tools)
        .saturating_add(input)
        .saturating_add(MIN_TEXT_ROWS)
        .saturating_add(MIN_KANBAN_ROWS)
}

/// A requested input-box height, clamped into the range the frame supports.
///
/// Every entry point that takes an `input_rows` runs it through this, so the
/// ladder and the layout cannot be handed two different answers for the same
/// request.
///
/// [`NO_INPUT_ROWS`] (`0`) is the one value that bypasses the clamp, because it
/// is not "a small box", it is "no box": the band is granted nothing rather
/// than being rounded up to a box that will not be drawn. Note this makes `0`
/// special in a way no other value is — a caller who means "one-line box, I did
/// not measure" must pass [`MIN_INPUT_ROWS`], which is what
/// [`crate::App::input_rows`] returns for empty text.
fn clamped_input_rows(rows: u16) -> u16 {
    if rows == NO_INPUT_ROWS {
        return NO_INPUT_ROWS;
    }
    rows.clamp(MIN_INPUT_ROWS, MAX_INPUT_ROWS)
}

/// The box rows for `text_rows` of wrapped input text.
pub fn input_rows(text_rows: u16) -> u16 {
    clamped_input_rows(INPUT_BORDER_ROWS.saturating_add(text_rows))
}

/// How the two variable bands resolve themselves against a frame this tall.
///
/// The layout and every test of it go through this, with the same numbers, so
/// "who gives way when the window is short" is decided in exactly one place.
/// The priority is the box first: the input box is taken at its requested
/// height and the tool rows are cut down to whatever is left, because a preview
/// row you are not typing into is worth less than a row of the question you
/// are. The transcript band keeps [`MIN_TEXT_ROWS`] for the same reason.
///
/// The result is *always* spendable: `tools + input + STATUS_ROWS +
/// MIN_TEXT_ROWS <= frame_rows`, so [`frame_areas`] never runs a fixed band
/// past the bottom edge of the area it was given.
///
/// The kanban band is deliberately **not** in here. It ranks below every band
/// this function resolves, so it must not be part of what they are measured
/// against: adding a term for it here is exactly how "the board shrank my input
/// box" becomes representable. It is granted afterwards, out of the surplus
/// this function's floor leaves behind — see [`kanban_rows`].
pub fn bands(tool_rows: u16, input_rows: u16, frame_rows: u16) -> (u16, u16) {
    let input = clamped_input_rows(input_rows);
    let tools = tool_rows
        .min(MAX_TOOL_ROWS)
        .min(frame_rows.saturating_sub(STATUS_ROWS + input + MIN_TEXT_ROWS));
    (tools, input)
}

/// The frame's five bands, top to bottom, **named**.
///
/// A struct rather than a `[Rect; N]` because the bands are five
/// differently-behaved things, not N of one thing: one is a `Min` that absorbs
/// the remainder, one is a wall, one is zero outside a single mode, one is a
/// fixed row and one is a box that grows. A tuple makes every one of those
/// differences invisible at the call site and encodes the position of each band
/// in every destructuring pattern, so adding or reordering a band is a
/// find-and-replace across the crate that the compiler can only check one index
/// at a time — and `[Rect; 5]` indexing (`small[4]`) says nothing at all about
/// which band it is.
///
/// With fields, the compile-checked thing is the *name*: `areas.status` cannot
/// silently become the input box, and a band added to the struct shows up as a
/// missing field at every site that must decide what to do with it.
///
/// The field order is the draw order, top to bottom, so reading a
/// `FrameAreas` value reads the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameAreas {
    /// Band 1: the transcript, and the frame's `Constraint::Min`.
    pub transcript: Rect,
    /// Band 2: the live cards (tool calls, open compaction).
    pub cards: Rect,
    /// Band 3: the beads board. **Zero rows** in every frame that is not
    /// drawing it — see [`KanbanBudget::Off`].
    pub kanban: Rect,
    /// Band 4: the status row.
    pub status: Rect,
    /// Band 5: the input box. [`NO_INPUT_ROWS`] when the session took the
    /// keyboard.
    pub input: Rect,
}

/// The frame's five bands, top to bottom: transcript, tool rows, kanban,
/// status, input.
///
/// The area it is handed is the whole window: the frame tiles what it is given,
/// and nothing else. Resolved through [`bands`] and [`kanban_rows`] against
/// *this* area rather than a remembered terminal size, so the same arithmetic
/// that decided who gives way is the arithmetic that spends the rows — anything
/// that made the two disagree would show up as blank rows inside the frame,
/// which is a bug nobody can trace from a screenshot.
///
/// The kanban band's height is whatever `kanban` buys: [`kanban_rows`] returns
/// `0` for [`KanbanBudget::Off`], and a zero-length `Constraint::Length(0)` is
/// a band that occupies no row at all, which is what keeps every other band
/// exactly where it was before the band existed. The *gate* — beads mode only —
/// is not this function's call; it lives with the mode
/// ([`crate::App::kanban_budget`]), and this function is given the answer.
pub fn frame_areas(
    area: Rect,
    tool_rows: u16,
    input_rows: u16,
    kanban: KanbanBudget,
) -> FrameAreas {
    let (tools, input) = bands(tool_rows, input_rows, area.height);
    let board = kanban_rows(area.height, tool_rows, input_rows, kanban);
    let [transcript, cards, kanban, status, input] = Layout::vertical([
        Constraint::Min(MIN_TEXT_ROWS),
        Constraint::Length(tools),
        Constraint::Length(board),
        Constraint::Length(STATUS_ROWS),
        Constraint::Length(input),
    ])
    .areas(area);
    FrameAreas {
        transcript,
        cards,
        kanban,
        status,
        input,
    }
}

/// The whole window, owned outright: the `Terminal` under the frame.
///
/// This is the whole amount of *mechanics* the screen needs once nothing has to
/// be asked of the terminal. Three operations: draw a frame, ask how big the
/// window is (an `ioctl`, not a round trip), and reclaim the canvas after a
/// full-screen child painted over it. There is no height to negotiate, no
/// anchor to publish, no stream to stop, and no erase that has to be justified
/// before it is allowed to run.
///
/// The type exists over a bare `Terminal` for two reasons: to keep the viewport
/// choice (`Viewport::Fullscreen`) in one place instead of at every call site,
/// and to give [`Self::repaint_all`] a name — it is the one operation here that
/// is not obvious from ratatui's API, and the one a contributor is most likely
/// to get wrong by reaching for `Terminal::clear()`.
pub struct ScreenFrame<B: Backend> {
    term: Terminal<B>,
}

impl<B: Backend> ScreenFrame<B> {
    /// Take the whole window.
    ///
    /// Writes nothing at construction time: unlike the inline viewport, a
    /// full-screen one does not need to know where the cursor is, so building
    /// this cannot block on a terminal that is not answering. The alternate
    /// screen itself is not switched here — that is a ledgered mode
    /// ([`crate::teardown::Mode::AltScreen`]) and goes on before this, so that
    /// every byte this type writes lands on the screen we own.
    pub fn full(backend: B) -> Result<Self, B::Error> {
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .map(|term| Self { term })
    }

    /// Draw one frame.
    ///
    /// ratatui's `draw` resizes itself first if the window changed
    /// (`autoresize`), which for a full-screen viewport is a size `ioctl` and a
    /// buffer reallocation — no cursor query, so the key stream never has to
    /// stop for it. The frame area is the whole window.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> Result<(), B::Error> {
        self.term.draw(render).map(|_| ())
    }

    /// The real window, from the backend. An `ioctl`; nothing is asked of the
    /// terminal's parser, so nothing can time out.
    pub fn size(&self) -> Result<Size, B::Error> {
        self.term.size()
    }

    /// Forget what the terminal is showing and repaint the whole thing.
    ///
    /// Needed exactly once per full-screen handover: the child drew over cells
    /// that our back buffer still believes are ours, and the diff is a promise
    /// about a screen that no longer exists. Trusting it is ADR-0001's "screen
    /// is garbled after exiting vim" bug.
    ///
    /// This is deliberately *not* [`Terminal::clear`], which is the obvious
    /// call and the wrong one: `clear` starts by asking the terminal where the
    /// cursor is (`ESC[6n`), and the async key reader is parked on that same
    /// stdin and eats the answer. `resize` to the size we already have does the
    /// same job through the full-screen branch — `ESC[2J` plus a reset back
    /// buffer, which forces the next `draw` to repaint every cell — and asks
    /// nothing of anybody.
    pub fn repaint_all(&mut self) -> Result<(), B::Error> {
        let area: Rect = self.term.size()?.into();
        self.term.resize(area)
    }

    #[allow(dead_code)] // test seam: the mechanics tests read the screen this type writes
    pub fn backend(&self) -> &B {
        self.term.backend()
    }

    #[allow(dead_code)] // test seam: the reclaim is only observable by painting over the screen behind the frame's back buffer
    pub fn backend_mut(&mut self) -> &mut B {
        self.term.backend_mut()
    }
}

/// Adopts the real window from the size `ioctl` once per tick, as a backstop to
/// the `Event::Resize` path (looprs-pdl.15).
///
/// # The hole this closes
///
/// "The user drags the window while nothing is happening and the transcript
/// re-wraps" used to have exactly one delivery channel, and every link in it is
/// outside this app: `SIGWINCH` → crossterm's `EventStream` → `Event::Resize`
/// → [`crate::App::set_window`]. The first link is the load-bearing one, and
/// it is not a promise about *us*: `SIGWINCH` is not delivered to the process
/// that is drawing on the terminal, it is delivered to the **foreground
/// process group of the terminal's session** (`tty_ioctl(4)`). A process
/// drawing into a pty it never made its controlling terminal — a piped
/// harness, a test driver, a child of a script that forgot `setsid` — is not
/// in that group, and hears nothing at all. Measured in
/// `spikes/resize_e2e.py` (groups `untouched` and `attached`): the same idle `TIOCSWINSZ` costs
/// **0 bytes** with no controlling terminal and **~1.2 KB** with one, on the
/// same binary. The ticket this closes was filed off the zero, and the zero was
/// never about the app's resize handling — which was, and is, correct.
///
/// # Why the poll is the fix, and what it costs
///
/// `TIOCGWINSZ` needs none of the process-group relationship. It asks about
/// the file descriptor this process already holds, so it reports the new window
/// whether or not the signal arrived, was coalesced, or was swallowed by a
/// stream we do not control. Asking once per ~16 ms tick is one syscall of
/// about a microsecond — the *same* call ratatui's `autoresize` makes on every
/// draw, so the worst case added here is one extra `ioctl` per frame already
/// painted. The event stays as the fast path; the poll is what turns a missed
/// signal from "a stale frame until the next keystroke" into sixteen
/// milliseconds.
///
/// It also reaches what the event alone never could during a passthrough: a
/// full-screen child lives on a *different* pty, so nothing resizes the child's
/// window but [`crate::App::forward_resize`] does. A drag with no `SIGWINCH`
/// left vim inside looprs at a size the user was not using — `spikes/resize_e2e.py`
/// group `held` asks the child itself, with `stty size`, and it agrees.
///
/// # The two guards, each of which is the whole reason to own this type
///
/// * **one unreadable size stops the poll, permanently.** crossterm's
///   `terminal::size()` falls back to spawning `tput` when the `ioctl` fails
///   (`crossterm::sys::unix::terminal::size`), and its own source comments
///   that this "can take a really long time" — from an event loop's point of
///   view. Polling that is a subprocess every sixteen milliseconds. A tty that
///   cannot answer a size query once is not going to answer it later, so the
///   poll gives up on the first error, says so once in the log, and gets out
///   of the way.
/// * **a degenerate size is never adopted.** `0 x 0` is what a pty reports
///   while it is being torn down; adopting it would forward a zero-sized
///   window to every child pty and lay the frame out on nothing. The event
///   path has always had its own exposure there; this path does not add to it.
#[derive(Debug, Default)]
pub struct WindowPoll {
    /// A read failed and the poll is over. `false` = still asking.
    stopped: bool,
}

impl WindowPoll {
    /// A poll that has not asked yet, and so has not failed yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// `read` reports the window the terminal actually has (in practice
    /// `|| frame.size()`); `adopted` is the window this app is currently
    /// wrapping for.
    ///
    /// `Some(size)` means "this is the window now" — the caller adopts it the
    /// same way it adopts `Event::Resize`, and only the difference between
    /// `size` and `adopted` makes that worth doing. `None` means the poll
    /// agrees with the app, saw nothing worth adopting, or has stopped.
    pub fn poll<F>(&mut self, read: F, adopted: Size) -> Option<Size>
    where
        F: FnOnce() -> std::io::Result<Size>,
    {
        if self.stopped {
            return None;
        }
        let size = match read() {
            Ok(size) => size,
            Err(e) => {
                self.stopped = true;
                tracing::warn!(
                    "window poll stopped after a failed size query ({e}); \
                     a resize now repaints only if SIGWINCH reaches us"
                );
                return None;
            }
        };
        if size.width == 0 || size.height == 0 {
            // Torn down, not resized. Nothing to lay a frame out in.
            tracing::debug!("ignoring the degenerate window {size:?}");
            return None;
        }
        (size != adopted).then_some(size)
    }

    /// Has the poll given up? `true` means every resize from here is riding on
    /// the event stream alone — which is what the log says out loud when it
    /// happens, and what the test below checks it said.
    #[allow(dead_code)] // diagnostic seam: read by the test that proves the poll retires itself
    pub fn stopped(&self) -> bool {
        self.stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::{ClearType, TestBackend, WindowSize};
    use ratatui::buffer::{Cell, CellWidth};
    use ratatui::layout::Position;
    use ratatui::text::Line;
    use ratatui::widgets::Widget;

    const W: usize = 24;

    /// Every budget the frame can be handed, for the table tests: off, the
    /// height function, a pin that disables, a pin at the smallest real band,
    /// and a pin at the ceiling.
    const BUDGETS: [KanbanBudget; 5] = [
        KanbanBudget::Off,
        KanbanBudget::Auto,
        KanbanBudget::Pinned(0),
        KanbanBudget::Pinned(MIN_KANBAN_ROWS),
        KanbanBudget::Pinned(MAX_KANBAN_ROWS),
    ];

    /// A subscriber that keeps the text of every `warn!` (or worse) raised
    /// while it is installed.
    ///
    /// It exists because "an unparseable value falls back **with a logged
    /// warning**" is half of the knob's contract, and the other half is worth
    /// as much on its own: a fallback nobody can see is a knob the user keeps
    /// turning. Capturing the event rather than reading a log file keeps the
    /// assertion in the same place as the decision it describes, and thread-
    /// local, so it cannot pick up another test's noise.
    #[derive(Debug, Default, Clone)]
    struct WarnCapture {
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl WarnCapture {
        /// Run `f` with this subscriber as the thread's only listener, and hand
        /// back what it was told at WARN or above.
        fn record<T>(f: impl FnOnce() -> T) -> Vec<String> {
            let cap = Self::default();
            let lines = cap.lines.clone();
            let _guard = tracing::subscriber::set_default(cap);
            f();
            lines.lock().unwrap().clone()
        }
    }

    impl tracing::Subscriber for WarnCapture {
        fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
            *meta.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            // Nothing here is span-aware; one throwaway id is enough to keep
            // `span!` calls from having anywhere to be recorded *to*.
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() > tracing::Level::WARN {
                return;
            }
            struct Grab<'a>(&'a mut Vec<String>);
            impl tracing::field::Visit for Grab<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0.push(format!("{value:?}"));
                    }
                }
            }
            event.record(&mut Grab(&mut self.lines.lock().unwrap()));
        }
    }

    // ---------------------------------------------------------------- policy

    /// The whole table of requests, against every window height: the five bands
    /// tile the frame exactly, in order, with no gaps and no overlap, and no
    /// band ever runs past the bottom edge — with the board off, on, pinned at
    /// the floor, and pinned at the ceiling.
    #[test]
    fn the_bands_tile_the_window_at_every_size() {
        for mode_rows in 0u16..=120 {
            for tools in [0u16, 1, 4, 9] {
                for input in [MIN_INPUT_ROWS, MAX_INPUT_ROWS, NO_INPUT_ROWS] {
                    for budget in BUDGETS {
                        let area = Rect::new(0, 0, W as u16, mode_rows);
                        let FrameAreas {
                            transcript: text,
                            cards: card,
                            kanban: board,
                            status,
                            input: box_band,
                        } = frame_areas(area, tools, input, budget);
                        let tag = format!(
                            "rows={mode_rows} tools={tools} input={input} board={budget:?}"
                        );

                        assert_eq!(
                            text.height as u32
                                + card.height as u32
                                + board.height as u32
                                + status.height as u32
                                + box_band.height as u32,
                            mode_rows as u32,
                            "{tag}: the bands do not tile the frame"
                        );
                        assert_eq!(text.top(), 0, "{tag}");
                        assert_eq!(text.bottom(), card.top(), "{tag}: gap");
                        assert_eq!(card.bottom(), board.top(), "{tag}: gap");
                        assert_eq!(board.bottom(), status.top(), "{tag}: gap");
                        assert_eq!(status.bottom(), box_band.top(), "{tag}: gap");
                        assert_eq!(box_band.bottom(), mode_rows, "{tag}: off the edge");

                        // Above the affordability line every band gets exactly
                        // the rows the ladder counted. Below it the window
                        // cannot hold the frame that was asked for, and how
                        // ratatui's solver spreads the shortfall between the
                        // constraints is its business — the only promise is
                        // that nothing runs off the bottom edge, which the
                        // tiling above already covers.
                        let (want_tools, want_input) = bands(tools, input, mode_rows);
                        let want_board = kanban_rows(mode_rows, tools, input, budget);
                        let afford =
                            want_board + want_tools + want_input + STATUS_ROWS + MIN_TEXT_ROWS;
                        // The board may never add demand to the frame: it is
                        // paid out of what the ladder left over, so its rows
                        // are always fewer than the surplus. (The total is not
                        // always affordable — a 2-row window cannot hold a
                        // 3-row box, and that shortfall predates the board —
                        // but every row of it that *is* over is a row the
                        // higher bands asked for, not a row the board took.)
                        let spare = mode_rows
                            .saturating_sub(want_tools + want_input + STATUS_ROWS + MIN_TEXT_ROWS);
                        assert!(
                            want_board <= spare,
                            "{tag}: the board over-sold the frame ({want_board} > {spare})"
                        );
                        if mode_rows >= afford {
                            assert_eq!(
                                box_band.height, want_input,
                                "{tag}: the box was not paid what the ladder counted"
                            );
                            assert_eq!(
                                card.height, want_tools,
                                "{tag}: the tool band spent rows the ladder did not grant it"
                            );
                            assert_eq!(
                                board.height, want_board,
                                "{tag}: the board spent rows the budget did not grant it"
                            );
                            assert_eq!(status.height, STATUS_ROWS, "{tag}");
                            assert_eq!(
                                text.height,
                                mode_rows - afford + MIN_TEXT_ROWS,
                                "{tag}: the transcript band did not absorb the rest"
                            );
                        } else {
                            for band in [text, card, board, status, box_band] {
                                assert!(
                                    band.bottom() <= mode_rows,
                                    "{tag}: {band:?} runs past the frame"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// **"The board gives way first"** spelled as the frames it cannot cause.
    /// Turning the board on in a window that already has a box and a wall of
    /// cards moves exactly one band: the transcript, which pays out of its
    /// surplus. The box, the cards and the status row keep their rows and their
    /// positions, because a band that shrank the input box so it could show
    /// three more closed tickets is the failure this ticket exists to make
    /// unrepresentable.
    #[test]
    fn turning_the_board_on_only_ever_costs_the_transcript_its_surplus() {
        for total in 0u16..=120 {
            for tools in [0u16, 1, 4, 9] {
                for input in [MIN_INPUT_ROWS, MAX_INPUT_ROWS, NO_INPUT_ROWS] {
                    let area = Rect::new(0, 0, W as u16, total);
                    let FrameAreas {
                        transcript: text_off,
                        cards: cards_off,
                        kanban: board_off,
                        status: status_off,
                        input: box_off,
                    } = frame_areas(area, tools, input, KanbanBudget::Off);
                    let FrameAreas {
                        transcript: text_on,
                        cards: cards_on,
                        kanban: board_on,
                        status: status_on,
                        input: box_on,
                    } = frame_areas(area, tools, input, KanbanBudget::Auto);
                    let tag = format!("rows={total} tools={tools} input={input}");

                    assert_eq!(board_off.height, 0, "{tag}: Off still took rows");
                    assert_eq!(
                        box_on.height, box_off.height,
                        "{tag}: the box lost rows to the board"
                    );
                    assert_eq!(box_on.top(), box_off.top(), "{tag}: the box moved");
                    assert_eq!(
                        cards_on.height, cards_off.height,
                        "{tag}: the card wall lost rows to the board"
                    );
                    assert_eq!(
                        status_on.top(),
                        status_off.top(),
                        "{tag}: the status row moved"
                    );
                    assert_eq!(
                        text_off.height,
                        text_on.height + board_on.height,
                        "{tag}: the board was not paid out of the transcript's surplus"
                    );
                    assert!(
                        text_on.height >= MIN_TEXT_ROWS.min(total),
                        "{tag}: the board took the transcript's floor"
                    );
                }
            }
        }
    }

    /// The height function, end to end: nothing below the threshold, the
    /// smallest real band at it, the ceiling once the window can pay for it,
    /// and nothing between 0 and that minimum at any size — so the transition
    /// at the threshold is 0 → a whole band and never a frame with half one.
    #[test]
    fn the_band_is_zero_or_a_whole_board_never_a_stub() {
        for total in 0u16..=200 {
            for tools in [0u16, 1, 4, 9] {
                for input in [MIN_INPUT_ROWS, MAX_INPUT_ROWS, NO_INPUT_ROWS] {
                    for budget in [KanbanBudget::Auto, KanbanBudget::Pinned(MAX_KANBAN_ROWS)] {
                        let rows = kanban_rows(total, tools, input, budget);
                        assert!(
                            rows == 0 || rows >= MIN_KANBAN_ROWS,
                            "rows={total} tools={tools} input={input} {budget:?}: a {rows}-row \"board\" is header and footer with nothing between them"
                        );
                    }
                }
            }
        }
    }

    /// The threshold, named and checkable: below `min_frame_rows_for_board` the
    /// band is 0, at it the band is exactly the minimum board, and it never
    /// grows past the ceiling however tall the window gets.
    #[test]
    fn the_board_appears_at_its_threshold_and_stops_at_its_ceiling() {
        for tools in [0u16, 1, 4, 9] {
            for input in [MIN_INPUT_ROWS, MAX_INPUT_ROWS, NO_INPUT_ROWS] {
                let at = min_frame_rows_for_board(tools, input);
                let tag = format!("tools={tools} input={input} threshold={at}");
                assert_eq!(
                    kanban_rows(at.saturating_sub(1), tools, input, KanbanBudget::Auto),
                    0,
                    "{tag}: a board was drawn one row below the threshold"
                );
                assert_eq!(
                    kanban_rows(at, tools, input, KanbanBudget::Auto),
                    MIN_KANBAN_ROWS,
                    "{tag}: at the threshold the band is the smallest thing that is a board"
                );
                assert_eq!(
                    kanban_rows(at + MAX_KANBAN_ROWS, tools, input, KanbanBudget::Auto),
                    MAX_KANBAN_ROWS,
                    "{tag}: the band grew past its ceiling"
                );
                assert_eq!(
                    kanban_rows(400, tools, input, KanbanBudget::Auto),
                    MAX_KANBAN_ROWS,
                    "{tag}: a 400-row window bought more board, not more transcript"
                );
                // And the threshold is the threshold for every *height function*
                // answer: no window below it grants a band, none above it
                // grants zero.
                for total in 0u16..=400 {
                    let rows = kanban_rows(total, tools, input, KanbanBudget::Auto);
                    assert_eq!(
                        rows > 0,
                        total >= at,
                        "{tag}: rows={rows} at height {total} does not match the threshold"
                    );
                }
            }
        }
    }

    /// The band's height never goes *down* as the window grows — including with
    /// the pinned budgets, where a pin can be met and then held while the
    /// window keeps growing.
    #[test]
    fn the_band_is_monotonic_non_decreasing_in_window_height() {
        for tools in [0u16, 1, 4, 9] {
            for input in [MIN_INPUT_ROWS, MAX_INPUT_ROWS, NO_INPUT_ROWS] {
                for budget in BUDGETS {
                    let mut prev = kanban_rows(0, tools, input, budget);
                    for total in 1u16..=400 {
                        let now = kanban_rows(total, tools, input, budget);
                        assert!(
                            now >= prev,
                            "{budget:?} tools={tools} input={input}: the band shrank from {prev} to {now} rows as the window grew to {total}"
                        );
                        prev = now;
                    }
                }
            }
        }
    }

    /// A pinned band is the number the user asked for, in every window that can
    /// hold it; `0` is off everywhere; and a pin the frame cannot afford
    /// collapses to zero rather than to a stub or to a shoved status row.
    #[test]
    fn a_pinned_band_is_honoured_up_to_what_the_window_can_pay() {
        // A window with room: every legal pin lands exactly.
        for pin in MIN_KANBAN_ROWS..=MAX_KANBAN_ROWS {
            assert_eq!(
                kanban_rows(60, 0, MIN_INPUT_ROWS, KanbanBudget::Pinned(pin)),
                pin,
                "pin {pin} was not paid"
            );
        }
        // A pin above the ceiling *is* the ceiling, and the ceiling is what the
        // height function would have asked for anyway.
        assert_eq!(
            kanban_rows(200, 0, MIN_INPUT_ROWS, KanbanBudget::Pinned(u16::MAX)),
            MAX_KANBAN_ROWS
        );
        assert_eq!(
            kanban_rows(200, 4, MAX_INPUT_ROWS, KanbanBudget::Pinned(u16::MAX)),
            kanban_rows(200, 4, MAX_INPUT_ROWS, KanbanBudget::Auto)
        );
        // `0` disables at every window size, with any chrome.
        for total in 0u16..=120 {
            for tools in [0u16, 4, 9] {
                for input in [MIN_INPUT_ROWS, MAX_INPUT_ROWS, NO_INPUT_ROWS] {
                    assert_eq!(kanban_rows(total, tools, input, KanbanBudget::Pinned(0)), 0);
                }
            }
        }
        // A pin taller than the surplus is cut to the surplus, and the surplus
        // here is what the frame has left above the transcript's floor.
        let h = 30u16;
        assert_eq!(
            h - STATUS_ROWS - MIN_INPUT_ROWS - MIN_TEXT_ROWS,
            25,
            "the surplus this pin is priced against"
        );
        assert_eq!(
            kanban_rows(h, 0, MIN_INPUT_ROWS, KanbanBudget::Pinned(30)),
            MAX_KANBAN_ROWS,
            "and even then the ceiling is the last word"
        );
        assert_eq!(
            kanban_rows(h, 0, MIN_INPUT_ROWS, KanbanBudget::Pinned(4)),
            4,
            "a pin below the surplus is simply the pin"
        );
        // One row above the threshold the surplus is 4, so a pin of 6 is cut
        // to 4: the frame pays what it has and never takes the difference from
        // a band above it.
        assert_eq!(
            kanban_rows(
                min_frame_rows_for_board(0, MIN_INPUT_ROWS) + 1,
                0,
                MIN_INPUT_ROWS,
                KanbanBudget::Pinned(6)
            ),
            4,
            "a pin too tall for the window is cut to the window, not to the pin"
        );
    }

    /// The environment cannot reach inside the policy: the budget is an
    /// argument, so the same four numbers always produce the same rows. This is
    /// the same purity claim [`the_frame_is_a_pure_function_of_the_area_it_gets`]
    /// makes of the layout, made of the band that decides how tall the board
    /// is — `LOOPRS_KANBAN_ROWS` is resolved by `main` once, through
    /// [`KanbanBudget::from_raw`], and nothing in here reads a process value.
    #[test]
    fn the_band_is_a_pure_function_of_its_arguments() {
        for _ in 0..3 {
            assert_eq!(
                kanban_rows(40, 4, MAX_INPUT_ROWS, KanbanBudget::Auto),
                kanban_rows(40, 4, MAX_INPUT_ROWS, KanbanBudget::Auto)
            );
            assert_eq!(
                frame_areas(
                    Rect::new(0, 0, 80, 40),
                    4,
                    MIN_INPUT_ROWS,
                    KanbanBudget::Pinned(5)
                ),
                frame_areas(
                    Rect::new(0, 0, 80, 40),
                    4,
                    MIN_INPUT_ROWS,
                    KanbanBudget::Pinned(5)
                )
            );
        }
    }

    /// The whole `LOOPRS_KANBAN_ROWS` mapping, as a table over the string the
    /// environment holds — including the two failure modes the ticket names:
    /// an unparseable value falls back to the height function with a logged
    /// warning rather than a panic, and `0` is off, not "a one-row band".
    #[test]
    fn the_env_knob_resolves_onto_the_budget() {
        let unset: Option<&str> = None;
        assert_eq!(KanbanBudget::from_raw(unset), KanbanBudget::Auto);
        assert_eq!(KanbanBudget::from_raw(Some("")), KanbanBudget::Auto);
        assert_eq!(KanbanBudget::from_raw(Some("   ")), KanbanBudget::Auto);
        assert_eq!(KanbanBudget::from_raw(Some("0")), KanbanBudget::Pinned(0));
        assert_eq!(KanbanBudget::from_raw(Some(" 6 ")), KanbanBudget::Pinned(6));
        assert_eq!(
            KanbanBudget::from_raw(Some("8")),
            KanbanBudget::Pinned(MAX_KANBAN_ROWS)
        );
        assert_eq!(
            KanbanBudget::from_raw(Some("999")),
            KanbanBudget::Pinned(MAX_KANBAN_ROWS),
            "above the ceiling is clamped, not obeyed"
        );
        // Below the smallest board there is nothing to pin, so the honest
        // resolution is off — with a warning, which the next test checks.
        assert_eq!(KanbanBudget::from_raw(Some("1")), KanbanBudget::Pinned(0));
        assert_eq!(KanbanBudget::from_raw(Some("2")), KanbanBudget::Pinned(0));
        // Garbage is the default, never a panic and never "off".
        for bad in ["banana", "-3", "3.5", "8 rows", "0x4", "eight"] {
            assert_eq!(
                KanbanBudget::from_raw(Some(bad)),
                KanbanBudget::Auto,
                "{bad:?} must fall back to the height function"
            );
        }
    }

    /// The fallback *says* something. A silently ignored `LOOPRS_KANBAN_ROWS`
    /// is a knob the user keeps turning, and a silently clamped one is the
    /// same. Each refused value warns once; each obeyed value says nothing.
    #[test]
    fn the_env_knob_warns_when_it_refuses_an_answer() {
        let warns = WarnCapture::record(|| {
            KanbanBudget::from_raw(Some("banana"));
            KanbanBudget::from_raw(Some("-3"));
            KanbanBudget::from_raw(Some("999"));
            KanbanBudget::from_raw(Some("1"));
        });
        let joined = warns.join("\n");
        assert_eq!(
            warns.len(),
            4,
            "each refused value warns exactly once:\n{joined}"
        );
        for want in [
            "is not a number of rows",
            "above the ceiling",
            "below the smallest band",
        ] {
            assert!(joined.contains(want), "no warning said {want:?}:\n{joined}");
        }

        // And the values that are obeyed say nothing at all.
        let quiet = WarnCapture::record(|| {
            let _ = KanbanBudget::from_raw(None);
            for ok in ["0", "3", "6", "8", " 6 "] {
                let _ = KanbanBudget::from_raw(Some(ok));
            }
            for _ in 0..10 {
                let _ = kanban_rows(60, 4, MAX_INPUT_ROWS, KanbanBudget::Auto);
            }
        });
        assert!(
            quiet.is_empty(),
            "obeyed values warned:\n{}",
            quiet.join("\n")
        );
    }

    /// The priority order, spelled as a property: give the box more rows and
    /// every one of them comes out of the *tool* band, never off the box and
    /// never off the transcript's floor.
    #[test]
    fn the_input_box_outranks_the_tool_wall() {
        // A window that cannot fit both: the wall shrinks, the box does not.
        let tall = MAX_INPUT_ROWS;
        for total in (STATUS_ROWS + tall + MIN_TEXT_ROWS)..=40u16 {
            let FrameAreas {
                cards: card,
                input: box_band,
                ..
            } = frame_areas(Rect::new(0, 0, W as u16, total), 4, tall, KanbanBudget::Off);
            assert_eq!(
                box_band.height, tall,
                "rows={total}: the box was cut before the tool wall was"
            );
            assert!(card.height <= MAX_TOOL_ROWS, "rows={total}: {card:?}");
        }
        // And the ladder says the same thing in one call.
        assert_eq!(bands(4, tall, 12), (2, tall));
        assert_eq!(
            bands(4, MIN_INPUT_ROWS, 12),
            (4, MIN_INPUT_ROWS),
            "with a small box there was room for the whole wall"
        );
    }

    /// Caps, not stacking: more cards than `MAX_TOOL_ROWS` cost the same four
    /// rows, and more typed text than the cap costs the same box.
    #[test]
    fn neither_variable_band_grows_past_its_cap() {
        assert_eq!(bands(9, MIN_INPUT_ROWS, 100).0, MAX_TOOL_ROWS);
        assert_eq!(bands(u16::MAX, MIN_INPUT_ROWS, 100).0, MAX_TOOL_ROWS);
        for rows in [MAX_INPUT_TEXT_ROWS + 1, 40, u16::MAX] {
            assert_eq!(input_rows(rows), MAX_INPUT_ROWS, "input text rows={rows}");
        }
        assert_eq!(
            bands(4, input_rows(7), 60),
            bands(4, input_rows(40), 60),
            "7 rows of typed text and 40 of it ask the frame for the same thing"
        );
    }

    /// `0` means **no box**, not "an empty box": an empty box is still a box,
    /// and `input_rows` can never produce the hidden value because it adds the
    /// borders before clamping.
    #[test]
    fn a_hidden_band_is_no_rows_while_an_empty_box_is_still_a_box() {
        assert_eq!(NO_INPUT_ROWS, 0);
        assert_eq!(input_rows(0), MIN_INPUT_ROWS, "empty text is still a box");
        assert_eq!(bands(0, NO_INPUT_ROWS, 60), (0, 0));
        // A too-short request is a box, so it is rounded up — only `0` hides.
        assert_eq!(bands(0, 1, 60), (0, MIN_INPUT_ROWS));
    }

    /// With the box hidden the status row is the frame's last row — at every
    /// window size and with any tool wall — because a status row is load-bearing
    /// exactly when a session has taken the keyboard away.
    #[test]
    fn a_hidden_input_box_leaves_the_status_row_on_the_bottom_edge() {
        for total in 1u16..=60 {
            for tools in [0u16, 1, 4, 9] {
                let FrameAreas {
                    cards: card,
                    status,
                    input: box_band,
                    ..
                } = frame_areas(
                    Rect::new(0, 0, W as u16, total),
                    tools,
                    NO_INPUT_ROWS,
                    KanbanBudget::Off,
                );
                let tag = format!("rows={total} tools={tools}");
                assert_eq!(box_band.height, 0, "{tag}: a hidden box still took rows");
                assert_eq!(
                    box_band.top(),
                    total,
                    "{tag}: the band is not empty at the bottom"
                );
                assert_eq!(
                    status.bottom(),
                    total,
                    "{tag}: the row does not end the frame"
                );
                assert!(card.height <= MAX_TOOL_ROWS, "{tag}");
            }
        }
    }

    /// The transcript band keeps its floor whatever the chrome is asking for, so
    /// a wall of cards on a short window can take the transcript away.
    #[test]
    fn the_transcript_band_keeps_its_floor_against_the_chrome() {
        for total in (STATUS_ROWS + MIN_INPUT_ROWS + MIN_TEXT_ROWS)..=60u16 {
            let FrameAreas {
                transcript: text, ..
            } = frame_areas(
                Rect::new(0, 0, W as u16, total),
                MAX_TOOL_ROWS,
                MAX_INPUT_ROWS,
                KanbanBudget::Off,
            );
            assert!(
                text.height >= MIN_TEXT_ROWS.min(total),
                "rows={total}: the live text lost its floor ({} rows)",
                text.height
            );
        }
    }

    /// The box is never the thing that gets cut. Growing it from three rows to
    /// eight in the same window takes five rows out of the frame and none off the
    /// box.
    ///
    /// Which band they come out of is the ladder's business, and the ladder says:
    /// the **wall** gives way first when the wall is what cannot fit (that half
    /// is [`the_input_box_outranks_the_tool_wall`]), and the **transcript**
    /// absorbs everything the ladder did not spend — down to, but never below,
    /// [`MIN_TEXT_ROWS`].
    #[test]
    fn a_growing_box_is_never_cut_and_never_takes_the_transcripts_floor() {
        let total = 20u16;
        let small = frame_areas(
            Rect::new(0, 0, W as u16, total),
            4,
            MIN_INPUT_ROWS,
            KanbanBudget::Off,
        );
        let big = frame_areas(
            Rect::new(0, 0, W as u16, total),
            4,
            MAX_INPUT_ROWS,
            KanbanBudget::Off,
        );
        assert_eq!(small.input.height, MIN_INPUT_ROWS);
        assert_eq!(
            big.input.height, MAX_INPUT_ROWS,
            "the box got every row it asked for"
        );
        assert_eq!(
            small.input.bottom(),
            big.input.bottom(),
            "the frame did not grow to fit the box: the rows came from above"
        );
        assert!(
            big.transcript.height >= MIN_TEXT_ROWS,
            "the transcript lost its floor to the box: {} rows",
            big.transcript.height
        );
        // With the wall affordable, the transcript is what pays — which is the
        // difference between "the transcript absorbs the rest" and "the wall
        // always pays", and only one of them is the ladder.
        assert_eq!(
            big.cards.height, small.cards.height,
            "the wall kept its rows because it could afford to"
        );
        assert!(
            big.transcript.height < small.transcript.height,
            "nobody paid for the box: {} -> {}",
            small.transcript.height,
            big.transcript.height
        );
    }

    /// Nothing here consults a remembered terminal size: the frame is a pure
    /// function of the area it is handed. Two identical areas must produce
    /// identical bands, whatever happened to the window in between.
    #[test]
    fn the_frame_is_a_pure_function_of_the_area_it_gets() {
        let a = Rect::new(0, 0, 80, 40);
        for _ in 0..3 {
            assert_eq!(
                frame_areas(a, 2, MIN_INPUT_ROWS, KanbanBudget::Off),
                frame_areas(a, 2, MIN_INPUT_ROWS, KanbanBudget::Off)
            );
        }
        // And the same request at a different window is a different frame — the
        // transcript band absorbs the difference, on its own, because there is
        // no margin above it to keep visible and no pane below it to save room
        // for. A 120-row window spends all 120.
        let big = Rect::new(0, 0, 80, 120);
        let FrameAreas {
            transcript: big_text,
            ..
        } = frame_areas(big, 0, MIN_INPUT_ROWS, KanbanBudget::Off);
        assert_eq!(
            big_text.height,
            120 - STATUS_ROWS - MIN_INPUT_ROWS,
            "a tall window is all transcript, not a capped pane"
        );
    }

    // ------------------------------------------------------------- mechanics
    //
    // The screen itself. `TestBackend` models the visible screen but remembers
    // nothing about what was *asked* of it, and every claim worth making here is
    // a claim about what was asked: nobody queried the cursor, and the one clear
    // that does happen is the reclaim.

    fn rows_trimmed(b: &TestBackend) -> Vec<String> {
        let w = b.buffer().area.width.max(1) as usize;
        b.buffer()
            .content
            .chunks(w)
            .map(|row| {
                let mut s = String::new();
                let mut skip = 0usize;
                for c in row {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    s.push_str(c.symbol());
                    skip = c.cell_width().saturating_sub(1) as usize;
                }
                s.trim_end().to_string()
            })
            .collect()
    }

    /// A `TestBackend` that keeps the two counters the frame is measured on.
    #[derive(Debug)]
    struct Probe {
        inner: TestBackend,
        /// `get_cursor_position` calls: an `ESC[6n` round trip, the thing this
        /// whole migration exists to stop needing.
        asked_cursor: usize,
        /// Whole-screen clears (`ClearType::All`, i.e. `ESC[2J`) — what the
        /// reclaim writes, and what a resize writes.
        cleared_all: usize,
    }

    impl Probe {
        fn new(w: u16, h: u16) -> Self {
            Self {
                inner: TestBackend::new(w, h),
                asked_cursor: 0,
                cleared_all: 0,
            }
        }

        /// Somebody else painted over the visible screen behind our back.
        ///
        /// Written through the backend's own `draw` because that is what a
        /// full-screen child does: it puts cells on the screen without asking
        /// the frame, and the frame's back buffer has no idea.
        fn scribble(&mut self) {
            let area = self.inner.buffer().area;
            let mut cell = Cell::default();
            cell.set_symbol("#");
            let cells = area.positions().map(|p| (p.x, p.y, &cell));
            self.inner.draw(cells).unwrap();
        }
    }

    impl Backend for Probe {
        type Error = <TestBackend as Backend>::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.draw(content)
        }
        fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
            self.inner.append_lines(n)
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.asked_cursor += 1;
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(p)
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            self.cleared_all += 1;
            self.inner.clear()
        }
        fn clear_region(&mut self, t: ClearType) -> Result<(), Self::Error> {
            if matches!(t, ClearType::All) {
                self.cleared_all += 1;
            }
            self.inner.clear_region(t)
        }
        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size()
        }
        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.flush()
        }
        fn scroll_region_up(
            &mut self,
            area: std::ops::Range<u16>,
            top: u16,
        ) -> Result<(), Self::Error> {
            self.inner.scroll_region_up(area, top)
        }
        fn scroll_region_down(
            &mut self,
            area: std::ops::Range<u16>,
            bottom: u16,
        ) -> Result<(), Self::Error> {
            self.inner.scroll_region_down(area, bottom)
        }
    }

    fn paint(s: &'static str) -> impl Fn(&mut Frame) {
        move |f: &mut Frame| {
            Line::from(s).render(f.area(), f.buffer_mut());
        }
    }

    /// The frame takes the whole window, not a strip near the bottom of it.
    #[test]
    fn the_frame_area_is_the_whole_window() {
        let mut frame = ScreenFrame::full(Probe::new(40, 18)).unwrap();
        let mut area = Rect::ZERO;
        frame
            .draw(|f| {
                area = f.area();
            })
            .unwrap();
        assert_eq!(area, Rect::new(0, 0, 40, 18));
        assert_eq!(frame.size().unwrap(), Size::new(40, 18));
    }

    /// The frame never asks the terminal where the cursor is.
    ///
    /// This is the assertion the old inline `LiveView` could not make at all — it
    /// asked at construction, and every key-stream stop/restart pair in the old
    /// run loop was the price of that one question.
    #[test]
    fn the_frame_never_asks_the_terminal_where_the_cursor_is() {
        let mut frame = ScreenFrame::full(Probe::new(20, 3)).unwrap();
        frame.draw(paint("one")).unwrap();
        frame.draw(paint("two")).unwrap();
        frame.repaint_all().unwrap();
        frame.draw(paint("three")).unwrap();
        assert_eq!(
            frame.backend().asked_cursor,
            0,
            "the frame asked the terminal where the cursor is"
        );
    }

    /// Without the reclaim, a screen a child scribbled on stays scribbled: the
    /// diff compares against our own last frame, finds it unchanged, and writes
    /// nothing. With it, every cell is repainted.
    #[test]
    fn a_reclaim_repaints_cells_the_diff_would_have_skipped() {
        let mut frame = ScreenFrame::full(Probe::new(20, 3)).unwrap();
        frame.draw(paint("ours")).unwrap();
        assert_eq!(rows_trimmed(&frame.backend().inner)[0], "ours");
        assert_eq!(
            frame.backend().cleared_all,
            0,
            "a plain draw clears nothing"
        );

        // A full-screen child paints over the canvas. Our back buffer still
        // believes the screen says `ours`, so the next draw writes nothing.
        frame.backend_mut().scribble();
        frame.draw(paint("ours")).unwrap();
        assert_ne!(
            rows_trimmed(&frame.backend().inner)[0],
            "ours",
            "the scribble did not land, so this test would be proving nothing"
        );

        frame.repaint_all().unwrap();
        assert_eq!(
            frame.backend().cleared_all,
            1,
            "the reclaim is one clear of the whole screen, and nothing else"
        );
        frame.draw(paint("ours")).unwrap();
        assert_eq!(
            rows_trimmed(&frame.backend().inner)[0],
            "ours",
            "the canvas was not reclaimed"
        );
    }

    /// A window resize taken between frames is picked up by the next draw with
    /// no help from us: `draw` autoresizes, and for a full-screen viewport that
    /// is a size `ioctl` plus a clear — not a cursor query, and not a key
    /// stream that has to be stopped out of the way first.
    #[test]
    fn a_resize_between_frames_is_taken_by_the_next_draw() {
        let mut frame = ScreenFrame::full(Probe::new(20, 4)).unwrap();
        frame.draw(paint("small")).unwrap();
        assert_eq!(frame.size().unwrap(), Size::new(20, 4));

        frame.backend_mut().inner.resize(30, 9);
        frame.draw(paint("big")).unwrap();

        assert_eq!(frame.size().unwrap(), Size::new(30, 9));
        assert_eq!(rows_trimmed(&frame.backend().inner)[0], "big");
        assert_eq!(frame.backend().asked_cursor, 0);
        assert!(
            frame.backend().cleared_all >= 1,
            "the new geometry is cleared before the repaint, so nothing of the \
             old width survives in the tail"
        );
    }

    /// Bands at a width no chrome was designed for: still four, still tiling,
    /// still ending on the bottom edge.
    #[test]
    fn a_narrow_window_still_tiles() {
        for width in 1u16..40 {
            let FrameAreas {
                transcript: text,
                cards: card,
                status,
                input,
                ..
            } = frame_areas(
                Rect::new(0, 0, width, 12),
                MAX_TOOL_ROWS,
                MAX_INPUT_ROWS,
                KanbanBudget::Off,
            );
            assert_eq!(
                (text.width, card.width, status.width, input.width),
                (width, width, width, width)
            );
            assert_eq!(input.bottom(), 12, "width={width}");
        }
    }

    // ------------------------------------------------------- the window poll
    //
    // looprs-pdl.15. The poll is the resize path that does not depend on a
    // signal reaching us, and its whole contract is *when to speak and when to
    // stop*. Four claims, each injected rather than waited for: the size source
    // is a closure precisely so a test can be the terminal.

    use std::cell::Cell as Counter;
    use std::io;

    fn ok_size(w: u16, h: u16) -> impl FnOnce() -> io::Result<Size> {
        move || Ok(Size::new(w, h))
    }

    /// The one thing the poll exists to return: a window the app has not
    /// adopted. And once the app *has* adopted it, the same reading is no
    /// longer a change — which is what keeps the run loop from re-adopting a
    /// size every tick for the rest of the run.
    #[test]
    fn a_window_the_app_does_not_have_is_reported_once() {
        let mut poll = WindowPoll::new();
        assert_eq!(
            poll.poll(ok_size(80, 24), Size::new(60, 24)),
            Some(Size::new(80, 24))
        );
        assert_eq!(poll.poll(ok_size(80, 24), Size::new(80, 24)), None);
    }

    /// The steady state is silence. Sixty ticks of an unchanged window must
    /// ask for nothing: an idle app that repainted for a window that never
    /// moved would be the exact regression this type is not allowed to cause
    /// (see `spikes/resize_e2e.py`: the app goes back to zero bytes out
    /// between drags).
    #[test]
    fn a_steady_window_asks_for_nothing_for_sixty_ticks() {
        let mut poll = WindowPoll::new();
        for _ in 0..60 {
            assert_eq!(poll.poll(ok_size(120, 40), Size::new(120, 40)), None);
        }
        assert!(!poll.stopped(), "agreement is not a failure");
    }

    /// **The `tput` guard.** crossterm's `terminal::size()` falls back to
    /// spawning `tput` when the `ioctl` fails. A poll that retried that would
    /// fork twice a frame, forever, on a machine with no tty to ask. One
    /// failed read retires the poll and it never touches the size source
    /// again — counted here, because "stopped" without a count is a claim
    /// about a promise rather than about a behaviour.
    #[test]
    fn a_size_that_cannot_be_read_retires_the_poll_for_good() {
        let asks = Counter::new(0usize);
        let mut poll = WindowPoll::new();
        let first = poll.poll(
            || {
                asks.set(asks.get() + 1);
                Err(io::Error::other("not a tty"))
            },
            Size::new(80, 24),
        );
        assert_eq!(first, None, "a failed read adopts nothing");
        assert!(poll.stopped(), "a failed read ends the poll");

        for _ in 0..10 {
            assert_eq!(
                poll.poll(
                    || {
                        asks.set(asks.get() + 1);
                        Ok(Size::new(100, 30))
                    },
                    Size::new(80, 24),
                ),
                None,
                "a retired poll reports nothing whatever the terminal says"
            );
        }
        assert_eq!(asks.get(), 1, "asked once, failed, and never asked again");
    }

    /// A zero dimension is a pty coming apart, not a window arriving: adopting
    /// it would forward `rows = 0` to every child pty and lay the five bands
    /// out on nothing. Refused — and refused *without* retiring the poll,
    /// because the read itself worked and the next one may be real.
    #[test]
    fn a_degenerate_window_is_refused_and_the_poll_survives_it() {
        let mut poll = WindowPoll::new();
        for bad in [Size::new(0, 0), Size::new(0, 40), Size::new(120, 0)] {
            assert_eq!(
                poll.poll(move || Ok(bad), Size::new(120, 40)),
                None,
                "{bad:?} must never be adopted"
            );
        }
        assert!(!poll.stopped(), "a zero size is not a read failure");
        assert_eq!(
            poll.poll(ok_size(100, 30), Size::new(120, 40)),
            Some(Size::new(100, 30)),
            "and the next real window still gets through"
        );
    }
}
