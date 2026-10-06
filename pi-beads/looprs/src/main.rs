mod app;
mod components;
mod screen;
mod services;
mod session;
mod signals;
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

use components::card::LiveCardPreview;
use components::text_stream::LiveTextPreview;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::services::notification;
use crate::session::router::{Router, SHUTDOWN_GRACE};
use crate::session::{ChatState, SessionConfig, TerminalType};
use crate::signals::ExitSignals;
use crate::state::transcript::Entry;
use crate::teardown::{LiveAnchor, Mode, Teardown, install_panic_hook, panic_injected};

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

    // The teardown is built before anything that can switch a terminal mode on,
    // and the panic hook goes in before anything that can fail with one switched
    // on. From this line to the end of the process there is an object that knows
    // which modes are ours and how to hand them back — which is the only way the
    // guarantee (exactly once, from every path) can be true of the whole app and
    // not just of the paths that remembered to arrange it.
    //
    // The anchor is handed in rather than read out afterwards for the same reason:
    // it is the one fact the exit path needs about the screen, and it is published
    // by the live view from the first frame onward (see `viewport` /
    // `teardown::LiveAnchor`).
    let anchor = LiveAnchor::new();
    let exit = Arc::new(Teardown::new(anchor.clone()));
    install_panic_hook(exit.clone());

    // Every way in to the terminal state lives behind this one call, and the hand-
    // back sits in front of its result. That ordering is the point: before it, an
    // early `?` out of setup — `LiveView::new` asking the cursor where it is and
    // timing out is the real-world one — returned from `main` with raw mode on and
    // nothing left to turn it off. Now there is.
    let res = app(anchor, exit.clone()).await;
    // `restore` is idempotent, so this is a no-op when the run loop or the panic
    // hook already did the job, and the "exactly once" holds for the whole set of
    // callers rather than for each of them separately.
    exit.restore();
    // `res` is returned rather than swallowed: a run that failed is worth an exit
    // code, and the terminal is already safe by the time we get here to say so.
    res
}

/// The app proper: the live view, the Router, the run loop.
///
/// Split out of `main` so that `main`'s tail — the unconditional `restore` — is
/// on the path of every `?` this function contains.
async fn app(anchor: LiveAnchor, exit: Arc<Teardown>) -> Result<()> {
    // The modes this app switches on, switched on *through the ledger*.
    //
    // This is the only place a terminal mode gets turned on, which is what makes
    // "the ledger knows every mode we hold" a fact rather than a hope: `startup_set`
    // is the whole list, and `restore` hands back exactly that list. Raw mode goes
    // through here too for that reason — it is not a byte string, it is
    // `tcsetattr`, and it is still something we do to the user's tty.
    //
    // Parsed before any of it is applied, so an unknown name in `LOOPRS_MODES`
    // stops the app with a message instead of half-starting with half a mode set.
    for mode in Mode::startup_set().map_err(anyhow::Error::msg)? {
        exit.enable(mode)?;
    }

    let initial = TerminalType::Beeds;
    // The live region opens at the height the policy wants for an empty stream —
    // its chrome plus one row — instead of the constant 10 it used to be. From
    // here on the frame decides the shape; see `viewport`.
    let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
    let boot_h = viewport::desired_height(initial, rows, 0, 0, viewport::MIN_INPUT_ROWS);
    // `ManuallyDrop`, deliberately: ratatui's `Terminal::drop` shows the cursor,
    // and a cursor is a ledgered mode (`Mode::CursorHidden`). A destructor that
    // switches a terminal mode is a mode switch that happens after the teardown,
    // outside the ledger, in an order nobody controls and nothing can make
    // once-only — the `?25h` it writes used to land *after* the closing newline,
    // which is the tail the shutdown spike had to forgive.
    //
    // Holding the view here instead means the destructor never runs: the cursor
    // comes back from the ledger, in the ledger's order, exactly once, on every
    // path including the panic one. What is not freed is the view's two cell
    // buffers, in a process that is on its way out; that is the whole cost, and
    // it is paid once.
    let mut live = std::mem::ManuallyDrop::new(viewport::LiveView::with_anchor(
        CrosstermBackend::new(io::stdout()),
        boot_h,
        |_| Ok(CrosstermBackend::new(io::stdout())),
        anchor,
    )?);

    // The signals are installed before the loop, not inside it: a `SIGHUP` that
    // arrives while the handlers are still the default ones is a process killed
    // with the ledger still holding everything it switched on.
    let mut signals = ExitSignals::install()?;

    run(&mut live, initial, &exit, &mut signals).await
}

