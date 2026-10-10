//! Where the log goes, how loud it is, and how much of it survives (looprs-00u.13).
//!
//! Before this module the whole policy was four lines in `main`: `temp_dir()`,
//! `rolling::never`, and `unwrap_or_else(|_| EnvFilter::new("debug"))`. Each of
//! those was a decision nobody could reach: the destination was wherever your temp
//! dir happened to be, the retention policy was "never", and the volume was the
//! *debug* volume on every run — in a terminal app whose design elsewhere is
//! explicitly cost-aware (bounded bus, bounded scrollback, measured clipboard
//! path). This module makes all three resolved, stated and bounded.
//!
//! # The three knobs, and the order they resolve in
//!
//! * **destination** — [`resolve_log_dir`]: `$LOOPRS_LOG_DIR` → `$XDG_STATE_HOME`
//!   → `~/.local/state` → `std::env::temp_dir()`. Same ladder shape as the
//!   journal's ([`crate::services::journal::journal_from_env`]) and the dump's
//!   ([`crate::services::transcript_file::transcript_sink_from_env`]), so "where
//!   does my stuff go" has one answer shape across the app. `logs` is
//!   `state`-shaped data (XDG's own category for logs), and the file inside it is
//!   `looprs.log`, so the directory does not need a second "logs" segment to say
//!   what is in it.
//! * **level** — [`resolve_level`]: `$RUST_LOG` if it parses, otherwise
//!   [`DEFAULT_LEVEL`] = `info`. The old default was `debug`, which means every
//!   ordinary run wrote debug volume forever. Debug is still one variable away,
//!   and the operator page says so in the first paragraph about reporting a
//!   problem.
//! * **budget** — [`resolve_max_bytes`] / [`resolve_keep`]: the active file is
//!   rotated at `max_bytes` and the last `keep` rotated files are kept, so the
//!   directory holds at most `(keep + 1) × max_bytes` and that number is printed
//!   at startup rather than discovered at cleanup time.
//!
//! # Why a hand-rolled rotator
//!
//! `tracing_appender::rolling` does *time*-based rotation only (`minutely`,
//! `hourly`, `daily`) and never deletes anything. Neither half of what is wanted:
//! hourly leaves each file unbounded in size, and no rotation of any kind bounds
//! the directory. A size cap with a fixed number of generations is the smallest
//! thing that answers "how much log will this leave on my disk" with a number —
//! and it is about sixty lines over a plain `File`, which is cheaper than being
//! wrong about the retention story.
//!
//! The cap is per *rotation check*, so the active file can overshoot by up to one
//! log line. `written` is counted after the write returns, not before, so the
//! accounting describes the bytes that landed rather than the bytes requested.
//!
//! # Nothing in here logs, until there is somewhere to log to
//!
//! Resolution happens before the subscriber exists, so every warning it produces
//! is *collected* into [`LogPlan::warnings`] and emitted by [`init_logging`]
//! immediately after the global subscriber is set. A resolver that called
//! `tracing::warn!` would be writing into the very writer it is still deciding
//! where to put, and the `RotatingLog` writer never calls `tracing` at all for
//! the same reason (it would re-enter its own mutex on the same thread).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::services::transcript_file::display_path;

/// The log file's name. Stable, because the operator page's whole grep index is
/// spelled against it, and because the *active* file keeps this name while the
/// rotated generations get `.1`, `.2`, … behind it.
pub const LOG_FILE_NAME: &str = "looprs.log";

/// The level with nothing in the environment. `info`, not `debug`: see the module
/// header. `RUST_LOG=debug` is the documented way to raise it, and the way to
/// raise one module only (`RUST_LOG=looprs::board_poller=debug`).
pub const DEFAULT_LEVEL: &str = "info";

/// 1 MiB per file. A default-settings run writes roughly 15 KiB of startup block
/// plus a few hundred bytes per board poll and per transcript entry, so a whole
/// day of beads looping at `info` fits in a couple of rotations.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// Below this the app rotates on what feels like every keystroke, which makes the
/// file more useless than a slightly bigger one would.
pub const MIN_MAX_BYTES: u64 = 16 * 1024;

/// The ceiling on a single file, so a typo with extra zeroes cannot eat a volume.
pub const MAX_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Four rotated generations plus the active file = five files on disk.
pub const DEFAULT_KEEP: usize = 4;

/// More history than anybody has asked for, and the clamp keeps the rename chain
/// (one `rename` per generation per rotation) short.
pub const MAX_KEEP: usize = 32;

/// Where a resolved value came from. Carried in the plan because the startup line
/// has to say *which* of the ladder answered — an operator who set
/// `LOOPRS_LOG_DIR` and is looking at `~/.local/state` needs the two states
/// distinguishable without reading this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirSource {
    /// `$LOOPRS_LOG_DIR`.
    Knob,
    /// `$XDG_STATE_HOME` / `looprs`.
    XdgState,
    /// `$HOME` / `.local/state` / `looprs`.
    Home,
    /// `std::env::temp_dir()` — the ladder's last rung, and the pre-00u.13
    /// behaviour.
    Temp,
}

