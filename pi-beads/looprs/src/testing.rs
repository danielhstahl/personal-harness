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
