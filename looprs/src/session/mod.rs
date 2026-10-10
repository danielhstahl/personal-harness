//! The session layer: one subprocess + one event stream per terminal state.
//!
//! This is ADR-0002 made executable. Everything a later ticket needs to agree on — the
//! [`Session`] trait, the [`SessionEvent`] / [`Msg`] envelope, the switch-lifecycle
//! policy, the [`Router`](router::Router) that owns the sessions, and the per-session
//! [`SessionView`](view::SessionView) — is declared here.
//!
//! Three sentences that explain the whole design:
//!
//! 1. A *session* is one child process plus one stream of events about that process.
//! 2. Every event carries the [`SessionId`] that produced it; the UI never guesses.
//! 3. A `Tab` changes which session you are *looking at*; it never destroys work.
//!
//! ## Dead code, and why there is no blanket `allow` here
//!
//! This module used to open with `#![allow(dead_code)]`, which silenced the whole
//! subtree — five files of half-wired paths reporting nothing. That is the wrong
//! trade: a blanket allow makes "unreachable" the same color as "fine".
//!
//! So there is none, and each item that is not reachable from the binary carries its
//! own `#[allow(dead_code)]` with the reason it stays: usually "test seam" (the
//! lifecycle tests would otherwise be sleeps) or a named ticket that will read it
//! (looprs-guh's status row is the usual suspect). An allow without a reason is a
//! warning we deleted rather than answered; `cargo clippy --all-targets -- -D
//! warnings` is the gate that keeps that list honest.
//!
//! Naming a ticket on an allow is a **promise**, and a promise is the part that
//! rots: the ticket lands, routes somewhere else, and the comment goes on
//! asserting a reader that does not exist. That reads as coverage, which is worse
//! than no comment at all. So the rule is that when the named ticket closes, the
//! allow is settled the same day — wired to the reader that turned out to want it,
//! or deleted with the comment. `./scripts/dead_audit.py` asks the compiler,
//! through the allow (`--force-warn=dead_code`), whether the guarded code is
//! still dead; a redundant allow fails it, and `./scripts/check.sh` runs that
//! gate. looprs-2nd is that pass over the pdl.9 promises.

pub mod bash;
pub mod beads;
pub mod cancel;
pub mod pi_chat;
pub mod router;
pub mod view;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::wire::PiEvent;

pub use bash::BashSession;
pub use beads::BeadsSession;
pub use pi_chat::PiChatSession;
pub use view::ChatState;

/// The three terminal states. This is a *session* identity, not a widget property, so
/// it lives here; `components::input` re-exports it for the input box that cycles it.
///
/// One spelling everywhere: the variant, the input box's label and the ADR prose
/// all call the mode `Beads`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalType {
    Beads,
    Pi,
    Bash,
}

impl TerminalType {
    /// All modes, in Tab order.
    ///
    /// The one place the order is written down. The status row (looprs-guh) walks
    /// it rather than only reporting the mode on screen — which is the point of the
    /// row: the modes you are *not* looking at are the ones that need reporting —
    /// and the mode-table tests walk it instead of hard-coding "Beads, Pi, Bash" in
    /// nine spots.
    pub const ALL: [TerminalType; 3] = [TerminalType::Beads, TerminalType::Pi, TerminalType::Bash];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::Beads => "Beads",
            Self::Pi => "Pi",
        }
    }

    /// What `Tab` goes to next. Kept on the type (not on `InputState`) so the router
    /// can reason about the cycle too.
    pub fn next(self) -> Self {
        match self {
            Self::Bash => Self::Beads,
            Self::Beads => Self::Pi,
            Self::Pi => Self::Bash,
        }
    }

    /// What a `Tab` *away from* this mode does to it. See ADR-0002 Q3.
    pub fn switch_away_policy(self) -> SwitchAway {
        match self {
            // A real shell (ADR-0001). Killing it loses cwd/env/jobs, and a running
            // command is the user's, not ours. Output buffers until they look again.
            Self::Bash => SwitchAway::KeepRunning,
            // Chat context lives in this child's memory. Keep it; let the in-flight
            // answer finish and land in the transcript. Tab is not Cancel; Esc is.
            Self::Pi => SwitchAway::KeepRunning,
            // The loop self-advances. While nobody is watching it must not keep
            // starting billable passes: finish the current one, then park.
            Self::Beads => SwitchAway::DrainThenPark,
        }
    }
}

