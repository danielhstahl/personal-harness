//! The whole-transcript-to-a-file sink (looprs-pdl.13).
//!
//! The escape hatch. A clipboard is the right size for an answer and the wrong
//! size for a session: a transcript that has been running for an hour does not
//! go into a paste buffer usefully, and the thing the user wants to do with it is
//! `grep`, `diff`, or send it to somebody. So `Ctrl-S t` writes the whole thing
//! to a file, which is the same shape of problem as the clipboard wearing a
//! different hat — and therefore gets the same four things the clipboard got:
//!
//! * an **injected sink** (`App::set_transcript_sink`), never a direct
//!   `std::fs::write` from the UI, so "where does my transcript go" has one
//!   answer per run and the App never invents a path of its own;
//! * **a receipt that is polled, never awaited** — the same [`Receipt`] shape as
//!   [`crate::services::clipboard`], because a file write is a syscall that can
//!   block on a full disk, an unresponsive NFS mount, or a laptop asleep over a
//!   Thunderbolt volume, and none of those are allowed to stop the keyboard;
//! * **a default that is a `Noop`** rather than a real writer, so the test suite
//!   stays off the filesystem by construction rather than by nobody remembering
//!   to set an environment variable — the exact rule
//!   [`clipboard_from_env`](crate::services::clipboard::clipboard_from_env)
//!   states for the transport;
//! * **a toast that reports the count and the path**, because "it wrote a file"
//!   without a path is a riddle, and a count that describes the string we asked
//!   for rather than the bytes that landed is the confident lie R19 exists to
//!   prevent.
//!
//! # What goes in the file
//!
//! [`Transcript::plain_text`](crate::state::transcript::Transcript::plain_text):
//! the entries' content, no frame chrome, no styles, no band markers. The file's
//! use is grep and paste, and decoration ruins both. The *live* tail is not in
//! there, for the same reason a drag selection cannot reach it: it has no final
//! form. The settled transcript is the thing worth keeping.
//!
//! # Bounded, so blocking is not on the table anyway
//!
//! The payload is capped by the view's own buffer
//! ([`DEFAULT_VIEW_BUFFER`](crate::session::view::DEFAULT_VIEW_BUFFER), 256 KiB
//! by default), so the write is small by construction and the timeout is
//! belt-and-braces. That is *not* why the write happens off the UI task: the UI
//! task's rule is "never block on I/O", and a rule that depends on the payload
//! being small is a rule that breaks the first time the cap is raised.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;

use crate::services::clipboard::{Chars, thousands};

/// How long to wait for a write before declaring it dead.
///
/// A 256 KiB write to a local volume completes in single-digit milliseconds; two
/// seconds is two orders of magnitude of headroom, which is what makes a timeout
/// firing at all a signal about the *volume* rather than about us.
pub const DUMP_TIMEOUT: Duration = Duration::from_secs(2);

/// What came back from a dump attempt.
///
/// Two states, because a file write has nothing subtler to report than "it is
/// there" or "it is not". There is deliberately no *partial* state: a
/// half-written transcript file is a corrupt file, not a shorter one, and a
/// `Copied 200 of 38,000`-shaped answer (cf. [`CopyOutcome::Partial`]) would be
/// a lie about a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DumpOutcome {
    /// The whole transcript is at `path`.
    Written { path: PathBuf, chars: Chars },
    /// Nothing landed, with the reason worth showing.
    Failed { reason: String },
}

impl DumpOutcome {
    /// Is this a "nothing landed" answer? Decides the toast's tone.
    pub fn is_failure(&self) -> bool {
        !matches!(self, DumpOutcome::Written { .. })
    }

