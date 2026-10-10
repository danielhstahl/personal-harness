//! The clipboard sink (looprs-pdl.10).
//!
//! **"The copy is selected at the speed of the mouse and lands at the speed of a
//! process spawn."** That one sentence is the whole design of this file. The
//! user's drag release is an interactive event — the toast that confirms it is
//! worth ~25 ms — and the thing it triggers is an external program that can
//! stall on a wedged Wayland compositor, a locked keychain, or an SSH socket
//! that stopped pumping. A copy that runs inline on the UI task is therefore a
//! UI that stops responding to the keyboard for as long as `pbcopy` takes, and
//! a copy that runs inline *and* is confirmed optimistically is a lie the user
//! discovers later, somewhere else, at the worst possible moment.
//!
//! So this is a **sink**, in the exact shape [`crate::services::notification`]
//! established for the same problem, with one difference that this ticket had to
//! reason about separately (see "Which failures are early and which are late"
//! below): the notifier confirms a *fact* nobody is waiting on, and this confirms
//! a *result* the user is standing there waiting for.
//!
//! # The rule, inherited from the notifier
//!
//! * `copy` **never blocks**. It is a queue hand-off, not a write. The write
//!   happens in this module's own task.
//! * `SessionConfig::default()` carries [`Noop`], so the whole test suite is
//!   clipboard-free **by construction** rather than by nobody remembering to
//!   unset an environment variable. `main` is the only caller of
//!   [`clipboard_from_env`], the same discipline as `notifier_from_env`.
//! * The UI never gets an error to handle. A copy that failed is a *report*, not
//!   a `Result` on the interactive path: the app's job is to show it, not to
//!   decide what to do about it.
//!
//! # Depth 1, latest wins (ADR-0004 R11)
//!
//! The queue holds **one** pending copy, and a new copy replaces it. This is not
//! an optimisation. A queue of depth four is four writes of three selections the
//! user has already replaced, three clipboard-manager history entries for text
//! nobody wants, and — if the fourth one is slow — a `Copied 40 characters`
//! toast for the *superseded* selection while the user is looking at the new
//! one. The most recent request is the only one that still describes what the
//! user selected, so it is the only one that survives being queued.
//!
//! # Which failures are early and which are late
//!
//! The notifier's rule is "fire and log". That is wrong here, because the toast
//! has to report the result the user cares about, not merely "queued". The
//! ladder (ADR-0004 R20) splits on whether the transport can tell us anything:
//!
//! | When | What we know | What the user sees |
//! | --- | --- | --- |
//! | at the call | the sink is off / the queue is dead | a failure toast, immediately |
//! | after a native write + read-back | verified, or a mismatch | `Copied …` / `Not copied …` |
//! | after an OSC 52 write | it was *sent*, never that it *landed* | `Copied … · OSC 52 (not confirmed)` |
//! | never (a stalled helper or a stalled tty) | nothing | a **late** failure at [`COPY_TIMEOUT`], not silence |
//!
//! A native failure is *detectable*; an OSC 52 failure is not — `spikes/results/terminal-matrix.md` #3
//! measured Apple Terminal taking a 64 B copy and delivering **nothing in 6.01 s
//! with no error of any kind on either end**. The design does not pretend to see
//! that. It does the next best thing: it says *which kind* of "we sent it" this
//! was, in the same words that carry the count, and it never lets the last thing
//! the user saw be an optimistic `Copied` over a transport that cannot confirm.
//!
//! # Transports are chosen by *where this process is*, not by a query (R5–R7)
//!
//! `auto`: same session as the clipboard (no SSH variables) and a helper on
//! `PATH` → native helper **verified by read-back**; otherwise OSC 52, which is
//! the only thing that reaches the clipboard on the *user's* side of an SSH hop.
//! Exactly one transport per copy (R8) — a fallback happens *instead*, never *as
//! well*, because if both are written nothing says which one `Cmd-V` reads.
//! `LOOPRS_CLIPBOARD=auto|native|osc52|off` overrides. There is deliberately
//! no capability query: everything the decision needs is free to read, and a
//! terminal that answers a query tells you about its parser, not its policy.
//!
//! # Never truncate (R9)
//!
//! What the user selected is what goes, or nothing goes and the toast says so.
//! The one exception is a cap the *user* asked for (`LOOPRS_CLIPBOARD_MAX_BYTES`),
//! and even then the copy reports the count that actually went rather than the
//! count that was asked for — see [`CopyOutcome::Partial`].

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::process::Command;
use tokio::sync::{Notify, oneshot};

/// How long the copy task gets to answer before we stop calling it "in flight"
/// (ADR-0004 R21: 2 s).
///
/// Both transports that can wedge are covered by it: a native helper parked on a
/// compositor, and an OSC 52 write into an SSH connection that has stopped
/// pumping. Measured healthy costs for comparison: **~19 ms** for a verified
/// native copy and **0.13 s** for a 4 KiB OSC 52 copy across a real SSH hop
/// (`spikes/results/clipboard-cost.log`), so this is ~100× the slow thing that
/// works and ~1× the thing that does not.
pub const COPY_TIMEOUT: Duration = Duration::from_secs(2);

/// The count the toast prints, and the unit it prints it in.
///
/// **Characters: `chars().count()` of the exact string handed to the sink**
/// (ADR-0004 R19). Not `str::len()` (bytes) and not `unicode_width` (cells):
/// the spike measured those three apart by up to 9× on ordinary text — the ZWJ
/// family `👩‍👩‍👦` is 5 characters, 2 cells, 18 bytes — and cells are not even a
/// property of the copy, because our own wrap can change a row's cell count on a
/// resize while the copied text does not change at all.
///
/// The type exists so the count cannot drift to `len()` under pressure, which is
/// the exact drift the terminal matrix warns about. It is computed **once**, here,
/// from the string that goes to the sink, and every [`CopyOutcome`] carries that
/// one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Chars(usize);

impl Chars {
    /// The count of `text`, in characters, from the string that is about to be copied.
    pub fn of(text: &str) -> Self {
        Chars(text.chars().count())
    }

    pub fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for Chars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", thousands(self.0))
    }
}

/// `1284` -> `1,284`. The separator belongs to the toast, not to the count
/// (R19): the count's job is to name its unit, the display's job is to be
/// readable.
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3 + 1);
    for (i, c) in digits.char_indices() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Which transport carried the bytes. Exactly one per copy (R8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A native helper (`pbcopy` / `wl-copy` / `xclip`).
    Native,
    /// `ESC ] 52 ; c ; <base64> ST` to our own tty.
    Osc52,
}

impl Transport {
    /// The word the toast uses for this transport.
    pub fn label(self) -> &'static str {
        match self {
            Transport::Native => "clipboard",
            Transport::Osc52 => "OSC 52",
        }
    }
}