/// What leaving a terminal state does to that session (ADR-0002 Q3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchAway {
    /// Nothing happens to the session: child, work-in-flight and all. Output keeps
    /// streaming into its own transcript. Switching back shows the backlog.
    KeepRunning,
    /// No *new* children are started while hidden; the pass already running is
    /// allowed to finish. On re-entry the loop resumes.
    DrainThenPark,
}

/// The ticket a beads pass is holding.
///
/// The title travels with the id because the status row (looprs-guh) has to say
/// what the loop is spending money on in words a human recognises, and because
/// reading it back from `bd` at draw time would put a subprocess inside the frame.
///
/// This is the harness's own knowledge of its claim (looprs-w7q), not a copy of
/// the bead: it is set when `bd update --claim` succeeded and cleared when the pass
/// ends, which is exactly the window in which the loop is accountable for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveBead {
    pub id: String,
    pub title: String,
}

impl std::fmt::Display for ActiveBead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.id, self.title)
    }
}

/// Which session produced an event, and which *incarnation* of it.
///
/// `generation` (`gen`) is mandatory, not decoration. `TerminalType` alone cannot tell a live Pi
/// session from the Pi session that was killed three switches ago, and it certainly
/// cannot tell a respawned one from the corpse whose late events are still in flight.
/// A session keeps its `SessionId` for its whole life; every re-spawn bumps the generation,
/// so a stale event is *detectable* rather than merely *probably* ignored
/// (looprs-05j's "no late event from the dead session is applied to the new mode").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId {
    pub mode: TerminalType,
    /// `gen` is a reserved keyword in edition 2024, hence the long name.
    pub generation: u64,
}

impl SessionId {
    pub fn new(mode: TerminalType, generation: u64) -> Self {
        Self { mode, generation }
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.mode.label(), self.generation)
    }
}

/// Which pipe a byte came from.
///
/// Under ADR-0001 Bash mode is a pty, which has *one* byte stream: stdout and
/// stderr are already interleaved by the line discipline and cannot be told apart.
/// `Merged` is therefore what BashSession sends. `Stdout`/`Stderr` exist for
/// backends that do read separate pipes (a piped fallback, or a tool's own output),
/// and to stop anyone from inventing a split they cannot back up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteStream {
    Merged,
    /// Not constructed yet: the only producer today is a pty, which cannot tell the
    /// two apart. Kept (individually, rather than by relaxing the whole enum) so
    /// that a backend which *does* read separate pipes has a way to say so, and so
    /// nobody builds a "stderr" label out of a stream that never had one.
    #[allow(dead_code)] // consumer: a piped (non-pty) shell fallback, not yet wired
    Stdout,
    /// As [`ByteStream::Stdout`]: the pipe a tool's own stderr would arrive on.
    #[allow(dead_code)] // consumer: per-tool stderr routing, not yet wired
    Stderr,
}

/// Liveness of a session's child, for the status row (looprs-guh) and for input
/// gating. Derived state only: nothing in the App branches on it for correctness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionStatus {
    /// Lazily-created mode that has never been entered, or a session reset after
    /// [`ExitReason::Shutdown`]. No child exists.
    #[default]
    NotStarted,
    /// Child alive, nothing in flight.
    Idle,
    /// Child alive and working: a pi run is streaming, or a shell command is running.
    Running,
    /// Cancel was requested and we are waiting for the child to unwind (looprs-5g7).
    Aborting,
    /// The child is gone (crash, EOF, our kill). A later submit may respawn it.
    Dead,
}

impl SessionStatus {
    /// "Do not let the user start another thing right now."
    ///
    /// The bin's input gating still runs off the active *view*'s `awaiting_user`
    /// rather than off liveness, but the status row asks this question of every
    /// mode, on screen or not, to decide what is doing work and what is merely
    /// warm (looprs-guh: `Sess::busy`, `Status::background`).
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Running | Self::Aborting)
    }
    /// "There is a process behind this."
    pub fn is_alive(self) -> bool {
        matches!(self, Self::Idle | Self::Running | Self::Aborting)
    }
}

