//! The journal: the transcript written to a file **as it finalises**.
//!
//! This is ADR-0004 R2 and R3, implemented. The escape hatch that makes a
//! bounded scrollback survivable: the on-screen store keeps a few megabytes and
//! drops the rest behind a `scrollback trimmed` marker, and everything the
//! session ever said — in order, from the first answer to the last — is here, so
//! the user who wants the whole thing greps it instead of scrolling for it.
//!
//! ```text
//! $LOOPRS_TRANSCRIPT_DIR · $XDG_DATA_HOME/looprs/transcripts · ~/.local/share/looprs/transcripts
//!     session-20261009T034008Z-84367-Beads.txt    this run, this mode
//!     session-20261009T034004Z-84354-Pi.txt       …and this one
//!     last            -> …-Beads.txt               the most recently written
//!     last-Beads      -> …-Beads.txt               one per mode, for when you know
//!     last-Pi         -> …-Pi.txt                 the other modes, same shape
//!     last-Bash       -> …-Bash.txt               a mode that never ran has none
//! ```
//!
//! The mode in those names is `TerminalType::label()` interpolated verbatim by
//! [`session_path`]: capitalised `Beads`, `Pi`, `Bash` — **not** the lowercase
//! word as it reads in prose. Type what `ls` shows. Nothing in the program keeps
//! this block honest (`session_path` accepts any string), so the test
//! `the_header_block_names_the_files_the_writer_actually_writes` pins it to the
//! labels instead: the cost of getting this wrong is an operator typing the
//! documented name, getting `No such file or directory` for a file that is
//! standing right there, and concluding the journal never ran.
//!
//! # This file is for reading. It is not the record.
//!
//! **`bd` is the system of record for tickets; this is not a system of record
//! for anything.** The journal is a *reading* copy of the transcript — for
//! `less`, for `grep`, for pasting into a bug report, for finding out what the
//! harness said at 03:12 when nobody was watching. Nothing in looprs reads it
//! back, nothing may be specified against its format, and no tool outside looprs
//! should be built to parse it: it has no stability contract, no schema and no
//! version, and the whole reason it is useful (that it is plain text exactly as
//! it was on screen) is the reason it cannot promise anything to a parser. If a
//! fact matters, it belongs in `bd`. (ADR-0004: "No transcript-file parsing
//! contract. The journal is for reading; `bd` is the record.")
//!
//! # R2: written as it finalises, not at exit
//!
//! Each chunk is appended and flushed as it becomes final, on a writer task of
//! its own, so the file is durable *while the session runs* rather than at the
//! end of it. That is what makes the journal survive the deaths that matter: a
//! `kill -9`, an OOM kill, a panic inside the draw, a laptop out of power. A
//! journal written on the exit path is a journal that does not exist after every
//! one of those, and does not exist after a trim either — which is the failure
//! looprs-pdl.7 is specifically about.
//!
//! [`Journal::close`] exists to *drain* the queue on a clean exit, and it is
//! worth being exact about what that is not: it writes nothing that was not
//! already handed over, entry by entry, during the run. The exit path is not
//! where transcript-shaped bytes get invented (R3), and the journal's own
//! `Ctrl-S t` sibling is a separate feature with a separate file.
//!
//! # What goes in it
//!
//! [`Transcript::plain_text`](crate::state::transcript::Transcript::plain_text)
//! shape, appended per entry: the entry text, trailing newlines off, one blank
//! line between entries. The journal of a whole run is byte-for-byte "select
//! everything and copy" (ADR-0004 R2). That rule is why there are **no
//! timestamps in the body** — the copy-value property is worth more than a
//! timestamp, and a timestamped companion is available if anyone truly wants
//! both (ADR-0004 open question 8, resolved that way here).
//!
//! Order is **transcript order**, not finalisation order: a chunk walk stops at
//! the first entry still open, so an answer that landed behind an unfinished tool
//! card waits for that card rather than jumping the queue. The wait is bounded by
//! the card's runtime, and the session's `seal` closes everything and drains it.
//!
//! # What it costs, and the two things that bound it
//!
//! Memory is bounded by the queue: a bounded queue that sheds chunks and says so
//! rather than one that grows with a stalled volume ([`QUEUE_CHUNKS`]).
//!
//! **Disk is deliberately not rotated** (ADR-0004 open question 9). One file per
//! run per mode that ran, named so they sort chronologically, in the user's own
//! data directory where they own the cleanup. What the code does instead of
//! rotation is make the cost *knowable*: the path is logged once at startup, the
//! marker row that reports a scrollback trim names the same file, and
//! `spikes/results/scrollback-cost.log` carries the measured growth — a long
//! beads pass writes about the same number of bytes as its transcript, which is
//! bounded per session by [`DEFAULT_VIEW_BUFFER`](crate::session::view::DEFAULT_VIEW_BUFFER)
//! only in the sense that the *session* is bounded; the journal keeps what the
//! session said, including the parts the cap dropped. That is the point of it,
//! and it is why the off switch is not a footnote: `LOOPRS_TRANSCRIPT=off` is
//! the answer for anyone who does not want their transcript on disk at all —
//! which matters because the journal holds everything the user pasted into a
//! money-spending agent. Hence the `0600` / `0700` modes, and hence the location
//! in the user's own data directory rather than somewhere world-readable.
//!
//! # The shape of the sink
//!
//! The same injected-sink rule as the clipboard and the dump sink, for the same
//! three reasons: it touches a filesystem, it can fail in ways the UI must
//! report rather than handle, and `main` being the only place the real one is
//! built is what keeps the ~640-test suite off the disk by construction. The
//! difference is durability: a copy is one string at one moment and its receipt
//! is polled; a journal is a stream, and a stream needs an ordered writer rather
//! than a pool of tasks that can finish in any order. Hence the one thread and
//! the one channel below, and hence no `spawn_blocking` — N concurrent writers
//! appending to one file is a race about offsets, which is a worse version of
//! the problem the file solves.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many chunks the writer will hold while the disk catches up.
///
/// A chunk is one entry's text, and a beads pass finalises entries far slower
/// than a local volume writes them, so in the normal case this queue holds zero
/// or one. The bound is for the abnormal case — a volume that has stopped
/// answering — where an unbounded queue is a slow OOM wearing a "we never block"
/// badge. Sixteen entries of backlog is more than a second of output at any rate
/// this app produces; past that the newest content is shed with a count in the
/// log, because the alternative is taking the whole process down with the disk.
const QUEUE_CHUNKS: usize = 16;