async fn run(
    live: &mut viewport::LiveView<CrosstermBackend<Stdout>>,
    initial: TerminalType,
    exit: &Teardown,
    signals: &mut ExitSignals,
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
    // Out-of-band notification: a ticket this harness finished reaches a human who
    // is not looking at this terminal. Built here, once, before the Router, so the
    // poster task outlives every session — a beads session gets respawned per
    // generation and parked on Tab, and a notifier that died and returned along
    // with it would drop the announcement that was timed worst. This is the only
    // place the real sink is built: `SessionConfig::default()` carries `Noop`,
    // which is what keeps ~300 tests off the network by construction rather than
    // by nobody remembering to unset `LOOPRS_NTFY_URL`.
    let cfg = SessionConfig {
        notifier: notification::notifier_from_env(),
        ..SessionConfig::default()
    };
    let mut router = Router::new(initial, cfg, app_tx.clone());
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
    // The passthrough reports straight into the teardown's record of what the
    // real terminal got switched into. This is the whole wiring between "a `vim`
    // took the screen through us" and "the exit path puts the screen back": one
    // shared handle, and the bytes themselves are the report. It goes here, before
    // any session exists, because the first full-screen program a user runs should
    // not be the one that discovers the wire was never connected.
    app.set_screen_debt(exit.screen_debt());
    // The other half of the same wire: what to write when a child hands the screen
    // *back*. A mouse-tracking vim switches our `?1000/?1002/?1006` off on the
    // way out (looprs-pdl.2 #7), and the app would carry on as if it still had
    // them. The list comes from the ledger's own held set so the App is not
    // keeping a second guess at the mode set, and it excludes the alternate screen
    // itself: re-sending `?1049h` while already in it would save the current
    // contents as the user's main screen, which is not ours to lose.
    app.set_reassert_bytes(exit.reassert_bytes());
    // …and which screen that hand-back happens on. Same source as the ledger's
    // own startup list, so "who owns the alternate screen" is one answer shared
    // by the session that cuts the child's switches, the App that repaints, and
    // the exit that leaves the screen for good.
    app.set_alt_screen_hosted(Mode::alt_screen_claimed());
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
                    let cards = app.live_card_rows();
                    // Computed once and handed to both the height policy and the
                    // frame, for the same reason `preview` is: the rows the box
                    // asked for and the rows it is drawn with must be one number,
                    // or the box grows a row of blank space (or loses a row of
                    // typed text) every time the two disagree. This is also where
                    // a hidden box costs nothing: `input_band` returns zero rows
                    // for a session that has taken the keyboard, so the status row
                    // ends the live region instead of hanging above three blank
                    // rows where no box was drawn.
                    let input = app.input_band(app.width);
                    let want = viewport::desired_height(
                        app.active,
                        live.rows()?,
                        preview.len(),
                        cards,
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
                    if let Err(e) = live.draw(|f| {
                        // Fault injection for the exit contract (looprs-pdl.3):
                        // a panic that happens *inside* the frame, which is where
                        // the frame code is doing its damage and where a teardown
                        // that does not run is most visible. The panic hook holds
                        // the same `Teardown` this loop does, so what the spike
                        // sees is the same hand-back Ctrl-Q gets, not a second
                        // one written for the occasion.
                        if panic_injected() {
                            panic!(
                                                                "deliberate panic inside the draw (LOOPRS_PANIC=draw)"

                            );
                        }
                        view(&app, f, &preview, input)
                    }) {
                        tracing::warn!("frame not drawn: {e}");
                    } else {
                        app.dirty = false;
                    }
                }
            }
            // A signal from outside the terminal. It means the same thing Ctrl-Q
            // means and takes the same road: `should_quit` ends the loop below, and
            // the loop's own six-step exit hands the terminal back with every mode
            // on it. There is no second shutdown path for signals, because a
            // second shutdown path is a second set of promises about the terminal
            // and those two can disagree — which is the bug `looprs-ecr` removed.
            //
            // Ctrl-Q drains the sessions and repaints first because a human pressed
            // a key and can wait; a `SIGHUP` is a window closing and there is
            // nobody left to watch either way, so both get the same treatment and
            // the same bounded budget rather than a special case that is tested
            // less than the one people use.
            term = signals.recv() => {
                tracing::warn!(
                    "{term} received; taking the same way out as Ctrl-Q and giving the terminal back"
                );
                app.should_quit = true;
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

    // (4) + (5) The pane goes, every mode that was switched on comes back off,
    // raw mode with them, and the line closes. No cursor query anywhere in it: it
    // erases from the anchor the live view has been publishing, which
    // `insert_before` above follows downward as it pushes. See `crate::teardown`
    // for the ledger and for why none of it asks the terminal a question.
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
/// when the frame was sized — or [`viewport::NO_INPUT_ROWS`] when the active
/// session is not taking input — so the box is drawn into the rows it was promised
/// and not into a re-derived guess.
fn view(app: &App, f: &mut Frame, preview: &[Line<'static>], input_rows: u16) {
    let active = app.active_view();
    let cards: Vec<&Entry> = active
        .map(|v| {
            v.transcript
                .open_cards()
                .take(viewport::MAX_TOOL_ROWS as usize)
                .collect()
        })
        .unwrap_or_default();
    let [text_area, card_area, status_area, input] =
        viewport::frame_areas(f.area(), cards.len() as u16, input_rows);

    // What the live region shows is a property of the session on screen, not of any
    // session that happens to be streaming.
    if matches!(app.chat_state(), ChatState::Chat) {
        f.render_widget(LiveTextPreview::new(app.spinner, preview), text_area);
    }

    for (i, e) in cards.iter().enumerate() {
        let row = Rect {
            y: card_area.y + i as u16,
            height: 1,
            ..card_area
        };
        f.render_widget(LiveCardPreview::new(e, app.spinner), row);
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

    // input. `App::input_band` — the value `input_rows` was built from — is zero
    // for a session that has taken the keyboard, so the box is never drawn when it
    // is not wanted, and never drawn outside the band the height policy paid for.
    if app.need_input() && !input.is_empty() {
        app.input.render(f, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PiEvent;
    use crate::session::{BeadStep, SessionId, SessionStatus};
    use crate::state::transcript::MessageKind;
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

    /// The frame the run loop actually builds while the session holds the keyboard:
    /// the box's band is granted nothing (`App::input_band`), so the status row is
    /// the live region's last row instead of hanging above three rows of blank
    /// screen where a box would have been. Every row under it has to be gone, not
    /// just empty — an empty band still pushes the row up.
    #[test]
    fn a_hidden_box_leaves_the_status_row_on_the_last_row_of_the_screen() {
        let mut app = app(TerminalType::Beeds, false);
        app.update(Msg::BeadStep {
            session: SessionId::new(TerminalType::Beeds, 1),
            step: BeadStep::WorkTickets,
        });
        app.update(Msg::SessionStatus {
            session: SessionId::new(TerminalType::Beeds, 1),
            status: SessionStatus::Running,
        });
        let band = app.input_band(60);
        assert_eq!(
            band,
            viewport::NO_INPUT_ROWS,
            "a session that took the keyboard asks for no box rows"
        );

        let h = viewport::desired_height(TerminalType::Beeds, 40, 1, 0, band);
        let [_, _, status, input] = viewport::frame_areas(Rect::new(0, 0, 60, h), 0, band);
        assert_eq!(input.height, 0, "the hidden box kept rows: {input:?}");
        assert_eq!(status.bottom(), h, "the status row does not end the frame");

        let screen = paint_with(&app, h, band);
        assert_eq!(screen.len(), h as usize);
        assert!(
            screen[h as usize - 1].contains("working"),
            "the bottom row is not the status row: {screen:?}"
        );
        // The frame ends where the status row ends: there is no band of blank rows
        // under it to push it up.
        assert_eq!(
            screen.len(),
            status.bottom() as usize,
            "rows are left under the status row: {screen:?}"
        );
    }

    /// …and hiding it is not permanent: the moment the session hands the keyboard
    /// back the box's rows come back with it, so the status row only hugs the
    /// bottom while there is genuinely nothing under it.
    #[test]
    fn the_status_row_gives_the_rows_back_when_the_box_reopens() {
        let mut app = app(TerminalType::Beeds, false);
        let id = SessionId::new(TerminalType::Beeds, 1);
        app.view_mut(id)
            .set_status(SessionStatus::Running, Instant::now());
        assert_eq!(app.input_band(60), viewport::NO_INPUT_ROWS);

        app.view_mut(id)
            .set_status(SessionStatus::Idle, Instant::now());
        assert!(app.need_input(), "idle: the box is open again");
        let band = app.input_band(60);
        assert_eq!(band, app.input_rows(60), "the box did not come back whole");
        let h = viewport::desired_height(TerminalType::Beeds, 40, 1, 0, band);
        let [_, _, status, input] = viewport::frame_areas(Rect::new(0, 0, 60, h), 0, band);
        assert_eq!(input.height, band);
        assert_eq!(status.bottom(), input.top());
        assert!(
            status.bottom() < h,
            "the row should not be at the bottom now"
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

    /// The card is not merely *recorded*, it is drawn: with a compaction in flight
    /// the live region carries its row, so the screen says what the run is doing
    /// instead of stopping.
    #[test]
    fn a_live_compaction_is_drawn_in_the_card_band() {
        let mut app = app(TerminalType::Pi, true);
        app.update(Msg::Agent {
            session: SessionId::new(TerminalType::Pi, 1),
            event: PiEvent::CompactionStart {
                reason: "threshold".into(),
            },
        });

        let band = app.input_band(60);
        let rows = app.live_card_rows();
        assert_eq!(
            rows, 1,
            "the frame is told there is a card to make room for"
        );
        let h = viewport::desired_height(TerminalType::Pi, 40, 0, rows, band);
        let [_, cards, _, _] = viewport::frame_areas(Rect::new(0, 0, 60, h), rows, band);

        let screen = paint_with(&app, h, band);
        let drawn = &screen[cards.y as usize..cards.bottom() as usize];
        assert!(
            drawn
                .iter()
                .any(|r| r.contains("compacting context") && r.contains("threshold")),
            "the card band has no compaction row: {screen:?}"
        );
        assert!(
            !drawn.iter().any(|r| r.contains("✗")),
            "a compaction in progress is not a failure: {screen:?}"
        );
    }

    /// A cancel is drawn as a cancel — grey, ⊘ — and a failure as a failure. The
    /// frame is where that distinction is allowed to be seen, so it has to survive
    /// the render, not just the enum.
    #[test]
    fn a_cancelled_and_a_failed_compaction_are_drawn_differently() {
        let cancel = compaction_row(|mut e| {
            e.kind = MessageKind::Compaction {
                reason: "manual".into(),
                state: crate::components::compaction::CompactionState::Aborted,
            };
            e
        });
        let failure = compaction_row(|mut e| {
            e.kind = MessageKind::Compaction {
                reason: "overflow".into(),
                state: crate::components::compaction::CompactionState::Failed,
            };
            e.text = "provider refused the summary".into();
            e
        });

        assert!(
            cancel.contains("⊘") && cancel.contains("aborted"),
            "{cancel}"
        );
        assert!(failure.contains("✗"), "{failure}");
        assert!(failure.contains("provider refused"), "{failure}");
        assert_ne!(cancel, failure);
    }

    /// One open card of `kind`, rendered through the frame's own widget, returned
    /// as the row's text. Built by hand rather than by a scripted event so the two
    /// endings above can be compared without a session between them.
    fn compaction_row(build: impl Fn(Entry) -> Entry) -> String {
        use crate::components::card::LiveCardPreview;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let entry = build(Entry {
            kind: MessageKind::System,
            text: String::new(),
            done: false,
            styles: Vec::new(),
        });
        let backend = TestBackend::new(60, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(LiveCardPreview::new(&entry, 0), f.area());
        })
        .unwrap();
        rows(term.backend()).first().cloned().unwrap_or_default()
    }
}