/// Why a session's child is gone. Always the last thing that session reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// We asked for it: shutdown, or the beads pass boundary.
    Shutdown,
    /// The user cancelled and the child exited as a result.
    Cancelled,
    /// The child exited on its own. `None` code = killed by a signal / unreadable.
    Crashed { code: Option<i32> },
    /// stdout closed and the wait status never arrived.
    Unknown,
}

/// The one event type every session emits, regardless of backend.
///
/// A session never sees [`crate::wire::Msg`]: it knows nothing about ticks, resize
/// events, or which mode is on screen. The router's pump ([`Router::pump`]) is the
/// single place that stamps the [`SessionId`] and turns this into a `Msg`, so a
/// missing origin is a compile error rather than a 3 a.m. bug.
#[derive(Debug)]
pub enum SessionEvent {
    /// A pi RPC protocol event. Both the beads workers and the Pi chat session are
    /// pi processes, so both ride this — the `session` tag on the resulting `Msg`
    /// is what keeps them from being confused for each other.
    Agent(PiEvent),
    /// Shell output. A **chunk**, not a line: the reader hands us buffer-sized
    /// reads and re-splitting them on `\n` would rewrite the child's own framing
    /// (ADR-0001: no re-wrap, no re-flow).
    BashOutput { stream: ByteStream, chunk: String },
    /// The beads machine moved. Only BeadsSession ever sends this.
    BeadStep(BeadStep),
    /// This session's liveness, published on every change (looprs-guh).
    ///
    /// Pushed rather than polled because nobody upstream of the session task *can*
    /// poll it cheaply: the `Router` owns the `Box<dyn Session>` and lives on
    /// another task, and `App` is on the draw loop, which must not block on
    /// anything (ADR-0002 Q4). A pushed edge is the same shape as every other
    /// fact a session reports, and it is what lets the status row answer "is
    /// there a child behind this mode, and is it busy?" without a round trip.
    ///
    /// Published only on change, by [`publish_liveness`], from the one place each
    /// session task finishes handling a command — so "the row and the session
    /// agree" does not depend on remembering to say it at every mutation.
    Status(SessionStatus),
    /// The ticket the beads loop currently holds a claim on, `None` when it holds
    /// none. Only BeadsSession ever sends this.
    ///
    /// Published by the loop itself at the two moments that matter — the claim was
    /// taken, and the pass that held it ended — so the status row (looprs-guh) can
    /// name the active bead without asking `bd`, and without deriving it from
    /// whichever transcript line happens to be on screen. Same rule as
    /// [`SessionEvent::BeadStep`]: render it, never re-derive it.
    ActiveBead { bead: Option<ActiveBead> },
    /// A status line for the transcript ("working looprs-1", "board empty, ...").
    System(String),
    /// Text that belongs back in the user's input box rather than in the transcript.
    ///
    /// Only one thing produces it today: Pi's interactive `Esc`, which pulls the
    /// queued steering/follow-up messages out of the child (`clear_queue`) before
    /// aborting so the user gets their own words back instead of losing them
    /// (looprs-ctn). A session cannot write the box itself — the box is UI state,
    /// and a session has no handle on it — so the round trip is an event like any
    /// other, and it names its producer so a Pi cancel can never fill the Beads box.
    RestoreInput { text: String },
    /// A failure the human needs to see, attributable to this session.
    Error(String),
    /// A full-screen program took the real terminal over, or gave it back
    /// (ADR-0001 Q2 rule 2 — the screen-buffer path).
    ///
    /// While `active`, the UI must not draw and must not flush: the child believes
    /// it owns the screen, and during that window it does. The session's
    /// [`SessionEvent::BashOutput`] chunks are copied to the real terminal
    /// verbatim instead of going through the transcript, because a held screen and
    /// a rendered transcript of the same bytes is the same frame drawn twice.
    ScreenHeld { active: bool },
    /// Lifecycle edge: the child is gone and no further events will follow.
    Exited { reason: ExitReason },
}

