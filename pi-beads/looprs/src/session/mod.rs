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

#![allow(dead_code)] // some of this layer is contract surface for tickets that have not landed

pub mod bash;
pub mod beads;
pub mod cancel;
pub mod pi_chat;
pub mod router;
pub mod view;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::PiEvent;

pub use bash::BashSession;
pub use beads::BeadsSession;
pub use pi_chat::PiChatSession;
pub use view::ChatState;

/// The three terminal states. This is a *session* identity, not a widget property, so
/// it lives here; `components::input` re-exports it for the input box that cycles it.
///
/// Note the spelling: the variant is `Beeds` (as the input box labels it). This ADR
/// prose calls the mode "Beads"; renaming the variant is deliberately out of scope so
/// that the ADR ticket stays type-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalType {
    Beeds,
    Pi,
    Bash,
}

impl TerminalType {
    /// All modes, in Tab order.
    pub const ALL: [TerminalType; 3] = [TerminalType::Beeds, TerminalType::Pi, TerminalType::Bash];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::Beeds => "Beeds",
            Self::Pi => "Pi",
        }
    }

    /// What `Tab` goes to next. Kept on the type (not on `InputState`) so the router
    /// can reason about the cycle too.
    pub fn next(self) -> Self {
        match self {
            Self::Bash => Self::Beeds,
            Self::Beeds => Self::Pi,
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
            Self::Beeds => SwitchAway::DrainThenPark,
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
    Stdout,
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
/// A session never sees [`crate::app::Msg`]: it knows nothing about ticks, resize
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

/// The beads loop's own state, owned by the beads *session*, never by the UI.
///
/// `looprs-msj` is the ticket whose whole bug was that `App` used to derive this
/// state from `self.input.mode`. With the envelope above, the App only *renders*
/// a step it was handed — and the loop that moves between these steps is moved by
/// its own worker, inside `BeadsSession`, not by anything upstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeadStep {
    AwaitInput,
    CreateTickets,
    WorkTickets,
}

/// Everything a session needs in order to be started, injected so tests can point
/// at fake binaries (see `src/testing.rs`).
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub pi_bin: String,
    pub bd_bin: String,
    /// The shell BashSession spawns. `$SHELL` if set, else `/bin/bash`.
    pub shell_bin: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            pi_bin: std::env::var("LOOPRS_PI_BIN").unwrap_or_else(|_| "pi".to_string()),
            bd_bin: std::env::var("LOOPRS_BD_BIN").unwrap_or_else(|_| "bd".to_string()),
            shell_bin: default_shell_bin(),
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
    if let Ok(v) = std::env::var("LOOPRS_SHELL_BIN") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(s) = std::env::var("SHELL") {
        if looks_like_bash(&s) {
            return s;
        }
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
        TerminalType::Beeds => BeadsSession::start(id, cfg),
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

    /// Convenience for `App`: is this session doing something right now?
    fn is_running(&self) -> bool {
        self.status().is_busy()
    }
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

    // `#[tokio::test]` because the Beads backend starts a task as part of starting
    // a session: a handle without a runtime is not a thing that can exist.
    #[tokio::test]
    async fn every_mode_spawns_a_session_with_its_own_id() {
        let cfg = SessionConfig {
            pi_bin: "true".into(),
            bd_bin: "true".into(),
            shell_bin: "true".into(),
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
        assert_eq!(TerminalType::Beeds.switch_away_policy(), DrainThenPark);
    }
}