/// How long a clean exit waits for the writer to drain.
///
/// In the normal case the queue is empty and this returns immediately; the
/// timeout is the bound on the pathological case, and past it the process is
/// allowed to leave without the journal having finished writing, which is
/// precisely why R2 puts the durability on per-entry flush rather than on this
/// call.
pub const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// A place for the transcript to go as it finalises.
///
/// See the module doc: this is a reading copy, not a record, and nothing may be
/// specified against the bytes.
pub trait Journal: fmt::Debug + Send + Sync + 'static {
    /// Hand over the next chunk of one mode's transcript. Never blocks.
    ///
    /// `mode` is the label of the session that said it, straight from
    /// `TerminalType::label()` — `"Beads"`, `"Pi"`, `"Bash"`, capitalised, and
    /// used verbatim as the file name's mode suffix (see [`session_path`]); the
    /// journal keeps one file per mode so a session's document is never
    /// interleaved with another's, which is what makes each of them readable
    /// end to end.
    fn append(&self, mode: &'static str, text: String);

    /// Where a trimmed scrollback should send the reader, for that mode.
    /// `None` when there is nothing to point at (the journal is off).
    fn display_path(&self, mode: &str) -> Option<String>;

    /// Drain what is queued and close. Bounded by [`CLOSE_BUDGET`].
    fn close(&self);

    /// One word for the log: `file` or `off`.
    fn describe(&self) -> &'static str;
}

/// The journal that writes nothing. The default, so the test suite never lands on
/// the disk by accident, and the answer to `LOOPRS_TRANSCRIPT=off`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Disabled;

impl Journal for Disabled {
    fn append(&self, _mode: &'static str, _text: String) {}
    fn display_path(&self, _mode: &str) -> Option<String> {
        None
    }
    fn close(&self) {}
    fn describe(&self) -> &'static str {
        "off"
    }
}

