//! Test-only fake `pi` / `bd` executables.
//!
//! These are *real* subprocesses driven by real pipes, which is the point: the tests
//! then assert process-level truth — was a child spawned at all, was it prompted, was
//! the previous one reaped — instead of trusting a mock that never had a pid to begin with.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `bd ready --json` with nothing to work on.
pub const EMPTY_BOARD: &str = r#"{"data":[],"schema_version":1}"#;

/// `bd ready --json` with a single ready bead.
pub const ONE_BEADED_BOARD: &str = r#"{
  "data": [
    {"id": "looprs-26r", "title": "Beads loop never self-starts", "status": "open", "issue_type": "bug"}
  ],
  "schema_version": 1
}"#;

/// How the fake `pi` behaves when it is started / prompted.
#[derive(Clone, Copy, Debug)]
pub enum PiFake {
    /// Answers a prompt with `success:true, disposition:"started"` and stays alive.
    Started,
    /// Answers a prompt with `disposition:"handled"`: pi took it, started no run.
    Handled,
    /// Answers a prompt with `success:false`.
    Rejects,
    /// Exits immediately, so the pipes close before anything is answered.
    DiesImmediately,
}

/// How the fake `bd` behaves.
#[derive(Clone, Copy, Debug)]
pub enum BdFake {
    /// Prints the current board JSON and exits 0.
    Ok,
    /// Prints nothing and exits 3.
    Fails,
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// A scratch dir holding the fake binaries plus their recording logs.
/// Removing the dir on drop keeps test runs from leaving debris.
pub struct Fakes {
    dir: PathBuf,
    pi_bin: PathBuf,
    bd_bin: PathBuf,
    pi_log: PathBuf,
    bd_log: PathBuf,
    board_file: PathBuf,
}

impl Fakes {
    pub fn new(tag: &str, pi: PiFake, bd: BdFake, board: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("looprs-fakes-{}-{}-{}", tag, std::process::id(), n));
        // A previous crashed run may have left the dir behind; start clean.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let pi_log = dir.join("pi.log");
        let bd_log = dir.join("bd.log");
        let board_file = dir.join("board.json");
        let pi_bin = dir.join("pi");
        let bd_bin = dir.join("bd");

        write_script(&pi_bin, &pi_script(&pi_log, pi));
        write_script(&bd_bin, &bd_script(&bd_log, &board_file, bd));
        std::fs::write(&board_file, board).unwrap();

        Self {
            dir,
            pi_bin,
            bd_bin,
            pi_log,
            bd_log,
            board_file,
        }
    }

    /// Point the loop's fakes at a different board without restarting them.
    pub fn set_board(&self, board: &str) {
        std::fs::write(&self.board_file, board).unwrap();
    }

    pub fn pi_bin(&self) -> &str {
        self.pi_bin.to_str().unwrap()
    }

    pub fn bd_bin(&self) -> &str {
        self.bd_bin.to_str().unwrap()
    }

    fn read(&self, path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Every fake `pi` process that was spawned, in order.
    pub fn pi_pids(&self) -> Vec<u32> {
        self.read(&self.pi_log)
            .lines()
            .filter_map(|l| l.strip_prefix("spawn pid="))
            .filter_map(|l| l.split(' ').next()?.parse().ok())
            .collect()
    }

    pub fn pi_spawns(&self) -> usize {
        self.pi_pids().len()
    }

    /// Every prompt command line a fake `pi` actually received.
    pub fn pi_prompts(&self) -> Vec<String> {
        self.read(&self.pi_log)
            .lines()
            .filter_map(|l| l.strip_prefix("prompt ").map(str::to_string))
            .collect()
    }

    pub fn bd_calls(&self) -> usize {
        self.read(&self.bd_log)
            .lines()
            .filter(|l| l.starts_with("bd "))
            .count()
    }
}

impl Drop for Fakes {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_script(path: &Path, body: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    f.flush().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn pi_script(log: &Path, mode: PiFake) -> String {
    let log = log.display();
    let head = format!("#!/usr/bin/env bash\nset -u\nLOG={log}\n");
    match mode {
        PiFake::DiesImmediately => format!("{head}echo \"spawn pid=$$\" >>\"$LOG\"\nexit 1\n"),
        PiFake::Started | PiFake::Handled | PiFake::Rejects => {
            let reply = match mode {
                PiFake::Started => {
                    "printf '{\"type\":\"response\",\"id\":\"%s\",\"command\":\"prompt\",\"success\":true,\"data\":{\"disposition\":\"started\"}}\\n' \"$id\""
                }
                PiFake::Handled => {
                    "printf '{\"type\":\"response\",\"id\":\"%s\",\"command\":\"prompt\",\"success\":true,\"data\":{\"disposition\":\"handled\"}}\\n' \"$id\""
                }
                _ => {
                    "printf '{\"type\":\"response\",\"id\":\"%s\",\"command\":\"prompt\",\"success\":false,\"error\":\"fake pi refused the prompt\"}\\n' \"$id\""
                }
            };
            format!(
                r#"{head}echo "spawn pid=$$ args=$*" >>"$LOG"
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      echo "prompt $line" >>"$LOG"
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      {reply}
      ;;
    *)
      echo "cmd $line" >>"$LOG"
      ;;
  esac
done
"#
            )
        }
    }
}

fn bd_script(log: &Path, board: &Path, mode: BdFake) -> String {
    let tail = match mode {
        BdFake::Ok => format!("cat {}\n", board.display()),
        BdFake::Fails => "exit 3\n".to_string(),
    };
    format!(
        "#!/usr/bin/env bash\necho \"bd $*\" >>{log}\n{tail}",
        log = log.display()
    )
}

/// Is this pid still a live process (not a reaped one)?
pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------------
// Fakes for the Router: a `Session` that records instead of spawning.
//
// The repo's test style is process-level (see the module docs), and the Beads backend
// keeps that: `session::beads::tests` asserts on real pids. But the Router's rules —
// one session per mode, park/resume, "a replaced generation has no route to the UI",
// "a submit that lost the race is dropped" — are lifecycle rules, and asserting them
// through three half-built child processes would test the fakes rather than the
// Router. So these record what was asked, in order, and let a test hold the event
// sender of a specific generation so it can make a *dead* one try to speak.
// ---------------------------------------------------------------------------------

use crate::session::{
    Session, SessionEvent, SessionFactory, SessionId, SessionStatus, Spawned, TerminalType,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Default)]
struct FakeInner {
    log: Vec<String>,
    senders: HashMap<SessionId, mpsc::UnboundedSender<SessionEvent>>,
    status: HashMap<SessionId, SessionStatus>,
    /// When true, `shutdown()` is recorded but the event stream is never closed,
    /// so the owning pump never finishes: a wedged session.
    silent_exit: bool,
}

/// A recorder shared by every fake session one test creates.
#[derive(Clone, Default)]
pub struct FakeBackend {
    inner: Arc<Mutex<FakeInner>>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// `shutdown()` records but never ends the stream — the wedged-child case.
    pub fn with_silent_exit(self) -> Self {
        self.inner.lock().unwrap().silent_exit = true;
        self
    }