/// Publish a session's liveness if it changed since the last publish.
///
/// A free function rather than three copies because the rule is identical in every
/// session and a drift between them is the status row lying about a mode. Each
/// session task calls it once, at the end of the turn in which it handled a
/// command, next to the mirror write that `Session::status()` already reads — so
/// the push cannot be forgotten at an individual mutation site, and a session that
/// changes liveness without handling a command is not a session this harness has.
///
/// `last` advances whether or not the send succeeds: a failed send means the UI is
/// gone, and there is nobody left to bring up to date.
pub fn publish_liveness(
    last: &mut SessionStatus,
    now: SessionStatus,
    tx: &mpsc::UnboundedSender<SessionEvent>,
) {
    if *last == now {
        return;
    }
    *last = now;
    let _ = tx.send(SessionEvent::Status(now));
}

/// The beads loop's own state, owned by the beads *session*, never by the UI.
///
/// `looprs-msj` is the ticket whose whole bug was that `App` used to derive this
/// state from `self.input.mode`. With the envelope above, the App only *renders*
/// a step it was handed — and the loop that moves between these steps is moved by
/// its own worker, inside `BeadsSession`, not by anything upstream.
///
/// The transition *into* one of these is not chosen here: see
/// [`StepCause`](crate::session::beads::StepCause), which is where the machine's
/// table lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeadStep {
    AwaitInput,
    CreateTickets,
    WorkTickets,
}

impl BeadStep {
    /// The one step that hands the keyboard back to the human.
    ///
    /// This predicate — not the step itself — is what the UI needs: the input box
    /// opens on "is it my turn", and two different steps that both mean "you may
    /// type" must not need their own `matches!` at every call site. The beads
    /// session and the view both ask it through here, so the answer cannot drift
    /// between them (looprs-6ol: one place per decision, and that place is testable
    /// without a subprocess).
    pub fn awaits_user(self) -> bool {
        matches!(self, Self::AwaitInput)
    }
}

/// Everything a session needs in order to be started, injected so tests can point
/// at fake binaries (see `src/testing.rs`).
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub pi_bin: String,
    pub bd_bin: String,
    /// The shell BashSession spawns. `$SHELL` if set, else `/bin/bash`.
    pub shell_bin: String,
    /// Where a finished ticket is announced, out of band
    /// ([`services::notification`](crate::services::notification)).
    ///
    /// Carried here rather than passed as another argument because this is already
    /// the one value that reaches every session through the factory, and because it
    /// is a *sink*: the loop that produces the fact must not get to choose where it
    /// goes, and it must not be able to block on it. `notify` is a queue send; the
    /// network lives in the sink's own task.
    ///
    /// The default is [`Noop`](crate::services::notification::Noop) and **not**
    /// the configured notifier, deliberately: the default config is what ~300 tests
    /// build, so a default that read the environment would make "no test ever
    /// touched the network" a matter of luck. `main` is the only place that calls
    /// [`notifier_from_env`](crate::services::notification::notifier_from_env).
    pub notifier: Arc<dyn crate::services::notification::Notifier>,
    /// Where a copy goes: the system clipboard, or nowhere
    /// ([`services::clipboard`](crate::services::clipboard)).
    ///
    /// Carried and reasoned about exactly like [`Self::notifier`], because it is
    /// the same shape of hazard: the UI must not touch the outside world
    /// directly, and the thing it does to the outside world is a process spawn
    /// (`pbcopy`) or a tty write that can stall. `copy` is a queue send; the
    /// transport lives in the sink's own task, under its own deadline
    /// (ADR-0004 R11).
    ///
    /// The default is [`Noop`](crate::services::clipboard::Noop) and **not**
    /// the configured sink, for the same reason the notifier's default is:
    /// ~550 tests build `SessionConfig::default()`, and a default that read the
    /// environment would make "no test ever touched a clipboard" a matter of
    /// luck rather than construction. `main` is the only caller of
    /// [`clipboard_from_env`](crate::services::clipboard::clipboard_from_env).
    // Read by the select-to-copy path being finished in looprs-pdl.10; the shipped
    // binary gets its clipboard through `App::set_clipboard`, so nothing in this
    // crate's compiled code touches the field yet.
    #[allow(dead_code)]
    pub clipboard: Arc<dyn crate::services::clipboard::Clipboard>,
    /// Is the **app** living in the alternate screen?
    ///
    /// The Bash session needs this one fact from the terminal mode ledger because it
    /// decides what the screen watcher does with a child's own `?1049h`/`?1049l`
    /// pair: cut it out of the stream when the alternate screen is ours
    /// (ADR-0004 rule 1 + ADR-0001 amendment 4), or tee it when we are in an
    /// inline pane and the child's switch is the only screen switch happening in
    /// the run. Read from [`crate::teardown::Mode::alt_screen_claimed`] — the
    /// same startup list the ledger was loaded from — so the session and the ledger
    /// cannot disagree about who owns the screen.
    pub alt_screen_hosted: bool,
    /// The credit supply that paces the byte-stream producers against the UI's
    /// drain rate (looprs-6cj).
    ///
    /// Built by the UI bus and handed through here because this is the one value
    /// that reaches every session, and because the direction of the dependency
    /// is the point: the *consumer* owns the ceiling and the producer asks
    /// permission. A session that could size its own output queue is a session
    /// that can outrun the screen again. See [`crate::bus::Budget`].
    ///
    /// The default is a supply that never runs out, on the same rule as the
    /// notifier's and the clipboard's defaults: `SessionConfig::default()` is
    /// what ~550 tests build, and a default that could block would turn every
    /// test with no UI draining the far end into a hang.
    pub output_budget: crate::bus::Budget,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            pi_bin: std::env::var("LOOPRS_PI_BIN").unwrap_or_else(|_| "pi".to_string()),
            bd_bin: crate::services::bd::bd_bin_from_env(),
            shell_bin: default_shell_bin(),
            notifier: Arc::new(crate::services::notification::Noop),
            clipboard: Arc::new(crate::services::clipboard::Noop),
            output_budget: crate::bus::Budget::unbounded(),
            alt_screen_hosted: crate::teardown::Mode::alt_screen_claimed(),
        }
    }
}