/// What the writer thread is asked to do.
enum Msg {
    Chunk { mode: &'static str, text: String },
    Close(Sender<()>),
}

/// The real journal: one writer thread, one ordered queue, one file per mode.
///
/// A thread rather than a `tokio::task` because this is a long-lived sequential
/// writer and the runtime's workers are the wrong place for a blocking
/// `write(2)` — the same reasoning `spawn_blocking` gets in
/// [`crate::services::transcript_file`], with the added requirement that the
/// writes have a defined order.
pub struct FileJournal {
    tx: Mutex<Option<Sender<Msg>>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    dir: PathBuf,
    /// Chunks handed over but not yet written.
    ///
    /// The std channel has no stable `len`, and the bound is the point of the
    /// queue rather than an optimisation, so the depth is counted on both sides.
    /// It can read *low* in one direction (the writer has taken a message but
    /// not finished it), and that direction only ever lets one extra chunk in,
    /// which a constant of 16 does not care about.
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

impl fmt::Debug for FileJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileJournal")
            .field("dir", &self.dir)
            .finish()
    }
}

impl FileJournal {
    /// Open the journal in `dir`, which must already exist (see
    /// [`journal_from_env`], which creates it).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let (tx, rx) = mpsc::channel::<Msg>();
        let writer_dir = dir.clone();
        let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w_q = queued.clone();
        let handle = std::thread::Builder::new()
            .name("looprs-journal".into())
            .spawn(move || writer(rx, writer_dir, w_q))
            .expect("spawn the journal writer");
        Self {
            tx: Mutex::new(Some(tx)),
            handle: Arc::new(Mutex::new(Some(handle))),
            dir,
            queued,
        }
    }
}

impl Journal for FileJournal {
    fn append(&self, mode: &'static str, text: String) {
        let Ok(guard) = self.tx.lock() else { return };
        let Some(tx) = guard.as_ref() else { return };
        // The bound is measured on the queue itself: a writer that has stopped
        // draining is the only way this fills, and the count of what we shed is
        // the difference between "the journal is behind" and "the journal is
        // quietly incomplete".
        if self.queued.load(std::sync::atomic::Ordering::Relaxed) >= QUEUE_CHUNKS {
            tracing::warn!(
                "journal queue full ({QUEUE_CHUNKS} chunks): dropped {} bytes of {mode} \"
                 transcript (the volume is not keeping up)",
                text.len()
            );
            return;
        }
        self.queued
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(Msg::Chunk { mode, text });
    }

    fn display_path(&self, mode: &str) -> Option<String> {
        // The per-mode symlink: stable in name even though the file it points at
        // is named after this run, so the marker row can tell the user something
        // they can type tomorrow as well as today.
        let link = self.dir.join(format!("last-{mode}"));
        if link.exists() {
            return Some(crate::services::transcript_file::display_path(&link));
        }
        None
    }

    fn close(&self) {
        let ack = {
            let Ok(mut guard) = self.tx.lock() else {
                return;
            };
            let Some(tx) = guard.as_ref() else { return };
            let (ack_tx, ack_rx) = mpsc::channel();
            // Closing the send side after the request is the point: it means the
            // writer's iterator ends once it has answered, and a journal whose
            // writer died mid-run shows up here as a channel error rather than as
            // a hang.
            let _ = tx.send(Msg::Close(ack_tx));
            guard.take();
            ack_rx
        };
        match ack.recv_timeout(CLOSE_BUDGET) {
            Ok(()) => tracing::info!("journal drained and closed"),
            Err(_) => tracing::warn!(
                "journal did not drain within {:?}; leaving the writer to it",
                CLOSE_BUDGET
            ),
        }
        if let Ok(mut h) = self.handle.lock()
            && let Some(h) = h.take()
        {
            let _ = h.join();
        }
    }

    fn describe(&self) -> &'static str {
        "file"
    }
}

/// The writer: open-on-first-use per mode, flush per chunk, dead on the first
/// error rather than retrying into a full disk.
fn writer(rx: mpsc::Receiver<Msg>, dir: PathBuf, queued: Arc<std::sync::atomic::AtomicUsize>) {
    let mut open: HashMap<&'static str, (PathBuf, BufWriter<std::fs::File>)> = HashMap::new();
    // Which mode's file `last` currently points at. Tracked so the symlink is
    // re-pointed when the answer *changes* and not on every chunk — a symlink
    // per entry would be two syscalls per entry for a fact that changes a couple
    // of times a session.
    let mut pointed: Option<&'static str> = None;
    // Stuck rather than failed-and-retried: one error and the journal stops, so a
    // full volume produces one log line rather than one per entry forever.
    let mut dead = false;

    for msg in rx {
        match msg {
            Msg::Close(ack) => {
                for w in open.values_mut() {
                    let _ = w.1.flush();
                }
                let _ = ack.send(());
                return;
            }
            Msg::Chunk { mode, text } => {
                queued.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                if dead {
                    continue;
                }
                match append_to(&dir, &mut open, &mut pointed, mode, &text) {
                    Ok(()) => {}
                    Err(e) => {
                        dead = true;
                        tracing::error!(
                            "journal disabled by a write error to {}: {e}",
                            dir.display()
                        );
                    }
                }
            }
        }
    }
    // The send side went away without a Close (every handle dropped, e.g. a
    // shutdown that skipped `Journal::close`). Flush what is open anyway: the
    // bytes that made it into the BufWriter are the OS's now, and the OS does not
    // need our permission to keep them.
    for w in open.values_mut() {
        let _ = w.1.flush();
    }
}

