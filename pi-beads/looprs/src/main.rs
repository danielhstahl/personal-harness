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
use anyhow::Result;
use app::{App, Msg, UiCommand};
use components::input::InputState;
use crossterm::event::{Event, EventStream};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use std::io::{self, Stdout};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

/// Inline viewports are fixed-height in stock ratatui. Live preview gets whatever is left
/// after status (1) + input (3). To resize dynamically, recreate the Terminal (or fork the
/// terminal layer like Codex does).
const VIEWPORT_H: u16 = 10;

type Term = Terminal<CrosstermBackend<Stdout>>;
use components::text_stream::LiveTextPreview;
use components::tool::LiveToolPreview;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::session::router::{Router, SHUTDOWN_GRACE};
use crate::session::{ChatState, SessionConfig, SessionStatus, TerminalType};
use crate::state::state::Entry;

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

fn insert_lines(term: &mut Term, lines: Vec<Line<'static>>) -> io::Result<()> {
    for chunk in lines.chunks(64) {
        //64 is arbitrary
        let height = chunk.len() as u16;
        term.insert_before(height, |buf| {
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
    let mut term = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_H),
        },
    )?;
    let res = run(&mut term).await;
    disable_raw_mode()?;
    term.clear()?; // erase the live region; scrollback stays
    println!();
    res
}

async fn run(term: &mut Term) -> Result<()> {
    // all terminal UI events and events originating outside the app
    // come from cmd_tx and are received on cmd_rx
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(16);
    // any app state changes come from app_tx and are received on app_rx
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<Msg>();

    let initial = TerminalType::Beeds;
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

    let mut app = App::new(cmd_tx, InputState::new(), initial, term.size()?.width);
    if let Some((id, awaiting)) = seed {
        app.view_mut(id).awaiting_user = awaiting;
    }
    // Tell the sessions the size they are being shown at before anyone runs a
    // command. A Bash shell spawned later still inherits this: `BashTask::resize`
    // records the size even with no shell up yet, and uses it for the pty it
    // eventually opens.
    let sz = term.size()?;
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
                        drop(keys);                               // stop reading stdin
                        if let Err(e) = term.resize(Rect::new(0, 0, w, h)) {
                            // don't kill the session over a failed re-anchor
                            tracing::warn!("resize failed: {e}");
                        }
                        keys = EventStream::new();                // resume
                        // …and the children get it too. A pty sized 80x24 while the
                        // window is 180x50 wraps every program's output for a terminal
                        // that is not there (ADR-0001 rule 6).
                        app.forward_resize(h, w);
                    }
                } else {
                    app.update(Msg::Term(ev));
                }
            }
            _ = tick.tick() => {
                app.update(Msg::Tick); //spinner only atm
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
                    let sz = term.size()?;
                    drop(keys);
                    if let Err(e) = term.resize(Rect::new(0, 0, sz.width, sz.height)) {
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
                    // The per-frame sequence is flush -> insert_before -> draw, and
                    // only for the ACTIVE view (ADR-0002 Q5). A hidden view buffers;
                    // its backlog goes out as one burst when you switch to it.
                    let lines = app.flush_active(app.width);
                    insert_lines(term, lines)?;
                    term.draw(|f| view(&app, f))?;
                    app.dirty = false;
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
fn view(app: &App, f: &mut Frame) {
    let active = app.active_view();
    let tools: Vec<&Entry> = active
        .map(|v| v.transcript.open_tools().take(4).collect())
        .unwrap_or_default();
    let [text_area, tool_area, _status, input] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(tools.len() as u16),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(f.area());

    // What the live region shows is a property of the session on screen, not of any
    // session that happens to be streaming.
    if matches!(app.chat_state(), ChatState::Chat)
        && let Some(view) = active
    {
        f.render_widget(LiveTextPreview::new(app.spinner, view), text_area);
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
