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
    /// A stateful chat child: streams a turn, remembers the conversation it was
    /// given, and holds each run open until [`Fakes::settle`] (see
    /// [`chat_pi_script`]). This is the one the Pi terminal state tests drive.
    Chat,
}

/// How the fake `bd` behaves.
///
/// The read verbs (`ready`, `list`, `show`) answer from files the test can rewrite
/// mid-run — that is what makes "the board changed under the loop" and "the worker
/// settled but never closed the bead" testable at all.
#[derive(Clone, Copy, Debug)]
pub enum BdFake {
    /// Prints the current board JSON and exits 0.
    Ok,
    /// Prints nothing and exits 3.
    Fails,
    /// Exits 0 with bytes that are not JSON at all.
    Malformed,
    /// Exits 0 printing nothing (the sneaky one: "empty output" must not read as
    /// "empty board").
    EmptyOutput,
    /// Like [`BdFake::Ok`], plus `bd show <id> --json` answers from `show.json`
    /// (set with [`Fakes::set_show`]) — the read the claim guard is built on.
    ShowStatus,
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
    show_file: PathBuf,
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
        let show_file = dir.join("show.json");
        let pi_bin = dir.join("pi");
        let bd_bin = dir.join("bd");

        let script = match pi {
            PiFake::Chat => chat_pi_script(),
            _ => pi_script(&pi_log, pi),
        };
        write_script(&pi_bin, &script);
        write_script(
            &bd_bin,
            &bd_script(&bd_log, &board_file, &show_file, &dir.join("fail"), bd),
        );
        std::fs::write(&board_file, board).unwrap();
        // No bead is known to `bd show` until a test says otherwise: an unset
        // show file reads as "no such bead", which is the safe default.
        std::fs::write(&show_file, "[]").unwrap();

        Self {
            dir,
            pi_bin,
            bd_bin,
            pi_log,
            bd_log,
            board_file,
            show_file,
        }
    }

    /// Point the loop's fakes at a different board without restarting them.
    pub fn set_board(&self, board: &str) {
        std::fs::write(&self.board_file, board).unwrap();
    }

    /// Make every *subsequent* fake `bd` call fail (exit 3), without restarting it.
    ///
    /// The lever for "the board was readable when the pass started and is not now".
    /// That distinction is not expressible with a `BdFake` personality, which is
    /// fixed at construction, and it is exactly what the planner verification
    /// (looprs-k7v) has to survive: a read that fails halfway through a pass must
    /// read as "unverifiable", never as "the board is empty".
    pub fn fail_bd(&self, on: bool) {
        let mark = self.dir.join("fail");
        if on {
            std::fs::write(&mark, b"").unwrap();
        } else {
            let _ = std::fs::remove_file(&mark);
        }
    }

    /// What `bd show <id> --json` answers: a single bead object, or `[]`.
    ///
    /// This is the fake's lever for "the worker settled but never closed the bead",
    /// which is the whole looprs-w7q hazard and is not expressible through the
    /// `ready` board alone.
    pub fn set_show(&self, bead: &str) {
        std::fs::write(&self.show_file, bead).unwrap();
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

    /// Every command the fake `pi` received, in the order it received it, as
    /// `"<verb> <rest>"` lines. Order is the whole point of several tests: Esc has
    /// to `clear_queue` *before* it `abort`s, a follow-up has to arrive as
    /// `steer` and not as a second `prompt`.
    pub fn pi_commands(&self) -> Vec<String> {
        self.read(&self.pi_log)
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("recv ")?;
                Some(rest.to_string())
            })
            .collect()
    }

    /// Just the verbs, in order: `["prompt", "steer", "clear_queue", "abort"]`.
    pub fn pi_verbs(&self) -> Vec<String> {
        self.pi_commands()
            .iter()
            .map(|c| c.split(' ').next().unwrap_or("").to_string())
            .collect()
    }

    /// Touch the trigger that lets the chat fake finish the run it is holding open.
    ///
    /// Without this there is no way for a test to do anything *during* a run, and
    /// "during a run" is where steer, Esc and crash recovery all live.
    pub fn settle(&self) {
        std::fs::write(self.dir.join("settle"), b"").unwrap();
    }

    /// Wait until the fake `pi` has written `needle` at the start of a log line.
    /// Polls, because a test that sleeps is a test that is wrong half the time.
    pub async fn wait_for_log_line(&self, needle: &str) {
        for _ in 0..400 {
            if self
                .read(&self.pi_log)
                .lines()
                .any(|l| l.starts_with(needle) || l.contains(&format!("recv {needle}")))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "fake pi never logged {needle:?}; log was:\n{}",
            self.read(&self.pi_log)
        );
    }

    /// Wait until the fake `pi` has been spawned at least `n` times.
    ///
    /// Same polling rationale as [`Fakes::wait_for_log_line`]: a test that sleeps a
    /// fixed amount is a test that fails on a slow machine and passes for the wrong
    /// reason on a fast one.
    pub async fn wait_for_pi_spawns(&self, n: usize) {
        for _ in 0..800 {
            if self.pi_spawns() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "only {} fake pi spawn(s) after the wait, wanted {n}; log was:\n{}",
            self.pi_spawns(),
            self.read(&self.pi_log)
        );
    }

    pub fn bd_calls(&self) -> usize {
        self.read(&self.bd_log)
            .lines()
            .filter(|l| l.starts_with("bd "))
            .count()
    }

    /// Every `bd` command line the fake received, in order.
    pub fn bd_log(&self) -> Vec<String> {
        self.read(&self.bd_log)
            .lines()
            .filter_map(|l| l.strip_prefix("bd ").map(str::to_string))
            .collect()
    }

    /// Did the harness claim this bead, with the command it was supposed to use?
    pub fn claimed(&self, id: &str) -> bool {
        self.bd_log()
            .iter()
            .any(|l| l.contains(&format!("update {id} --claim")))
    }

    /// How many times this exact `bd` command line was run (for "the board is
    /// queried once per decision, not once per question" style assertions).
    pub fn bd_call_count(&self, cmd: &str) -> usize {
        self.bd_log().iter().filter(|l| *l == cmd).count()
    }
}