/// What a copy attempt ended up being.
///
/// The verb is the payload: `Copied` is reserved for text we can stand behind,
/// and the suffix says *how* we can stand behind it. There is no
/// "probably worked" in this type, and no optimistic variant at all — R20's
/// "an optimistic toast on a failed copy is worse than no toast".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// A native helper wrote it and a read-back through the matching reader
    /// agreed, byte for byte (R6). The only fully-confirmed state.
    Verified { chars: Chars },
    /// The bytes are gone, and no reader exists to prove they landed: OSC 52,
    /// a remote session, or a native helper with no matching reader.
    ///
    /// `note` carries a fallback that happened on the way here — "helper failed:
    /// pbcopy exited 1" — appended **once**, because the user's question is
    /// "is it on my clipboard?", not "how many transports did you try?".
    Sent {
        chars: Chars,
        transport: Transport,
        note: Option<String>,
    },
    /// The native write succeeded and the read-back came back **different**.
    /// Somebody else copied between the two spawns, or the helper mangled it.
    /// Never reported as a copy (R20's mismatch row).
    Mismatch { chars: Chars },
    /// A cap the *user* configured cut the payload short. Reports the count that
    /// **actually went** and the count that was asked for, so a truncated copy
    /// can never carry a count that describes the untruncated selection.
    Partial {
        chars: Chars,
        asked: Chars,
        transport: Transport,
        max_bytes: usize,
    },
    /// Nothing went anywhere. `reason` is written to be acted on.
    Failed { reason: String },
}

impl CopyOutcome {
    /// Did text reach (or go to) the clipboard? `false` for the failure states,
    /// which is the difference between a `Copied` toast and a `Copy failed` one.
    pub fn is_copied(&self) -> bool {
        matches!(
            self,
            CopyOutcome::Verified { .. } | CopyOutcome::Sent { .. } | CopyOutcome::Partial { .. }
        )
    }

    /// Is this a state the toast should paint as an error?
    pub fn is_failure(&self) -> bool {
        !self.is_copied()
    }

    /// The toast's text. Kept here, next to the variants, so the wording cannot
    /// drift from the state it describes and so every branch is a table row in
    /// the tests below.
    pub fn toast(&self) -> String {
        match self {
            CopyOutcome::Verified { chars } => {
                format!("Copied {chars} characters \u{b7} clipboard")
            }
            CopyOutcome::Sent {
                chars,
                transport,
                note,
            } => {
                let base = format!(
                    "Copied {chars} characters \u{b7} {} (not confirmed)",
                    transport.label()
                );
                match note {
                    Some(n) if !n.trim().is_empty() => format!("{base} \u{2014} {}", n.trim()),
                    _ => base,
                }
            }
            CopyOutcome::Mismatch { chars } => format!(
                "Not copied: the clipboard changed under us \u{2014} select again ({chars} characters were written and read back differently)"
            ),
            CopyOutcome::Partial {
                chars,
                asked,
                transport,
                max_bytes,
            } => format!(
                "Copied {chars} of {asked} characters \u{b7} {} ({} bytes), truncated at the {} byte cap you set",
                transport.label(),
                thousands(*max_bytes),
                thousands(*max_bytes)
            ),
            CopyOutcome::Failed { reason } => format!("Copy failed: {reason}"),
        }
    }
}

/// The handle a copy comes back with.
///
/// The UI holds one of these and **polls** it — from `App::on_tick`, never by
/// awaiting, because the UI task must not be parked on a clipboard under any
/// circumstances. `None` means still in flight; the caller's own deadline
/// ([`COPY_TIMEOUT`]) is what turns "in flight forever" into a visible failure,
/// so a sink that never answers cannot leave the user staring at nothing.
#[derive(Debug)]
pub struct Receipt {
    rx: Mutex<Option<oneshot::Receiver<CopyOutcome>>>,
}

impl Receipt {
    fn pending(rx: oneshot::Receiver<CopyOutcome>) -> Self {
        Self {
            rx: Mutex::new(Some(rx)),
        }
    }

    /// A receipt whose reply has not come and will not come of its own accord —
    /// the seam [`crate::testing::StallClipboard`] uses so that "the transport
    /// never answered" is a test case rather than a story.
    #[allow(dead_code)] // test seam: only `crate::testing` builds one; the shipped binary never stalls a receipt on purpose
    pub fn stalled(rx: oneshot::Receiver<CopyOutcome>) -> Self {
        Self::pending(rx)
    }

    /// A receipt already answered, for sinks that decide on the spot ([`Noop`],
    /// [`crate::testing::RecordingClipboard`]).
    pub fn ready(outcome: CopyOutcome) -> Self {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(outcome);
        Self::pending(rx)
    }

    /// Take the result if it has arrived. `None` = still in flight.
    ///
    /// A dropped sender is answered as a failure rather than as `None` forever:
    /// the copy task died, which the user is owed to hear.
    pub fn poll(&self) -> Option<CopyOutcome> {
        let mut guard = self.rx.lock().ok()?;
        let rx = guard.as_mut()?;
        match rx.try_recv() {
            Ok(outcome) => {
                *guard = None;
                Some(outcome)
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                *guard = None;
                Some(CopyOutcome::Failed {
                    reason: "the copy task died without answering".into(),
                })
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
        }
    }

    /// Abandon the receipt: the caller has reported the timeout and is done with
    /// it. Drops the receiver so the task's late answer goes nowhere rather than
    /// being held.
    pub fn abandon(&self) {
        if let Ok(mut guard) = self.rx.lock() {
            *guard = None;
        }
    }
}

/// A sink for "put this text on the user's clipboard, and tell me later whether
/// you did".
///
/// Implementations may not block and may not fail the caller. `copy` hands the
/// bytes over and returns a [`Receipt`]; everything that can wedge — a process
/// spawn, a tty write, a compositor — happens in the sink's own task, under
/// [`COPY_TIMEOUT`].
pub trait Clipboard: Send + Sync + fmt::Debug + 'static {
    /// Queue a copy. Returns immediately.
    fn copy(&self, text: String) -> Receipt;

    /// What this sink *is*, for the log line at startup and for a failure's text.
    fn describe(&self) -> &'static str;
}

/// The sink that throws the text away.
///
/// The default on `SessionConfig`, which is the whole reason ~550 tests can
/// exist without a clipboard. It answers immediately with a failure rather than
/// never answering, because the caller still has a toast to get right: a
/// clipboard that is off did **not** copy, and saying `Copied` from a Noop is
/// exactly the confident lie this ticket exists to make impossible.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noop;

