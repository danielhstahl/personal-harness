mod app;
mod components;
#[cfg(test)]
mod measure;
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
use ratatui::widgets::Paragraph;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use components::card::LiveCardPreview;
use components::selection::SelectionHighlight;
use components::text_stream::{NewRowsPill, TranscriptBand, band_layout};
use components::toast::ToastOverlay;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::services::clipboard;
use crate::services::notification;
use crate::session::router::{Router, SHUTDOWN_GRACE};
use crate::session::{ChatState, SessionConfig, TerminalType};
use crate::signals::ExitSignals;
use crate::state::selection::BandSnapshot;
use crate::state::transcript::Entry;
use crate::teardown::{Mode, Teardown, install_panic_hook, panic_injected};

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
    // It takes no argument any more. The inline pane needed the live region's top
    // row published to it so the exit could erase downward from a known row; the
    // full-screen frame leaves nothing to erase, because `?1049l` is the whole
    // hand-back (ADR-0004 R1, ADR-0006).
    let exit = Arc::new(Teardown::new());
    install_panic_hook(exit.clone());

    // Every way in to the terminal state lives behind this one call, and the hand-
    // back sits in front of its result. That ordering is the point: before it, an
    // early `?` out of setup returned from `main` with raw mode on and nothing
    // left to turn it off. Now there is.
    let res = app(exit.clone()).await;
    // `restore` is idempotent, so this is a no-op when the run loop or the panic
    // hook already did the job, and the "exactly once" holds for the whole set of
    // callers rather than for each of them separately.
    exit.restore();
    // `res` is returned rather than swallowed: a run that failed is worth an exit
    // code, and the terminal is already safe by the time we get here to say so.
    res
}

/// The app proper: the frame, the Router, the run loop.
///
/// Split out of `main` so that `main`'s tail — the unconditional `restore` — is
/// on the path of every `?` this function contains.
async fn app(exit: Arc<Teardown>) -> Result<()> {
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

    // The whole window, taken after the alternate screen is on: every byte the
    // frame writes from here lands on the screen we own, and none of it lands on
    // the user's scrollback on the way in.
    //
    // `ManuallyDrop`, deliberately: ratatui's `Terminal::drop` shows the cursor,
    // and a cursor is a ledgered mode (`Mode::CursorHidden`). A destructor that
    // switches a terminal mode is a mode switch that happens after the teardown,
    // outside the ledger, in an order nobody controls and nothing can make
    // once-only — the `?25h` it writes used to land *after* the closing newline,
    // which is the tail the shutdown spike had to forgive.
    //
    // Holding the frame here instead means the destructor never runs: the cursor
    // comes back from the ledger, in the ledger's order, exactly once, on every
    // path including the panic one. What is not freed is the frame's two cell
    // buffers, in a process that is on its way out; that is the whole cost, and
    // it is paid once.
    let mut frame = std::mem::ManuallyDrop::new(viewport::ScreenFrame::full(
        CrosstermBackend::new(io::stdout()),
    )?);

    // The signals are installed before the loop, not inside it: a `SIGHUP` that
    // arrives while the handlers are still the default ones is a process killed
    // with the ledger still holding everything it switched on.
    let mut signals = ExitSignals::install()?;

    run(&mut frame, TerminalType::Beeds, &exit, &mut signals).await
}