/// Is `path` plausibly a bash (including a renamed one like `bash5`)?
///
/// This matters because Bash mode's whole integration is bash-specific: `--rcfile`
/// and the `PROMPT_COMMAND` exit marker have no zsh or fish equivalent. Pointing
/// Bash mode at `$SHELL` because it is *the* shell is how you get a pane that runs
/// zsh with no exit codes, no readiness signal and a `--rcfile` it does not
/// understand. So `$SHELL` is consulted, but only honored when it is a bash.
pub fn looks_like_bash(path: &str) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("bash"))
}

/// The shell Bash mode spawns: an explicit override, else a **bash**, found rather
/// than assumed.
fn default_shell_bin() -> String {
    // An override is honored verbatim, even if it names a non-bash: it is also
    // how you point at a bash built somewhere unusual, and BashSession warns about
    // the non-bash case at spawn rather than silently misbehaving.
    if let Ok(v) = std::env::var("LOOPRS_SHELL_BIN")
        && !v.is_empty()
    {
        return v;
    }
    if let Ok(s) = std::env::var("SHELL")
        && looks_like_bash(&s)
    {
        return s;
    }
    for cand in ["/bin/bash", "/opt/homebrew/bin/bash", "/usr/local/bin/bash"] {
        if std::path::Path::new(cand).exists() {
            return cand.to_string();
        }
    }
    "/bin/bash".to_string()
}

/// What [`spawn`] hands back: the control handle and the event stream.
pub struct Spawned {
    pub session: Box<dyn Session>,
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
}

/// Start the backend for `mode`: one child process, one event stream.
///
/// This is the shape the ADR was written to pin down. The draft in looprs-u9e was
/// `spawn(mode, deps) -> Result<(Box<dyn Session>, UnboundedReceiver<Msg>)>`; it is
/// `SessionEvent` rather than `Msg` so that a session cannot reach UI-only concerns,
/// and a generation counter is added because a bare `TerminalType` is not an identity.
pub fn spawn(mode: TerminalType, cfg: &SessionConfig, generation: u64) -> anyhow::Result<Spawned> {
    let id = SessionId::new(mode, generation);
    match mode {
        TerminalType::Beads => BeadsSession::start(id, cfg),
        TerminalType::Pi => PiChatSession::start(id, cfg),
        TerminalType::Bash => BashSession::start(id, cfg),
    }
}