impl Clipboard for Noop {
    fn copy(&self, _text: String) -> Receipt {
        Receipt::ready(CopyOutcome::Failed {
            reason: "clipboard is off (LOOPRS_CLIPBOARD=off)".into(),
        })
    }

    fn describe(&self) -> &'static str {
        "off"
    }
}

/// The blocking half: one copy, start to finish.
///
/// Async rather than sync on purpose: a native helper is a process, and the only
/// honest way to bound a process that may never exit is `tokio::time::timeout`
/// with `kill_on_drop` on the child. A `std::process::Child` waited on a blocking
/// thread with a timeout is a thread and a child process we cannot reap.
pub(crate) trait TransportWriter: Send + Sync + fmt::Debug + 'static {
    fn write<'a>(&'a self, text: &'a str, chars: Chars) -> BoxFuture<'a, CopyOutcome>;
}

// ─────────────────────────── native helper ───────────────────────────

/// A native clipboard helper: the write command and the matching reader.
///
/// Paired deliberately. A writer with no reader is a transport we cannot verify,
/// which makes it an OSC 52 with extra steps — so the pair is the unit that gets
/// detected (`native_helpers`), and the ladder treats a missing pair as "no
/// native transport here".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Helper {
    /// `argv[0..]` of the writer; text goes to its stdin.
    pub write: Vec<String>,
    /// `argv[0..]` of the reader; the clipboard comes back on its stdout.
    pub read: Vec<String>,
    /// A short name for the log and for failure text.
    pub name: &'static str,
}

/// The helpers this host has, in preference order.
///
/// macOS's `pbcopy` first because it is the one with a matching reader that
/// cannot lie about a trailing newline; then Wayland, then X11. Detection is a
/// `PATH` scan rather than a `command -v` spawn because it happens at startup and
/// must cost one `stat` per candidate, not one process.
pub fn native_helpers() -> Vec<Helper> {
    let mut out = Vec::new();
    if which("pbcopy").is_some() && which("pbpaste").is_some() {
        out.push(Helper {
            write: vec!["pbcopy".into()],
            read: vec!["pbpaste".into()],
            name: "pbcopy",
        });
    }
    if which("wl-copy").is_some() && which("wl-paste").is_some() {
        out.push(Helper {
            // `--no-clipboard` keeps wl-copy from *also* claiming the X11
            // primary selection through its own synchronisation, which is a
            // second clipboard we did not ask for and cannot read back.
            write: vec!["wl-copy".into(), "--no-clipboard".into()],
            // `--no-newline` for the same reason `--no-clipboard` is on the
            // write: `wl-paste` adds a trailing newline that the copy never
            // had, and that newline reads as a mismatch on every single copy.
            read: vec!["wl-paste".into(), "--no-newline".into()],
            name: "wl-copy",
        });
    }
    if which("xclip").is_some() {
        out.push(Helper {
            write: vec!["xclip".into(), "-selection".into(), "clipboard".into()],
            read: vec![
                "xclip".into(),
                "-selection".into(),
                "clipboard".into(),
                "-o".into(),
            ],
            name: "xclip",
        });
    }
    out
}

/// `PATH` lookup without spawning anything.
pub fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(std::path::Path::new)
        .find(|p| p.join(cmd).is_file())
        .map(|p| p.join(cmd))
}

/// The native-helper transport: write, then read it back and compare (R6).
struct NativeWriter {
    helper: Helper,
    /// Where the read-back goes. A seam so the read path is testable without a
    /// clipboard on the machine running the tests.
    ///
    /// Manual `Debug` because a boxed closure is not `Debug`; the helper's name
    /// is the part worth seeing in a log anyway.
    reader: Option<Box<dyn Fn() -> anyhow::Result<String> + Send + Sync>>,
}

impl fmt::Debug for NativeWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeWriter")
            .field("helper", &self.helper.name)
            .field(
                "reader",
                &if self.reader.is_some() {
                    "injected"
                } else {
                    "spawn"
                },
            )
            .finish()
    }
}

impl NativeWriter {
    fn new(helper: Helper) -> Self {
        Self {
            helper,
            reader: None,
        }
    }
}

impl TransportWriter for NativeWriter {
    fn write<'a>(&'a self, text: &'a str, chars: Chars) -> BoxFuture<'a, CopyOutcome> {
        Box::pin(async move {
            let helper = &self.helper;
            let write = spawn_stdin(&helper.write, text.as_bytes()).await;
            match write {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    return CopyOutcome::Failed {
                        reason: format!(
                            "{} exited {}: {}",
                            helper.name,
                            out.status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "on a signal".into()),
                            trim_brief(&out.stderr)
                        ),
                    };
                }
                Err(e) => {
                    return CopyOutcome::Failed {
                        reason: format!("cannot run {}: {e}", helper.name),
                    };
                }
            };
            // The write worked. Now the half that makes `Copied` mean something:
            // read it back through the route a paste takes.
            let back = match self.reader.as_ref() {
                Some(f) => f(),
                None => run(&helper.read)
                    .await
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()),
            };
            match back {
                Ok(read) if read == text => CopyOutcome::Verified { chars },
                Ok(read) => {
                    tracing::debug!(
                        wrote = text.len(),
                        read = read.len(),
                        "clipboard read-back did not match the copy"
                    );
                    CopyOutcome::Mismatch { chars }
                }
                Err(e) => {
                    // We did write it; we just cannot prove it. That is the OSC
                    // 52 class of confidence, on a native transport, and it is
                    // reported as such rather than as a verification.
                    CopyOutcome::Sent {
                        chars,
                        transport: Transport::Native,
                        note: Some(format!("no read-back: {e}")),
                    }
                }
            }
        })
    }
}

// ───────────────────────────── OSC 52 ─────────────────────────────

/// The OSC 52 transport: base64 the payload, frame it, write it to our own tty.
///
/// Cost measured: **0.002 ms** for 128 B and **6.1 ms** for 1 MiB
/// (`spikes/results/clipboard-cost.log`) — 10²–10⁴× cheaper in-process than a
/// native spawn, which is why it is the right answer when there is nothing else
/// to read the result back from, and the wrong answer to *verify* (see
/// `terminal-matrix.md` #3: Apple Terminal landed **0 bytes at any size**).
///
/// It is also the only transport that crosses a network hop into the window the
/// user is actually looking at (R7), which is the whole reason it is not simply
/// dropped.
#[derive(Debug, Clone)]
pub struct Osc52Writer {
    /// A cap the user asked for, in bytes of payload. `None` (the default) means
    /// **no cap of our own** (R9).
    pub max_bytes: Option<usize>,
    /// Where the framed bytes go. `crate::screen::tee` in production; a capture
    /// closure in a test, so a unit test never writes escape sequences into the
    /// test harness's own stdout.
    sink: fn(&[u8]),
}

