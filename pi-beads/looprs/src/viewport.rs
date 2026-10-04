//! The shape of the live region: how tall it should be, and how the `Terminal`
//! actually gets there (looprs-afw).
//!
//! Until this module existed the live region was `const VIEWPORT_H: u16 = 10` in
//! `main.rs`, picked before anyone knew what the three modes' output looks like.
//! A long answer got five rows of preview while forty rows of terminal sat empty
//! above it, and a wall of four concurrent tool calls took most of those five.
//!
//! There are two decisions here and they cannot be separated: the **policy** (what
//! height do we want) and the **mechanics** (how does an inline viewport of a
//! different height come to exist without disturbing the scrollback already above
//! it). The mechanics are the dangerous half: getting them wrong duplicates or
//! erases lines in the one place the user cannot get them back.
//!
//! # The policy
//!
//! `desired_height(mode, term_rows, preview_rows, tool_rows)` — the live region is
//! exactly as tall as its chrome plus the live text that wants to be on screen,
//! clamped at both ends:
//!
//! * **floor** — the chrome plus one row of live text. A frame that cannot show
//!   its input box is broken, so the floor wins over the ceiling on a six-row
//!   window.
//! * **ceiling** — [`MAX_LIVE_ROWS`] and [`KEEP_SCROLLBACK_ROWS`] below the top
//!   of the terminal. Both halves answer the ticket's "avoid a live region that
//!   grows to the whole screen and makes scrollback jump": the margin keeps some
//!   scrollback *visible* so the live region reads as a pane inside a transcript,
//!   and the cap keeps a 100-row window from becoming one giant preview.
//! * **per mode** — Pi and the beads loop are prose, and grow. Bash is capped at
//!   one preview row on purpose: complete lines of shell output go straight to the
//!   scrollback as the transcript's own lines (ADR-0001 rule 1), so the live
//!   region holds only the *partial* line the shell has not finished writing. A
//!   shell that paints a screen takes the whole terminal instead (ADR-0001 Q2,
//!   [`crate::screen`]) and nothing of ours is drawn at all.
//!
//! # The mechanics
//!
//! ratatui 0.30 has no setter for the viewport: `Viewport::Inline(h)` is read
//! inside `Terminal::resize` / `autoresize` from a private field, so the height of
//! a live `Terminal` cannot be changed. Reaching a new height means building a new
//! `Terminal` over a backend that is *still looking at the same screen*. That is
//! cheap here — the backend is `CrosstermBackend<Stdout>`, a handle on the same
//! stdout the old one was writing to — and it is what [`LiveView`] wraps in one
//! call, [`LiveView::fit`], whose whole contract is:
//!
//! > the rows above the live region are not touched.
//!
//! It holds because of how ratatui computes an inline viewport: the new region
//! starts at the *cursor row* and reserves its room by printing newlines below it.
//! So `fit` parks the cursor on the live region's own top row before rebuilding:
//!
//! * **growing** scrolls the screen by exactly the shortfall, which lands the new
//!   viewport immediately below the last line already written; the extra rows come
//!   from blank screen below. The rows it drew over were the previous frame's live
//!   region, which we own. Nothing that was flushed is inside it.
//! * **shrinking** cannot scroll at all — there is strictly more room below the
//!   same top row than the smaller viewport needs — so the flushed rows keep the
//!   exact rows they had, and the rows being given back are blank space under the
//!   new, shorter live region.
//!
//! Either way the whole old region is **erased** before the rebuild, and that is
//! not tidiness. A freshly built `Terminal` diffs against a blank back buffer, so
//! it never writes the cells that are blank in the new frame; a cell that is blank
//! in the new frame but not on the screen is last frame's text, welded there for
//! the rest of the session. `Terminal::resize` clears for exactly this reason, and
//! a viewport *height* change has no `resize` to lean on.
//!
//! `fit` is a no-op when the height did not change, which is the common case: the
//! rebuild is paid for only on a real shape change, not every 16 ms frame.
//!
//! Two things [`LiveView`] must be told, because both move the viewport out from
//! under it: a window resize ([`LiveView::resize_window`]) and a full-screen
//! child giving the screen back. Both mark the anchor unknown, and the next frame
//! relearns it from the area that frame drew into. Guessing an anchor that is wrong
//! would put a whole viewport of redraw on top of real scrollback, so "unknown"
//! means "draw this frame at the height you already have", one frame of staleness
//! traded for never overwriting somebody else's line.
//!
//! # Reading the cursor back, and what it costs
//!
//! Everything above spends a cursor query: building an inline `Terminal` asks the
//! terminal where the cursor is (`ESC[6n`) to find the row to anchor to. That
//! one fact governs the frame loop that uses this module, because the query and
//! the async key stream are fighting over the same stdin — if the stream is
//! alive, *it* reads the answer and the query waits out crossterm's two-second
//! timeout. Measured here, that is the difference between a repaint 60 ms after
//! a resize and the app dying with "The cursor position could not be read within
//! a normal duration". Three rules follow, and all of them live in `main.rs`:
//!
//! * the key stream is **stopped** across anything that can query — `fit`,
//!   `resize_window` — and restarted after. That is why `needs_fit` exists, to
//!   ask first rather than have every frame pay for a restart it did not need;
//! * the real window size is **polled** every frame
//!   ([`LiveView::sees_window`]). `SIGWINCH` is not a byte on stdin, so a
//!   resize that lands while the stream is stopped is never delivered at all —
//!   and ratatui's fallback would then be to resize itself mid-draw, which is
//!   to say: the query, with the stream running. An ioctl answers the same
//!   question for free and leaves `autoresize` nothing to notice;
//! * neither a failed shape change nor a failed frame ends the session. `fit`
//!   keeps the old height, which keeps `needs_fit` true, so the next frame
//!   retries from the same parked cursor; and a draw that failed did so in
//!   `autoresize`, before it had swapped buffers or flushed anything, so there
//!   is nothing inconsistent to recover from. A live region stuck at yesterday's
//!   shape is a better-looking bug than a run that was killed over it.