/// How the Router constructs sessions, as a value.
///
/// Injectable rather than hard-wired because the router's lifecycle rules — park,
/// resume, one per mode, no late events from a dead generation — are the part worth
/// testing, and testing them needs sessions whose behavior the test controls. The
/// repo's process-level test style survives: `default_factory` is the real thing,
/// and a test can equally hand the router fakes backed by real child processes
/// (see `crate::testing`).
pub type SessionFactory =
    Arc<dyn Fn(TerminalType, u64) -> anyhow::Result<Spawned> + Send + Sync + 'static>;

/// The production factory: real backends behind [`spawn`].
pub fn default_factory(cfg: SessionConfig) -> SessionFactory {
    Arc::new(move |mode, generation| spawn(mode, &cfg, generation))
}

/// One live terminal state.
///
/// Every method is **synchronous and non-blocking**, deliberately:
///
/// * No `async` in the trait means no `async-trait` dependency and no boxed-future
///   dance to keep the trait object-safe.
/// * A `Box<dyn Session>` is a *handle* onto a session that lives in its own tokio
///   task, which is why a non-blocking `send_text` is honest: it queues the work,
///   it does not do it. The result always arrives on the event stream.
/// * Consequence, and it matters: the router must never block waiting on a child.
///   If it did, `Esc` would queue behind a 30-second `prompt()` and cancellation
///   would be a lie. Nothing here returns anything a caller should await.
pub trait Session: Send {
    /// The id this session was spawned with, unchanged until it dies.
    fn id(&self) -> SessionId;

    /// The mode this session is the backend for.
    ///
    /// A convenience over `id().mode`. The router keys everything by
    /// [`SessionId`] and never asks a session for its mode, so this is the test
    /// surface for "the factory gave me the mode I asked for" (and for the
    /// lying-factory refusal).
    #[allow(dead_code)] // consumers: spawn/factory tests; routing is by `SessionId`
    fn mode(&self) -> TerminalType {
        self.id().mode
    }

    /// Queue a user turn. `Ok(())` means *accepted and queued*, not *finished*.
    /// Completion is [`SessionEvent::Exited`] for a shell command /
    /// [`PiEvent::AgentSettled`] for a run, never this return value.
    fn send_text(&mut self, text: String) -> anyhow::Result<()>;

    /// `Esc`. Cancel the in-flight work and leave the session usable. Idle session
    /// => no-op, not an error (looprs-5g7).
    ///
    /// Every implementation owes the same four-word answer, and [`cancel`] is the
    /// contract: silence when there is nothing to stop, `cancelling …` the moment
    /// the keystroke is acted on, `cancelled` when the child unwinds, and — if it
    /// has not unwound within [`cancel::GRACE`] — one loud line naming what is stuck
    /// and what the session did about it (ADR-0003). Never a quit, and never a
    /// timeout that only the log knows about.
    fn abort(&mut self) -> anyhow::Result<()>;

    /// Orderly teardown: close the child's stdin so it can dispose its runtime,
    /// then arrange for the kill. Returns immediately; the teardown itself runs in
    /// the session's task and ends with [`SessionEvent::Exited`].
    ///
    /// The caller bounds the wait, not the callee (looprs-ecr): if `Exited` does
    /// not arrive within the grace period, escalate to SIGKILL and move on.
    fn shutdown(&mut self) -> anyhow::Result<()>;

    /// Called by the router when this session becomes, or stops being, the one on
    /// screen. Default: nothing (correct for Bash and Pi, which are `KeepRunning`).
    /// BeadsSession uses it to stop starting new passes while hidden
    /// ([`SwitchAway::DrainThenPark`]).
    fn set_active(&mut self, _active: bool) -> anyhow::Result<()> {
        Ok(())
    }

    /// Raw keystrokes, for a child that currently owns the screen
    /// ([`SessionEvent::ScreenHeld`]).
    ///
    /// Distinct from [`Session::send_text`] in the way that matters: these bytes go
    /// to the child exactly as typed, with no line appended and no interpretation.
    /// A held program reads `0x1b` as `Esc` (vim: leave insert mode) and `0x0d` as
    /// Enter; a line-oriented `send_text` would send neither. Only a shell on a
    /// pty can honour this, so the default is a refusal rather than a silent
    /// ignore — a mode that cannot take raw keys should say so.
    fn send_bytes(&mut self, _bytes: Vec<u8>) -> anyhow::Result<()> {
        anyhow::bail!("this session has no raw keyboard");
    }