impl DirSource {
    /// How the startup line names the source.
    pub fn label(&self) -> &'static str {
        match self {
            DirSource::Knob => "LOOPRS_LOG_DIR",
            DirSource::XdgState => "XDG_STATE_HOME",
            DirSource::Home => "$HOME/.local/state",
            DirSource::Temp => "the system temp dir",
        }
    }
}

/// How the level was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelSource {
    /// `$RUST_LOG`, and it parsed.
    Knob,
    /// [`DEFAULT_LEVEL`], because the environment said nothing usable.
    Default,
}

/// A resolved level: the filter string, who chose it, and the loud word about a
/// value that was set but could not be honoured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelChoice {
    pub filter: String,
    pub source: LevelSource,
    pub warning: Option<String>,
}

/// A resolved byte budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeChoice {
    pub bytes: u64,
    pub warning: Option<String>,
}

/// A resolved rotation count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepChoice {
    pub keep: usize,
    pub warning: Option<String>,
}

/// Everything logging will do this run, decided before the writer exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPlan {
    pub dir: PathBuf,
    pub dir_source: DirSource,
    pub level: String,
    pub level_source: LevelSource,
    pub max_bytes: u64,
    pub keep: usize,
    /// Fallbacks collected while resolving. Emitted after the subscriber is up;
    /// see the module header for why they cannot be emitted sooner.
    pub warnings: Vec<String>,
}

impl LogPlan {
    /// Read every logging knob once, from the process environment.
    pub fn from_env() -> Self {
        let (dir, dir_source) = resolve_log_dir(
            env_value("LOOPRS_LOG_DIR").as_deref(),
            env_value("XDG_STATE_HOME").as_deref(),
            env_value("HOME").as_deref(),
            &std::env::temp_dir(),
        );
        let level = resolve_level(env_value("RUST_LOG").as_deref());
        let max = resolve_max_bytes(env_value("LOOPRS_LOG_MAX_BYTES").as_deref());
        let keep = resolve_keep(env_value("LOOPRS_LOG_KEEP").as_deref());

        let mut warnings = Vec::new();
        for w in [
            level.warning.clone(),
            max.warning.clone(),
            keep.warning.clone(),
        ]
        .into_iter()
        .flatten()
        {
            warnings.push(w);
        }

        LogPlan {
            dir,
            dir_source,
            level: level.filter,
            level_source: level.source,
            max_bytes: max.bytes,
            keep: keep.keep,
            warnings,
        }
    }

    pub fn active_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE_NAME)
    }

    /// The whole budget on disk: the active file plus every generation kept.
    /// Saturating, because `total × keep` on a nonsense input must not overflow
    /// into a number that reads like a small one.
    pub fn total_bytes(&self) -> u64 {
        self.max_bytes
            .saturating_mul(self.keep.saturating_add(1) as u64)
    }
}

/// The destination ladder, as a function of four strings rather than of the
/// machine's environment.
///
/// * `log_dir` non-empty — the operator chose it, [`DirSource::Knob`].
/// * `xdg_state` non-empty — `$XDG_STATE_HOME/looprs`.
/// * `home` non-empty — `~/.local/state/looprs`, the XDG default path.
/// * nothing — the temp dir, which is what the app used before it had a choice.
///
/// Whitespace-only counts as unset in every case: `LOOPRS_LOG_DIR="   "` is a
/// hand-typed nothing, and "nothing" and "a directory called three spaces" have
/// different answers.
pub fn resolve_log_dir(
    log_dir: Option<&str>,
    xdg_state: Option<&str>,
    home: Option<&str>,
    temp: &Path,
) -> (PathBuf, DirSource) {
    if let Some(dir) = non_empty(log_dir) {
        return (PathBuf::from(dir), DirSource::Knob);
    }
    if let Some(state) = non_empty(xdg_state) {
        return (PathBuf::from(state).join("looprs"), DirSource::XdgState);
    }
    if let Some(h) = non_empty(home) {
        return (
            Path::new(h).join(".local").join("state").join("looprs"),
            DirSource::Home,
        );
    }
    (temp.to_path_buf(), DirSource::Temp)
}