use ratatui::backend::{Backend, ClearType};
use ratatui::layout::{Position, Rect, Size};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::session::TerminalType;
use crate::teardown::LiveAnchor;

/// The status row. `Constraint::Length(1)` in [`frame_areas`], and the same number
/// in the height policy — the two must be one constant or the frame grows a row of
/// blank space that nobody can explain.
pub const STATUS_ROWS: u16 = 1;
/// The input box, borders included.
pub const INPUT_ROWS: u16 = 3;
/// Concurrent tool calls shown at once (was: `tools.take(4)` in `main::view`).
pub const MAX_TOOL_ROWS: u16 = 4;
/// A live region is never shorter than this much text, so "streaming" is visible
/// even before the first line lands.
pub const MIN_PREVIEW_ROWS: u16 = 1;
/// The shell's live region holds one thing: the line it has not finished writing.
/// See the module docs for why Bash output does not need more.
pub const BASH_PREVIEW_ROWS: usize = 1;
/// Rows of terminal kept free *above* the live region, so a long stream still
/// shows scrollback rather than taking the whole window.
pub const KEEP_SCROLLBACK_ROWS: u16 = 3;
/// The live region's ceiling on a tall terminal. Past this the answer is not more
/// preview; it is the scrollback above, which is where finished text lives.
pub const MAX_LIVE_ROWS: u16 = 40;

/// Rows of the frame that are not live preview text.
pub fn chrome_rows(tool_rows: u16) -> u16 {
    STATUS_ROWS + INPUT_ROWS + tool_rows.min(MAX_TOOL_ROWS)
}

/// How much live text this mode may ever show, before the screen is consulted.
pub const fn preview_limit(mode: TerminalType) -> usize {
    match mode {
        // Every completed line of shell output is already in the scrollback as
        // itself (raw, unwrapped). Re-showing it here would be a second copy.
        TerminalType::Bash => BASH_PREVIEW_ROWS,
        // Prose: bounded by the screen, not by the mode.
        TerminalType::Beeds | TerminalType::Pi => usize::MAX,
    }
}

/// The most the live region may take of a terminal this tall.
///
/// Three limits stacked, and every one of them is `saturating` because this runs on
/// frames taken mid-resize: the screen minus the keep-visible margin, the absolute
/// [`MAX_LIVE_ROWS`] cap, and the mode's own ceiling (a mode that only ever shows
/// one live row cannot fill a fifty-row window).
pub fn max_live(mode: TerminalType, term_rows: u16) -> u16 {
    let text = preview_limit(mode).min(u16::MAX as usize) as u16;
    let mode_cap = chrome_rows(0).saturating_add(text);
    term_rows
        .saturating_sub(KEEP_SCROLLBACK_ROWS)
        .min(MAX_LIVE_ROWS)
        .min(mode_cap)
}

/// The height the live region wants this frame.
///
/// Pure, and total: every combination of arguments returns a usable height,
/// because this runs on frames taken while the window is being dragged and the
/// modes are being switched, and a panic here takes the app down.
pub fn desired_height(
    mode: TerminalType,
    term_rows: u16,
    preview_rows: usize,
    tool_rows: u16,
) -> u16 {
    if term_rows == 0 {
        return 0;
    }

    // A wall of concurrent tool calls must not push the status row or the input
    // box off the screen, so the tool rows are the first thing given back.
    let tools = tool_rows
        .min(MAX_TOOL_ROWS)
        .min(term_rows.saturating_sub(STATUS_ROWS + INPUT_ROWS + MIN_PREVIEW_ROWS));
    let chrome = chrome_rows(tools);

    // The smallest honest frame: its chrome, plus a line of live text.
    let floor = (chrome + MIN_PREVIEW_ROWS).min(term_rows);
    // Never below that floor: on a short window, eating the keep-visible margin
    // beats an input box that is not on screen.
    let ceiling = max_live(mode, term_rows).max(floor).min(term_rows);

    let live = preview_rows
        .min(preview_limit(mode))
        .min(ceiling.saturating_sub(chrome) as usize);
    (chrome + live as u16).clamp(floor, ceiling)
}

/// The frame's four bands, top to bottom: live text, tool rows, status, input.
///
/// Lives next to the policy because the two must tile the *same* height:
/// `desired_height` adds the chrome up, and this spends it back out. Anything that
/// makes the two disagree shows up as blank rows inside the live region, which is
/// a bug nobody can trace from a screenshot.
pub fn frame_areas(area: Rect, tool_rows: u16) -> [Rect; 4] {
    use ratatui::layout::{Constraint, Layout};
    let tools = tool_rows.min(MAX_TOOL_ROWS);
    Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(tools),
        Constraint::Length(STATUS_ROWS),
        Constraint::Length(INPUT_ROWS),
    ])
    .areas(area)
}

/// Build a backend that is looking at the same screen as the one being replaced.
///
/// For the real app that is `CrosstermBackend::new(io::stdout())`; the parameter
/// is the backend being retired, so a test can hand back a snapshot of the screen
/// the old one was looking at. Named because the closure type is otherwise three
/// lines of noise in the struct.
type Respawn<B> = dyn FnMut(&B) -> Result<B, <B as Backend>::Error>;

/// The `Terminal` plus the one fact ratatui will not tell you back: the height it
/// was built with, and where on the real screen its inline viewport currently is.
///
/// The height is what `fit` compares against to decide whether a rebuild is needed
/// at all. The anchor (`top`) is what makes the rebuild safe, and it is
/// deliberately `Option`: it comes from the area of the last frame actually drawn,
/// and any event that can move the viewport without our knowledge — a window
/// resize, a full-screen hand-back — clears it rather than leaving a guess
/// standing.
///
/// The anchor is a [`LiveAnchor`] rather than a plain `Option<u16>` because the
/// exit path has to read it too, and cannot ask the `Terminal`: ratatui's own
/// `clear()` finds the row by *querying the cursor*, which is the one thing that
/// must never happen at exit (see [`crate::teardown`). Publishing it from here —
/// every writer of the anchor goes through this type — is what makes the exit
/// path's idea of "where the pane is" and this type's idea of the same fact one
/// fact, not two that can drift.
pub struct LiveView<B: Backend> {
    term: Terminal<B>,
    height: u16,
    top: LiveAnchor,
    /// The real window this view was last reshaped for. The frame loop compares
    /// the actual size against it every tick, because a `SIGWINCH` that lands
    /// while the key stream is stopped is never delivered at all — see
    /// [`Self::sees_window`].
    window: Option<Size>,
    respawn: Box<Respawn<B>>,
}