impl Osc52Writer {
    /// The production writer: straight to the real terminal.
    pub fn new(max_bytes: Option<usize>) -> Self {
        Self {
            max_bytes,
            sink: crate::screen::tee,
        }
    }

    /// The same writer with the terminal replaced by something a test can read.
    #[allow(dead_code)] // test seam: `main` uses `new()`; tests use this to read the bytes
    pub fn with_sink(max_bytes: Option<usize>, sink: fn(&[u8])) -> Self {
        Self { max_bytes, sink }
    }
}

impl TransportWriter for Osc52Writer {
    fn write<'a>(&'a self, text: &'a str, chars: Chars) -> BoxFuture<'a, CopyOutcome> {
        Box::pin(async move {
            let max = self.max_bytes.unwrap_or(usize::MAX);
            // Over the user's cap: send what fits, snapped down to a character
            // boundary. A copy that ends in the middle of a multi-byte character
            // is not a shorter copy, it is a corrupt one.
            let cut = floor_char_boundary(text, max);
            let payload = &text[..cut];
            let copied = Chars::of(payload);
            let bytes = osc52_bytes(payload);
            // The sink writes straight to the real terminal, which is the same
            // door the full-screen passthrough uses: an OSC 52 is not screen
            // content, so it must not go through the frame (and the frame is
            // diffed, so a sequence that produces no cell change would be
            // elided entirely).
            (self.sink)(&bytes);
            if copied < chars {
                CopyOutcome::Partial {
                    chars: copied,
                    asked: chars,
                    transport: Transport::Osc52,
                    max_bytes: max,
                }
            } else {
                CopyOutcome::Sent {
                    chars,
                    transport: Transport::Osc52,
                    note: None,
                }
            }
        })
    }
}

/// The OSC 52 frame for `text`, from crossterm's own writer.
///
/// Split out and pure so a test can assert the wire bytes without a terminal:
/// `ESC ] 52 ; c ; <base64> ST`.
pub fn osc52_bytes(text: &str) -> Vec<u8> {
    let mut wire = Vec::new();
    // The `osc52` feature is enabled in Cargo.toml for the spike that measured
    // these bytes; using the same writer here means the app's framing and the
    // spike's measured framing are one implementation, not two that agree.
    let _ = crossterm::queue!(
        &mut wire,
        crossterm::clipboard::CopyToClipboard::to_clipboard_from(text.as_bytes())
    );
    wire
}

/// The largest byte index `<= max` that is a character boundary.
fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut i = max;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// ──────────────────────── the layered transport ────────────────────────

/// Primary with a fallback taken **instead** of as well (R8).
#[derive(Debug)]
struct FallbackWriter {
    primary: Box<dyn TransportWriter>,
    fallback: Box<dyn TransportWriter>,
    primary_name: &'static str,
}

impl TransportWriter for FallbackWriter {
    fn write<'a>(&'a self, text: &'a str, chars: Chars) -> BoxFuture<'a, CopyOutcome> {
        Box::pin(async move {
            let first = self.primary.write(text, chars).await;
            if !first.is_failure() {
                return first;
            }
            let reason = match &first {
                CopyOutcome::Failed { reason } => reason.clone(),
                other => other.toast(),
            };
            tracing::warn!("native clipboard copy failed ({reason}); falling back to OSC 52");
            let second = self.fallback.write(text, chars).await;
            if second.is_failure() {
                // R20's both-transports row: name both, because "it failed" with
                // two transports involved is not actionable.
                let CopyOutcome::Failed {
                    reason: second_reason,
                } = &second
                else {
                    return second;
                };
                return CopyOutcome::Failed {
                    reason: format!(
                        "nothing was copied ({}: {reason}; OSC 52: {second_reason})",
                        self.primary_name
                    ),
                };
            }
            // The fallback worked; carry the fallback's own note plus the fact
            // that a helper was tried and failed, once.
            match second {
                CopyOutcome::Sent {
                    chars,
                    transport,
                    note,
                } => CopyOutcome::Sent {
                    chars,
                    transport,
                    note: Some(match note {
                        Some(n) => format!("{} failed: {reason}; {n}", self.primary_name),
                        None => format!("{} failed: {reason}", self.primary_name),
                    }),
                },
                other => other,
            }
        })
    }
}

// ───────────────────────── the queued sink itself ─────────────────────────

/// One slot, one task, one write at a time.
struct Slot {
    job: Mutex<Option<Job>>,
    wake: Notify,
    /// Set when the sink is dropped, so the worker stops instead of parking on
    /// `notified()` for the rest of the process. The sink's lifetime is the
    /// task's lifetime, which is the promise `Ntfy::spawn` makes and this
    /// mirrors.
    closed: AtomicBool,
}

struct Job {
    text: String,
    chars: Chars,
    reply: oneshot::Sender<CopyOutcome>,
}

/// The production sink: a depth-1 latest-wins queue read by one task that owns
/// the transport.
///
/// `Debug` because `SessionConfig` is `Debug` and gets logged; the queue contents
/// deliberately are not, because they are the user's copied text.
pub struct QueuedClipboard {
    slot: Arc<Slot>,
    what: &'static str,
}

impl fmt::Debug for QueuedClipboard {
    /// The queued text is deliberately *not* in here: it is whatever the user
    /// last selected, and `SessionConfig` gets logged.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuedClipboard")
            .field("transport", &self.what)
            .finish()
    }
}

impl Clipboard for QueuedClipboard {
    /// Hand the text over and walk away.
    ///
    /// The count is taken here, from this exact string, before anything moves it:
    /// the number in the toast and the bytes on the wire are then the same
    /// computation over the same value (R19).
    fn copy(&self, text: String) -> Receipt {
        let chars = Chars::of(&text);
        let (tx, rx) = oneshot::channel();
        let job = Job {
            text,
            chars,
            reply: tx,
        };
        // **Latest wins.** Overwriting whatever is in the slot means a burst of
        // releases writes once — the last one — and the superseded selections
        // never reach a transport at all.
        if let Ok(mut slot) = self.slot.job.lock() {
            *slot = Some(job);
        } else {
            return Receipt::ready(CopyOutcome::Failed {
                reason: "the copy queue is poisoned".into(),
            });
        }
        self.slot.wake.notify_one();
        Receipt::pending(rx)
    }

    fn describe(&self) -> &'static str {
        self.what
    }
}

impl Drop for QueuedClipboard {
    /// Dropping the sink ends the worker task.
    ///
    /// The same lifetime rule as `Ntfy::spawn`: a worker outliving its sink is a
    /// writer with nobody left to report to, and a task parked on `notified()`
    /// never wakes to find that out.
    fn drop(&mut self) {
        self.slot.closed.store(true, Ordering::SeqCst);
        self.slot.wake.notify_one();
    }
}