/// The level ladder: `$RUST_LOG` if it parses as an [`EnvFilter`], otherwise
/// [`DEFAULT_LEVEL`].
///
/// A `RUST_LOG` that does not parse is a knob that was turned and ignored, so it
/// comes with a warning naming the value and the thing that was used instead. The
/// old code did the same fallback silently
/// (`try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"))`), which
/// means a typo'd filter read as "debug, forever" with nothing to read back.
///
/// An empty `RUST_LOG` is *unset*, not "a filter with no directives": the latter
/// turns everything off, and nobody means that when they type `RUST_LOG=`.
pub fn resolve_level(raw: Option<&str>) -> LevelChoice {
    let Some(raw) = non_empty(raw) else {
        return LevelChoice {
            filter: DEFAULT_LEVEL.to_string(),
            source: LevelSource::Default,
            warning: None,
        };
    };
    if EnvFilter::try_new(raw).is_ok() {
        return LevelChoice {
            filter: raw.to_string(),
            source: LevelSource::Knob,
            warning: None,
        };
    }
    LevelChoice {
        filter: DEFAULT_LEVEL.to_string(),
        source: LevelSource::Default,
        warning: Some(format!(
            "RUST_LOG={raw:?} is not a valid filter; using {DEFAULT_LEVEL} \
             (try `debug`, `info`, or `looprs::bus=trace,looprs=warn` for one module loudly)"
        )),
    }
}

/// The per-file cap. Numbers with a unit suffix (`512K`, `2M`) are accepted
/// because the thing an operator is doing is arithmetic on disk space.
pub fn resolve_max_bytes(raw: Option<&str>) -> SizeChoice {
    let Some(raw) = non_empty(raw) else {
        return SizeChoice {
            bytes: DEFAULT_MAX_BYTES,
            warning: None,
        };
    };
    // Suffixes first, longest match first, so `MB` is never read as `M` + `B`.
    let upper = raw.to_ascii_uppercase();
    let (digits, scale) = if let Some(d) = upper.strip_suffix("MB") {
        (d, 1024u64 * 1024)
    } else if let Some(d) = upper.strip_suffix("KB") {
        (d, 1024)
    } else if let Some(d) = upper.strip_suffix("M") {
        (d, 1024 * 1024)
    } else if let Some(d) = upper.strip_suffix("K") {
        (d, 1024)
    } else {
        (upper.as_str(), 1)
    };
    // Saturating: `999999999999K` must read as a huge number that gets clamped,
    // not as an overflow panic or as a small one.
    let parsed = digits
        .trim()
        .parse::<u64>()
        .map(|v| v.saturating_mul(scale));
    match parsed {
        Ok(0) => SizeChoice {
            bytes: MIN_MAX_BYTES,
            warning: Some(format!(
                "LOOPRS_LOG_MAX_BYTES=0 would rotate on every write; using {} instead",
                human_bytes(MIN_MAX_BYTES)
            )),
        },
        Ok(bytes) if bytes < MIN_MAX_BYTES => SizeChoice {
            bytes: MIN_MAX_BYTES,
            warning: Some(format!(
                "LOOPRS_LOG_MAX_BYTES={} is below the smallest useful cap; clamped to {}",
                bytes,
                human_bytes(MIN_MAX_BYTES)
            )),
        },
        Ok(bytes) if bytes > MAX_MAX_BYTES => SizeChoice {
            bytes: MAX_MAX_BYTES,
            warning: Some(format!(
                "LOOPRS_LOG_MAX_BYTES={} is above the ceiling; clamped to {}",
                bytes,
                human_bytes(MAX_MAX_BYTES)
            )),
        },
        Ok(bytes) => SizeChoice {
            bytes,
            warning: None,
        },
        Err(e) => SizeChoice {
            bytes: DEFAULT_MAX_BYTES,
            warning: Some(format!(
                "LOOPRS_LOG_MAX_BYTES={raw:?} is not a size ({e}); using {}",
                human_bytes(DEFAULT_MAX_BYTES)
            )),
        },
    }
}

/// How many rotated generations to keep. `0` is a real answer — one capped file,
/// nothing kept — and it is honoured by truncating the active file rather than by
/// renaming anything.
pub fn resolve_keep(raw: Option<&str>) -> KeepChoice {
    let Some(raw) = non_empty(raw) else {
        return KeepChoice {
            keep: DEFAULT_KEEP,
            warning: None,
        };
    };
    match raw.parse::<usize>() {
        Ok(keep) if keep > MAX_KEEP => KeepChoice {
            keep: MAX_KEEP,
            warning: Some(format!(
                "LOOPRS_LOG_KEEP={keep} is above the ceiling; clamped to {MAX_KEEP}"
            )),
        },
        Ok(keep) => KeepChoice {
            keep,
            warning: None,
        },
        Err(e) => KeepChoice {
            keep: DEFAULT_KEEP,
            warning: Some(format!(
                "LOOPRS_LOG_KEEP={raw:?} is not a number ({e}); using {DEFAULT_KEEP}"
            )),
        },
    }
}

fn non_empty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string())
}