/// A shape change that did not happen.
///
/// Logged rather than propagated, because the only caller is the frame loop and
/// the only sane thing to do there is keep drawing. `fit` leaves the old height
/// in place, so the intent is not lost: it comes back next frame. Still returned
/// as an `Err` so a caller cannot ignore it by accident.
fn deferred<E: std::fmt::Display>(e: E) -> E {
    tracing::warn!("live region resize deferred, will retry next frame: {e}");
    e
}

impl<B: Backend> LiveView<B> {
    /// Wrap a fresh `Terminal` whose inline viewport is `height` rows, over an
    /// anchor nobody else holds.
    #[allow(dead_code)] // test seam: the app needs `with_anchor`, so the exit path shares the anchor; the mechanics tests have no exit path
    pub fn new(
        backend: B,
        height: u16,
        respawn: impl FnMut(&B) -> Result<B, B::Error> + 'static,
    ) -> Result<Self, B::Error> {
        Self::with_anchor(backend, height, respawn, LiveAnchor::new())
    }

    /// As [`Self::new`], over an anchor somebody else already holds.
    ///
    /// The app uses this so `main` can hand the *same* anchor to the panic hook
    /// before the run loop has drawn a single frame: the window between
    /// `enable_raw_mode()` and the first draw is exactly the window a panic in
    /// `LiveView::new` would land in, and the terminal still has to come back.
    pub fn with_anchor(
        backend: B,
        height: u16,
        respawn: impl FnMut(&B) -> Result<B, B::Error> + 'static,
        anchor: LiveAnchor,
    ) -> Result<Self, B::Error> {
        let term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        Ok(Self {
            window: term.size().ok(),
            term,
            height,
            // Unknown until the first frame reports where it landed.
            top: anchor,
            respawn: Box::new(respawn),
        })
    }

    /// The handle the exit path reads. Clones point at the same row this view keeps
    /// updating, which is the point of it — there is no second copy of the truth
    /// to fall out of agreement with the pane.
    #[allow(dead_code)] // test seam: `main` supplies the anchor to `with_anchor` rather than reading it back; the anchor tests below assert it
    pub fn anchor(&self) -> LiveAnchor {
        self.top.clone()
    }

    /// Has the live region already been reshaped for a window of this size?
    ///
    /// Two paths can learn about the same resize — the `Event::Resize` the key
    /// stream reports, and the frame loop's own poll of the real size — and
    /// reshaping twice buys nothing but a second cursor read and a second clear.
    pub fn sees_window(&self, size: Size) -> bool {
        self.window == Some(size)
    }

    /// The height the live region is right now.
    #[allow(dead_code)] // test seam: the mechanics tests assert the shape it landed on
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Where the live region starts on the real screen, once a frame has said so.
    #[allow(dead_code)] // test seam: anchoring is only observable from the outside
    pub fn top(&self) -> Option<u16> {
        self.top.get()
    }

    /// Rows of the real terminal.
    pub fn rows(&self) -> Result<u16, B::Error> {
        Ok(self.term.size()?.height)
    }

    /// The real terminal's size, straight from the backend.
    pub fn screen_size(&self) -> Result<Size, B::Error> {
        self.term.size()
    }

    #[allow(dead_code)] // test seam: read the screen the backend is looking at
    pub fn backend(&self) -> &B {
        self.term.backend()
    }