    /// The real terminal changed shape. Bash forwards this to its pty so the
    /// child wraps for the window it is actually shown in (ADR-0001 rule 6);
    /// the pi-backed modes do not care and inherit the no-op.
    fn resize(&mut self, _rows: u16, _cols: u16) -> anyhow::Result<()> {
        Ok(())
    }

    /// Liveness for the status row and for input gating.
    fn status(&self) -> SessionStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trait must stay object-safe and `Send`, or `Router`'s
    /// `HashMap<TerminalType, Box<dyn Session>>` does not compile.
    #[test]
    fn session_is_a_send_trait_object() {
        fn assert_trait_object<T: ?Sized + Send>() {}
        assert_trait_object::<dyn Session>();
    }

    /// **A default config cannot reach the network.**
    ///
    /// Roughly every test in this crate builds one of these, so this is the line
    /// that keeps "the suite never opened a socket to ntfy" a property of the type
    /// rather than a fact about whoever last set `LOOPRS_NTFY_URL`. `main` opts in
    /// to the real sink explicitly (see `main::run`); nothing else does.
    ///
    /// Checked through `Debug` because the trait is deliberately not downcastable:
    /// a session that could ask which sink it holds is a session that could choose
    /// to skip it.
    #[test]
    fn the_default_config_carries_the_silent_sink() {
        use crate::services::notification::Notifier;
        let cfg = SessionConfig::default();
        let shown = format!("{:?}", cfg.notifier);
        assert!(
            shown.contains("Noop"),
            "a default config must carry the silent sink, got {shown}"
        );
        // And using it is inert — compiles, sends, and fails nothing.
        let n: &dyn Notifier = cfg.notifier.as_ref();
        n.notify(crate::services::notification::BeadDone {
            id: "looprs-x".into(),
            title: "silent".into(),
        });
    }

    // `#[tokio::test]` because the Beads backend starts a task as part of starting
    // a session: a handle without a runtime is not a thing that can exist.
    #[tokio::test]
    async fn every_mode_spawns_a_session_with_its_own_id() {
        let cfg = SessionConfig {
            pi_bin: "true".into(),
            bd_bin: "true".into(),
            shell_bin: "true".into(),
            notifier: Arc::new(crate::services::notification::Noop),
            alt_screen_hosted: false,
            ..SessionConfig::default()
        };
        for (generation, mode) in TerminalType::ALL.into_iter().enumerate() {
            let spawned = spawn(mode, &cfg, generation as u64).expect("stub spawn must not fail");
            assert_eq!(spawned.session.mode(), mode);
            assert_eq!(
                spawned.session.id(),
                SessionId::new(mode, generation as u64)
            );
            assert_eq!(spawned.session.status(), SessionStatus::NotStarted);
        }
    }

