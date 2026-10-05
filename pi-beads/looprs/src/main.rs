mod app;
mod components;
mod screen;
mod services;
mod session;
mod state;
mod teardown;
#[cfg(test)]
mod testing;
mod theme;
mod utils;
mod viewport;
use anyhow::Result;
use app::{App, Msg, UiCommand};
use components::input::InputState;
use crossterm::event::{Event, EventStream};
use crossterm::terminal::enable_raw_mode;
use futures::StreamExt;
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Rect, Size};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use components::text_stream::LiveTextPreview;
use components::tool::LiveToolPreview;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::session::router::{Router, SHUTDOWN_GRACE};
use crate::session::{ChatState, SessionConfig, TerminalType};
use crate::state::transcript::Entry;
use crate::teardown::{LiveAnchor, Teardown, install_panic_hook};

fn init_logging() -> anyhow::Result<WorkerGuard> {
    //let dir = std::env::temp_dir(); // or a proper data dir, e.g. via the `dirs` crate
    let dir = std::env::current_dir().unwrap();
    let appender = tracing_appender::rolling::never(&dir, "looprs.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        //.with_max_level(Level::DEBUG)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
        )
        .init();

    Ok(guard)
}

fn insert_lines(
    live: &mut viewport::LiveView<CrosstermBackend<Stdout>>,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    for chunk in lines.chunks(64) {
        //64 is arbitrary
        let height = chunk.len() as u16;
        live.insert_before(height, |buf| {
            Paragraph::new(chunk.to_vec()).render(buf.area, buf);
        })?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = init_logging()?; // keep alive until exit, or buffered logs are lost

    enable_raw_mode()?;
    // The teardown is built *before* the live view, and the hook goes in before
    // anything that can fail in raw mode. `LiveView::new` constructs a `Terminal`,
    // which queries the cursor and can error; a panic or an `?` in that window is
    // exactly what a terminal-left-in-raw-mode story is made of, so the way back
    // exists before the thing that can break does.
    //
    // The anchor is handed in rather than read out afterwards for the same reason:
    // it is the one fact the exit path needs about the screen, and it is published
    // by the live view from the first frame onward (see `viewport` /
    // `teardown::LiveAnchor`).
    let anchor = LiveAnchor::new();
    let exit = Arc::new(Teardown::new(anchor.clone()));
    install_panic_hook(exit.clone());

    let initial = TerminalType::Beeds;
    // The live region opens at the height the policy wants for an empty stream —
    // its chrome plus one row — instead of the constant 10 it used to be. From
    // here on the frame decides the shape; see `viewport`.
    let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
    let boot_h = viewport::desired_height(initial, rows, 0, 0, viewport::MIN_INPUT_ROWS);
    let mut live = viewport::LiveView::with_anchor(
        CrosstermBackend::new(io::stdout()),
        boot_h,
        |_| Ok(CrosstermBackend::new(io::stdout())),
        anchor,
    )?;
    let res = run(&mut live, initial, &exit).await;
    // The run loop restores the terminal itself, on its own exit path. This is the
    // net under every way of getting here that skipped it — an early `?` out of
    // `run`, most of which are terminal-write failures, which is precisely when
    // the terminal most needs taking back. `restore` is idempotent, so this is a
    // no-op when the loop already did the job, and the "exactly once" holds for
    // the two of them together rather than for each of them separately.
    exit.restore();
    // `res` is returned rather than swallowed: a run that failed is worth an exit
    // code, and the terminal is already safe by the time we get here to say so.
    res
}

async fn run(
    live: &mut viewport::LiveView<CrosstermBackend<Stdout>>,
    initial: TerminalType,
    exit: &Teardown,
) -> Result<()> {
    // all terminal UI events and events originating outside the app
    // come from cmd_tx and are received on cmd_rx
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(16);
    // any app state changes come from app_tx and are received on app_rx
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<Msg>();

    // The Router owns every backend from here: one session per terminal state, and
    // only this task's copy of `cmd_tx` can ask it for anything. The old shape —
    // `BeadsLoop::new(...)` hand-wired here, with a Tab being a border-color change
    // — is what looprs-05j replaces.
    let mut router = Router::new(initial, SessionConfig::default(), app_tx.clone());
    // The run loop keeps **no** sender of its own. This is not tidiness: the exit
    // drain ends when `app_rx` closes, and `app_rx` closes when the last sender
    // is gone. A copy held here is a sender that never goes away, so the drain
    // would sit out its whole timeout on every single quit — a 2.5s freeze on a
    // screen that has nothing left to receive — and "the sessions said their
    // piece" would stop being a fact the exit can observe.
    drop(app_tx);
    // Bring up the mode we open in *before* the App exists: the beads loop's first
    // pass runs now, so the input box opens in the right state instead of
    // flickering once the BeadStep message lands.
    router.boot().await?;
    // Sole owner of the sessions from here on. It never blocks on a child, so Esc
    // cannot queue behind somebody's model call.
    // The liveness edges fired during `boot()` arrived before this App existed, so
    // prime the open mode's view from the Router's own mirror. The status row and
    // the keyboard rule both read that view, and neither should have to guess what
    // state the session came up in — one `set_status` here is the same write the
    // missing message would have made.
    let boot = router
        .id_of(initial)
        .map(|id| (id, router.status_of(initial)));

    let mut app = App::new(
        cmd_tx,
        InputState::new(),
        initial,
        live.screen_size()?.width,
    );
    if let Some((id, status)) = boot {
        app.view_mut(id).set_status(status, Instant::now());
    }
    // Tell the sessions the size they are being shown at before anyone runs a
    // command. A Bash shell spawned later still inherits this: `BashTask::resize`
    // records the size even with no shell up yet, and uses it for the pty it
    // eventually opens.
    let sz = live.screen_size()?;
    app.forward_resize(sz.height, sz.width);

    // Sole owner of the sessions from here on: nothing after this point may touch a
    // backend except by sending the Router a command.
    let mut router_task = tokio::spawn(router.run(cmd_rx));

    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60 fps cap
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            //app_rx receives events that require state updates
            Some(ev) = app_rx.recv() => app.update(ev),
            Some(Ok(ev)) = keys.next() => {
                if let Event::Resize(w, h) = ev {
                    app.width = w;
                    if app.passthrough() {
                        // The child owns the real screen. Resizing our inline
                        // viewport now would query the cursor and clear a region
                        // we are not showing — onto the *child's* screen, in the
                        // middle of its frame. So the child gets the new size
                        // (ADR-0001 rule 6: it wraps for the window it is really
                        // shown in) and our own geometry waits to be rebuilt when
                        // the screen comes back.
                        app.reanchor = true;
                        app.forward_resize(h, w);
                    } else {
                        // …and the children get it too. A pty sized 80x24 while the
                        // window is 180x50 wraps every program's output for a terminal
                        // that is not there (ADR-0001 rule 6).
                        app.forward_resize(h, w);
                        // The frame's own size poll may have applied this resize
                        // already — it runs whether or not the key stream was
                        // alive when the window changed — and reshaping twice buys
                        // nothing but a second cursor read and a second clear.
                        if !live.sees_window(Size::new(w, h)) {
                            drop(keys);                           // stop reading stdin
                            // `resize_window` also marks the live region's anchor
                            // unknown: the viewport moved and we will only learn
                            // where from the next frame's area, and `fit` refuses
                            // to rebuild onto a guess.
                            if let Err(e) = live.resize_window(Rect::new(0, 0, w, h)) {
                                // don't kill the session over a failed re-anchor
                                tracing::warn!("resize failed: {e}");
                            }
                            keys = EventStream::new();            // resume
                        }
                    }
                } else {
                    app.update(Msg::Term(ev));
                }
            }
            _ = tick.tick() => {
                app.update(Msg::Tick); //spinner only atm
                // Notice a window resize the key stream never delivered. A
                // `SIGWINCH` is not a byte on stdin, so one that arrives while the
                // stream is stopped — around a rebuild, which has to stop it — is
                // gone for good, and ratatui's fallback is to read the cursor back
                // mid-draw from its own `autoresize`, the same race against the
                // same stream. Asking the backend for its size is an ioctl with no
                // round trip, so this settles the question cheaply before any draw
                // and leaves nothing for `autoresize` to notice.
                let sz = live.screen_size()?;
                if !live.sees_window(sz) && !app.passthrough() {
                    app.width = sz.width;
                    app.forward_resize(sz.height, sz.width);
                    drop(keys);
                    if let Err(e) = live.resize_window(Rect::new(0, 0, sz.width, sz.height)) {
                        tracing::warn!("resize failed: {e}");
                    }
                    keys = EventStream::new();
                }
                if app.reanchor {
                    // The full-screen child let go of the terminal. Re-anchor the
                    // inline viewport at where the cursor actually is now and force
                    // a full repaint of it: ratatui's diff still describes the
                    // screen as it was before the child painted over it, and
                    // trusting that is ADR-0001's "screen is garbled after exiting
                    // vim" bug. Resizing to the real size recomputes the viewport,
                    // clears it and resets the back buffer, which is the same job
                    // as recreating the Terminal without dropping the borrow.
                    //
                    // The key stream is stopped across it because a re-anchor reads
                    // the cursor position back, and the async reader would eat the
                    // answer — exactly why the resize arm above does the same.
                    let sz = live.screen_size()?;
                    drop(keys);
                    live.anchor_lost();
                    if let Err(e) = live.resize_window(Rect::new(0, 0, sz.width, sz.height)) {
                        tracing::warn!("re-anchor after the full-screen program failed: {e}");
                    }
                    keys = EventStream::new();
                    app.reanchor = false;
                    app.dirty = true;
                }
                // The gate: while a child holds the screen we draw nothing at all,
                // and the bytes are going out from `App::update` instead. Drawing
                // over a program that believes it owns the terminal is the bug this
                // whole path exists to fix, so it is blocked here rather than
                // trusted to be absent.
                if app.dirty && !app.passthrough() {
                    // The per-frame sequence is fit -> flush -> insert_before -> draw,
                    // and only for the ACTIVE view (ADR-0002 Q5). A hidden view
                    // buffers; its backlog goes out as one burst when you switch to it.
                    //
                    // `fit` comes first because it re-anchors against the row the last
                    // frame reported; letting `insert_before` move the viewport first
                    // would make it anchor on a row that has already changed. Growing
                    // is where the extra screen the flushed lines were not using gets
                    // spent, which is the whole point of looprs-afw.
                    let lines = app.flush_active(app.width);
                    let preview = app.preview_active(app.width);
                    let tools = app.live_tool_rows();
                    // Computed once and handed to both the height policy and the
                    // frame, for the same reason `preview` is: the rows the box
                    // asked for and the rows it is drawn with must be one number,
                    // or the box grows a row of blank space (or loses a row of
                    // typed text) every time the two disagree.
                    let input = app.input_rows(app.width);
                    let want = viewport::desired_height(
                        app.active,
                        live.rows()?,
                        preview.len(),
                        tools,
                        input,
                    );
                    // Rebuilding an inline viewport reads the cursor position
                    // back, and the async key reader would eat the answer — the
                    // same reason the resize and re-anchor arms above stop the
                    // stream first. So the stream is stopped only when the shape
                    // is actually going to change, not every frame.
                    if live.needs_fit(want) {
                        drop(keys);
                        // A resize that fails (the rebuild reads the cursor back,
                        // so it is the one step here that can time out) is logged
                        // inside `fit` and left with the old height, which keeps
                        // `needs_fit` true: the next frame tries again. A shape
                        // that will not change is not worth the session.
                        let _ = live.fit(want);
                        keys = EventStream::new();
                    }
                    insert_lines(live, lines)?;
                    // A frame that could not be drawn is not a reason to take the
                    // session down with it. `try_draw` fails inside `autoresize`,
                    // before it has swapped buffers or flushed anything, so there
                    // is nothing inconsistent to recover from: leave `dirty` set
                    // and the next frame tries again.
                    if let Err(e) = live.draw(|f| view(&app, f, &preview, input)) {
                        tracing::warn!("frame not drawn: {e}");
                    } else {
                        app.dirty = false;
                    }
                }
            }
        }
        if app.should_quit {
            break;
        }
    }

    // ── exit ─────────────────────────────────────────────────────────────────
    //
    // Six steps, in this order, because every one of them is here to stop a
    // specific way of losing something (looprs-ecr). The shared `exit` object is
    // the same one the panic hook holds, so steps (4) and (5) cannot drift from
    // what a crash does — see `crate::teardown`.
    //
    //   1. stop accepting input
    //   2. tell every session to shut down
    //   3. drain what they say into the scrollback, bounded
    //   4. clear the live pane            ┐
    //   5. raw mode off, one newline     ┘ `exit.restore()`, exactly once
    //   6. bound-wait on the session tasks
    //
    // (1) The key stream goes first because it is the one thing in this function
    // that can steal a terminal reply. Its reader thread is parked on the same
    // stdin everything else reads, and any escape-sequence answer it takes is a
    // two-second timeout somewhere else. Nothing below queries the cursor — that
    // rule is what makes this list safe — but stopping the reader costs nothing
    // and it makes the rule independent of whoever adds the next line here.
    drop(keys);
    drop(tick);

    // (2) A command, not a `drop(app)`. Closing the channel does shut the
    // sessions down, but it also closes the only reader of what they say next:
    // the tail of a streamed answer that was still in the live preview region,
    // and the `SessionDown` that seals each transcript. Losing that is the
    // headline bug this ticket was filed for.
    app.request_shutdown().await;

    // (3) Bounded, because "until they are done talking" is not a bound: a child
    // that streams forever is a `yes | cat` away from an app that never exits.
    // Everything that was on screen goes out *before* the wait, so a quit
    // mid-stream is safe in scrollback immediately and does not sit on the
    // sessions' goodwill.
    let drain_budget = SHUTDOWN_GRACE + Duration::from_millis(500);
    if tokio::time::timeout(drain_budget, drain_sessions(&mut app, live, &mut app_rx))
        .await
        .is_err()
    {
        tracing::warn!(
            "sessions were still talking after {drain_budget:?}; leaving them where they are"
        );
    }

    // (4) + (5) The pane goes, raw mode comes off, the line closes. No cursor
    // query anywhere in it: it erases from the anchor the live view has been
    // publishing, which `insert_before` above follows downward as it pushes.
    exit.restore();

    // (6) The router task is the task that waited on every pump, so joining it
    // joins the shutdown. It has already bounded itself at `SHUTDOWN_GRACE` with
    // `abort()` past that; this is the net under the whole thing, and past *this*
    // the task is cut rather than waited on, because the user has already pressed
    // the key that said "leave".
    let join_budget = Duration::from_secs(1);
    if tokio::time::timeout(join_budget, &mut router_task)
        .await
        .is_err()
    {
        tracing::warn!("router task did not finish within {join_budget:?}; cutting it");
        router_task.abort();
    }
    Ok(())
}

