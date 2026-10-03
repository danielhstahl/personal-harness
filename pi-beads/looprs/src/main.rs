mod app;
mod components;
mod services;
mod state;
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
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::app::{BeadStep, BeadsLoop, ChatState, parse};
use crate::components::scrollback::Flusher;
use crate::components::tool::LiveToolPreview;
use crate::services::pi::PiRpc;
use crate::state::state::{Entry, Transcript};

fn init_logging() -> anyhow::Result<WorkerGuard> {
    //let dir = std::env::temp_dir(); // or a proper data dir, e.g. via the `dirs` crate
    let dir = std::env::current_dir().unwrap();
    let appender = tracing_appender::rolling::never(&dir, "looprs.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
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
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<Msg>();
    //let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<PiEvent>(); // bounded => backpressure on the agent
    //let args = vec![];
    //let (pi, mut ev_rx) = PiRpc::spawn(&args)?;
    //spawn_agent(cmd_rx, pi);
    let bead_loop = BeadsLoop::new(app_tx)?;
    bead_loop.listen_input(cmd_rx);
    let input_state = InputState::new();
    let transcript = Transcript::new();
    let mut app = App::new(cmd_tx, input_state, transcript, term.size()?.width);
    let mut flusher = Flusher::new();
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60 fps cap
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(ev) = app_rx.recv() => app.update(ev),
            Some(Ok(ev)) = keys.next() => {
                if let Event::Resize(w, h) = ev {
                    drop(keys);                               // stop reading stdin
                    if let Err(e) = term.resize(Rect::new(0, 0, w, h)) {
                        // don't kill the session over a failed re-anchor
                        tracing::warn!("resize failed: {e}");
                    }
                    keys = EventStream::new();                // resume
                    app.width = w;
                } else {
                    app.update(Msg::Term(ev));
                }
            }
            _ = tick.tick() => {
                app.update(Msg::Tick); //spinner only atm
                if app.dirty {
                    let lines = flusher.drain(&app.transcript, app.width);  // reads transcript, mutates flusher
                    insert_lines(term, lines)?;
                    term.draw(|f| view(&app,  &flusher, f))?;
                    app.dirty = false;
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Pure function of state (the tail preview re-parses only the open block).
fn view(app: &App, flusher: &Flusher, f: &mut Frame) {
    /*let [preview, status, input] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(f.area());*/
    let tools: Vec<&Entry> = app.transcript.open_tools().take(4).collect();
    let [text_area, tool_area, _status, input] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(tools.len() as u16),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(f.area());

    if matches!(app.chat_state, ChatState::Chat) {
        f.render_widget(
            LiveTextPreview::new(app.spinner, &app.transcript, flusher),
            text_area,
        );
    }

    //let [preview, tool_area] =
    //    Layout::vertical([Constraint::Min(0), Constraint::Length(tools.len() as u16)])
    //        .areas(preview);

    for (i, e) in tools.iter().enumerate() {
        let row = Rect {
            y: tool_area.y + i as u16,
            height: 1,
            ..tool_area
        };
        f.render_widget(LiveToolPreview::new(e, app.spinner), row);
    }
    // input
    if app.need_input {
        app.input.render(f, input)
    }
}
/*
fn spawn_agent(mut cmd_rx: mpsc::Receiver<UiCommand>, pi: PiRpc) {
    tokio::spawn(async move {
        while let Some(input) = cmd_rx.recv().await {
            match input {
                UiCommand::UserMessage(text) => {
                    let res = pi.prompt(&text).await;
                    match res {
                        Ok(_v) => tracing::debug!("Success"),
                        Err(e) => tracing::info!("This is err: {}", e),
                    };
                }
                _ => {}
            }
        }
    });
}*/