/// Build the sink and its worker task.
///
/// The task lives as long as the `QueuedClipboard`: it exits when the last
/// `Arc<Slot>` sender goes, which is the sink's own lifetime — the same shape as
/// `Ntfy::spawn`, and for the same reason (a worker outliving its sink would be
/// a writer with nobody to report to).
pub fn spawn(writer: impl TransportWriter, what: &'static str) -> Arc<dyn Clipboard> {
    spawn_with(writer, what, COPY_TIMEOUT)
}

/// [`spawn`] with the deadline as an argument, so the late-failure path is a
/// test that takes milliseconds instead of a test that sleeps for two seconds.
pub fn spawn_with(
    writer: impl TransportWriter,
    what: &'static str,
    timeout: Duration,
) -> Arc<dyn Clipboard> {
    let slot = Arc::new(Slot {
        job: Mutex::new(None),
        wake: Notify::new(),
        closed: AtomicBool::new(false),
    });
    let worker_slot = Arc::clone(&slot);
    let task_writer: Arc<dyn TransportWriter> = Arc::new(writer);
    // The transport lives in the task, not in the sink: nothing on the UI side
    // holds a handle that could write to a clipboard directly.
    tokio::spawn(async move {
        loop {
            worker_slot.wake.notified().await;
            if worker_slot.closed.load(Ordering::SeqCst) {
                tracing::debug!("clipboard task finished: the sink was dropped");
                return;
            }
            let taken = match worker_slot.job.lock() {
                Ok(mut guard) => guard.take(),
                Err(_) => return,
            };
            let Some(job) = taken else { continue };
            let (text, chars, reply) = (job.text, job.chars, job.reply);
            let outcome = match tokio::time::timeout(timeout, task_writer.write(&text, chars)).await
            {
                Ok(outcome) => outcome,
                Err(_) => {
                    tracing::warn!(
                        "clipboard copy did not answer within {timeout:?}; the transport may still be wedged"
                    );
                    CopyOutcome::Failed {
                        reason: format!(
                            "clipboard did not answer in {}s \u{2014} nothing confirmed",
                            timeout.as_secs()
                        ),
                    }
                }
            };
            // A dead receiver means the App timed this copy out and painted the
            // late-failure toast already; nothing left to do but let the write go.
            let _ = reply.send(outcome);
        }
    });
    Arc::new(QueuedClipboard { slot, what })
}

/// The sink the environment asks for.
///
/// `LOOPRS_CLIPBOARD=auto|native|osc52|off`, default `auto`. Never a startup
/// failure: a sink the environment asks for that cannot be built degrades to
/// [`Noop`] with the reason logged, because the beads loop is the feature and it
/// works without a clipboard.
pub fn clipboard_from_env() -> Arc<dyn Clipboard> {
    match clipboard_for(
        env_value("LOOPRS_CLIPBOARD").as_deref(),
        env_value("LOOPRS_CLIPBOARD_MAX_BYTES").as_deref(),
        native_helpers(),
        ssh_session(
            std::env::var("SSH_CONNECTION"),
            std::env::var("SSH_CLIENT"),
            std::env::var("SSH_TTY"),
        ),
    ) {
        Ok(sink) => {
            tracing::info!("clipboard: {}", sink.describe());
            sink
        }
        Err(e) => {
            tracing::error!("clipboard disabled: {e}");
            Arc::new(Noop)
        }
    }
}

/// The transport decision as a pure function.
///
/// Pure (environment, PATH results and the SSH facts come in as arguments) so
/// the ladder is a table in the tests rather than a mutation of process-global
/// state that every other test in the process shares.
fn clipboard_for(
    choice: Option<&str>,
    max_bytes: Option<&str>,
    helpers: Vec<Helper>,
    remote: bool,
) -> anyhow::Result<Arc<dyn Clipboard>> {
    let cap = parse_cap(max_bytes)?;
    let osc = || Osc52Writer::new(cap);
    match choice.unwrap_or("auto") {
        "off" | "none" | "0" => Ok(Arc::new(Noop)),
        "osc52" => Ok(spawn(osc(), "osc52")),
        "native" => {
            let helper = helpers
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("LOOPRS_CLIPBOARD=native but no clipboard helper (pbcopy/wl-copy/xclip) is on PATH"))?;
            let name: &'static str = helper.name;
            Ok(spawn(NativeWriter::new(helper), name))
        }
        // Everything else — `auto`, and an unset variable — is the
        // decide-from-the-environment path.
        _ => {
            if remote {
                // R7: a native helper on the far side writes the *remote*
                // clipboard, which is not where the user's paste is.
                Ok(spawn(osc(), "osc52 (remote session)"))
            } else if let Some(helper) = helpers.into_iter().next() {
                let name: &'static str = helper.name;
                // The fallback rides *inside* the transport chain, so it is one
                // copy with one answer rather than two copies.
                Ok(spawn(
                    FallbackWriter {
                        primary: Box::new(NativeWriter::new(helper)),
                        fallback: Box::new(osc()),
                        primary_name: name,
                    },
                    "native+osc52",
                ))
            } else {
                Ok(spawn(osc(), "osc52 (no native helper)"))
            }
        }
    }
}

/// `LOOPRS_CLIPBOARD_MAX_BYTES`, parsed. Unset means **no cap** (R9).
fn parse_cap(raw: Option<&str>) -> anyhow::Result<Option<usize>> {
    let Some(raw) = raw else { return Ok(None) };
    let n: usize = raw.parse().map_err(|_| {
        anyhow::anyhow!("LOOPRS_CLIPBOARD_MAX_BYTES must be a byte count, not {raw:?}")
    })?;
    Ok(Some(n))
}

/// Is the automatic copy-on-select path on?
///
/// `LOOPRS_COPY_ON_SELECT=0` turns the **automatic** path off and nothing else:
/// the selection still exists, the keyboard copy still works, and the toast is
/// still the toast (R17). Anything that is not an explicit `0` leaves it on.
pub fn copy_on_select_enabled(raw: Option<String>) -> bool {
    match raw.map(|v| v.trim().to_ascii_lowercase()) {
        None => true,
        Some(v) => !(v == "0" || v == "off" || v == "no" || v == "false"),
    }
}

