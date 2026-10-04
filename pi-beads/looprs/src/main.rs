mod app;
mod components;
mod screen;
mod services;
mod session;
mod state;
#[cfg(test)]
mod testing;
mod theme;
mod utils;
mod viewport;
use anyhow::Result;
use app::{App, Msg, UiCommand};
use components::input::InputState;
use crossterm::event::{Event, EventStream};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Rect, Size};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use std::io::{self, Stdout};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use components::text_stream::LiveTextPreview;
use components::tool::LiveToolPreview;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::session::router::{Router, SHUTDOWN_GRACE};
use crate::session::{ChatState, SessionConfig, SessionStatus, TerminalType};
use crate::state::transcript::Entry;

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
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        prev(info);
    }));

    enable_raw_mode()?;
    let initial = TerminalType::Beeds;
    // The live region opens at the height the policy wants for an empty stream —
    // its chrome plus one row — instead of the constant 10 it used to be. From
    // here on the frame decides the shape; see `viewport`.
    let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
    let boot_h = viewport::desired_height(initial, rows, 0, 0);
    let mut live = viewport::LiveView::new(CrosstermBackend::new(io::stdout()), boot_h, |_| {
        Ok(CrosstermBackend::new(io::stdout()))
    })?;
    let res = run(&mut live, initial).await;
    disable_raw_mode()?;
    live.clear()?; // erase the live region; scrollback stays
    println!();
    res
}

async fn run(
    live: &mut viewport::LiveView<CrosstermBackend<Stdout>>,
    initial: TerminalType,
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
    // Bring up the mode we open in *before* the App exists: the beads loop's first
    // pass runs now, so the input box opens in the right state instead of
    // flickering once the BeadStep message lands.
    router.boot().await?;
    // Sole owner of the sessions from here on. It never blocks on a child, so Esc
    // cannot queue behind somebody's model call.
    // The BeadStep that set the initial input gating was emitted before this App
    // existed, so seed the view from the Router's own state rather than guessing.
    let seed = router.id_of(initial).map(|id| {
        (
            id,
            !matches!(router.status_of(initial), SessionStatus::Running),
        )
    });

    let mut app = App::new(
        cmd_tx,
        InputState::new(),
        initial,
        live.screen_size()?.width,
    );
    if let Some((id, awaiting)) = seed {
        app.view_mut(id).awaiting_user = awaiting;
    }
    // Tell the sessions the size they are being shown at before anyone runs a
    // command. A Bash shell spawned later still inherits this: `BashTask::resize`
    // records the size even with no shell up yet, and uses it for the pty it
    // eventually opens.
    let sz = live.screen_size()?;
    app.forward_resize(sz.height, sz.width);

    // Sole owner of the sessions from here on: nothing after this point may touch a
    // backend except by sending the Router a command.
    let router_task = tokio::spawn(router.run(cmd_rx));

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
                    let want = viewport::desired_height(
                        app.active,
                        live.rows()?,
                        preview.len(),
                        tools,
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
                    if let Err(e) = live.draw(|f| view(&app, f, &preview)) {
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

    // Dropping the App closes `cmd_tx`, which is the Router's cue to shut every
    // session down. Await it: the children are reaped on that path, and quitting
    // before it finishes is how a session outlives the TUI.
    drop(app);
    let grace = SHUTDOWN_GRACE + Duration::from_secs(1);
    if tokio::time::timeout(grace, router_task).await.is_err() {
        tracing::warn!("router did not finish shutting its sessions down within {grace:?}");
    }
    Ok(())
}

/// Pure function of state (the tail preview re-parses only the open block).
///
/// `preview` is the active view's live tail, rendered once by the caller and shared
/// with the height policy so the number that sized this frame and the lines drawn
/// into it are the same value, not two renders that might disagree.
fn view(app: &App, f: &mut Frame, preview: &[Line<'static>]) {
    let active = app.active_view();
    let tools: Vec<&Entry> = active
        .map(|v| {
            v.transcript
                .open_tools()
                .take(viewport::MAX_TOOL_ROWS as usize)
                .collect()
        })
        .unwrap_or_default();
    let [text_area, tool_area, _status, input] =
        viewport::frame_areas(f.area(), tools.len() as u16);

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
    // input
    if app.need_input() {
        app.input.render(f, input)
    }
}