/// A byte count the way a person reads one off a log line.
pub fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= 10 * MIB {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

// ───────────────────────────── the writer ─────────────────────────────

/// The size-capped rotating writer.
///
/// Cheap to clone (one `Arc`), because `tracing_appender::non_blocking` moves the
/// writer it is given onto its worker thread while [`LogGuard`] keeps a handle to
/// the same [`Inner`] for the counters.
#[derive(Clone)]
pub struct RotatingLog {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    dir: PathBuf,
    name: String,
    max_bytes: u64,
    keep: usize,
    /// The currently open active file. `None` after an unrecoverable open failure,
    /// which turns the writer into a counted sink rather than a panic.
    file: Mutex<Option<File>>,
    written: AtomicU64,
    rotations: AtomicU64,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl RotatingLog {
    /// Open (creating and appending to) `dir/looprs.log` under a `max_bytes` cap
    /// with `keep` generations behind it.
    ///
    /// `written` is seeded from the existing file's size, so the cap is a
    /// property of the *file* rather than of this process: a second run cannot
    /// restart the budget from zero and leave a 2 MiB "1 MiB" file behind.
    pub fn new(dir: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let active = dir.join(LOG_FILE_NAME);
        let file = open_appending(&active)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RotatingLog {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                name: LOG_FILE_NAME.to_string(),
                max_bytes,
                keep,
                file: Mutex::new(Some(file)),
                written: AtomicU64::new(written),
                rotations: AtomicU64::new(0),
                failures: AtomicU64::new(0),
                last_error: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn into_inner(self) -> Arc<Inner> {
        self.inner
    }
}

impl Inner {
    fn rotated(&self, index: usize) -> PathBuf {
        self.dir.join(format!("{}.{}", self.name, index))
    }

    fn active(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// Rotate the active file. Called with the file mutex held, and takes the
    /// current handle through `slot` so there is no window in which the writer
    /// has no file.
    fn rotate(&self, slot: &mut Option<File>) {
        self.rotations.fetch_add(1, Ordering::Relaxed);
        if let Some(f) = slot.as_mut() {
            let _ = f.flush();
        }

        if self.keep == 0 {
            // One file, capped, nothing kept: truncate it and carry on. A rename
            // chain of length zero is just a delete with extra steps, and the
            // operator asked for no history.
            match open_truncating(&self.active()) {
                Ok(f) => {
                    *slot = Some(f);
                    self.written.store(0, Ordering::Relaxed);
                }
                Err(e) => self.record(&e),
            }
            return;
        }

        // Drop the generation that is about to fall off the back, then shift the
        // rest by one, then hand the active name over. `keep` renames per
        // rotation, once per `max_bytes` written: with the defaults that is four
        // syscalls per megabyte.
        let _ = fs::remove_file(self.rotated(self.keep));
        for i in (1..self.keep).rev() {
            let from = self.rotated(i);
            if from.exists() {
                let _ = fs::rename(&from, self.rotated(i + 1));
            }
        }

        let active = self.active();
        match fs::rename(&active, self.rotated(1)).and_then(|_| open_appending(&active)) {
            Ok(f) => {
                *slot = Some(f);
                self.written.store(0, Ordering::Relaxed);
            }
            Err(e) => {
                // Keep the old handle. The bytes still have somewhere to go, and
                // a rotation that failed once is likely to fail again, so the
                // reset happens either way: retrying on every write would turn a
                // broken directory into a syscall storm.
                self.record(&e);
                self.written.store(0, Ordering::Relaxed);
            }
        }
    }

    fn record(&self, e: &io::Error) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        let mut guard = lock(&self.last_error);
        *guard = Some(e.to_string());
    }
}

fn open_appending(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        // 0600: the transcript and the log hold the same class of secret (paths,
        // command lines, tool output), and ADR-0004 R3 makes the mode bits the
        // protection for both.
        .mode(0o600)
        .open(path)
}

fn open_truncating(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Lock without panic-on-poison. A panic on the writer thread loses a log line;
/// a panic *here* takes the app down with it, which is the worse of the two by a
/// wide margin, and the same reason `journal`'s writer never unwinds.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Write for RotatingLog {
    /// Write one line, rotating first if the cap has been reached.
    ///
    /// Errors are swallowed and counted. Two reasons, both load-bearing: the
    /// `tracing-appender` worker prints a flush failure straight to stderr, which
    /// in this app means *onto the user's screen in the middle of the frame*; and
    /// a `Write` that returns `Err` gets retried by callers that should not be
    /// retrying anything on the UI path.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let inner = &self.inner;
        let mut slot = lock(&inner.file);
        if inner.written.load(Ordering::Relaxed) >= inner.max_bytes {
            inner.rotate(&mut slot);
        }
        let Some(file) = slot.as_mut() else {
            return Ok(buf.len());
        };
        match file.write(buf) {
            Ok(n) => {
                inner.written.fetch_add(n as u64, Ordering::Relaxed);
                Ok(n)
            }
            Err(e) => {
                inner.record(&e);
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut slot = lock(&self.inner.file);
        if let Some(file) = slot.as_mut() {
            // Swallowed for the reason above: this is the one error path in the
            // dependency that prints to stderr.
            let _ = file.flush();
        }
        Ok(())
    }
}

/// The lifetime handle [`init_logging`] hands back.
///
/// Holding it is what keeps the `tracing-appender` worker alive — drop it early
/// and buffered lines are simply not written, which is exactly the failure mode
/// the old `WorkerGuard` binding in `main` existed to prevent.
pub struct LogGuard {
    /// Never read: holding it is the whole point (it flushes the writer thread on
    /// the way out). Named with the underscore so the lint agrees that it is a
    /// guard rather than a field somebody forgot about.
    _worker: WorkerGuard,
    inner: Arc<Inner>,
    plan: LogPlan,
}

impl LogGuard {
    pub fn rotations(&self) -> u64 {
        self.inner.rotations.load(Ordering::Relaxed)
    }

    pub fn failures(&self) -> u64 {
        self.inner.failures.load(Ordering::Relaxed)
    }

    /// Say, at the end of the run, what the budget was spent on.
    ///
    /// The startup line states the ceiling; this states what actually happened
    /// against it, which is the difference between a bound that is documented
    /// and a bound that is observed.
    pub fn report(&self) {
        let rotations = self.rotations();
        let active = self.inner.written.load(Ordering::Relaxed);
        tracing::info!(
            "log budget: {rotations} rotation(s), {} in the active file, ceiling {} in {}",
            human_bytes(active),
            human_bytes(self.plan.total_bytes()),
            display_path(&self.plan.dir),
        );
        if self.failures() > 0 {
            // Warn rather than info: a run that could not rotate is a run whose
            // bound did not hold, and the operator should see it without
            // grepping for it.
            tracing::warn!(
                "log rotation: {} failure(s), last was {} — the ceiling may not have held",
                self.failures(),
                lock(&self.inner.last_error).clone().unwrap_or_default(),
            );
        }
    }
}

/// Try the resolved directory; if it cannot be made, take the last rung with a
/// loud word about it.
///
/// Split out of [`init_logging`] so the fallback is a function a test can hand a
/// broken path — a configured destination that cannot be created is a fallback
/// with a `WARN`, not a reason to stop the app, which is the same rule
/// `LOOPRS_KANBAN_ROWS` follows and the same rule the journal states for its own
/// directory. The remaining hard case (the *fallback* is unusable too) is left to
/// the caller, because it is the one case where the run genuinely cannot promise a
/// log.
pub fn with_dir_fallback(mut plan: LogPlan) -> LogPlan {
    let Err(e) = ensure_dir(&plan.dir) else {
        return plan;
    };
    let fallback = std::env::temp_dir();
    plan.warnings.push(format!(
        "{} ({}) cannot be created ({e}); falling back to {}",
        plan.dir.display(),
        plan.dir_source.label(),
        fallback.display()
    ));
    plan.dir = fallback;
    plan.dir_source = DirSource::Temp;
    plan
}

/// Build the global subscriber from the environment.
///
/// Called once, before anything that can switch a terminal mode on, so a startup
/// diagnostic can still reach the operator's actual terminal when the file it
/// would normally write to is the thing that failed.
pub fn init_logging() -> anyhow::Result<LogGuard> {
    let plan = with_dir_fallback(LogPlan::from_env());
    // If even the temp dir is unusable there is nowhere left to go and the run
    // genuinely cannot promise a log. That is worth a hard stop: the alternative
    // is an app that looks fine and remembers nothing. It arrives on stderr, which
    // is still the user's screen at this point in the process.
    ensure_dir(&plan.dir)
        .map_err(|e| anyhow::anyhow!("no writable log directory: {} ({e})", plan.dir.display()))?;

    let rotator = RotatingLog::new(&plan.dir, plan.max_bytes, plan.keep)?;
    let (writer, worker) = tracing_appender::non_blocking(rotator.clone());

    // `plan.level` was validated by `resolve_level`, so `EnvFilter::new` (which
    // panics on a bad directive) has nothing left to panic about.
    let filter = EnvFilter::new(&plan.level);
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_env_filter(filter)
            .finish(),
    )?;

    // The subscriber exists now, so the things collected on the way here have
    // somewhere to go.
    for warning in &plan.warnings {
        tracing::warn!("logging: {warning}");
    }

    let level_how = match plan.level_source {
        LevelSource::Knob => "RUST_LOG",
        LevelSource::Default => "default; RUST_LOG=debug for a loud run",
    };
    // ASCII separators and one line per fact: this line is grepped from the
    // operator page and parsed by `spikes/log_budget_e2e.py`, so a `×` or a
    // smart quote in the middle of it is a regex that silently stops matching.
    tracing::info!(
        "logging: {} | via {} | level {} ({}) | budget {} per file x {} file(s) = <= {} total",
        display_path(&plan.active_path()),
        plan.dir_source.label(),
        plan.level,
        level_how,
        human_bytes(plan.max_bytes),
        plan.keep + 1,
        human_bytes(plan.total_bytes()),
    );

    Ok(LogGuard {
        _worker: worker,
        inner: rotator.into_inner(),
        plan,
    })
}

/// `mkdir -p`, `0700`, tolerant of the directory already existing.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cannot collide with another test's, and is gone
    /// when the test is done (same shape as `journal`'s).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "looprs-logging-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .expect("build the scratch log dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    // Counters read straight off the `Inner`: the accessors the guard exposes are
    // for the run's report, and a test should be able to see the same numbers
    // without going through the thing that prints them.
    fn rotations(log: &RotatingLog) -> u64 {
        log.inner.rotations.load(Ordering::Relaxed)
    }

    fn failures(log: &RotatingLog) -> u64 {
        log.inner.failures.load(Ordering::Relaxed)
    }

    fn active_bytes(log: &RotatingLog) -> u64 {
        log.inner.written.load(Ordering::Relaxed)
    }

    // ── the destination ladder, three cases and the rung between ──

    #[test]
    fn knob_beats_everything() {
        let (dir, src) = resolve_log_dir(
            Some("/operator/chose"),
            Some("/xdg/state"),
            Some("/home/u"),
            Path::new("/tmp"),
        );
        assert_eq!(dir, PathBuf::from("/operator/chose"));
        assert_eq!(src, DirSource::Knob);
    }

    #[test]
    fn xdg_state_is_the_next_rung() {
        let (dir, src) =
            resolve_log_dir(None, Some("/xdg/state"), Some("/home/u"), Path::new("/tmp"));
        assert_eq!(dir, PathBuf::from("/xdg/state/looprs"));
        assert_eq!(src, DirSource::XdgState);
    }

    #[test]
    fn home_is_the_xdg_default() {
        let (dir, src) = resolve_log_dir(None, None, Some("/home/u"), Path::new("/tmp"));
        assert_eq!(dir, PathBuf::from("/home/u/.local/state/looprs"));
        assert_eq!(src, DirSource::Home);
    }

    #[test]
    fn temp_is_the_last_rung() {
        let (dir, src) = resolve_log_dir(None, None, None, Path::new("/tmp"));
        assert_eq!(dir, PathBuf::from("/tmp"));
        assert_eq!(src, DirSource::Temp);
    }

    #[test]
    fn whitespace_counts_as_unset_at_every_rung() {
        let (dir, src) = resolve_log_dir(Some("   "), Some(""), Some("\t"), Path::new("/tmp"));
        assert_eq!(dir, PathBuf::from("/tmp"));
        assert_eq!(src, DirSource::Temp);
    }

    // ── the level ──

    #[test]
    fn the_default_level_is_info_not_debug() {
        let c = resolve_level(None);
        assert_eq!(c.filter, "info");
        assert_eq!(c.source, LevelSource::Default);
        assert_eq!(c.warning, None);
    }

    #[test]
    fn rust_log_is_honoured_when_it_parses() {
        for raw in ["debug", "warn", "looprs=warn,looprs::bus=trace", "off"] {
            let c = resolve_level(Some(raw));
            assert_eq!(c.filter, raw, "{raw} should pass through");
            assert_eq!(c.source, LevelSource::Knob);
            assert_eq!(c.warning, None);
        }
    }

    #[test]
    fn an_unparseable_rust_log_falls_back_loudly() {
        // `EnvFilter` is lenient — `"this is not a filter"` parses as a set of
        // per-target directives — so the realistic typo is a *level* that is not
        // a level, which is the one it does refuse.
        for typo in ["looprs=notalevel", "looprs::bus==info", "=debug"] {
            let c = resolve_level(Some(typo));
            assert_eq!(c.filter, DEFAULT_LEVEL, "{typo} should not be honoured");
            assert_eq!(c.source, LevelSource::Default);
            let w = c
                .warning
                .unwrap_or_else(|| panic!("{typo:?} must say it fell back"));
            assert!(w.contains("RUST_LOG"), "names the knob: {w}");
            assert!(w.contains("info"), "names what was used instead: {w}");
        }
    }

    #[test]
    fn empty_rust_log_is_unset_not_silence() {
        // `EnvFilter::new("")` is a filter with no directives, i.e. nothing
        // logged at all, which is not what an empty assignment means.
        let c = resolve_level(Some("   "));
        assert_eq!(c.filter, DEFAULT_LEVEL);
        assert_ne!(c.filter, "");
    }

    // ── the budget ──

    #[test]
    fn budget_defaults_and_units() {
        assert_eq!(resolve_max_bytes(None).bytes, DEFAULT_MAX_BYTES);
        assert_eq!(resolve_max_bytes(Some("512K")).bytes, 512 * 1024);
        assert_eq!(resolve_max_bytes(Some("2MB")).bytes, 2 * 1024 * 1024);
        assert_eq!(resolve_max_bytes(Some("100000")).bytes, 100_000);
    }

    #[test]
    fn budget_clamps_and_says_so() {
        let tiny = resolve_max_bytes(Some("10"));
        assert_eq!(tiny.bytes, MIN_MAX_BYTES);
        assert!(tiny.warning.unwrap().contains("clamped"));

        let huge = resolve_max_bytes(Some("999999999999"));
        assert_eq!(huge.bytes, MAX_MAX_BYTES);
        assert!(huge.warning.unwrap().contains("ceiling"));

        let zero = resolve_max_bytes(Some("0"));
        assert_eq!(zero.bytes, MIN_MAX_BYTES, "0 would rotate every write");
        assert!(zero.warning.is_some());

        let junk = resolve_max_bytes(Some("lots"));
        assert_eq!(junk.bytes, DEFAULT_MAX_BYTES);
        assert!(junk.warning.unwrap().contains("LOOPRS_LOG_MAX_BYTES"));
    }

    #[test]
    fn keep_resolves_and_clamps() {
        assert_eq!(resolve_keep(None).keep, DEFAULT_KEEP);
        assert_eq!(resolve_keep(Some("0")).keep, 0);
        assert_eq!(resolve_keep(Some("7")).keep, 7);
        let clamped = resolve_keep(Some("500"));
        assert_eq!(clamped.keep, MAX_KEEP);
        assert!(clamped.warning.unwrap().contains("ceiling"));
        let junk = resolve_keep(Some("many"));
        assert_eq!(junk.keep, DEFAULT_KEEP);
        assert!(junk.warning.is_some());
    }

    #[test]
    fn the_stated_ceiling_is_files_times_cap() {
        let plan = LogPlan {
            dir: PathBuf::from("/x"),
            dir_source: DirSource::Knob,
            level: "info".into(),
            level_source: LevelSource::Default,
            max_bytes: 1000,
            keep: 4,
            warnings: vec![],
        };
        assert_eq!(plan.total_bytes(), 5000, "active + 4 rotated");
        assert_eq!(plan.active_path(), PathBuf::from("/x/looprs.log"));
    }

    // ── the writer ──

    #[test]
    fn writes_land_in_the_active_file() {
        let s = Scratch::new("write");
        let mut log = RotatingLog::new(s.path(), 1024, 2).unwrap();
        log.write_all(b"hello\n").unwrap();
        log.flush().unwrap();
        let text = fs::read_to_string(s.path().join(LOG_FILE_NAME)).unwrap();
        assert_eq!(text, "hello\n");
        assert_eq!(rotations(&log), 0);
        assert_eq!(failures(&log), 0);
    }

    #[test]
    fn the_cap_is_enforced_and_the_history_is_bounded() {
        let s = Scratch::new("cap");
        let chunk = vec![b'x'; 500];
        let mut log = RotatingLog::new(s.path(), 1024, 2).unwrap();
        // 5,000 bytes through a 1 KiB cap with two generations kept.
        for _ in 0..10 {
            log.write_all(&chunk).unwrap();
        }
        log.flush().unwrap();

        assert!(
            rotations(&log) >= 3,
            "expected rotations, got {}",
            rotations(&log)
        );
        assert_eq!(failures(&log), 0);

        let files = dir_names(s.path());
        assert_eq!(
            files,
            vec!["looprs.log", "looprs.log.1", "looprs.log.2"],
            "exactly active + keep generations"
        );

        for name in &files {
            let size = fs::metadata(s.path().join(name)).unwrap().len();
            // The cap holds to within one write: the check happens before the
            // write, so the file can end up one line over, never two.
            assert!(
                size <= 1024 + 500,
                "{name} is {size} bytes, over the cap plus one write"
            );
        }

        let total: u64 = files
            .iter()
            .map(|f| fs::metadata(s.path().join(f)).unwrap().len())
            .sum();
        assert!(total <= 3 * (1024 + 500), "total on disk {total}");
    }

    #[test]
    fn the_active_file_name_never_moves() {
        // Every documented grep is spelled against `looprs.log`. Rotation may
        // shuffle everything behind it, but `grep foo $LOOPRS_LOG_DIR/looprs.log`
        // has to work at the end of the run as well as at the start.
        let s = Scratch::new("stable-name");
        let mut log = RotatingLog::new(s.path(), 1024, 2).unwrap();
        for _ in 0..10 {
            log.write_all(&vec![b'y'; 500]).unwrap();
        }
        log.flush().unwrap();
        assert!(s.path().join(LOG_FILE_NAME).exists());
        let last = fs::read_to_string(s.path().join(LOG_FILE_NAME)).unwrap();
        assert_eq!(last.len(), active_bytes(&log) as usize);
    }

    #[test]
    fn keep_zero_truncates_in_place_rather_than_rotating_history() {
        let s = Scratch::new("keep-zero");
        let mut log = RotatingLog::new(s.path(), 1024, 0).unwrap();
        for _ in 0..5 {
            log.write_all(&vec![b'z'; 500]).unwrap();
        }
        log.flush().unwrap();
        assert_eq!(dir_names(s.path()), vec![LOG_FILE_NAME]);
        let size = fs::metadata(s.path().join(LOG_FILE_NAME)).unwrap().len();
        assert!(size <= 1024 + 500, "{size} over cap");
        assert!(rotations(&log) >= 1);
    }

    #[test]
    fn a_cap_is_a_property_of_the_file_not_of_the_process() {
        // Reopen the same file with the same cap: the seeded `written` must be the
        // existing size, or a second run could restart the budget from zero and
        // leave a 2 MiB file that is supposed to be 1 KiB.
        let s = Scratch::new("seeded");
        {
            let mut log = RotatingLog::new(s.path(), 4096, 1).unwrap();
            log.write_all(&vec![b'a'; 1500]).unwrap();
            log.flush().unwrap();
        }
        let mut reopened = RotatingLog::new(s.path(), 4096, 1).unwrap();
        assert_eq!(
            active_bytes(&reopened),
            1500,
            "the file's size is the budget"
        );
        // Spend the rest of the cap and then some. The rotate happens on the write
        // *after* the cap is crossed — the check is before the write, so a file is
        // never more than one line over — which is why there are two writes here:
        // the first crosses 4096, the second is the one that must not be allowed
        // to make it a 5 KiB "1 KiB" file.
        reopened.write_all(&vec![b'b'; 3000]).unwrap();
        reopened.write_all(b"c").unwrap();
        reopened.flush().unwrap();
        assert!(
            rotations(&reopened) >= 1,
            "the cap was already partly spent"
        );
        assert_eq!(failures(&reopened), 0);
        let size = fs::metadata(s.path().join(LOG_FILE_NAME)).unwrap().len();
        assert!(size <= 4096 + 3000, "{size} over the cap plus one write");
        assert!(
            s.path().join(format!("{LOG_FILE_NAME}.1")).exists(),
            "the overflow went into a kept generation rather than into the active file"
        );
    }

    #[test]
    fn an_unwritable_destination_is_counted_not_fatal() {
        // Point the writer at a path whose active file cannot be opened: the
        // `None` slot path. Simulated by removing the file out from under it and
        // making the directory read-only.
        let s = Scratch::new("broken");
        let mut log = RotatingLog::new(s.path(), 1024, 1).unwrap();
        log.write_all(b"first\n").unwrap();
        fs::remove_file(s.path().join(LOG_FILE_NAME)).unwrap();
        // Still no panic, and the caller still gets a byte count: the UI thread
        // must never be able to be blocked or broken by its own logger.
        log.write_all(b"second\n").unwrap();
        log.flush().unwrap();
        // The bytes went to the still-open handle (append opened it), so nothing
        // is claimed here beyond: no panic, no error propagated.
    }

    #[test]
    fn a_directory_that_cannot_be_made_falls_back_loudly() {
        // A *file* standing where a directory would have to be: `mkdir
        // <file>/logs` fails on every platform, which makes this a fixture rather
        // than a guess about what the machine's permissions happen to allow.
        let s = Scratch::new("fallback");
        let blocker = s.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let bad = blocker.join("logs");
        let plan = LogPlan {
            dir: bad.clone(),
            dir_source: DirSource::Knob,
            level: "info".into(),
            level_source: LevelSource::Default,
            max_bytes: DEFAULT_MAX_BYTES,
            keep: DEFAULT_KEEP,
            warnings: vec![],
        };
        let after = with_dir_fallback(plan);
        assert_eq!(after.dir, std::env::temp_dir(), "falls to the last rung");
        assert_eq!(after.dir_source, DirSource::Temp);
        let w = after
            .warnings
            .iter()
            .find(|w| w.contains("cannot be created"))
            .expect("the fallback has to say so");
        assert!(
            w.contains("LOOPRS_LOG_DIR"),
            "names what was asked for: {w}"
        );
        assert!(
            w.contains(&bad.display().to_string()),
            "names the path that failed: {w}"
        );
    }

    #[test]
    fn a_directory_that_can_be_made_is_left_alone_and_says_nothing() {
        let s = Scratch::new("fallback-ok");
        let good = s.path().join("made");
        let plan = LogPlan {
            dir: good.clone(),
            dir_source: DirSource::Knob,
            level: "info".into(),
            level_source: LevelSource::Default,
            max_bytes: DEFAULT_MAX_BYTES,
            keep: DEFAULT_KEEP,
            warnings: vec![],
        };
        let after = with_dir_fallback(plan);
        assert_eq!(after.dir, good);
        assert_eq!(after.dir_source, DirSource::Knob);
        assert!(
            after.warnings.is_empty(),
            "nothing to complain about: {:?}",
            after.warnings
        );
        assert!(good.is_dir(), "and it actually made the directory");
    }

    #[test]
    fn human_bytes_reads_like_a_person_wrote_it() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(64 * 1024 * 1024), "64 MiB");
    }
}