async fn run(
    frame: &mut viewport::ScreenFrame<CrosstermBackend<Stdout>>,
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
    // only this task's copy of `cmd_tx` can ask it for anything.
    //
    // Out-of-band notification: a ticket this harness finished reaches a human who
    // is not looking at this terminal. Built here, once, before the Router, so the
    // poster task outlives every session — a beads session gets respawned per
    // generation and parked on Tab, and a notifier that died and returned along
    // with it would drop the announcement that was timed worst. This is the only
    // place the real sink is built: `SessionConfig::default()` carries `Noop`,
    // which is what keeps ~300 tests off the network by construction rather than
    // by nobody remembering to unset `LOOPRS_NTFY_URL`.
    let clipboard = clipboard::clipboard_from_env();
    let cfg = SessionConfig {
        notifier: notification::notifier_from_env(),
        // The same rule as the notifier, and the same reason for stating it:
        // this is the only place the real clipboard sink is built, so
        // `SessionConfig::default()` carrying `Noop` is what keeps ~550 tests
        // off the clipboard *by construction* rather than by nobody
        // remembering to unset `LOOPRS_CLIPBOARD`.
        clipboard: clipboard.clone(),
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
    // The liveness edges fired during `boot()` arrived before this App existed, so
    // prime the open mode's view from the Router's own mirror. The status row and
    // the keyboard rule both read that view, and neither should have to guess what
    // state the session came up in — one `set_status` here is the same write the
    // missing message would have made.
    let boot = router
        .id_of(initial)
        .map(|id| (id, router.status_of(initial)));

    let sz = frame.size()?;
    let mut app = App::new(cmd_tx, InputState::new(), initial, sz.width, sz.height);
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
    // The clipboard, on the same wire as the notifier: the App builds nothing
    // itself, it is handed the sink that `SessionConfig` carries, so "which
    // transport copies my selections" has exactly one answer per run and the App
    // never reaches for `pbcopy` on its own initiative. Same for the automatic
    // path: `LOOPRS_COPY_ON_SELECT=0` is read once, here, and the App is told.
    app.set_clipboard(clipboard);
    app.set_copy_on_select(clipboard::copy_on_select_enabled(
        std::env::var("LOOPRS_COPY_ON_SELECT").ok(),
    ));
    // The escape hatch, on the same injected-sink rule as the clipboard and for
    // the same three reasons: it touches a filesystem, it can fail in ways the UI
    // must report rather than handle, and `main` being the only place the real one
    // is built is what keeps the test suite off the disk by construction.
    // `LOOPRS_TRANSCRIPT_DIR` chooses the directory; with nothing set it is the
    // system temp dir, because a chord the user just discovered should return a
    // transcript rather than a tutorial.
    let transcript_sink = crate::services::transcript_file::transcript_sink_from_env();
    app.set_transcript_sink(transcript_sink);
    // …and the journal, which is not the same thing and is not a duplicate of
    // it. `Ctrl-S t` answers "give me this transcript, now" into a fresh file
    // the user asked for; the journal answers "what did the loop say at 3am"
    // for a run nobody was watching, and it answers it whether or not this
    // process survives — because it is appended and flushed entry by entry while
    // the session runs, not on the exit path. That is ADR-0004 R2, and it is
    // also the thing that makes a *bounded* scrollback survivable: the store
    // drops with a marker, and the marker names the file that kept everything.
    let journal = crate::services::journal::journal_from_env();
    app.set_journal(journal);
    // Tell the sessions the size they are being shown at before anyone runs a
    // command. A Bash shell spawned later still inherits this: `BashTask::resize`
    // records the size even with no shell up yet, and uses it for the pty it
    // eventually opens.
    app.forward_resize(sz.height, sz.width);

    // Sole owner of the sessions from here on: nothing after this point may touch a
    // backend except by sending the Router a command.
    let mut router_task = tokio::spawn(router.run(cmd_rx));

    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60 fps cap
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The backstop for the resize repaint. Why a poll exists next to a resize
    // *event* is the whole doc on `viewport::WindowPoll` (looprs-pdl.15);
    // three lines here, and the reason it is a type and not an `if` is that it
    // has to remember that it gave up.
    let mut window = viewport::WindowPoll::new();

    // The loop has one shape now: read a message, read a key, redraw if anything
    // changed. What is *not* here is the reason the old loop had four shapes —
    // nothing below stops `keys` to go ask the terminal where the cursor is, so
    // the stream is started once and left started for the whole run.
    loop {
        tokio::select! {
            //app_rx receives events that require state updates
            Some(ev) = app_rx.recv() => app.update(ev),
            Some(Ok(ev)) = keys.next() => {
                if let Event::Resize(w, h) = ev {
                    // The frame picks the new window up itself on the next draw
                    // (`autoresize`: a size `ioctl` and a clear, not a cursor
                    // query). What the event is *for* is the two things ratatui
                    // cannot know — the width our own wrapping uses, and the pty
                    // sizes the children wrap for (ADR-0001 rule 6).
                    app.set_window(w, h);
                } else {
                    app.update(Msg::Term(ev));
                }
            }
            _ = tick.tick() => {
                // Adopt the window the terminal has, if the event never told us.
                // Read *before* anything else in this arm so the width the tick
                // goes on to wrap for is the width the `ioctl` just reported —
                // the same one-number rule that makes `preview_active` and
                // `input_band` be taken once per frame.
                if let Some(sz) = window.poll(|| frame.size(), Size::new(app.width, app.height)) {
                    tracing::debug!(
                        "window adopted from the size ioctl: {}x{} -> {}x{}",
                        app.width,
                        app.height,
                        sz.width,
                        sz.height
                    );
                    app.set_window(sz.width, sz.height);
                }
                app.update(Msg::Tick); //spinner only atm
                // A full-screen child had the canvas and gave it back. Our back
                // buffer still describes the screen as it was before the child
                // painted over it, and a diff against that is ADR-0001's "screen
                // is garbled after exiting vim" bug. One full repaint fixes it;
                // see `viewport::ScreenFrame::repaint_all` for why this is
                // `resize` and not `clear`.
                if app.repaint_all && !app.passthrough() {
                    if let Err(e) = frame.repaint_all() {
                        tracing::warn!("full repaint after the full-screen program failed: {e}");
                    }
                    app.repaint_all = false;
                    app.dirty = true;
                }
                // The gate: while a child holds the screen we draw nothing at all,
                // and the bytes are going out from `App::update` instead. Drawing
                // over a program that believes it owns the terminal is the bug this
                // whole path exists to fix, so it is blocked here rather than
                // trusted to be absent.
                if app.dirty && !app.passthrough() {
                    // Settled lines are *made* final by the flush, so it happens
                    // here, in the branch that draws, and with the width this
                    // frame will draw at. Flushing outside a draw would wrap the
                    // lines for a width that may never be painted; flushing after
                    // the preview is taken would show a line in the live tail one
                    // frame after it stopped being live.
                    app.flush_active(app.width);
                    // Both of these are taken once and handed down, for the same
                    // reason the old loop took them once: the value that sized a
                    // band and the lines drawn into it must be one number, or the
                    // box grows a row of blank space every time the two disagree.
                    // (The third value the old loop needed — the height the live
                    // region should be — is gone: the frame is the window, so
                    // there is no height to negotiate.)
                    let preview = app.preview_active(app.width);
                    let input = app.input_band(app.width);
                    // A frame that could not be drawn is not a reason to take the
                    // session down with it. `try_draw` fails inside `autoresize`,
                    // before it has swapped buffers or flushed anything, so there
                    // is nothing inconsistent to recover from: leave `dirty` set
                    // and the next frame tries again.
                    if let Err(e) = frame.draw(|f| {
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
            // the loop's own exit hands the terminal back with every mode on it.
            // There is no second shutdown path for signals, because a second
            // shutdown path is a second set of promises about the terminal and
            // those two can disagree — which is the bug `looprs-ecr` removed.
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
    // Five steps now, where it used to be six. What fell out is step 4, "clear the
    // live pane": with the alternate screen the leave *is* the erase, and the
    // pane it would have cleared does not exist. Every one of the remaining steps
    // is still here to stop a specific way of losing something (looprs-ecr), and
    // the shared `exit` object is the same one the panic hook holds, so step (4)
    // cannot drift from what a crash does — see `crate::teardown`.
    //
    //   1. stop accepting input
    //   2. tell every session to shut down
    //   3. drain what they say into the transcript, bounded
    //   4. hand every mode back            `exit.restore()`, exactly once
    //   5. bound-wait on the session tasks
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
    // sessions down, but it also closes the only reader of what they say next.
    app.request_shutdown().await;

    // (3) Bounded, because "until they are done talking" is not a bound: a child
    // that streams forever is a `yes | cat` away from an app that never exits.
    //
    // What the drain collects goes into the transcript now, not onto the screen.
    // The old step had to finish before the pane was erased because the pane was
    // the only copy of the live tail; the store is that copy now, so what this
    // step is really for is letting each session get its parting word in — and
    // the `SessionDown` that seals it — before the process goes.
    let drain_budget = SHUTDOWN_GRACE + Duration::from_millis(500);
    if tokio::time::timeout(drain_budget, drain_sessions(&mut app, &mut app_rx))
        .await
        .is_err()
    {
        tracing::warn!(
            "sessions were still talking after {drain_budget:?}; leaving them where they are"
        );
    }

    // (3b) The journal drains before anything else goes. Note what this is and
    // is not: it is a *drain* of work already handed over during the run, not
    // a transcript written on the exit path — R3 forbids the latter, and R2 put
    // the durability on the per-entry flush precisely so that a run which never
    // reaches this line is no less durable than one that does. The bound is
    // there because a volume that has stopped answering is not a reason to hang
    // an app the user already told to leave.
    app.journal().close();

    // (4) Every mode that was switched on comes back off, newest first, with raw
    // mode last because the bytes above are written through a tty that is only
    // byte-at-a-time while it is raw. No cursor query anywhere in it, and no
    // erase: the alternate screen's leave puts the user's own screen back, row
    // for row and cursor for cursor.
    exit.restore();

    // (5) The router task is the task that waited on every pump, so joining it
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

/// The exit drain: everything the sessions say on the way out, collected into
/// their own transcripts before the process goes (looprs-ecr step 3).
///
/// Note the order inside the loop — **flush, then wait**. Flushing first means the
/// text that was still streaming when the user pressed Ctrl-Q is finalized into
/// the transcript within one iteration of this loop, whatever the children decide
/// to do afterwards. It is the same reason the flush ran before the insert it
/// used to feed: never leave the newest thing the session said sitting behind a
/// wait on the session.
///
/// Only the active view is drained. A hidden view's backlog is not on the screen,
/// so nothing about it is "lost" by the exit — and dumping a session the user
/// walked away from into *their scrollback* at the worst possible moment was its
/// own kind of noise. It is dropped with the view.
async fn drain_sessions(app: &mut App, rx: &mut mpsc::UnboundedReceiver<Msg>) {
    loop {
        app.flush_active(app.width);
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
/// `preview` is the active view's live tail, rendered once by the caller and
/// shared with the bands so the number that laid this frame out and the lines
/// drawn into it are the same value, not two renders that might disagree.
///
/// `input_rows` is shared for the same reason: it is the height the box asked
/// for when the frame was laid out — or [`viewport::NO_INPUT_ROWS`] when the
/// active session is not taking input — so the box is drawn into the rows it was
/// promised and not into a re-derived guess.
fn view(app: &App, f: &mut Frame, preview: &[Line<'static>], input_rows: u16) {
    let active = app.active_view();
    // The card count comes from the App so the band and the cards cannot ask for
    // two different numbers of rows: `live_card_rows` is the cap, and taking
    // exactly that many cards is what makes the rows counted and the rows drawn
    // one number.
    let card_rows = app.live_card_rows();
    let cards: Vec<&Entry> = active
        .map(|v| v.transcript.open_cards().take(card_rows as usize).collect())
        .unwrap_or_default();
    let [text_area, card_area, status_area, input] =
        viewport::frame_areas(f.area(), card_rows, input_rows);

    // The live tail shows only while the view is following the tail. Scrolled up
    // into history, the band belongs to the history: putting a live line under the
    // rows the user stopped on would redraw the thing they are reading with every
    // delta, and the "N new" pill is the honest signal about what is happening
    // down there without showing it.
    let streaming = matches!(app.chat_state(), ChatState::Chat);
    let live: &[Line<'static>] = if streaming && app.pinned() {
        preview
    } else {
        &[]
    };
    // The live tail takes its rows off the bottom of the band before the store
    // gets any, so the newest *settled* row sits above it rather than under it —
    // the same order the band has always laid out, now measured against the store
    // that decides which rows exist.
    let room = text_area.height.saturating_sub(live.len() as u16).max(1) as usize;
    let window = app.transcript_window(room);
    f.render_widget(
        TranscriptBand::new(window, live, app.spinner, streaming),
        text_area,
    );

    // Publish where the band's rows landed, taken from the *same* layout the
    // band just drew from, so the drag hit-test and the pixels cannot disagree
    // about which row the pointer is on (looprs-pdl.9, `App::band`).
    let lay = band_layout(text_area, window.len(), live.len());
    app.record_band(BandSnapshot::new(
        text_area,
        lay.settled_y,
        lay.settled,
        window.get(lay.skip).map(|r| r.anchor()),
    ));

    // The selection band: reverse video over the cells the range covers. Drawn
    // over the rows it selects (it is a restyle, never a re-layout) and
    // **under** every piece of chrome, so chrome cannot come away highlighted
    // whatever the range says (ADR-0004 R16).
    let runs = app.selection().cells(window);
    if !runs.is_empty() {
        f.render_widget(SelectionHighlight::new(&runs, lay), text_area);
    }

    // The "N new" affordance: a pill over the band's bottom row, saying the tail
    // moved and naming the one action that gets back to it. Overlaid rather than
    // a row of its own so an arrival cannot shift the text the user is reading
    // (ADR-0004 R21's reasoning for the copy toast, applied one band over).
    if app.scrollback().shows_new() {
        // Both this and the copy toast are bottom-right overlays of the same
        // band, and two overlays in one cell is a smear. When the toast is up it
        // gets the bottom row — it is the more urgent of the two, and it is
        // leaving in two seconds — and the pill is handed an area one row
        // shorter, which moves the *pill*, not a single row of transcript.
        let pill_area = if app.toast().is_some() {
            Rect {
                height: text_area.height.saturating_sub(1),
                ..text_area
            }
        } else {
            text_area
        };
        f.render_widget(NewRowsPill(app.new_rows()), pill_area);
    }

    // The copy confirmation (looprs-pdl.10, ADR-0004 R18): a pill of the
    // text's own width, reverse-video, in the band's bottom-right corner,
    // drawn **last** so it lands over everything in the band and never takes a
    // row of its own. A toast that re-shaped the transcript would move the text
    // the user just selected out from under the pointer, which is the failure
    // R21 prices at ~9 ms a pop and refuses outright.
    if let Some(toast) = app.toast() {
        f.render_widget(ToastOverlay::new(toast.text(), toast.tone()), text_area);
    }

    for (i, e) in cards.iter().enumerate() {
        let row = Rect {
            y: card_area.y + i as u16,
            height: 1,
            ..card_area
        };
        f.render_widget(LiveCardPreview::new(e, app.spinner), row);
    }

    // The status row (looprs-guh): the row `frame_areas` reserves and this draws
    // into. Drawn unconditionally — every state, including "no view, no session,
    // no idea", has an answer worth showing, and a row that is only drawn when
    // there is something to report is a row that is missing exactly when it is
    // needed. `status_line` has already cut itself to this area's width, so there
    // is nothing here to wrap and no reason for the row to reflow anything.
    f.render_widget(
        Paragraph::new(app.status_line(status_area.width)),
        status_area,
    );

    // input. `App::input_band` — the value `input_rows` was built from — is zero
    // for a session that has taken the keyboard, so the box is never drawn when
    // it is not wanted, and never drawn outside the band the frame laid out for
    // it.
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
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::style::Modifier;

    /// The window the frame tests paint into: a 60-column, 24-row terminal — the
    /// `App::new` width the rest of this module already used, plus the height the
    /// store needs in order to know how tall a page of scrollback is.
    const WIDTH: u16 = 60;
    const HEIGHT: u16 = 24;

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
        let mut app = App::new(tx, InputState::new(), mode, WIDTH, HEIGHT);
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
        let h = 40; // the window: the frame *is* the window now
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

        let h = 40; // the window: the frame *is* the window now
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
        let h = 40; // the window: the frame *is* the window now
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
        let h = 40; // the window: the frame *is* the window now
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

    // ─────────── the scrollback store drives the band (looprs-pdl.6) ───────────

    /// As [`paint`], with a live tail handed to the frame the way the run loop
    /// hands it: rendered once by the caller, drawn by the band.
    fn paint_preview(app: &App, h: u16, preview: &[Line<'static>], input_rows: u16) -> Vec<String> {
        let backend = TestBackend::new(60, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| view(app, f, preview, input_rows)).unwrap();
        rows(term.backend())
    }

    /// `n` settled system lines in the given view, flushed at 60 columns.
    fn settle(app: &mut App, id: SessionId, n: usize) {
        for i in 0..n {
            app.view_mut(id)
                .push_note(MessageKind::System, format!("settled {i}"));
        }
        app.flush_active(60);
    }

    /// The live tail belongs to the tail. Scrolled up into the transcript, a
    /// streaming line must not keep repainting the thing the user stopped to
    /// read — the band is the history then, and the pill is what says the
    /// session is still talking.
    #[test]
    fn the_live_tail_shows_only_while_the_view_follows_the_tail() {
        let mut app = app(TerminalType::Pi, true);
        app.set_window(60, 30);
        let id = SessionId::new(TerminalType::Pi, 1);
        settle(&mut app, id, 30);
        app.view_mut(id).chat = ChatState::Chat;
        let preview = vec![Line::from("LIVE-TAIL".to_string())];

        let on = paint_preview(&app, 30, &preview, viewport::MIN_INPUT_ROWS).concat();
        assert!(
            on.contains("LIVE-TAIL"),
            "pinned, the live line is on the band: {on:?}"
        );

        app.top_active();
        assert!(!app.pinned(), "the top of the transcript is not the tail");
        let off = paint_preview(&app, 30, &preview, viewport::MIN_INPUT_ROWS).concat();
        assert!(
            !off.contains("LIVE-TAIL"),
            "the live tail drew over the history the user stopped on: {off:?}"
        );
        assert!(
            off.contains("settled 0"),
            "the band is showing the head of the transcript: {off:?}"
        );
    }

    /// The "N new" affordance: visible while the tail has moved away from the
    /// view, naming the count and the one action that answers it — and gone
    /// once the user is back at the tail, because there is nothing to say.
    #[test]
    fn the_new_rows_pill_shows_while_off_the_tail_and_only_then() {
        let mut app = app(TerminalType::Pi, true);
        app.set_window(60, 30);
        let id = SessionId::new(TerminalType::Pi, 1);
        settle(&mut app, id, 30);
        let band = app.transcript_band_rows();
        let band_area =
            viewport::frame_areas(Rect::new(0, 0, 60, 30), 0, viewport::MIN_INPUT_ROWS)[0];
        assert_eq!(
            band as u16, band_area.height,
            "the band the app counts and the band the frame lays out are the same"
        );

        // Pinned: nothing is unseen, so nothing is said.
        let pinned_screen = paint(&app, 30).concat();
        assert!(
            !pinned_screen.contains("new"),
            "a pinned view was told about rows it can see: {pinned_screen:?}"
        );

        app.scroll_active(-(band as isize));
        app.view_mut(id)
            .push_note(MessageKind::System, "late arrival".into());
        app.flush_active(60);
        assert_eq!(app.new_rows(), 2, "the line and its separator");

        let off = paint(&app, 30);
        let off_concat = off.concat();
        assert!(
            off_concat.contains("2 new"),
            "no affordance for the unseen rows: {off:?}"
        );
        assert!(
            off_concat.contains("End"),
            "the affordance does not name the way back: {off:?}"
        );
        // It sits on the band\'s bottom row, which is the row the tail would be on.
        let bottom = &off[band_area.bottom() as usize - 1];
        assert!(
            bottom.contains("2 new"),
            "the pill is not on the band\'s bottom row: {off:?}"
        );

        app.tail_active();
        let back = paint(&app, 30).concat();
        assert!(
            !back.contains("End for the tail"),
            "the pill outlived the thing it was reporting: {back:?}"
        );
        assert!(
            back.contains("late arrival"),
            "and the tail is shown: {back:?}"
        );
    }

    /// **The resize property, painted.** A window drag re-wraps the transcript,
    /// and the line the view was resting on stays on the screen. The row that
    /// line occupies changes — that is what a re-wrap *is* — so an
    /// index-preserving scroll position would move the text out from under the
    /// user, which is the difference between a usable scrollback and a useless
    /// one.
    #[test]
    fn a_resize_keeps_the_line_the_user_was_looking_at_on_screen() {
        let mut app = app(TerminalType::Pi, true);
        let id = SessionId::new(TerminalType::Pi, 1);
        let filler = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega aaaa bbbb cccc dddd eeee ffff gggg hhhh";
        for i in 0..12 {
            app.view_mut(id)
                .push_note(MessageKind::Answer, format!("MARK-{i} {filler}"));
        }
        app.flush_active(80);
        let wide = app.scrollback().len();
        let band = viewport::frame_areas(Rect::new(0, 0, 60, 30), 0, viewport::MIN_INPUT_ROWS)[0]
            .height as usize;

        // Rest the view with MARK-7 as its bottom-most visible row.
        let marker = app
            .scrollback()
            .rows()
            .iter()
            .position(|r| r.to_string().contains("MARK-7"))
            .expect("the marker row is in the store");
        let offset = wide - marker - 1;
        assert!(
            offset <= app.scrollback().max_scroll(band),
            "setup: MARK-7 has to be reachable, offset={offset} max={}",
            app.scrollback().max_scroll(band)
        );
        app.scroll_active(-(offset as isize));
        assert!(
            app.transcript_window(band)
                .last()
                .unwrap()
                .to_string()
                .contains("MARK-7")
        );
        let before = paint(&app, 30).concat();
        assert!(before.contains("MARK-7"), "{before:?}");

        // A narrower window: the paragraphs take more rows each.
        app.set_window(56, 30);
        app.flush_active(56);
        let narrow = app.scrollback().len();
        assert!(narrow > wide, "nothing re-wrapped: {wide} -> {narrow}");

        let after = paint(&app, 30).concat();
        assert!(
            after.contains("MARK-7"),
            "the resize moved the content the view was resting on off the screen: {after:?}"
        );
        assert_ne!(before, after, "the frame did not change at all");
        // …and it is still the row the view rests on, not merely still somewhere.
        assert!(
            app.transcript_window(band)
                .last()
                .unwrap()
                .to_string()
                .contains("MARK-7")
        );
    }

    /// Paint the frame and hand the terminal back, so a test can read *cells*
    /// rather than the text of them.
    fn paint_cells(app: &App, h: u16, input_rows: u16) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(WIDTH, h)).unwrap();
        term.draw(|f| view(app, f, &[], input_rows)).unwrap();
        term
    }

    fn reversed_cells(term: &Terminal<TestBackend>, area: Rect) -> usize {
        let mut n = 0usize;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if term
                    .backend()
                    .buffer()
                    .cell((x, y))
                    .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
                {
                    n += 1;
                }
            }
        }
        n
    }

    fn mreport(kind: MouseEventKind, row: u16, col: u16) -> Msg {
        Msg::Term(Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    /// **The highlight is a paint over the band, and the chrome does not see
    /// it.**
    ///
    /// The frame-level version of two properties the widget test cannot reach:
    /// the status row and the input box come out of the frame *identical*
    /// before and after a selection is made — they are not transcript, and the
    /// highlight must not smear across them — and the whole rendered frame is
    /// otherwise unchanged, row for row, which is the no-reflow rule read off
    /// the screen rather than asserted about a widget.
    #[test]
    fn the_highlight_lands_in_the_band_and_never_on_the_chrome() {
        let mut app = app(TerminalType::Pi, true);
        // The copy-on-release path is off for this test. Not a dodge: this test
        // is about *where the highlight paints*, and a release that hands the
        // selection to a clipboard sink paints a toast over the band on its way
        // past — which is looprs-pdl.10's subject, tested there, and would make
        // the "the frame moved nothing" assertion below read as a geometry
        // failure when it is only a toast.
        app.set_copy_on_select(false);
        settle(&mut app, SessionId::new(TerminalType::Pi, 1), 6);
        let h = HEIGHT;
        let input_rows = viewport::MIN_INPUT_ROWS;
        let [text, _, status, input] = viewport::frame_areas(
            Rect::new(0, 0, WIDTH, h),
            app.live_card_rows(),
            app.input_band(WIDTH),
        );

        let before = paint_cells(&app, h, input_rows);
        assert_eq!(
            reversed_cells(&before, text),
            0,
            "setup: nothing in the band is reversed to begin with"
        );

        // Press on a settled row, drag two rows down, release.
        let win = app.transcript_window(text.height as usize);
        let lay = components::text_stream::band_layout(text, win.len(), 0);
        assert!(lay.settled >= 4, "setup: enough drawn rows to drag over");
        let y = lay.settled_y;
        app.update(mreport(
            MouseEventKind::Down(MouseButton::Left),
            y + 1,
            text.x + 2,
        ));
        app.update(mreport(
            MouseEventKind::Drag(MouseButton::Left),
            y + 3,
            text.x + 9,
        ));
        app.update(mreport(
            MouseEventKind::Up(MouseButton::Left),
            y + 3,
            text.x + 9,
        ));
        assert!(app.selection().is_live(), "setup: a live selection");

        let after = paint_cells(&app, h, input_rows);
        assert!(
            reversed_cells(&after, text) > 0,
            "the band took the highlight"
        );
        assert_eq!(
            reversed_cells(&after, status),
            reversed_cells(&before, status),
            "the status row is byte-identical: it is not transcript"
        );
        assert_eq!(
            reversed_cells(&after, input),
            reversed_cells(&before, input),
            "nor is the box"
        );
        assert_eq!(
            rows(after.backend()),
            rows(before.backend()),
            "and the frame moved nothing: a highlight is a style, not a reflow"
        );
    }
}