    /// A factory that builds fakes for whatever mode the Router asks for.
    pub fn factory(&self) -> SessionFactory {
        self.factory_claiming(None)
    }

    /// As [`FakeBackend::factory`], but every session claims `Some(mode)` no matter
    /// what was requested — for the "a lying factory is refused" test.
    pub fn factory_claiming(&self, claimed: Option<TerminalType>) -> SessionFactory {
        let me = self.clone();
        Arc::new(move |mode: TerminalType, generation: u64| {
            let id = SessionId::new(claimed.unwrap_or(mode), generation);
            let (tx, rx) = mpsc::unbounded_channel::<SessionEvent>();
            {
                let mut st = me.inner.lock().unwrap();
                st.log
                    .push(format!("spawn {} #{generation}", id.mode.label()));
                st.senders.insert(id, tx.clone());
                st.status.entry(id).or_insert(SessionStatus::Idle);
            }
            Ok(Spawned {
                session: Box::new(FakeSession {
                    id,
                    backend: me.clone(),
                    events: tx,
                }),
                events: rx,
            })
        })
    }

    pub fn log(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.clone()
    }

    pub fn clear_log(&self) {
        self.inner.lock().unwrap().log.clear();
    }

    pub fn calls(&self, verb: &str) -> usize {
        self.log().iter().filter(|c| c.starts_with(verb)).count()
    }

    pub fn was_called(&self, verb: &str) -> bool {
        self.calls(verb) > 0
    }

    pub fn spawn_count(&self, mode: TerminalType) -> usize {
        self.log()
            .iter()
            .filter(|c| c.starts_with(&format!("spawn {}", mode.label())))
            .count()
    }

    /// The event sender for one specific generation, so a test can make a *dead*
    /// generation try to reach the UI.
    pub fn events(&self, id: SessionId) -> mpsc::UnboundedSender<SessionEvent> {
        self.inner
            .lock()
            .unwrap()
            .senders
            .get(&id)
            .cloned()
            .unwrap_or_else(|| panic!("no fake session {id} was created"))
    }

    pub fn set_status(&self, id: SessionId, status: SessionStatus) {
        self.inner.lock().unwrap().status.insert(id, status);
    }
}

/// A [`Session`] that does nothing but say what it was asked to do.
struct FakeSession {
    id: SessionId,
    backend: FakeBackend,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl Session for FakeSession {
    fn id(&self) -> SessionId {
        self.id
    }

    fn send_text(&mut self, text: String) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "send_text {} #{}: {text}",
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn abort(&mut self) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "abort {} #{}",
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        let silent = {
            let mut st = self.backend.inner.lock().unwrap();
            st.log.push(format!(
                "shutdown {} #{}",
                self.id.mode.label(),
                self.id.generation
            ));
            st.silent_exit
        };
        if !silent {
            // An orderly goodbye: the stream ends, so the pump completes.
            let _ = self.events.send(SessionEvent::Exited {
                reason: crate::session::ExitReason::Shutdown,
            });
        }
        Ok(())
    }

    fn advance(&mut self) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "advance {} #{}",
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> anyhow::Result<()> {
        self.backend.inner.lock().unwrap().log.push(format!(
            "set_active {} {} #{}",
            if active { "true" } else { "false" },
            self.id.mode.label(),
            self.id.generation
        ));
        Ok(())
    }

    fn status(&self) -> SessionStatus {
        self.backend
            .inner
            .lock()
            .unwrap()
            .status
            .get(&self.id)
            .copied()
            .unwrap_or(SessionStatus::Idle)
    }
}

/// Standalone factory that always produces `claimed`, ignoring the requested mode.
pub fn fake(claimed: TerminalType) -> SessionFactory {
    FakeBackend::new().factory_claiming(Some(claimed))
}