    /// The reason the generation exists: same mode, different incarnation, not equal — and
    /// both hash distinctly, so a map keyed by `SessionId` cannot collide them.
    #[test]
    fn generation_distinguishes_reincarnations_of_one_mode() {
        let a = SessionId::new(TerminalType::Pi, 1);
        let b = SessionId::new(TerminalType::Pi, 2);
        assert_ne!(a, b);
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(a));
        assert!(seen.insert(b));
        assert_eq!(seen.len(), 2);
        // ...while the mode alone says "same thing", which is exactly the bug class.
        assert_eq!(a.mode, b.mode);
    }

    /// Policy is total and matches ADR-0002 Q3's table.
    #[test]
    fn switch_away_policy_covers_every_mode() {
        use SwitchAway::*;
        assert_eq!(TerminalType::Bash.switch_away_policy(), KeepRunning);
        assert_eq!(TerminalType::Pi.switch_away_policy(), KeepRunning);
        assert_eq!(TerminalType::Beads.switch_away_policy(), DrainThenPark);
    }

    /// The input box opens on one step and no other. Asserted here, on the step
    /// itself, because two different types (the beads session and the view) ask
    /// this question and must not answer it twice in two places.
    #[test]
    fn only_await_input_hands_the_keyboard_back_to_the_human() {
        assert!(BeadStep::AwaitInput.awaits_user());
        assert!(
            !BeadStep::CreateTickets.awaits_user(),
            "a planning loop owns the keyboard, not the user"
        );
        assert!(
            !BeadStep::WorkTickets.awaits_user(),
            "a working loop owns the keyboard, not the user"
        );
    }

    /// **The mode table, pure and with no subprocess** (looprs-6ol: "the mode
    /// table (Bash/Beads/Pi x Tab)").
    ///
    /// Tab is a permutation, not a suggestion. Asserted as the table itself, then
    /// as the properties the table has to have, so a change to `next()` that
    /// breaks the cycle fails with the row named rather than in somebody's muscle
    /// memory: a Tab that lands on the mode you were already on reads as a dropped
    /// keystroke, and a Tab that skips a mode reads as a lost session.
    #[test]
    fn tab_walks_the_whole_mode_table_and_back_to_where_it_started() {
        use TerminalType::*;
        let table = [(Bash, Beads), (Beads, Pi), (Pi, Bash)];

        // Every row of the table is what `next()` actually does.
        for (from, to) in table {
            assert_eq!(
                from.next(),
                to,
                "Tab from {} goes to {}",
                from.label(),
                to.label()
            );
        }

        // It is one 3-cycle, not three separate hops: each mode is exactly one
        // source and exactly one target, and none of them is a fixed point.
        for mode in TerminalType::ALL {
            let as_source = table.iter().filter(|(f, _)| *f == mode).count();
            let as_target = table.iter().filter(|(_, t)| *t == mode).count();
            assert_eq!(
                (as_source, as_target),
                (1, 1),
                "{mode:?} is not one-in-one-out of the Tab table"
            );
            assert_ne!(
                mode.next(),
                mode,
                "Tab from {mode:?} must actually change mode"
            );
        }

        // Three Tabs is exactly one lap, from anywhere: no early close, no drift.
        for start in TerminalType::ALL {
            let mut m = start;
            for _ in 0..2 {
                m = m.next();
                assert_ne!(m, start, "the cycle closed early from {start:?}");
            }
            m = m.next();
            assert_eq!(m, start, "three Tabs from {start:?} must land back on it");
        }

        // The table and `ALL` describe the same three modes, once each — so a
        // fourth mode cannot be added to the enum without a row here.
        let mut from_table: Vec<String> = table.iter().map(|(f, _)| format!("{f:?}")).collect();
        let mut all: Vec<String> = TerminalType::ALL.iter().map(|m| format!("{m:?}")).collect();
        from_table.sort();
        all.sort();
        assert_eq!(from_table, all, "the Tab table does not cover `ALL`");
    }

    /// **The mode x Tab x policy table.** What `Tab` does *to* the mode it leaves
    /// is per-mode policy (ADR-0002 Q3), and it is the row a future change has to
    /// answer for. Asserted as one row per mode, with the coverage count spelled
    /// out: three modes, three rows, no missing cell.
    #[test]
    fn every_row_of_the_mode_table_names_where_tab_goes_and_what_it_leaves_behind() {
        use SwitchAway::*;
        use TerminalType::*;
        let table = [
            // mode,   Tab goes to, what leaving it does
            (Beads, Pi, DrainThenPark),
            (Pi, Bash, KeepRunning),
            (Bash, Beads, KeepRunning),
        ];

        assert_eq!(
            table.len(),
            TerminalType::ALL.len(),
            "the table must have exactly one row per mode"
        );
        for (mode, target, policy) in table {
            assert_eq!(mode.next(), target, "Tab target for {mode:?}");
            assert_eq!(
                mode.switch_away_policy(),
                policy,
                "switch-away policy for {mode:?}"
            );
        }
        // Only Beads is `DrainThenPark`: it is the mode that spends money on its
        // own, so it is the only one that must stop starting things while hidden.
        let parking: Vec<TerminalType> = TerminalType::ALL
            .into_iter()
            .filter(|m| m.switch_away_policy() == DrainThenPark)
            .collect();
        assert_eq!(parking, vec![Beads], "only the self-advancing mode parks");
    }
}