    /// The toast text. The verb is `Wrote`, not `Copied`: the clipboard was not
    /// touched, and a user who reads `Copied 38,000 characters` and then pastes
    /// gets whatever they had before. ADR-0004 R20's rule that the verb carries
    /// the meaning applies as much here as to the clipboard ladder.
    pub fn toast(&self) -> String {
        match self {
            DumpOutcome::Written { path, chars } => format!(
                "Wrote {} characters of transcript to {}",
                thousands(chars.get()),
                display_path(path)
            ),
            // Same opening as every refusal the dump path makes: the first two words of
            // the toast are the answer to "did I get my transcript out?", and
            // the reason follows.
            DumpOutcome::Failed { reason } => format!("Nothing written: {reason}"),
        }
    }
}

/// The handle a dump comes back with. Polled from the tick, exactly like
/// [`crate::services::clipboard::Receipt`], for exactly the same reason.
#[derive(Debug)]
pub struct DumpReceipt {
    rx: Mutex<Option<oneshot::Receiver<DumpOutcome>>>,
}

impl DumpReceipt {
    fn pending(rx: oneshot::Receiver<DumpOutcome>) -> Self {
        Self {
            rx: Mutex::new(Some(rx)),
        }
    }

    /// A receipt answered on the spot, for sinks that decide immediately
    /// ([`Noop`], [`crate::testing::RecordingTranscriptSink`]).
    pub fn ready(outcome: DumpOutcome) -> Self {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(outcome);
        Self::pending(rx)
    }

    /// A receipt that will not answer of its own accord — the seam
    /// [`crate::testing::StallTranscriptSink`] uses so "the disk never answered"
    /// is a test rather than a story.
    #[allow(dead_code)] // test seam: only `crate::testing` builds one
    pub fn stalled(rx: oneshot::Receiver<DumpOutcome>) -> Self {
        Self::pending(rx)
    }

    /// Take the result if it has arrived. `None` = still in flight.
    pub fn poll(&self) -> Option<DumpOutcome> {
        let mut guard = self.rx.lock().ok()?;
        let rx = guard.as_mut()?;
        match rx.try_recv() {
            Ok(outcome) => {
                *guard = None;
                Some(outcome)
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                *guard = None;
                Some(DumpOutcome::Failed {
                    reason: "the write task died without answering".into(),
                })
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
        }
    }

    /// Stop waiting; a late answer goes nowhere rather than being held.
    pub fn abandon(&self) {
        if let Ok(mut guard) = self.rx.lock() {
            *guard = None;
        }
    }
}

/// A sink for "write this transcript somewhere, and tell me later whether you
/// did".
pub trait TranscriptSink: Send + Sync + fmt::Debug + 'static {
    /// Hand the text over and walk away. Never blocks.
    ///
    /// `mode` names the mode the dump was asked in, and is used for the file name
    /// and nothing else: which transcript got written is decided by the caller,
    /// which is the only party that knows what it was looking at.
    fn dump(&self, mode: &'static str, text: String) -> DumpReceipt;

    /// What this sink *is*, for the startup log and for failure text.
    fn describe(&self) -> &'static str;
}

/// The sink that writes nothing. The default, so ~600 tests stay off the disk.
///
/// It answers with a failure rather than never answering, because the caller
/// still has a toast to get right: a dump that is off did **not** write a file.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noop;

impl TranscriptSink for Noop {
    fn dump(&self, _mode: &'static str, _text: String) -> DumpReceipt {
        DumpReceipt::ready(DumpOutcome::Failed {
            reason: "transcript dump is off (LOOPRS_TRANSCRIPT_DUMP=off)".into(),
        })
    }

    fn describe(&self) -> &'static str {
        "off"
    }
}

/// The production sink: one timestamped text file in a directory.
#[derive(Clone, Debug)]
pub struct FileSink {
    dir: PathBuf,
}

impl FileSink {
    /// A sink writing into `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    /// The file this dump goes to: `looprs-<mode>-<epoch-micros>.txt`.
    ///
    /// Microseconds, not seconds, because two dumps in one second are possible
    /// (a double-tap on the chord) and two files of the same name is one file
    /// silently eaten. Nothing about the name is parsed by anything: it only has
    /// to be sortable and unique, and this is both.
    pub fn path_for(dir: &Path, mode: &str, when: SystemTime) -> PathBuf {
        let micros = when
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_micros();
        PathBuf::from(dir).join(format!("looprs-{mode}-{micros}.txt"))
    }
}