/// The exit drain: everything the sessions say on the way out, written above the
/// live pane before the pane disappears (looprs-ecr step 3).
///
/// Note the order inside the loop — **flush, then wait**. Flushing only after a
/// message arrives would mean the text already on screen when the user pressed
/// Ctrl-Q has to wait for the sessions to finish before it is safe, and a quit
/// during a long silence would then be a quit with nothing drained at all. Flushing
/// first means "what was on screen is in the scrollback" is true within one
/// iteration of this loop, whatever the children decide to do afterwards.
///
/// Only the active view is drained. A hidden view's backlog is not on the screen,
/// so nothing about it is "lost" by the exit — and dumping a session the user
/// walked away from into their scrollback at the worst possible moment, one
/// message at a time, is its own kind of noise. It is dropped with the view.
async fn drain_sessions(
    app: &mut App,
    live: &mut viewport::LiveView<CrosstermBackend<Stdout>>,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
) {
    loop {
        let lines = app.flush_active(app.width);
        if !lines.is_empty() && insert_lines(live, lines).is_err() {
            // The screen stopped accepting output. There is no point trying to
            // drain anything further, and no point taking the app down over it
            // either: the restore that follows is what matters now.
            tracing::warn!("the final flush never reached the screen; leaving the rest undrained");
            return;
        }
        match rx.recv().await {
            Some(msg) => app.update(msg),
            // Every sender is gone: each session got its parting word out, or had
            // its pump cut after the grace period. Either way this is the end.
            None => return,
        }
    }
}