impl Drop for Fakes {
    fn drop(&mut self) {
        // Kill every child we recorded before deleting the scratch dir. A fake that
        // outlives its test is a fake that turns up in somebody else's process
        // count assertion, and the chat fake holds runs open on purpose.
        for pid in self.pi_pids() {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
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
        // The chat fake is a different program entirely (see `chat_pi_script`);
        // `Fakes::new` never routes here for it.
        PiFake::Chat => unreachable!("the chat fake is built by chat_pi_script"),
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

/// A stateful fake `pi --mode rpc`, in python because it needs real concurrency:
/// it must keep reading commands *while* a run is in flight, which is exactly when
/// a test wants to `steer`, `Esc`, or `kill -9` it.
///
/// Three properties the Pi terminal state tests depend on, and nothing else:
///
/// * **It remembers.** Every prompt lands in this process's own memory, and the
///   answer to turn N quotes turns 1..N-1. A client that spawned a child per
///   message would get an empty `<memory=>` on turn 2, which is the persistence
///   claim under test, falsifiable.
/// * **A run is held open** until the test touches `settle` next to this script,
///   so the test can act *during* a run rather than after it.
/// * **`abort` stops the held run** and settles it, the way pi unwinds.
///
/// It logs one line per command (`recv <verb> <args>`) so command *order* is
/// assertable — `clear_queue` before `abort` is not a detail.
fn chat_pi_script() -> String {
    r###"#!/usr/bin/env python3
import json, os, sys, threading, time

HERE = os.path.dirname(os.path.realpath(__file__))
LOG = os.path.join(HERE, "pi.log")
SETTLE = os.path.join(HERE, "settle")
# A run never stays open forever: a test that forgets to settle is a hung test.
MAX_HOLD = float(os.environ.get("LOOPRS_FAKE_HOLD", "20"))

out_lock = threading.Lock()
log_lock = threading.Lock()
st_lock = threading.Lock()

memory = []        # prompts this process has seen == its only context
queued = []        # steering / follow-up text, in queue order
state = {"turns": 0}


def log(msg):
    with log_lock:
        with open(LOG, "a") as f:
            f.write(msg + "\n")


def one_line(s):
    """Flatten a message so one command is one log line.

    `Fakes::pi_prompts()` reads this log *by line*. A real planner prompt is a
    multi-line block of instructions, and a raw newline through this path splits
    `"prompt " + msg` into a `"prompt "` line and an orphan — which makes every
    assertion about what the harness actually sent to the planner quietly useless.
    Only newlines are escaped, so plain substring assertions still match.
    """
    return s.replace("\r", " ").replace("\n", "\\n")


def emit(obj):
    with out_lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def response(rid, command, data=None, success=True, error=None):
    rec = {"type": "response", "command": command, "success": success}
    if rid is not None:
        rec["id"] = rid
    if data is not None:
        rec["data"] = data
    if error is not None:
        rec["error"] = error
    emit(rec)


def run_turn(n, user_text, answer, aborted):
    """Stream one assistant turn the way pi does, then hold it open."""
    emit({"type": "agent_start"})
    emit({"type": "turn_start"})
    emit({"type": "message_start", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_end", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
    emit({"type": "message_update", "assistantMessageEvent":
          {"type": "text_delta", "contentIndex": 0, "delta": answer}})
    emit({"type": "message_update", "assistantMessageEvent":
          {"type": "text_end", "contentIndex": 0, "content": answer}})
    emit({"type": "message_end", "message": {"role": "assistant", "content": []}})
    emit({"type": "turn_end", "message": {"role": "assistant"}, "toolResults": []})

    deadline = time.time() + MAX_HOLD
    while time.time() < deadline:
        if aborted["hit"]:
            break
        if os.path.exists(SETTLE):
            try:
                os.remove(SETTLE)
            except OSError:
                pass
            break
        time.sleep(0.01)

    emit({"type": "agent_end", "messages": [], "willRetry": False})
    emit({"type": "agent_settled"})
    log("aborted turn %d" % n if aborted["hit"] else "settled turn %d" % n)


def main():
    log("spawn pid=%d args=%s" % (os.getpid(), " ".join(sys.argv[1:])))
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            cmd = json.loads(line)
        except ValueError:
            continue
        kind = cmd.get("type")
        rid = cmd.get("id")
        msg = cmd.get("message", "")
        log("recv %s %s" % (kind, one_line(msg)))

        if kind == "prompt":
            # Also logged in the pre-existing "prompt <text>" shape so the shared
            # `pi_prompts()` reader works for both fakes.
            log("prompt " + one_line(msg))
            with st_lock:
                prior = list(memory)
                memory.append(msg)
                state["turns"] += 1
                n = state["turns"]
            # The answer's memory is the whole trick: it can only be non-empty if
            # the same child process that heard turn 1 is still here.
            answer = "reply %d <memory=%s>" % (n, "|".join(prior))
            response(rid, "prompt", {"disposition": "started"})
            aborted = {"hit": False}
            t = threading.Thread(target=run_turn, args=(n, msg, answer, aborted), daemon=True)
            t.start()
            live.append(aborted)
        elif kind == "steer":
            with st_lock:
                queued.append(msg)
            response(rid, "steer", {"disposition": "queued"})
        elif kind == "follow_up":
            with st_lock:
                queued.append(msg)
            response(rid, "follow_up", {"disposition": "queued"})
        elif kind == "clear_queue":
            with st_lock:
                taken = list(queued)
                del queued[:]
            response(rid, "clear_queue", {"steering": taken, "followUp": []})
        elif kind == "abort":
            # Tell every open run to unwind, then answer like pi does: abort waits
            # for the session to become idle.
            for a in list(live):
                a["hit"] = True
            del live[:]
            response(rid, "abort")
        elif kind == "get_state":
            response(rid, "get_state", {"isStreaming": len(live) > 0,
                                        "messageCount": len(memory)})
        else:
            response(rid, kind, success=False,
                     error="fake pi: unknown command %r" % kind)


live = []
main()
"###
    .to_string()
}

/// The fake `bd`: records every command line, then answers each verb from a file
/// the test can rewrite.
///
/// Written in bash and file-driven for the same reason the rest of the fakes are:
/// the assertions are about what the *harness* asked for, in what order, and a
/// mock that only supports one verb cannot answer "did it claim before prompting?"
fn bd_script(log: &Path, board: &Path, show: &Path, fail_mark: &Path, mode: BdFake) -> String {
    // (read verb, `bd show`, write verb) per personality.
    let (read_cmd, show_cmd, write_cmd): (String, String, String) = match mode {
        BdFake::Fails => ("exit 3".into(), "exit 3".into(), "exit 3".into()),
        BdFake::Malformed => (
            "printf 'not json at all\\n'".into(),
            "printf 'not json at all\\n'".into(),
            "exit 0".into(),
        ),
        BdFake::EmptyOutput => ("true".into(), "true".into(), "exit 0".into()),
        BdFake::Ok => (
            format!("cat {}", board.display()),
            "printf '[]\\n'".into(),
            "exit 0".into(),
        ),
        BdFake::ShowStatus => (
            format!("cat {}", board.display()),
            format!("cat {}", show.display()),
            "exit 0".into(),
        ),
    };
    format!(
        r#"#!/usr/bin/env bash
set -u
echo "bd $*" >>"{log}"
# Mid-run failure lever: see `Fakes::fail_bd`. Checked per invocation, so a test
# can flip the board's health between two reads of the same pass.
if [ -e "{fail_mark}" ]; then
  echo "fake bd: failing on request (fail marker set)" >&2
  exit 3
fi
verb="${{1:-}}"
case "$verb" in
  ready|list) {read_cmd} ;;
  show) {show_cmd} ;;
  update|create|close) {write_cmd} ;;
  *) echo "fake bd: unsupported verb $verb" >&2; exit 2 ;;
esac
"#,
        log = log.display(),
        fail_mark = fail_mark.display(),
        read_cmd = read_cmd,
        show_cmd = show_cmd,
        write_cmd = write_cmd,
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

/// Stop a process dead, from outside. This is the ticket's "kill the pi child out
/// from under the app": the app is not cooperating, and the child just goes away.
pub fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