    /// Draw one frame, and record where the viewport actually was.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> Result<(), B::Error> {
        let mut drawn = Rect::ZERO;
        self.term.draw(|f| {
            drawn = f.area();
            render(f);
        })?;
        self.top.set(Some(drawn.top()));
        Ok(())
    }

    /// Insert finalized lines above the live region (the append-only scrollback).
    ///
    /// This moves the viewport down; the record of where it went comes from the
    /// frame that is drawn immediately afterwards, which is why the run loop's
    /// order is `fit` -> `insert` -> `draw` and not some other order.
    /// Insert finalized lines above the live region, and follow the pane down.
    ///
    /// ratatui moves the viewport as part of the insert but does not report the new
    /// row back, so the anchor would otherwise be one operation stale — and the
    /// exit path erases *from* that row. Erasing from the row the pane occupied
    /// *before* the last insert would wipe the lines that insert just wrote. Hence
    /// this function is the only way in, and it moves the anchor with the pane.
    pub fn insert_before(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut ratatui::buffer::Buffer),
    ) -> Result<(), B::Error> {
        self.term.insert_before(height, draw_fn)?;
        self.follow_the_pane_down(height);
        Ok(())
    }

    /// Update the published anchor after `height` rows went in above the pane.
    ///
    /// The pane moves down by the rows that fitted below it; whatever did not fit
    /// went out through the top and is already in scrollback. So:
    ///
    /// ```text
    /// new_top = clamp(top + height, top, screen_height - pane_height)
    /// ```
    ///
    /// The upper bound is what makes the arithmetic agree with all three of
    /// ratatui's cases: room below (it scrolls the region down and the pane moves
    /// the full amount), no room below (the bound holds it at the last row that
    /// keeps the whole pane on screen), and a pane that already fills the screen
    /// (the bound *is* the current row, so it never moves). The lower bound of
    /// `top` covers a terminal that reported a size smaller than the pane; the pane
    /// cannot travel backwards, and neither does the record of it.
    ///
    /// An unknown anchor is left unknown rather than guessed at. This is the row the
    /// exit path clears from, and a wrong one here is an erase across rows that
    /// were never ours.
    fn follow_the_pane_down(&self, height: u16) {
        let Some(top) = self.top.get() else { return };
        // If the size cannot be read, assume it did not get in the way: overshoot
        // leaves the top of the pane on screen, which is untidy. Underguessing
        // erases output the user just got. Those are not equally bad.
        let floor = self
            .rows()
            .unwrap_or(u16::MAX)
            .saturating_sub(self.height)
            .max(top);
        let moved = top.saturating_add(height).min(floor).max(top);
        self.top.set(Some(moved));
    }

    /// The real window changed shape. The viewport moved; we do not know where to.
    pub fn resize_window(&mut self, area: Rect) -> Result<(), B::Error> {
        self.top.set(None);
        self.window = Some(area.as_size());
        self.term.resize(area)
    }

    /// The full-screen child let go. Same problem, same answer: no anchor until a
    /// frame reports where the viewport actually is again — the row we recorded
    /// before the takeover describes a screen that no longer exists.
    pub fn anchor_lost(&mut self) {
        self.top.set(None);
    }

    /// Would [`Self::fit`] have to rebuild the `Terminal` for this height?
    ///
    /// Checked *before* the caller stops the key stream, because the rebuild reads
    /// the cursor position back and the stream must not be stopped for nothing.
    /// Deliberately conservative: it can say yes when `fit` then finds the height
    /// clamped to the screen and changes nothing, which costs a restarted key
    /// stream and no more.
    pub fn needs_fit(&self, want: u16) -> bool {
        self.top.get().is_some() && want != self.height
    }

    /// Bring the live region to `want` rows.
    ///
    /// `Ok(true)` = the `Terminal` was rebuilt at the new height. `Ok(false)` =
    /// nothing happened, either because the height already was `want` (the normal
    /// case, and the reason this may be called every frame), because the anchor is
    /// unknown and rebuilding onto a guessed row is exactly how scrollback gets
    /// painted over, or because the rebuild failed and has been left for a retry.
    ///
    /// Never touches a row above the live region's top. See the module docs for the
    /// anchoring argument that makes that true.
    pub fn fit(&mut self, want: u16) -> Result<bool, B::Error> {
        let want = want.min(self.rows()?.max(1));
        if want == 0 || want == self.height {
            return Ok(false);
        }
        let Some(top) = self.top.get() else {
            tracing::debug!("viewport fit to {want} deferred: anchor unknown");
            return Ok(false);
        };

        // Park the cursor on the live region's own top row. Two things hang off
        // that one move: the erase below it has a known starting point, and the
        // new viewport anchors to whatever cursor it is constructed over.
        self.term
            .set_cursor_position(Position::new(0, top))
            .map_err(deferred)?;

        // Everything from the live region's top row down is ours to erase: the old
        // live region, and the unused screen below it. This is not tidiness, it is
        // correctness. A freshly built `Terminal` diffs against a *blank* back
        // buffer, so it never writes the cells that are blank in the new frame —
        // and a cell that is blank in the new frame but not on the screen is last
        // frame's text, welded there for the rest of the session. `Terminal::resize`
        // clears for exactly this reason; a viewport *height* change has no
        // `resize` to lean on, so the erase is done by hand, on the rows the live
        // region owns and nothing above them.
        self.term
            .backend_mut()
            .clear_region(ClearType::AfterCursor)
            .map_err(deferred)?;

        // A shape change that fails is not worth a session. The height is left at
        // its old value, which keeps `needs_fit` true, so the next frame walks
        // through this same sequence again — parked cursor, erase and rebuild are
        // all idempotent, so a retry that starts from half of this is still a
        // whole one. The live region just stays the shape it was until then.
        let backend = (self.respawn)(self.term.backend()).map_err(deferred)?;
        self.term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(want),
            },
        )
        .map_err(deferred)?;
        self.height = want;
        // Where it actually landed is reported by the next frame.
        self.top.set(Some(top));

        // Remember the real window this shape was built for. A freshly built
        // inline `Terminal` already records the backend's size as its own known
        // area (so its `autoresize` will not fire on the next draw), but this
        // view keeps its own copy so the frame loop can tell a size it has been
        // shaped for from one it has not — see [`Self::sees_window`].
        self.window = self.term.size().ok();
        Ok(true)
    }

    // No `clear()` here, on purpose. `Terminal::clear()` is the obvious call for
    // "erase the live region at exit" and it is a trap: it starts by asking the
    // terminal where the cursor is (`ESC[6n`), and at exit the async key stream's
    // reader thread is still parked on the same stdin and eats the answer — the
    // `"cursor position could not be read"` death this whole ticket is about. The
    // erase is done by [`crate::teardown`], from the anchor published here, with
    // no query in front of it.
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};
    use std::rc::Rc;

    const W: usize = 24;
    const H: u16 = 30;

    // ---------------------------------------------------------------- policy

    /// The ticket's headline: a long streamed answer must be able to use more than
    /// the ten rows `VIEWPORT_H` allowed, and more than the five the chrome leaves
    /// when tool rows eat the rest.
    #[test]
    fn a_long_answer_uses_much_more_than_the_old_ten_rows() {
        let got = desired_height(TerminalType::Pi, 50, 36, 0);
        assert!(got > 10, "36 lines of answer in a 50-row terminal: {got}");
        assert_eq!(got, chrome_rows(0) + 36, "exactly its chrome plus its text");
    }

    /// Short output does not reserve the moon: the region is the size of what it
    /// has to show, and nothing more.
    #[test]
    fn a_short_answer_does_not_reserve_more_than_it_has() {
        assert_eq!(
            desired_height(TerminalType::Pi, 50, 1, 0),
            chrome_rows(0) + 1
        );
        assert_eq!(
            desired_height(TerminalType::Pi, 50, 5, 0),
            chrome_rows(0) + 5
        );
    }

    /// Bash's live region is the line the shell has not finished writing. Nothing
    /// else can be there: every complete line is already in the scrollback as
    /// itself, and a program that paints a screen takes the whole terminal
    /// instead (ADR-0001). This is the "a mode keeps a small fixed region, on
    /// purpose" case the ticket asks to be documented.
    #[test]
    fn bash_keeps_one_live_row_and_says_why() {
        for preview in [0, 1, 9, 400] {
            assert_eq!(
                desired_height(TerminalType::Bash, 50, preview, 0),
                chrome_rows(0) + BASH_PREVIEW_ROWS as u16,
                "bash preview rows={preview}"
            );
        }
        assert_eq!(preview_limit(TerminalType::Bash), BASH_PREVIEW_ROWS);
        assert_eq!(preview_limit(TerminalType::Pi), usize::MAX);
        assert_eq!(preview_limit(TerminalType::Beeds), usize::MAX);
    }

    /// Every row of the mode table, at several terminal sizes: the live region is
    /// never bigger than the screen, never smaller than its own chrome, and never
    /// takes the keep-visible margin away unless the frame would otherwise not fit.
    #[test]
    fn the_whole_table_fits_the_terminal_it_is_given() {
        for mode in TerminalType::ALL {
            for term_rows in 0u16..=120 {
                for preview in [0usize, 1, 3, 9, 17, 44, 500] {
                    for tools in [0u16, 1, 4, 9] {
                        let h = desired_height(mode, term_rows, preview, tools);
                        assert!(
                            h <= term_rows,
                            "{mode:?} rows={term_rows} preview={preview} tools={tools}: {h} > screen"
                        );
                        let chrome = chrome_rows(tools.min(
                            term_rows.saturating_sub(STATUS_ROWS + INPUT_ROWS + MIN_PREVIEW_ROWS),
                        ));
                        if term_rows >= chrome + MIN_PREVIEW_ROWS {
                            assert!(
                                h >= chrome + MIN_PREVIEW_ROWS,
                                "{mode:?} rows={term_rows} preview={preview} tools={tools}: \
                                 the frame lost its own chrome ({h} < {})",
                                chrome + MIN_PREVIEW_ROWS
                            );
                        }
                        // The keep-visible margin holds whenever the frame can
                        // afford both it and its chrome.
                        let room = term_rows
                            .saturating_sub(KEEP_SCROLLBACK_ROWS)
                            .max(chrome + MIN_PREVIEW_ROWS);
                        assert!(h <= room, "{mode:?} rows={term_rows}: {h} ate the margin");
                    }
                }
            }
        }
    }

    /// The tall-terminal caps: `MAX_LIVE_ROWS` bounds the pane, so a 120-row
    /// window does not turn into one enormous preview with no transcript in sight.
    #[test]
    fn a_tall_terminal_is_capped_rather_than_filled() {
        let h = desired_height(TerminalType::Pi, 120, 500, 0);
        assert_eq!(h, MAX_LIVE_ROWS);
        assert!(h < 120 - KEEP_SCROLLBACK_ROWS);
        // And on a window shorter than the cap, the screen is what bounds it.
        assert_eq!(
            desired_height(TerminalType::Pi, 20, 500, 0),
            20 - KEEP_SCROLLBACK_ROWS
        );
    }

    /// More text never buys a shorter region: the policy is monotone in the
    /// preview, so a stream cannot make the pane flicker smaller as it grows.
    #[test]
    fn the_region_grows_monotonically_with_its_content() {
        for mode in TerminalType::ALL {
            for term_rows in [12u16, 24, 50, 90] {
                let mut last = 0u16;
                for preview in 0usize..60 {
                    let h = desired_height(mode, term_rows, preview, 2);
                    assert!(
                        h >= last,
                        "{mode:?} rows={term_rows}: preview {preview} shrank the region \
                         from {last} to {h}"
                    );
                    last = h;
                }
            }
        }
    }

    /// The tool wall is the first thing given back, so four concurrent calls can
    /// never push the status row or the input box off a short screen — the exact
    /// complaint in the ticket ("4 concurrent tool calls leave almost nothing for
    /// text" is answered by: they cannot eat more than their share, and the chrome
    /// always survives).
    #[test]
    fn a_wall_of_tools_gives_rows_back_before_the_chrome_goes() {
        // Plenty of screen: four tool rows cost four rows of text.
        assert_eq!(
            desired_height(TerminalType::Pi, 50, 4, 4),
            chrome_rows(4) + 4
        );
        assert_eq!(chrome_rows(4) - chrome_rows(0), MAX_TOOL_ROWS);
        // More tools than we show: capped, not stacked.
        assert_eq!(chrome_rows(9), chrome_rows(4));
        // A short screen with a tool wall: tools shrink until the chrome fits.
        let h = desired_height(TerminalType::Pi, 5, 9, 4);
        assert_eq!(
            h, 5,
            "the frame fills the tiny screen without overflowing it"
        );
        assert!(h >= STATUS_ROWS + INPUT_ROWS + MIN_PREVIEW_ROWS);
    }

    /// `desired_height` and `frame_areas` must be the same arithmetic seen from two
    /// ends, or the live region grows a band of blank space nobody can explain.
    #[test]
    fn the_policy_and_the_layout_spend_one_height() {
        for tools in 0u16..=6 {
            for total in chrome_rows(tools.min(MAX_TOOL_ROWS))..=60u16 {
                let area = Rect::new(0, 0, W as u16, total);
                let [text, tool_band, status, input] = frame_areas(area, tools);
                assert_eq!(tool_band.height, tools.min(MAX_TOOL_ROWS));
                assert_eq!(status.height, STATUS_ROWS);
                assert_eq!(input.height, INPUT_ROWS);
                assert_eq!(
                    text.height as u32
                        + tool_band.height as u32
                        + status.height as u32
                        + input.height as u32,
                    total as u32,
                    "tools={tools} total={total}: the bands do not tile the frame"
                );
                // And they tile it in order, with no gaps or overlaps.
                assert_eq!(text.bottom(), tool_band.top());
                assert_eq!(tool_band.bottom(), status.top());
                assert_eq!(status.bottom(), input.top());
                assert_eq!(input.bottom(), area.bottom());
            }
        }
    }

    /// The chrome constants the policy adds up are the constants the layout spends.
    #[test]
    fn chrome_is_one_number_not_two() {
        assert_eq!(chrome_rows(0), STATUS_ROWS + INPUT_ROWS);
        assert_eq!(chrome_rows(2), STATUS_ROWS + INPUT_ROWS + 2);
        assert_eq!(
            chrome_rows(u16::MAX),
            STATUS_ROWS + INPUT_ROWS + MAX_TOOL_ROWS
        );
    }

    /// Total function: nothing this computes may panic on a zero-row terminal,
    /// because it runs while the window is being dragged.
    #[test]
    fn a_zero_row_terminal_is_answered_not_panicked() {
        for mode in TerminalType::ALL {
            for preview in [0usize, 7] {
                for tools in [0u16, 4] {
                    assert_eq!(desired_height(mode, 0, preview, tools), 0);
                }
            }
        }
    }

    // ------------------------------------------------------------- mechanics
    //
    // The rebuild itself, against `TestBackend`. The fake's `append_lines` is the
    // same "scroll the top away when there is no room below the cursor" the real
    // terminal does, and its buffer is the visible screen — so these assertions
    // are about rows on a screen, which is the only place the `insert_before`
    // invariant can actually be broken.
    //
    // One thing the fake cannot do: carry its *scrollback* across the rebuild (no
    // setter), so the assertions below are about rows still on screen. Nothing in
    // `fit` writes outside the live region plus the rows it is giving back, which
    // is what they pin down.

    /// Read the fake's visible screen back as rows of text, right-trimmed: the
    /// screen is padded to the full width and nobody cares about the padding.
    fn rows_trimmed(b: &TestBackend) -> Vec<String> {
        rows_of(b)
            .iter()
            .map(|r| r.trim_end().to_string())
            .collect()
    }
    fn rows_of(b: &TestBackend) -> Vec<String> {
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
                let mut out: String = s.chars().take(w).collect();
                while out.chars().count() < w {
                    out.push(' ');
                }
                out
            })
            .collect()
    }

    /// A backend whose screen is `lines` on top and blank below, exactly `H` rows.
    fn backend_with(lines: &[String]) -> TestBackend {
        let mut all = lines.iter().take(H as usize).cloned().collect::<Vec<_>>();
        while all.len() < H as usize {
            all.push(" ".repeat(W));
        }
        TestBackend::with_lines(all)
    }

    /// Paint the live region so every row of it is labelled with its own absolute
    /// screen row. Any live row that lands where it should not be is then visible
    /// in the assertion output as a `live NN` in the wrong place.
    fn paint_live(f: &mut Frame) {
        let a = f.area();
        for y in a.top()..a.bottom() {
            Line::from(format!("live {}", y)).render(Rect::new(a.x, y, a.width, 1), f.buffer_mut());
        }
    }

    /// The same screen, a new backend: what a fresh `CrosstermBackend<Stdout>` is
    /// for the real app — still looking at the same terminal, cursor included.
    fn respawn_like_stdout(
        old: &TestBackend,
    ) -> Result<TestBackend, <TestBackend as Backend>::Error> {
        let mut nb = backend_with(&rows_of(old));
        nb.set_cursor_position(old.cursor_position())?;
        Ok(nb)
    }

    fn blank_screen() -> TestBackend {
        backend_with(&[])
    }

    /// A `LiveView` whose viewport starts at `top`, with `flushed` lines already
    /// written above it.
    fn view_at(top: u16, height: u16, flushed: usize) -> LiveView<TestBackend> {
        let lines: Vec<String> = (0..flushed)
            .map(|i| format!("f{i:03}"))
            .take(top as usize)
            .collect();
        let mut b = backend_with(&lines);
        b.set_cursor_position(Position::new(0, top)).unwrap();
        LiveView::new(b, height, respawn_like_stdout).unwrap()
    }

    /// Growing must not touch a row above the live region — so long as the frame
    /// knew where the live region was.
    #[test]
    fn growing_does_not_touch_the_rows_above_the_live_region() {
        let mut live = view_at(10, 8, 10);
        live.draw(paint_live).unwrap();
        let before = rows_trimmed(live.backend());
        let top = live.top().unwrap();
        assert_eq!(top, 10);
        assert_eq!(before[0], "f000");
        assert_eq!(before[9], "f009");

        // Room below: no scroll at all, so the flushed rows keep their exact rows.
        assert!(live.fit(20).unwrap(), "20 != 8, so it rebuilds");
        live.draw(paint_live).unwrap();
        let after = rows_trimmed(live.backend());
        assert_eq!(live.top().unwrap(), 10, "no scroll: the anchor held");
        for i in 0..10 {
            assert_eq!(after[i], before[i], "flushed row {i} moved: {:?}", after[i]);
        }
        assert_eq!(after[10], "live 10", "the live region starts below them");
    }

    /// When there is no room below, growing scrolls — which is the terminal moving
    // the scrollback up, exactly as if we had printed the rows ourselves. What must
    /// not happen is the viewport covering a flushed line.
    #[test]
    fn growing_without_room_scrolls_and_still_sits_below_the_flushed_lines() {
        let mut live = view_at(20, 8, 20);
        live.draw(paint_live).unwrap();
        assert_eq!(live.top().unwrap(), 20);
        assert_eq!(rows_trimmed(live.backend())[19], "f019");

        // 14 rows cannot fit from row 20 in a 30-row screen: it must scroll.
        assert!(live.fit(14).unwrap());
        live.draw(paint_live).unwrap();
        let after = rows_trimmed(live.backend());
        let top = live.top().unwrap();
        assert!(top < 20, "the screen scrolled: top {top}");
        assert_eq!(top, 16, "exactly the shortfall, not one row more");
        // The row above the live region is the last flushed line still on screen.
        assert_eq!(after[top as usize - 1], "f019");
        assert_eq!(after[top as usize], "live 16");
        // ...and no flushed line is inside the live region.
        for row in &after[top as usize..] {
            assert!(
                !row.starts_with('f'),
                "a flushed line is under the live region: {row}"
            );
        }
    }

    /// The rows a shrink gives back are below the new viewport, so nothing will
    /// ever draw there again. Without clearing them the previous answer is welded
    /// to the terminal for the rest of the session.
    #[test]
    fn shrinking_clears_every_row_it_gives_back() {
        let mut live = view_at(10, 20, 10);
        live.draw(paint_live).unwrap();
        assert_eq!(rows_trimmed(live.backend())[29], "live 29");

        assert!(live.fit(6).unwrap());
        // The give-back is cleared before the rebuild, so the rows below the new
        // viewport are blank, not stale live text.
        let after = rows_trimmed(live.backend());
        for (y, row) in after.iter().enumerate().skip(16) {
            assert_eq!(row.trim(), "", "row {y} still shows {row:?}");
        }
        live.draw(paint_live).unwrap();
        let after = rows_trimmed(live.backend());
        assert_eq!(live.top().unwrap(), 10, "a shrink never scrolls");
        assert_eq!(after[9], "f009", "and never moves the flushed rows");
        assert_eq!(after[10], "live 10");
        assert_eq!(after[15], "live 15");
        for (y, row) in after.iter().enumerate().skip(16) {
            assert_eq!(row.trim(), "", "row {y} was not cleared: {row:?}");
        }
    }

    /// A shrink with no room to spare would have to scroll *up*, which would take
    /// flushed rows with it. It cannot happen: the smaller viewport always fits
    /// below the same top row. Assert it, so a future change to the anchoring has
    /// to break this on purpose rather than by accident.
    #[test]
    fn a_shrink_never_scrolls_the_flushed_lines() {
        let mut live = view_at(28, 2, 28);
        live.draw(paint_live).unwrap();
        let before = rows_trimmed(live.backend());
        assert!(live.fit(1).unwrap());
        live.draw(paint_live).unwrap();
        let after = rows_trimmed(live.backend());
        for i in 0..28 {
            assert_eq!(after[i], before[i], "row {i} moved on a shrink");
        }
    }

    /// The rebuild costs what it costs, so it must not happen every frame.
    #[test]
    fn no_rebuild_when_the_height_is_already_right() {
        let count = Rc::new(std::cell::Cell::new(0usize));
        let mut b = blank_screen();
        b.set_cursor_position(Position::new(0, 20)).unwrap();
        let n = count.clone();
        let mut live = LiveView::new(b, 8, move |old| {
            n.set(n.get() + 1);
            respawn_like_stdout(old)
        })
        .unwrap();
        live.draw(paint_live).unwrap();
        for _ in 0..50 {
            live.fit(8).unwrap();
        }
        assert_eq!(count.get(), 0, "identical height must not rebuild, ever");
        assert!(live.fit(9).unwrap());
        assert_eq!(count.get(), 1);
        assert!(live.fit(4).unwrap());
        assert_eq!(count.get(), 2);
        assert_eq!(live.height(), 4);
    }

    /// Before a frame has reported where the viewport is — or after something
    /// moved it without us — `fit` must refuse rather than anchor on a guess. A
    /// wrong anchor is a viewport of redraw painted over real scrollback.
    #[test]
    fn fit_refuses_until_a_frame_says_where_the_region_is() {
        let mut live = view_at(10, 8, 10);
        assert_eq!(live.top(), None, "nothing drawn yet: no anchor");
        assert!(
            !live.fit(20).unwrap(),
            "it must defer rather than build on a guessed row"
        );
        assert_eq!(live.height(), 8, "and it must not have changed anything");

        live.draw(paint_live).unwrap();
        assert_eq!(live.top(), Some(10));
        assert!(live.fit(20).unwrap(), "now it can move");
    }

    /// A window resize moves the viewport without telling us where to. The next
    /// `fit` must wait for a frame the same way, and relearn rather than reuse.
    #[test]
    fn a_window_resize_clears_the_anchor_and_a_frame_relearns_it() {
        let mut live = view_at(10, 8, 10);
        live.draw(paint_live).unwrap();
        assert_eq!(live.top(), Some(10));

        live.resize_window(Rect::new(0, 0, W as u16, H)).unwrap();
        assert_eq!(live.top(), None, "the anchor is suspect after a resize");
        assert!(!live.fit(14).unwrap(), "so a fit must not run on it");

        live.draw(paint_live).unwrap();
        assert!(live.top().is_some(), "the frame reported where it landed");
        let relearned = live.top().unwrap();
        assert!(live.fit(14).unwrap(), "and now the shape can change");
        assert_eq!(
            live.top().unwrap(),
            relearned,
            "grown from the reported row"
        );
    }

    /// The full-screen hand-back is the same problem in a different costume.
    #[test]
    fn a_full_screen_hand_back_clears_the_anchor_too() {
        let mut live = view_at(10, 8, 10);
        live.draw(paint_live).unwrap();
        live.anchor_lost();
        assert_eq!(live.top(), None);
        assert!(!live.fit(20).unwrap());
        live.draw(paint_live).unwrap();
        assert!(live.top().is_some());
    }

    /// The anchor is not only for rebuilding — it is the row the **exit** path
    /// erases from. So it has to follow the pane down every time
    /// `insert_before` pushes it, or the final erase lands on the rows the last
    /// insert just wrote and deletes the answer instead of the pane. (This is
    /// looprs-ecr's half of the story: `teardown::restore_bytes` trusts this row
    /// with the user's scrollback.)
    #[test]
    fn the_anchor_follows_the_pane_down_when_rows_are_inserted() {
        let mut live = view_at(10, 8, 10);
        live.draw(paint_live).unwrap();
        assert_eq!(live.top(), Some(10));

        insert(&mut live, 3);
        assert_eq!(
            live.top(),
            Some(13),
            "there was room below: the pane moved the whole way"
        );

        // More rows than fit under the pane: the pane stops at the last row that
        // keeps all eight of its rows on screen, and everything past the top of
        // the screen went into scrollback — it is not the pane's to move.
        insert(&mut live, 40);
        assert_eq!(
            live.top(),
            Some(H - 8),
            "capped where the whole pane still fits, not at 53"
        );

        // A zero-row insert moves nothing, and must not move the record either.
        insert(&mut live, 0);
        assert_eq!(live.top(), Some(H - 8));
    }

    /// A pane that already fills the screen cannot move down at all; the rows go
    /// straight out through the top. The anchor has to stay put rather than claim
    /// the pane travelled.
    #[test]
    fn a_pane_that_fills_the_screen_does_not_move_when_rows_are_inserted() {
        let mut live = view_at(0, H, 0);
        live.draw(paint_live).unwrap();
        assert_eq!(live.top(), Some(0));
        insert(&mut live, 6);
        assert_eq!(
            live.top(),
            Some(0),
            "nowhere to go, and the record knows it"
        );
    }

    /// And an unknown anchor stays unknown. The exit path reads "unknown" as
    /// "erase nothing", which is the safe answer; inventing a row here would turn
    /// a shutdown into an erase across rows that were never ours.
    #[test]
    fn inserting_with_no_anchor_does_not_invent_one() {
        let mut live = view_at(10, 8, 10);
        assert_eq!(live.top(), None, "nothing has been drawn yet");
        insert(&mut live, 4);
        assert_eq!(live.top(), None);

        // Once a frame reports the row, the inserts can follow it.
        live.draw(paint_live).unwrap();
        let anchored = live.top().expect("the frame reported a row");
        insert(&mut live, 2);
        assert_eq!(
            live.top(),
            Some(anchored + 2),
            "the record moved with the pane from the row the frame reported"
        );
    }

    fn insert(live: &mut LiveView<TestBackend>, rows: u16) {
        live.insert_before(rows, |_buf| {}).unwrap();
    }

    /// The whole run loop's frame order — `fit` -> `insert_before` -> `draw` —
    /// driven like a stream, checking the one invariant that matters after every
    /// single step: the row directly above the live region is the newest flushed
    /// line, no flushed line is under the live region, and no flushed line shows
    /// up twice.
    #[test]
    fn a_streamed_session_never_duplicates_or_covers_a_flushed_line() {
        let mode = TerminalType::Pi;
        let mut live = view_at(24, 8, 0);
        live.draw(paint_live).unwrap();
        let mut flushed: Vec<String> = Vec::new();

        // Paragraph-sized bursts and single lines, with a grow, a cap, a shrink,
        // and a tool wall appearing and going away in the middle of it.
        let script: Vec<(usize, usize)> = vec![
            (1, 0), // one line flushed
            (0, 3), // three lines of preview, nothing flushed
            (1, 6),
            (0, 9),
            (2, 12), // a burst arrives while the preview is long
            (0, 15),
            (1, 20), // preview beyond the cap
            (3, 26),
            (0, 33),
            (1, 2), // the answer closed; the region collapses back down
            (0, 0),
            (1, 4),
            (2, 9),
            (0, 1),
            (1, 0),
            (0, 30), // a wall of preview, then a wall of tools
            (0, 4),
        ];

        for (n_flush, preview) in script {
            let new: Vec<String> = (0..n_flush)
                .map(|i| format!("f{:03}", flushed.len() + i))
                .collect();
            flushed.extend(new.iter().cloned());

            let rows = live.rows().unwrap();
            let want = desired_height(mode, rows, preview, 0);
            let _ = live.fit(want).unwrap();
            if !new.is_empty() {
                let lines: Vec<Line<'static>> = new.iter().map(|l| Line::from(l.clone())).collect();
                live.insert_before(lines.len() as u16, |buf| {
                    Paragraph::new(lines.clone()).render(buf.area, buf)
                })
                .unwrap();
            }
            live.draw(paint_live).unwrap();

            let screen = rows_trimmed(live.backend());
            let top = live.top().unwrap() as usize;
            let h = live.height() as usize;
            assert!(
                top + h <= screen.len(),
                "the live region runs off the screen: {top} + {h} > {}",
                screen.len()
            );

            // The live band is the live band.
            for (y, row) in screen.iter().enumerate().skip(top).take(h) {
                assert!(
                    row.starts_with("live"),
                    "row {y} of the live region is {row:?}"
                );
            }
            // Above it: only flushed lines, in order, newest immediately above,
            // and blank above that (nothing was ever written there).
            let above = &screen[..top];
            let seen: Vec<&String> = above.iter().rev().filter(|r| r.starts_with('f')).collect();
            if !flushed.is_empty() {
                assert_eq!(
                    seen[0],
                    flushed.last().unwrap(),
                    "the row above the live region is not the newest flushed line"
                );
            }
            // Contiguous in the flushed sequence, and each appears at most once.
            let idx = |s: &String| flushed.iter().position(|f| f == s);
            let mut prev: Option<usize> = None;
            for row in seen.iter() {
                let here =
                    idx(row).unwrap_or_else(|| panic!("unknown row above the region: {row}"));
                // Walking *up* from the live region, the flushed index steps down
                // by one: the newest line sits nearest the pane.
                if let Some(p) = prev {
                    assert_eq!(
                        here + 1,
                        p,
                        "flushed lines above the region are out of order or duplicated \
                         ({} next to {})",
                        flushed[here],
                        flushed[p]
                    );
                }
                prev = Some(here);
            }
            let unique: std::collections::HashSet<&String> = seen.iter().cloned().collect();
            assert_eq!(
                unique.len(),
                seen.len(),
                "a flushed line appears twice on screen"
            );
            // Nothing else is above the live region: no live rows, no stray text.
            for row in above {
                assert!(
                    row.starts_with('f') || row.trim().is_empty(),
                    "unexpected content above the live region: {row:?}"
                );
            }
            // The flushed run is contiguous and sits directly on top of the live
            // region: everything from the first flushed row down to the viewport
            // is flushed lines, and blanks are only ever *above* it.
            if let Some(first) = above.iter().position(|r| r.starts_with('f')) {
                for row in &above[first..] {
                    assert!(
                        row.starts_with('f'),
                        "a gap inside the flushed run at {row:?}"
                    );
                }
            }
        }
    }
}