/// Are we in a remote session, i.e. is the clipboard the user pastes from on the
/// other side of a wire? (R7.)
fn ssh_session(
    connection: std::result::Result<String, std::env::VarError>,
    client: std::result::Result<String, std::env::VarError>,
    tty: std::result::Result<String, std::env::VarError>,
) -> bool {
    connection.is_ok() || client.is_ok() || tty.is_ok()
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Spawn `argv` with `input` on stdin, returning its output.
async fn spawn_stdin(argv: &[String], input: &[u8]) -> anyhow::Result<std::process::Output> {
    let Some((program, args)) = argv.split_first() else {
        anyhow::bail!("empty command");
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A copy whose task timed out must not leave a `pbcopy` parked on the
        // compositor forever. Without this the child outlives its parent's
        // interest in it.
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // `write_all` on a helper that is not reading is exactly the stall R21
        // bounds; the timeout above owns it, so no extra guard here.
        let _ = stdin.write_all(input).await;
        let _ = stdin.flush().await;
    }
    let out = child.wait_with_output().await?;
    Ok(out)
}

/// Run a reader and hand back its stdout.
async fn run(argv: &[String]) -> anyhow::Result<std::process::Output> {
    let Some((program, args)) = argv.split_first() else {
        anyhow::bail!("empty command");
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    Ok(cmd.output().await?)
}

/// Shorten a helper's stderr for a one-line toast.
fn trim_brief(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let one = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let chars: Vec<char> = one.chars().take(120).collect();
    let mut out: String = chars.into_iter().collect();
    if one.chars().count() > 120 {
        out.push('\u{2026}');
    }
    if out.is_empty() {
        out = "no message".into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A transport that answers whatever the test tells it to, and counts the calls.
    struct Fixed {
        outcome: CopyOutcome,
        calls: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl fmt::Debug for Fixed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Fixed").finish_non_exhaustive()
        }
    }

    impl TransportWriter for Fixed {
        fn write<'a>(&'a self, text: &'a str, _chars: Chars) -> BoxFuture<'a, CopyOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(text.to_string());
            Box::pin(async move { self.outcome.clone() })
        }
    }

    fn fixed(outcome: CopyOutcome) -> (FixedWriter, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Fixed {
                outcome,
                calls: calls.clone(),
                seen: seen.clone(),
            },
            calls,
            seen,
        )
    }

    type FixedWriter = Fixed;

    // ─────────────── the count (R19) ───────────────

    /// Characters, not bytes, not cells — the three differ by up to 9x on the
    /// exact strings `spikes/results/clipboard-cost.log` measured.
    #[test]
    fn the_count_counts_characters_and_nothing_else() {
        assert_eq!(Chars::of("hello").get(), 5);
        assert_eq!(
            Chars::of("\u{6f22}\u{5b57}").get(),
            2,
            "kanji: 6 bytes, 4 cells, 2 chars"
        );
        assert_eq!(
            Chars::of("\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}").get(),
            5
        );
        assert_eq!(
            Chars::of("e\u{301}").get(),
            2,
            "a decomposed e is two characters"
        );
        assert_eq!(Chars::of("").get(), 0);
    }

    #[test]
    fn the_separator_is_every_three_digits_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1284), "1,284");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(thousands(12_345_678), "12,345,678");
    }

    /// The toast prints the count it was handed, in thousands-separated form, and
    /// names the unit in the same breath.
    #[test]
    fn the_ladder_reads_exactly_as_adr_0004_r20_writes_it() {
        let n = Chars(1284);
        let cases: Vec<(CopyOutcome, &str)> = vec![
            (
                CopyOutcome::Verified { chars: n },
                "Copied 1,284 characters \u{b7} clipboard",
            ),
            (
                CopyOutcome::Sent {
                    chars: n,
                    transport: Transport::Osc52,
                    note: None,
                },
                "Copied 1,284 characters \u{b7} OSC 52 (not confirmed)",
            ),
            (
                CopyOutcome::Mismatch { chars: n },
                "Not copied: the clipboard changed under us \u{2014} select again (1,284 characters were written and read back differently)",
            ),
            (
                CopyOutcome::Failed {
                    reason: "nothing was copied".into(),
                },
                "Copy failed: nothing was copied",
            ),
            (
                CopyOutcome::Partial {
                    chars: Chars(400),
                    asked: n,
                    transport: Transport::Osc52,
                    max_bytes: 400,
                },
                "Copied 400 of 1,284 characters \u{b7} OSC 52 (400 bytes), truncated at the 400 byte cap you set",
            ),
        ];
        for (outcome, want) in cases {
            assert_eq!(outcome.toast(), want, "{outcome:?}");
        }
    }

    /// A fallback is *mentioned*, once. The user's question is "is it on my
    /// clipboard?", not "how many transports did you try?".
    #[test]
    fn a_note_is_appended_once_and_a_blank_note_is_not_appended_at_all() {
        let s = CopyOutcome::Sent {
            chars: Chars(12),
            transport: Transport::Osc52,
            note: Some("pbcopy failed: exited 1".into()),
        };
        assert_eq!(
            s.toast(),
            "Copied 12 characters \u{b7} OSC 52 (not confirmed) \u{2014} pbcopy failed: exited 1"
        );
        let blank = CopyOutcome::Sent {
            chars: Chars(12),
            transport: Transport::Osc52,
            note: Some("   ".into()),
        };
        assert_eq!(
            blank.toast(),
            "Copied 12 characters \u{b7} OSC 52 (not confirmed)"
        );
    }

    /// The verb is the payload: `is_copied` is the difference between a `Copied`
    /// toast and a `Copy failed` one, and the mismatch is on the failure side.
    #[test]
    fn only_the_states_that_really_wrote_are_copied() {
        assert!(CopyOutcome::Verified { chars: Chars(1) }.is_copied());
        assert!(
            CopyOutcome::Sent {
                chars: Chars(1),
                transport: Transport::Osc52,
                note: None
            }
            .is_copied()
        );
        assert!(
            CopyOutcome::Partial {
                chars: Chars(1),
                asked: Chars(2),
                transport: Transport::Osc52,
                max_bytes: 1,
            }
            .is_copied()
        );
        assert!(!CopyOutcome::Mismatch { chars: Chars(1) }.is_copied());
        assert!(!CopyOutcome::Failed { reason: "x".into() }.is_copied());
    }

    // ─────────────── the sink (R11) ───────────────

    /// `Noop` does not copy, and says so. A default that reported success would
    /// make every test that never asserted a copy a false positive.
    #[test]
    fn the_noop_sink_says_it_copied_nothing() {
        let r = Noop.copy("anything".into());
        let outcome = r.poll().expect("Noop answers on the spot");
        assert!(outcome.is_failure());
        assert!(
            outcome.toast().contains("LOOPRS_CLIPBOARD=off"),
            "the failure names the switch: {}",
            outcome.toast()
        );
    }

    /// A receipt whose task died is a failure the caller hears, not a `None`
    /// that lasts forever.
    #[test]
    fn a_dead_task_is_a_failure_not_a_hang() {
        let (tx, rx) = oneshot::channel::<CopyOutcome>();
        drop(tx);
        let r = Receipt::pending(rx);
        assert_eq!(
            r.poll(),
            Some(CopyOutcome::Failed {
                reason: "the copy task died without answering".into()
            })
        );
        // and only once: the receipt is spent.
        assert_eq!(r.poll(), None);
    }

    #[tokio::test]
    async fn a_copy_is_answered_by_the_task_that_owns_the_transport() {
        let (w, calls, seen) = fixed(CopyOutcome::Verified { chars: Chars(3) });
        let sink = spawn(w, "test");
        let r = sink.copy("abc".into());
        let outcome = tokio::time::timeout(Duration::from_secs(2), poll_until_some(&r))
            .await
            .expect("answered");
        assert_eq!(outcome, CopyOutcome::Verified { chars: Chars(3) });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*seen.lock().unwrap(), vec!["abc".to_string()]);
    }

    /// **Depth 1, latest wins.** Three releases in a burst write once, and what
    /// they write is the last selection — not three, and not the first.
    ///
    /// The gate is the point: the copies are queued while the transport is busy,
    /// so without the latest-wins slot the queue would be three deep and the
    /// toast for the *first* one would arrive after the user had moved on.
    #[tokio::test]
    async fn a_burst_of_releases_copies_once_and_copies_the_last_one() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let w = GatedWriter {
            gate: gate.clone(),
            seen: seen.clone(),
            calls: calls.clone(),
        };
        let sink = spawn(w, "test");

        let r1 = sink.copy("first".into());
        // Let the task pick up and block on the gate so the next two queue up
        // *behind* a write in progress, which is the case the slot has to hold.
        let r2 = sink.copy("second".into());
        let r3 = sink.copy("third".into());
        gate.add_permits(4);

        for r in [&r1, &r2, &r3] {
            tokio::time::timeout(Duration::from_secs(2), poll_until_some(r))
                .await
                .expect("every receipt is answered, even the superseded ones");
        }
        let seen = seen.lock().unwrap();
        assert!(
            seen.contains(&"third".to_string()),
            "the last selection must be written: {seen:?}"
        );
        assert_eq!(
            seen.len(),
            calls.load(Ordering::SeqCst),
            "and it must be written no more than once per attempt: {seen:?}"
        );
        // The superseded selections may appear at most as the in-flight one that
        // was already running when they arrived; a queue of depth 3 is the bug.
        let distinct: std::collections::HashSet<&String> = seen.iter().collect();
        assert!(
            distinct.len() <= 2,
            "at most the in-flight copy plus the latest: {seen:?}"
        );
        assert!(
            !seen.contains(&"first".to_string()) || !seen.contains(&"second".to_string()),
            "not all three: {seen:?}"
        );
    }

    /// A transport that never answers must not leave the toast blank.
    #[tokio::test]
    async fn a_wedged_transport_reports_a_late_failure_instead_of_silence() {
        let sink = spawn_with(
            SleepWriter {
                how_long: Duration::from_secs(30),
            },
            "test",
            Duration::from_millis(50),
        );
        let r = sink.copy("wedged".into());
        let outcome = tokio::time::timeout(Duration::from_secs(2), poll_until_some(&r))
            .await
            .expect("the deadline answers even when the transport does not");
        assert!(outcome.is_failure());
        assert!(
            outcome.toast().contains("did not answer in 0s"),
            "{}",
            outcome.toast()
        );
    }

    /// The fallback is taken **instead** of as well (R8), and carries the reason
    /// the primary was abandoned.
    #[tokio::test]
    async fn a_failed_native_copy_falls_back_to_osc52_and_says_so_once() {
        let primary = failing("pbcopy exited 1");
        let sink = spawn(
            FallbackWriter {
                primary: Box::new(primary),
                fallback: Box::new(Osc52Writer::with_sink(None, capture)),
                primary_name: "pbcopy",
            },
            "test",
        );
        let r = sink.copy("fallback worked".into());
        let outcome = tokio::time::timeout(Duration::from_secs(2), poll_until_some(&r))
            .await
            .expect("answered");
        assert_eq!(
            outcome,
            CopyOutcome::Sent {
                chars: Chars(15),
                transport: Transport::Osc52,
                note: Some("pbcopy failed: pbcopy exited 1".into()),
            },
            "{outcome:?}"
        );
    }

    /// Both transports down: name both, because "it failed" with two in play is
    /// not something the user can act on.
    #[tokio::test]
    async fn when_both_transports_fail_both_are_named() {
        let sink = spawn(
            FallbackWriter {
                primary: Box::new(failing("no pbcopy")),
                fallback: Box::new(failing("tty closed")),
                primary_name: "native",
            },
            "test",
        );
        let r = sink.copy("nowhere".into());
        let outcome = tokio::time::timeout(Duration::from_secs(2), poll_until_some(&r))
            .await
            .expect("answered");
        assert_eq!(
            outcome.toast(),
            "Copy failed: nothing was copied (native: no pbcopy; OSC 52: tty closed)"
        );
    }

    fn failing(reason: &str) -> FixedWriter {
        Fixed {
            outcome: CopyOutcome::Failed {
                reason: reason.to_string(),
            },
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    // ─────────────── the OSC 52 wire ───────────────

    /// The frame, asserted on the wire: `ESC ] 52 ; c ; <base64> ST`, with ST
    /// (`ESC \`) and not a BEL, because a BEL inside a pasted string would end
    /// the sequence early.
    #[test]
    fn the_osc52_frame_is_the_one_the_spike_measured() {
        let wire = osc52_bytes("hi");
        assert_eq!(
            wire,
            b"\x1b]52;c;aGk=\x1b\\",
            "got {:?}",
            String::from_utf8_lossy(&wire)
        );
        // A payload containing a BEL and a newline must not be able to end the
        // sequence early: both are inside the base64, so neither is on the wire.
        let nasty = osc52_bytes("a\u{7}b\nc\u{1b}[31m");
        let body = &nasty[7..nasty.len() - 2];
        assert!(
            body.iter().all(|b| *b >= 0x20 && *b < 0x7f),
            "base64 body carries no control bytes: {:?}",
            String::from_utf8_lossy(body)
        );
    }

    /// The default is **no cap** (R9): a megabyte of selection goes out whole.
    #[tokio::test]
    async fn nothing_is_truncated_unless_the_user_asked_for_it() {
        let big = "x".repeat(1024 * 1024);
        let sink = spawn(Osc52Writer::with_sink(None, capture), "test");
        let r = sink.copy(big.clone());
        let outcome = tokio::time::timeout(Duration::from_secs(5), poll_until_some(&r))
            .await
            .expect("answered");
        assert_eq!(
            outcome,
            CopyOutcome::Sent {
                chars: Chars(1024 * 1024),
                transport: Transport::Osc52,
                note: None
            }
        );
    }

    /// Over a cap the *user* set: what went is reported, not what was asked.
    /// "Never a truncated copy with a count that describes the untruncated
    /// selection."
    #[tokio::test]
    async fn a_capped_copy_reports_the_count_that_actually_went() {
        let sink = spawn(Osc52Writer::with_sink(Some(3), capture), "test");
        let r = sink.copy("abcdef".into());
        let outcome = tokio::time::timeout(Duration::from_secs(2), poll_until_some(&r))
            .await
            .expect("answered");
        assert_eq!(
            outcome,
            CopyOutcome::Partial {
                chars: Chars(3),
                asked: Chars(6),
                transport: Transport::Osc52,
                max_bytes: 3
            }
        );
        assert_eq!(
            outcome.toast(),
            "Copied 3 of 6 characters \u{b7} OSC 52 (3 bytes), truncated at the 3 byte cap you set"
        );
    }

    /// A cap that would land in the middle of a multi-byte character is snapped
    /// down: a truncated character is corruption, not a shorter copy.
    #[tokio::test]
    async fn a_cap_never_splits_a_character() {
        // "\u{6f22}\u{5b57}" is 6 bytes; a 4-byte cap cannot fit the second character's
        // 3-byte tail, so exactly one character goes.
        let sink = spawn(Osc52Writer::with_sink(Some(4), capture), "test");
        let r = sink.copy("\u{6f22}\u{5b57}".into());
        let outcome = tokio::time::timeout(Duration::from_secs(2), poll_until_some(&r))
            .await
            .expect("answered");
        assert_eq!(
            outcome,
            CopyOutcome::Partial {
                chars: Chars(1),
                asked: Chars(2),
                transport: Transport::Osc52,
                max_bytes: 4
            }
        );
    }

    // ─────────────── the transport decision (R5-R7) ───────────────

    /// A remote session is OSC 52 **only**: a helper on the far side writes the
    /// wrong clipboard, silently.
    #[tokio::test]
    async fn a_remote_session_uses_osc52_even_with_a_helper_present() {
        let helpers = vec![helper("pbcopy")];
        let sink = clipboard_for(None, None, helpers.clone(), true).unwrap();
        assert!(sink.describe().contains("osc52"), "{}", sink.describe());
        assert!(!sink.describe().contains("native"), "{}", sink.describe());
    }

    #[tokio::test]
    async fn a_local_session_with_a_helper_uses_the_helper_with_osc52_behind_it() {
        let sink = clipboard_for(None, None, vec![helper("pbcopy")], false).unwrap();
        assert_eq!(sink.describe(), "native+osc52");
    }

    #[tokio::test]
    async fn a_local_session_with_no_helper_falls_to_osc52_and_says_which() {
        let sink = clipboard_for(None, None, vec![], false).unwrap();
        assert!(
            sink.describe().contains("no native helper"),
            "{}",
            sink.describe()
        );
    }

    /// An explicit choice is honoured strictly, including failing when the thing
    /// chosen does not exist. A silent substitution for a switch the user set on
    /// purpose is how a clipboard setting becomes a mystery.
    #[tokio::test]
    async fn forcing_native_without_a_helper_is_an_error_not_a_substitution() {
        let err = clipboard_for(Some("native"), None, vec![], false).unwrap_err();
        assert!(err.to_string().contains("pbcopy/wl-copy/xclip"), "{err}");
    }

    #[tokio::test]
    async fn off_is_off() {
        let sink = clipboard_for(Some("off"), None, vec![helper("pbcopy")], false).unwrap();
        assert_eq!(sink.describe(), "off");
        // …and it copies nothing, which is what "off" has to mean.
        let r = sink.copy("x".into());
        assert!(r.poll().unwrap().is_failure());
    }

    /// A malformed cap is refused at startup rather than becoming a cap of zero,
    /// which would silently copy nothing on every selection.
    #[tokio::test]
    async fn a_bad_cap_is_refused_rather_than_guessed() {
        assert!(clipboard_for(None, Some("lots"), vec![], false).is_err());
        assert!(clipboard_for(None, Some("0"), vec![], false).is_ok());
    }

    /// `LOOPRS_COPY_ON_SELECT=0` turns the automatic path off; everything else
    /// (including an unrecognised value) leaves it on, because the flag's absence
    /// is the common case and the copy is the feature.
    #[test]
    fn copy_on_select_is_on_unless_told_otherwise() {
        assert!(copy_on_select_enabled(None));
        assert!(copy_on_select_enabled(Some("1".into())));
        assert!(copy_on_select_enabled(Some("yes".into())));
        for off in ["0", "off", "no", "false", " OFF ", "False"] {
            assert!(
                !copy_on_select_enabled(Some(off.into())),
                "{off} should turn it off"
            );
        }
    }

    fn helper(name: &'static str) -> Helper {
        Helper {
            write: vec![name.into()],
            read: vec![format!("{name}-reader")],
            name,
        }
    }

    fn capture(_bytes: &[u8]) {}

    struct SleepWriter {
        how_long: Duration,
    }

    impl fmt::Debug for SleepWriter {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("SleepWriter").finish_non_exhaustive()
        }
    }

    impl TransportWriter for SleepWriter {
        fn write<'a>(&'a self, _t: &'a str, _c: Chars) -> BoxFuture<'a, CopyOutcome> {
            Box::pin(async move {
                tokio::time::sleep(self.how_long).await;
                CopyOutcome::Verified { chars: Chars(0) }
            })
        }
    }

    struct GatedWriter {
        gate: Arc<tokio::sync::Semaphore>,
        seen: Arc<Mutex<Vec<String>>>,
        calls: Arc<AtomicUsize>,
    }

    impl fmt::Debug for GatedWriter {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("GatedWriter").finish_non_exhaustive()
        }
    }

    impl TransportWriter for GatedWriter {
        fn write<'a>(&'a self, text: &'a str, _c: Chars) -> BoxFuture<'a, CopyOutcome> {
            let gate = self.gate.clone();
            let seen = self.seen.clone();
            let calls = self.calls.clone();
            let text = text.to_string();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push(text);
                let _ = gate.acquire().await;
                CopyOutcome::Verified { chars: Chars(3) }
            })
        }
    }

    /// Poll a receipt the way the UI does, but until it answers.
    async fn poll_until_some(r: &Receipt) -> CopyOutcome {
        loop {
            if let Some(o) = r.poll() {
                return o;
            }
            tokio::task::yield_now().await;
        }
    }
}