/// Pure function of state (the tail preview re-parses only the open block).
///
/// `preview` is the active view's live tail, rendered once by the caller and shared
/// with the height policy so the number that sized this frame and the lines drawn
/// into it are the same value, not two renders that might disagree.
///
/// `input_rows` is shared for the same reason: it is the height the box asked for
/// when the frame was sized, so the box is drawn into the rows it was promised and
/// not into a re-derived guess.
fn view(app: &App, f: &mut Frame, preview: &[Line<'static>], input_rows: u16) {
    let active = app.active_view();
    let tools: Vec<&Entry> = active
        .map(|v| {
            v.transcript
                .open_tools()
                .take(viewport::MAX_TOOL_ROWS as usize)
                .collect()
        })
        .unwrap_or_default();
    let [text_area, tool_area, status_area, input] =
        viewport::frame_areas(f.area(), tools.len() as u16, input_rows);

    // What the live region shows is a property of the session on screen, not of any
    // session that happens to be streaming.
    if matches!(app.chat_state(), ChatState::Chat) {
        f.render_widget(LiveTextPreview::new(app.spinner, preview), text_area);
    }

    for (i, e) in tools.iter().enumerate() {
        let row = Rect {
            y: tool_area.y + i as u16,
            height: 1,
            ..tool_area
        };
        f.render_widget(LiveToolPreview::new(e, app.spinner), row);
    }

    // The status row (looprs-guh): the row `frame_areas` has been reserving and
    // nothing drew into. Drawn unconditionally — every state, including "no view,
    // no session, no idea", has an answer worth showing, and a row that is only
    // drawn when there is something to report is a row that is missing exactly when
    // it is needed. `status_line` has already cut itself to this area's width, so
    // there is nothing here to wrap and no reason for the row to reflow anything.
    f.render_widget(
        Paragraph::new(app.status_line(status_area.width)),
        status_area,
    );

    // input
    if app.need_input() {
        app.input.render(f, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{BeadStep, SessionId, SessionStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;

    /// Read the backend's screen back as trimmed rows of text.
    fn rows(b: &TestBackend) -> Vec<String> {
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

    /// A rendered-able app whose active mode is `busy` iff `need_input` is false.
    ///
    /// Busy-ness is spelled `Running` because that is what the keyboard rule reads
    /// — see the mode x liveness table in `session::view`. Which means a *Bash*
    /// app cannot be built without an input box from here, and no test wants to:
    /// Bash is the mode that is always open.
    fn app(mode: TerminalType, need_input: bool) -> App {
        let (tx, _rx) = mpsc::channel::<UiCommand>(4);
        let mut app = App::new(tx, InputState::new(), mode, 60);
        if !need_input {
            app.view_mut(SessionId::new(mode, 1))
                .set_status(SessionStatus::Running, Instant::now());
        }
        app
    }

    fn paint(app: &App, h: u16) -> Vec<String> {
        paint_with(app, h, viewport::MIN_INPUT_ROWS)
    }

    /// As [`paint`], with the box given `input_rows` — the same value the height
    /// policy was asked to size the frame for.
    fn paint_with(app: &App, h: u16, input_rows: u16) -> Vec<String> {
        let backend = TestBackend::new(60, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| view(app, f, &[], input_rows)).unwrap();
        rows(term.backend())
    }

    /// The bug was a row that `frame_areas` reserved and `view` left empty. So:
    /// paint the whole frame and read the band back off the screen. Nothing else
    /// on the row matters if there is nothing **on** the row.
    #[test]
    fn the_status_row_is_painted_into_the_band_the_layout_reserved_for_it() {
        let app = app(TerminalType::Beeds, true);
        let [_, _, status, _] =
            viewport::frame_areas(Rect::new(0, 0, 60, 12), 0, viewport::MIN_INPUT_ROWS);
        let screen = paint(&app, 12);
        let row = &screen[status.y as usize];
        assert_eq!(status.height, 1);
        assert!(
            row.contains("Beeds") && row.contains("not started"),
            "the reserved row came back empty: {screen:?}"
        );
    }

    /// The wiring, not just the arithmetic: `view` must spend the `input_rows` it
    /// was handed, so a long message is actually painted across the rows the policy
    /// gave the box — with the status row keeping its place directly above it.
    #[test]
    fn the_box_is_painted_over_the_rows_the_policy_gave_it() {
        let mut app = app(TerminalType::Pi, true);
        app.input
            .set_text(format!("{} THE-END", "word ".repeat(20)));
        let want = app.input_rows(60);
        let h = viewport::desired_height(TerminalType::Pi, 40, 0, 0, want);
        let [_, _, status, input] = viewport::frame_areas(Rect::new(0, 0, 60, h), 0, want);
        assert!(
            input.height > viewport::MIN_INPUT_ROWS,
            "the box did not grow: {input:?}"
        );

        let screen = paint_with(&app, h, want);
        let band: String = screen[input.y as usize..input.bottom() as usize].concat();
        assert!(
            band.contains("THE-END"),
            "the tail of the message never reached the box: {screen:?}"
        );
        assert_eq!(status.bottom(), input.top());
        assert!(screen[status.y as usize].contains("Pi"), "{screen:?}");
    }

    /// The row is there when there is no input box to share the frame with. A
    /// beads pass that has taken the keyboard away is exactly when the row is
    /// load-bearing, and a row that only paints alongside the box would be dark.
    #[test]
    fn the_status_row_is_painted_even_with_no_input_box() {
        let mut app = app(TerminalType::Beeds, false);
        app.update(Msg::BeadStep {
            session: SessionId::new(TerminalType::Beeds, 1),
            step: BeadStep::WorkTickets,
        });
        app.update(Msg::SessionStatus {
            session: SessionId::new(TerminalType::Beeds, 1),
            status: SessionStatus::Running,
        });
        let [_, _, status, input] =
            viewport::frame_areas(Rect::new(0, 0, 60, 12), 0, viewport::MIN_INPUT_ROWS);
        let screen = paint(&app, 12);
        assert!(screen[status.y as usize].contains("working"), "{screen:?}");
        // …and the box really is gone, so the row is not being confused with it.
        let box_rows = &screen[input.y as usize..input.bottom() as usize];
        assert!(
            box_rows.iter().all(|r| r.trim().is_empty()),
            "the input box was drawn when it should not have been: {screen:?}"
        );
    }

    /// A row per frame, and one row only: it must never spill into the live text
    /// above it or the input below it. That is a reflow, and the ticket forbids it.
    #[test]
    fn the_status_row_stays_on_its_own_line_at_any_terminal_height() {
        let mut app = app(TerminalType::Bash, true);
        app.update(Msg::Error {
            session: Some(SessionId::new(TerminalType::Bash, 1)),
            text: "spawn failed: bash not found on PATH".into(),
        });
        for h in 5u16..=20 {
            let [_, _, status, _] =
                viewport::frame_areas(Rect::new(0, 0, 60, h), 0, viewport::MIN_INPUT_ROWS);
            let screen = paint(&app, h);
            let band = &screen[status.y as usize];
            assert!(band.contains("Bash"), "h={h}: {screen:?}");
            assert!(band.contains("✗"), "h={h}: {screen:?}");
            // Nothing of the row leaked into the neighbouring bands.
            if (status.y as usize) > 0 {
                assert!(
                    !screen[status.y as usize - 1].contains("spawn failed"),
                    "h={h}: leaked upward: {screen:?}"
                );
            }
            if status.bottom() < h {
                assert!(
                    !screen[status.bottom() as usize].contains("spawn failed"),
                    "h={h}: leaked downward: {screen:?}"
                );
            }
        }
    }
}