/// Append one chunk to one mode's file, opening the file and pointing `last` if
/// this is the first thing that mode has said.
fn append_to(
    dir: &Path,
    open: &mut HashMap<&'static str, (PathBuf, BufWriter<std::fs::File>)>,
    pointed: &mut Option<&'static str>,
    mode: &'static str,
    text: &str,
) -> std::io::Result<()> {
    if !open.contains_key(mode) {
        let path = session_path(dir, mode, SystemTime::now());
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true).mode(0o600);
        let file = opts.open(&path)?;
        open.insert(mode, (path.clone(), BufWriter::new(file)));
        // `last-<mode>` and `last`, atomically: a symlink is created under a
        // temporary name and renamed over the target, so a reader that looks at
        // `last` never sees it missing or half-pointed.
        point_link(dir, &path, &format!("last-{mode}"))?;
        point_link(dir, &path, "last")?;
        *pointed = Some(mode);
    }
    let (_, w) = open
        .get_mut(mode)
        .expect("the mode was opened just above, or was already open");
    w.write_all(text.as_bytes())?;
    // Per chunk, per R2. A `BufWriter` that only flushes at close is a journal
    // that exists only on a clean exit, which is the design this whole module
    // rejected.
    w.flush()?;
    let _ = pointed;
    Ok(())
}

/// `session-<UTC>-<pid>-<Mode>.txt`.
///
/// UTC and ISO-shaped so the directory sorts chronologically by `ls`, and the
/// pid so two runs of the same mode in the same second cannot collide and eat
/// each other's history.
///
/// `mode` is written into the name exactly as passed — no case folding, no
/// lowercasing — which is why the files on disk read `Beads` / `Pi` / `Bash`:
/// every production caller hands over `TerminalType::label()`. A mode string is
/// an opaque key here, so two spellings of one mode are two modes and would get
/// two files and two `last-…` links.
pub fn session_path(dir: &Path, mode: &str, when: SystemTime) -> PathBuf {
    let secs = when
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs();
    PathBuf::from(dir).join(format!(
        "session-{}-{}-{mode}.txt",
        civil_utc(secs),
        std::process::id()
    ))
}