impl TranscriptSink for FileSink {
    fn dump(&self, mode: &'static str, text: String) -> DumpReceipt {
        let path = FileSink::path_for(&self.dir, mode, SystemTime::now());
        let (tx, rx) = oneshot::channel();
        // `spawn_blocking`, not `tokio::spawn`: this is a synchronous syscall
        // and the async runtime's worker threads are the wrong place for one.
        // On a hung volume a `tokio::spawn`ed write stalls every other timer in
        // the process; a blocking-pool thread stalls itself and nothing else.
        tokio::task::spawn_blocking(move || {
            let outcome = write_and_report(&path, &text);
            // A dead receiver means the App gave up on this dump at
            // `DUMP_TIMEOUT` and already painted the late failure; the file, if
            // it got written, is still there and the late answer is simply
            // nobody's business any more.
            let _ = tx.send(outcome);
        });
        DumpReceipt::pending(rx)
    }

    fn describe(&self) -> &'static str {
        // The trait wants `&'static str`, so the directory is not in here; the
        // startup log prints `dir()` alongside it, and the toast prints the
        // actual path of the file that got written, which is the one path the
        // user needs.
        "file"
    }
}

/// One write, start to finish, with the outcome words chosen here.
fn write_and_report(path: &Path, text: &str) -> DumpOutcome {
    let chars = Chars::of(text);
    match std::fs::write(path, text) {
        Ok(()) => DumpOutcome::Written {
            path: path.to_path_buf(),
            chars,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DumpOutcome::Failed {
            reason: format!(
                "cannot write to {} — the directory does not exist ({e})",
                path.display()
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DumpOutcome::Failed {
            reason: format!("no permission to write {} ({e})", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::StorageFull => DumpOutcome::Failed {
            reason: format!("the volume holding {} is full ({e})", path.display()),
        },
        Err(e) => DumpOutcome::Failed {
            reason: format!("cannot write {} ({e})", path.display()),
        },
    }
}

/// The sink the environment asks for.
///
/// * `LOOPRS_TRANSCRIPT_DUMP=off|0|no` → [`Noop`].
/// * `LOOPRS_TRANSCRIPT_DIR=<path>` → [`FileSink`] there.
/// * Neither set → [`FileSink`] in the system temp directory, which is the
///   hatch's whole point: it works with no configuration at all, because a user
///   who has just discovered the chord should get their transcript, not a
///   tutorial.
///
/// A configured directory that cannot be created degrades to [`Noop`] with the
/// reason logged rather than failing startup: the beads loop is the feature and
/// it works without a transcript dump.
pub fn transcript_sink_from_env() -> std::sync::Arc<dyn TranscriptSink> {
    match transcript_sink_for(
        env_value("LOOPRS_TRANSCRIPT_DUMP").as_deref(),
        env_value("LOOPRS_TRANSCRIPT_DIR").as_deref(),
    ) {
        Ok(sink) => {
            // Both the one-word answer and the whole `Debug`, because the
            // directory is the fact an operator wants in the log and `describe`
            // is the one-word answer the toast/`Failed` text is built from.
            tracing::info!("transcript dump: {} ({sink:?})", sink.describe());
            sink
        }
        Err(e) => {
            tracing::error!("transcript dump disabled: {e}");
            std::sync::Arc::new(Noop)
        }
    }
}

/// The dump's home when nothing was configured: a cache directory under the
/// user's own, not the system temp dir.
///
/// Not a stylistic choice. The toast has to *name the file it wrote* on a row the
/// user can read, and macOS's `$TMPDIR` is
/// `/var/folders/17/zpwmvyzd1bdc4lkjs6x0_r6c0000gn/T/` — 49 columns of
/// directory before the file name starts. Measured through tmux at 100 columns:
/// the toast came out cut mid-path and named nothing. A cache path shortens to
/// `~/.cache/looprs/…` and the name at the end survives the row.
fn default_dump_dir() -> PathBuf {
    if let Some(xdg) = env_value("XDG_CACHE_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg.trim()).join("looprs");
    }
    if let Some(home) = env_value("HOME")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home.trim()).join(".cache").join("looprs");
    }
    std::env::temp_dir()
}

/// The path the way the toast says it: a leading `$HOME` folded to `~`. The
/// absolute path is never lost — it is what gets written, and the `info` log at
/// startup carries the directory — this is only about fitting a row.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = env_value("HOME") {
        let h = Path::new(home.trim());
        if !home.trim().is_empty()
            && let Ok(rest) = path.strip_prefix(h)
        {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// The decision as a pure function, so the ladder is a table in tests rather
/// than a mutation of process-global state every other test shares.
fn transcript_sink_for(
    switch: Option<&str>,
    dir: Option<&str>,
) -> anyhow::Result<std::sync::Arc<dyn TranscriptSink>> {
    if matches!(
        switch.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("off") | Some("0") | Some("no") | Some("false")
    ) {
        return Ok(std::sync::Arc::new(Noop));
    }
    let chosen = match dir {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => default_dump_dir(),
    };
    // Checked here, at startup, rather than discovered at the one moment the user
    // is relying on it: a dump that fails because of a missing directory is a
    // failure the user cannot act on mid-keystroke, and one the operator can act
    // on before anyone runs anything.
    if let Err(e) = std::fs::create_dir_all(&chosen) {
        anyhow::bail!(
            "LOOPRS_TRANSCRIPT_DIR={} cannot be created ({e})",
            chosen.display()
        );
    }
    Ok(std::sync::Arc::new(FileSink::new(chosen)))
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    /// The `~` fold is not decoration. The toast has to name the file inside a
    /// row the user can read, and the absolute macOS path is long enough to eat
    /// the row before the name starts — measured through tmux at 100 columns,
    /// where the toast came out cut mid-directory. The fold is what keeps the
    /// name on the screen.
    #[test]
    fn the_path_in_the_toast_folds_home_so_the_name_survives_the_row() {
        let home = std::env::var("HOME").expect("HOME in the test env");
        let p = std::path::Path::new(&home).join(".cache/looprs/looprs-pi-0211.txt");
        let shown = display_path(&p);
        assert!(shown.starts_with("~/"), "{shown}");
        assert!(shown.ends_with("looprs-pi-0211.txt"), "{shown}");
        // Room left over for the "Wrote N characters of transcript to " prefix
        // in an 80-column terminal.
        assert!(
            shown.len() + 37 <= 80,
            "the toast would not fit an 80-column row: {shown} ({} cols of path)",
            shown.len()
        );
    }

    /// A path outside the home is shown as itself; the fold never *invents* a
    /// `~` for something that is not under it.
    #[test]
    fn a_path_outside_home_is_not_folded() {
        let shown = display_path(std::path::Path::new("/elsewhere/deep/looprs-pi.txt"));
        assert_eq!(shown, "/elsewhere/deep/looprs-pi.txt");
    }

    /// The default dump directory is the user's cache, and short — which is the
    /// whole reason it is not `temp_dir()`.
    #[test]
    fn the_default_dump_directory_is_a_readable_cache_path() {
        let d = default_dump_dir();
        let shown = display_path(&d);
        assert!(
            shown.ends_with("looprs"),
            "the default ends in the app's own directory: {shown}"
        );
        assert!(
            shown.len() < 40,
            "default dir is long enough to clip: {shown}"
        );
    }

    use super::*;

    #[test]
    fn the_verb_is_wrote_because_the_clipboard_was_not_touched() {
        let t = DumpOutcome::Written {
            path: PathBuf::from("/tmp/looprs-pi-123.txt"),
            chars: Chars::of("1234567890"),
        }
        .toast();
        assert_eq!(
            t,
            "Wrote 10 characters of transcript to /tmp/looprs-pi-123.txt"
        );
        assert!(!t.starts_with("Copied"), "{t}");
    }

    /// There is no "partial" answer for a document. A half-written file is a
    /// corrupt file, so the only two answers are where it is and that it is not.
    #[test]
    fn a_dump_lands_or_it_does_not_and_there_is_no_middle() {
        assert!(
            !DumpOutcome::Written {
                path: PathBuf::from("/tmp/x"),
                chars: Chars::of("x")
            }
            .is_failure()
        );
        assert!(DumpOutcome::Failed { reason: "x".into() }.is_failure());
        assert_eq!(
            DumpOutcome::Failed {
                reason: "the volume is full".into()
            }
            .toast(),
            "Nothing written: the volume is full"
        );
    }

    /// Two dumps inside one clock second still get two files: the name carries
    /// microseconds precisely so a second tap cannot eat the first dump.
    #[test]
    fn two_dumps_in_one_second_do_not_collide() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let a = FileSink::path_for(Path::new("/tmp"), "pi", base);
        let b = FileSink::path_for(Path::new("/tmp"), "pi", base + Duration::from_micros(1));
        assert_ne!(a, b, "{a:?} vs {b:?}");
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("looprs-pi-")
        );
        // And a same-microsecond pair in different modes differs too.
        assert_ne!(
            FileSink::path_for(Path::new("/tmp"), "pi", base),
            FileSink::path_for(Path::new("/tmp"), "bash", base)
        );
    }

    /// A real write, to a real temp dir, read back: the file holds exactly the
    /// bytes the sink was handed and nothing else — no header, no frame chrome.
    #[tokio::test]
    async fn a_file_sink_writes_exactly_what_it_was_handed() {
        let dir = std::env::temp_dir().join(format!("looprs-dump-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = FileSink::new(&dir);
        let body = "answer line\n\nbash line\n";
        let r = sink.dump("test", body.into());
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(o) = r.poll() {
                    return o;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the write task answered");
        let DumpOutcome::Written { path, chars } = outcome else {
            panic!("expected a write, got {outcome:?}");
        };
        assert_eq!(chars.get(), body.chars().count());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The write happens on a blocking-pool task, so the receipt is answered by
    /// that task and not on the caller's stack: poll returns `None` first for a
    /// sink whose task has not run. Verified as "the receipt does answer", which
    /// is the contract the App depends on.
    #[tokio::test]
    async fn a_receipt_from_a_real_sink_is_always_answered() {
        let dir = std::env::temp_dir().join(format!("looprs-dump-async-{}", std::process::id()));
        let sink = transcript_sink_for(None, Some(dir.to_str().unwrap())).unwrap();
        let r = sink.dump("test", "x".repeat(1000));
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(o) = r.poll() {
                    return o;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the write task answered");
        assert!(!outcome.is_failure(), "{}", outcome.toast());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn off_is_off() {
        for switch in ["off", "0", "no", "FALSE", " off "] {
            let sink = transcript_sink_for(Some(switch), None).unwrap();
            assert_eq!(sink.describe(), "off", "{switch}");
            assert!(sink.dump("test", "x".into()).poll().unwrap().is_failure());
        }
    }

    /// An unusable directory is refused at the decision point rather than
    /// discovered the first time a user asks for their transcript.
    #[test]
    fn a_directory_that_cannot_be_made_is_refused() {
        let bogus = "/proc/definitely/not/a/directory/looprs";
        let err = transcript_sink_for(None, Some(bogus)).unwrap_err();
        assert!(err.to_string().contains("cannot be created"), "{err}");
    }

    #[test]
    fn a_blank_dir_is_not_treated_as_a_setting() {
        // Would otherwise write into `""`, i.e. the current directory.
        let sink = transcript_sink_for(None, Some("   ")).unwrap();
        assert_eq!(sink.describe(), "file");
        // and it is the temp dir, not the cwd
        let p = FileSink::path_for(&std::env::temp_dir(), "t", SystemTime::now());
        assert!(p.starts_with(std::env::temp_dir()), "{p:?}");
    }
}