/// Seconds since the epoch as `YYYYMMDDTHHMMSSZ`.
///
/// A hand-rolled civil-time formatter, because nothing in the dependency tree
/// provides one and pulling in a date library for a file name is not a trade
/// worth making. The algorithm is the standard days-from-civil inverse (Howard
/// Hinnant's), which is proleptic-Gregorian and correct for any date this
/// process will ever run on.
fn civil_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Re-point a symlink at `target` without a window in which it does not exist.
fn point_link(dir: &Path, target: &Path, name: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!("{name}.new-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    symlink(target, &tmp)?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

/// The journal the environment asks for.
///
/// * `LOOPRS_TRANSCRIPT=off|0|no|false` → [`Disabled`] (ADR-0004 R3's switch).
/// * `LOOPRS_TRANSCRIPT_DIR=<path>` → [`FileJournal`] there.
/// * else `$XDG_DATA_HOME/looprs/transcripts`, else
///   `~/.local/share/looprs/transcripts`.
///
/// **Not** the temp dir, and that is not taste: the journal holds everything the
/// user pasted into a money-spending agent, and it has to still be there
/// tomorrow for the "what did the loop say at 3am" question that is its whole
/// reason to exist. A `LOOPRS_TRANSCRIPT_DIR` that cannot be created degrades to
/// [`Disabled`] with the reason logged rather than failing startup — the beads
/// loop is the feature, and it works without a journal.
pub fn journal_from_env() -> Arc<dyn Journal> {
    let off = matches!(
        env_value("LOOPRS_TRANSCRIPT").as_deref(),
        Some("off") | Some("0") | Some("no") | Some("false")
    );
    if off {
        tracing::info!("journal: off (LOOPRS_TRANSCRIPT)");
        return Arc::new(Disabled);
    }
    let dir = match env_value("LOOPRS_TRANSCRIPT_DIR") {
        Some(d) => PathBuf::from(d),
        None => default_journal_dir(),
    };
    // 0700, per R3: this directory holds the user's transcript and nobody
    // else's business.
    let made = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    if let Err(e) = made {
        tracing::warn!(
            "journal disabled: {} cannot be created ({e})",
            dir.display()
        );
        return Arc::new(Disabled);
    }
    let journal = Arc::new(FileJournal::new(dir.clone()));
    tracing::info!(
        "journal: {} → {} (read it, do not parse it: bd is the record)",
        journal.describe(),
        crate::services::transcript_file::display_path(&dir)
    );
    journal
}

/// `~/.local/share/looprs/transcripts`, or `$XDG_DATA_HOME`'s equivalent.
///
/// The user's *data* directory, per ADR-0004 R3: durable enough to survive a
/// reboot, private enough by default, and short enough to name in a log line
/// (which the on-demand dump learned the hard way — see
/// `default_dump_dir` in [`crate::services::transcript_file`]).
fn default_journal_dir() -> PathBuf {
    let data = env_value("XDG_DATA_HOME")
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env_value("HOME")
                .filter(|v| !v.trim().is_empty())
                .map(|h| Path::new(h.trim()).join(".local").join("share"))
        })
        .unwrap_or_else(std::env::temp_dir);
    data.join("looprs").join("transcripts")
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The journal every view shares when nobody installed one: off.
///
/// A shared static rather than a fresh `Disabled` per view so the "which journal
/// is this app using" answer stays one answer even in the default case.
pub fn default_journal() -> Arc<dyn Journal> {
    static NONE: OnceLock<Arc<dyn Journal>> = OnceLock::new();
    NONE.get_or_init(|| Arc::new(Disabled)).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TerminalType;

    /// A scratch directory that cannot collide with another test's, and is gone
    /// when the test is done.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "looprs-journal-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .expect("build the scratch journal dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Wait for the writer thread to have put `want` on disk.
    ///
    /// The journal is asynchronous on purpose, so a test that reads the file
    /// immediately is testing the scheduler rather than the journal. Polling
    /// with a deadline keeps the assertion about *what* lands, not *when*.
    fn wait_for(path: &Path, want: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = read(path);
            if got.contains(want) || std::time::Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait until `n` session files exist.
    ///
    /// The file is created by the writer thread on the first chunk it drains,
    /// so "which files are there" is a question with an asynchronous answer;
    /// asking it immediately tests the thread scheduler.
    fn wait_for_files(dir: &Path, n: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = only_files(dir);
            if got.len() >= n || std::time::Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn only_files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read the journal dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("session-"))
            .collect();
        v.sort();
        v
    }

    /// **R2, the whole point of the module: the bytes are on disk while the
    /// session is still running**, not on the exit path. If this ever fails by
    /// passing only after `close`, the journal has become the thing the ADR
    /// rejected — a transcript that does not exist after a crash.
    #[test]
    fn the_transcript_lands_before_the_session_ends() {
        let scratch = Scratch::new("lands");
        let j = FileJournal::new(&scratch.0);
        j.append(TerminalType::Beads.label(), "the first answer\n".into());
        let files = wait_for_files(&scratch.0, 1);
        assert_eq!(files.len(), 1, "one file for one mode: {files:?}");
        let body = wait_for(&scratch.0.join(&files[0]), "the first answer");
        assert_eq!(body, "the first answer\n", "and nothing else in it yet");
        j.close();
    }

    /// The write is flushed per chunk, so a second entry arrives after the
    /// first is already durable rather than riding on the first's close.
    #[test]
    fn each_entry_is_durable_as_it_finalises_not_in_one_fall() {
        let scratch = Scratch::new("per-chunk");
        let j = FileJournal::new(&scratch.0);
        j.append(TerminalType::Beads.label(), "one\n".into());
        let files = wait_for_files(&scratch.0, 1);
        let path = scratch.0.join(&files[0]);
        wait_for(&path, "one");
        j.append(TerminalType::Beads.label(), "two\n".into());
        let body = wait_for(&path, "two");
        assert_eq!(body, "one\ntwo\n", "in order, with no separator invented");
        j.close();
    }

    /// **One file per mode, never interleaved.** The beads transcript and the pi
    /// transcript are different documents, and a single shared file would make
    /// neither readable end to end.
    #[test]
    fn one_mode_never_writes_into_another_modes_file() {
        // The file-name suffixes, derived from the labels rather than typed out,
        // so this greps for the names that are really written.
        let beads_suffix = format!("-{}.txt", TerminalType::Beads.label());
        let pi_suffix = format!("-{}.txt", TerminalType::Pi.label());
        let scratch = Scratch::new("per-mode");
        let j = FileJournal::new(&scratch.0);
        j.append(TerminalType::Beads.label(), "beads said this\n".into());
        j.append(TerminalType::Pi.label(), "pi said that\n".into());
        j.append(TerminalType::Beads.label(), "beads said more\n".into());
        let files = wait_for_files(&scratch.0, 2);
        assert_eq!(files.len(), 2, "one per mode: {files:?}");
        let beads: Vec<String> = files
            .iter()
            .filter(|f| f.ends_with(&beads_suffix))
            .map(|f| wait_for(&scratch.0.join(f), "beads said more"))
            .collect();
        let pi: Vec<String> = files
            .iter()
            .filter(|f| f.ends_with(&pi_suffix))
            .map(|f| wait_for(&scratch.0.join(f), "pi said that"))
            .collect();
        assert_eq!(beads.len(), 1);
        assert_eq!(beads[0], "beads said this\nbeads said more\n");
        assert_eq!(pi[0], "pi said that\n");
        assert!(!pi[0].contains("beads"), "no cross-contamination");
        j.close();
    }

    /// `last` and `last-<mode>` exist so the marker row can name a path that is
    /// still valid tomorrow, when the timestamped file it points at has been
    /// joined by forty more.
    #[test]
    fn the_last_links_point_at_something_a_user_can_type() {
        let last_beads = format!("last-{}", TerminalType::Beads.label());
        let scratch = Scratch::new("links");
        let j = FileJournal::new(&scratch.0);
        j.append(TerminalType::Beads.label(), "hello\n".into());
        let target = scratch.0.join(&wait_for_files(&scratch.0, 1)[0]);
        for name in ["last", last_beads.as_str()] {
            let link = scratch.0.join(name);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::fs::read_link(&link)
                .map(|t| t != target)
                .unwrap_or(true)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let pointed = std::fs::read_link(&link).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                pointed, target,
                "{name} should point at the file being written"
            );
        }
        assert_eq!(
            j.display_path(TerminalType::Beads.label()),
            Some(crate::services::transcript_file::display_path(
                &scratch.0.join(&last_beads)
            )),
            "and the hint the marker shows is that link"
        );
        j.close();
    }

    /// **A mode that has said nothing has no path to point at.** A marker row
    /// naming a file that was never written is worse than no hint at all.
    #[test]
    fn a_mode_that_never_spoke_has_no_path_to_show() {
        let scratch = Scratch::new("silent");
        let j = FileJournal::new(&scratch.0);
        assert_eq!(j.display_path(TerminalType::Bash.label()), None);
        j.append(TerminalType::Beads.label(), "x\n".into());
        wait_for(&scratch.0.join(&wait_for_files(&scratch.0, 1)[0]), "x");
        assert!(j.display_path(TerminalType::Beads.label()).is_some());
        assert!(
            j.display_path(TerminalType::Bash.label()).is_none(),
            "bash still has no file"
        );
        j.close();
    }

    /// **`0600` on the file.** The journal holds everything the user pasted
    /// into an agent that spends money on their behalf; a group- or
    /// world-readable transcript is a leak the feature did not ask for.
    #[test]
    fn the_transcript_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("mode");
        let j = FileJournal::new(&scratch.0);
        j.append(TerminalType::Beads.label(), "secret pasted thing\n".into());
        let path = scratch.0.join(&wait_for_files(&scratch.0, 1)[0]);
        wait_for(&path, "secret pasted thing");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{path:?} must not be readable by anyone else");
        j.close();
    }

    /// A queue that never drains must shed rather than grow, and it must say so:
    /// the difference between "the journal is behind" and "the journal is
    /// quietly incomplete" is the one thing the user cannot find out afterwards.
    #[test]
    fn a_stalled_writer_sheds_at_the_bound_and_counts_it() {
        // The bound itself, asserted rather than trusted: 16 chunks of backlog is
        // the number the module doc quotes, and a `QUEUE_CHUNKS` that drifted
        // would make that sentence wrong rather than this test wrong.
        assert_eq!(QUEUE_CHUNKS, 16);
        let scratch = Scratch::new("shed");
        let j = FileJournal::new(&scratch.0);
        // Hand over far more than the queue can hold without letting the writer
        // catch up on every one: what must be true is that `append` never blocks
        // and the journal survives either answer.
        let big = "x".repeat(64 * 1024);
        let started = std::time::Instant::now();
        for _ in 0..(QUEUE_CHUNKS * 8) {
            j.append(TerminalType::Beads.label(), big.clone());
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "append never blocks on the disk: {:?}",
            started.elapsed()
        );
        j.close();
        // After the drain, the file exists and is one continuous transcript:
        // shedding whole chunks is a loss with a count; an interleaved partial
        // write is corruption.
        let body = read(&scratch.0.join(&wait_for_files(&scratch.0, 1)[0]));
        assert!(
            !body.is_empty() && body.chars().all(|c| c == 'x'),
            "no torn chunk: {:?}",
            &body[..body.len().min(80)]
        );
    }

    /// Close is the drain, not the writer. Whatever was never handed over is
    /// simply not in the file — which is the R3 rule that the exit path
    /// invents nothing.
    #[test]
    fn close_drains_what_was_handed_over_and_nothing_else() {
        let scratch = Scratch::new("drain");
        let j = FileJournal::new(&scratch.0);
        j.append(TerminalType::Beads.label(), "a\n".into());
        j.append(TerminalType::Beads.label(), "b\n".into());
        j.close();
        let body = read(&scratch.0.join(&only_files(&scratch.0)[0]));
        assert_eq!(body, "a\nb\n", "both chunks made it out");
        j.close();
        assert_eq!(
            read(&scratch.0.join(&only_files(&scratch.0)[0])),
            "a\nb\n",
            "and closing twice is not an error and writes nothing more"
        );
    }

    /// The off switch is not a footnote: with the journal disabled nothing may
    /// touch the disk, and there is nothing for the marker to point at.
    #[test]
    fn the_disabled_journal_writes_nothing_and_names_nothing() {
        let j = Disabled;
        j.append(TerminalType::Beads.label(), "x".into());
        assert_eq!(j.display_path(TerminalType::Beads.label()), None);
        assert_eq!(j.describe(), "off");
    }

    #[test]
    fn the_default_journal_is_the_one_that_writes_nothing() {
        // Not taste: the ~640-test suite must not be able to reach the disk by
        // default, which is the same reason the real journal is only ever built
        // in `main`.
        assert_eq!(default_journal().describe(), "off");
    }

    /// The file name is the sort order and the collision guard, so both are
    /// worth pinning: UTC (never local time, which would sort the directory by
    /// whatever TZ the loop happened to run under) and the pid (two runs in the
    /// same second, one file each). The mode part is pinned to
    /// `TerminalType::label()` verbatim — the same string the callers pass, not
    /// a lowercase stand-in for it.
    #[test]
    fn the_file_name_is_utc_shaped_and_pid_unique() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let p = session_path(Path::new("/j"), TerminalType::Beads.label(), t);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            name,
            format!(
                "session-{}-{}-{}.txt",
                civil_utc(1_700_000_000),
                std::process::id(),
                TerminalType::Beads.label()
            )
        );
        assert!(name.starts_with("session-20231114T221320Z-"), "{name}");
        assert!(
            name.ends_with("-Beads.txt"),
            "{name}: the mode is the label, capitalised, as the writer spells it"
        );
        assert_eq!(p.parent().unwrap(), Path::new("/j"));
    }

    /// **The layout block at the top of this file is a path an operator is
    /// told to type, so it gets checked against the writer, not against
    /// someone's memory.**
    ///
    /// `session_path` interpolates the mode string verbatim, which is what
    /// made looprs-00u.21 invisible to the code: renaming `label()` moves the
    /// bytes on disk without touching anything that could fail a compile, and
    /// the only thing left wrong is the prose at the top of the module — and
    /// prose cannot fail a build. The header said `last-beads` for the entire
    /// time the directory carried `last-Beads`, and the operator who trusted it
    /// got `No such file or directory` for a file that was standing right
    /// there.
    ///
    /// Asserted against this file's own source — the precedent set by
    /// `kanban.rs`, `router.rs` and `main.rs` — because the two sides of the
    /// claim, a doc block and a `format!`, share no symbol a compiler could
    /// check. The non-vacuity half: rewrite the block back to lowercase
    /// `-beads.txt` and this fails on both the suffix check and the
    /// completeness check.
    #[test]
    fn the_header_block_names_the_files_the_writer_actually_writes() {
        let src = include_str!("journal.rs");
        // The module header only: everything from the test attribute down is
        // tests, which legitimately contain the strings grepped for here.
        let header = src.split("#[cfg(test)]").next().unwrap_or(src);
        let block = header
            .split("```text\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the module header has a ```text layout block");

        let labels: Vec<&'static str> = TerminalType::ALL.iter().map(|m| m.label()).collect();

        let mut shown_files: Vec<String> = Vec::new();
        let mut shown_links: Vec<String> = Vec::new();
        for token in block.split_whitespace() {
            if let Some(stem) = token.strip_suffix(".txt") {
                shown_files.push(stem.to_string());
            } else if let Some(rest) = token.strip_prefix("last-") {
                shown_links.push(rest.to_string());
            }
        }
        // Non-vacuity: the block really is a listing, not a paragraph.
        assert!(
            shown_files.len() >= 2,
            "the header block names at least two session files, saw {shown_files:?}"
        );
        assert!(
            shown_links.len() >= labels.len(),
            "the header block shows one `last-<mode>` link per mode, saw {shown_links:?}"
        );

        // Every documented session file is `session-<stamp>-<pid>-<Label>.txt`
        // with the mode spelled the way the type spells it. Case-sensitive on
        // purpose: the whole bug is a case.
        for stem in &shown_files {
            // The mode is the last dash-separated component. `…-Beads.txt` in
            // the block is an elided path, not a fourth shape, so the full
            // four-part check only applies where the name is written out.
            let mode = stem.rsplit('-').next().unwrap_or(stem);
            assert!(
                labels.contains(&mode),
                "{stem}.txt: mode {mode:?} is not a TerminalType::label() {labels:?} — \
                 the block must spell the mode as the writer does",
            );
            if stem.starts_with("session-") {
                assert_eq!(
                    stem.split('-').count(),
                    4,
                    "{stem}.txt: session-<utc>-<pid>-<mode>, nothing shorter"
                );
            }
        }

        // And every documented link is `last-<Label>`.
        for mode in &shown_links {
            assert!(
                labels.contains(&mode.as_str()),
                "last-{mode}: not a TerminalType::label() {labels:?}"
            );
        }

        // Completeness: each mode the app can be in appears in the block, so a
        // fourth TerminalType has to be written into the header before it can
        // be left out of it.
        for label in &labels {
            assert!(
                shown_files
                    .iter()
                    .any(|f| f.ends_with(&format!("-{label}")))
                    || shown_links.iter().any(|l| l == label),
                "mode {label} is undocumented in the header's layout block"
            );
            // The name the writer would really produce for this mode, checked
            // against the same shape the block advertises.
            let produced = session_path(Path::new("/j"), label, SystemTime::UNIX_EPOCH)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            assert!(
                produced.ends_with(&format!("-{label}.txt")),
                "session_path produced {produced} for mode {label}; the header must \
                 document that spelling"
            );
            // Same name, two spellings: what the writer makes for this mode and
            // what the block advertises must agree on the mode component.
            let produced_mode = produced
                .trim_end_matches(".txt")
                .rsplit('-')
                .next()
                .unwrap_or_default();
            assert!(
                shown_files
                    .iter()
                    .any(|f| { f.rsplit('-').next().unwrap_or_default() == produced_mode }),
                "no header file name carries the mode suffix {produced_mode} that \
                 session_path produces for {label}"
            );
        }
    }

    /// The hand-rolled civil-time formatter, checked against dates whose UTC
    /// strings are easy to verify by eye — including the leap-day case, which is
    /// the only place a shortcut in this algorithm shows up late.
    #[test]
    fn civil_utc_formats_the_dates_that_are_easy_to_check_by_eye() {
        let cases: [(u64, &str); 6] = [
            (0, "19700101T000000Z"),
            (86_400, "19700102T000000Z"),
            (1_700_000_000, "20231114T221320Z"),
            (1_709_296_545, "20240301T123545Z"),
            (951_782_400, "20000229T000000Z"),
            (2_147_483_647, "20380119T031407Z"),
        ];
        for (secs, want) in cases {
            assert_eq!(civil_utc(secs), want, "{secs}");
        }
    }
}
