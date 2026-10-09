//! ADR-0001 spike: how does the Bash terminal state get a shell?
//!
//! Two options, one probe script ([`spikes/probes.sh`]), so the ONLY difference
//! between a `pipes` run and a `pty` run is the ~20 lines marked below as
//! "Option A" / "Option B". Everything after the spawn is shared.
//!
//! ```sh
//! # Run both under a real terminal (`script` allocates one) so the /dev/tty,
//! # vim and resize probes are meaningful:
//! script -q /dev/null cargo run -q --example spike_bash -- pipes  | tee spikes/results/pipes.log
//! script -q /dev/null cargo run -q --example spike_bash -- pty    | tee spikes/results/pty.log
//! ```
//!
//! Nothing in here is production code; it exists to prove or disprove the risky
//! claims in the ADR.

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const PROBE_SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/spikes/probes.sh");
/// No new bytes for this long (after every step has been written) => run is over.
const QUIET: Duration = Duration::from_secs(6);
/// Hard stop, so a wedged child cannot wedge the spike.
const MAX_RUN: Duration = Duration::from_secs(90);

/// What we push into the child. `Line` is a normal command; `Raw` carries bytes a
/// terminal would send (Ctrl-C, Esc) that must not get a newline glued on.
#[derive(Debug)]
enum Step {
    Line(Vec<u8>),
    Raw(Vec<u8>),
    Wait(u64),
    Resize(u16, u16),
}

/// The child, whichever way we made it.
enum Killer {
    Pipes(Child),
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

struct Shell {
    stdin: Box<dyn Write + Send>,
    rx: Option<Receiver<Vec<u8>>>,
    /// PTY only: kept so the spike can prove resize propagates.
    master: Option<Box<dyn MasterPty + Send>>,
    killer: Killer,
}

impl Drop for Shell {
    /// A spike that leaks a bash child into the user's session is a bad spike.
    fn drop(&mut self) {
        match &mut self.killer {
            Killer::Pipes(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            Killer::Pty(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl Shell {
    fn send(&mut self, bytes: &[u8]) {
        let _ = self.stdin.write_all(bytes);
        let _ = self.stdin.flush();
    }

    fn resize(&mut self, rows: u16, cols: u16) -> &'static str {
        match &mut self.master {
            Some(m) => match m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                Ok(()) => "resize sent to pty",
                Err(e) => {
                    println!("NOTE resize failed: {e}");
                    "resize errored"
                }
            },
            None => "no pty to resize (Option A has nothing to resize)",
        }
    }
}

/// Copy a reader into the shared channel on its own thread.
fn pump<R: Read + Send + 'static>(mut r: R, tx: Sender<Vec<u8>>) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

fn term_env() -> String {
    env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string())
}

/// OPTION A -- `bash -i` with piped stdin/stdout/stderr. No new crate.
fn spawn_pipes(bin: &str) -> Shell {
    let mut cmd = Command::new(bin);
    cmd.arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TERM", term_env());
    let mut child = cmd.spawn().expect("spawn bash -i (pipes)");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    // Two readers, one channel: whoever wakes up first writes first. Ordering of
    // stdout vs stderr is a scheduling race, not the program's order.
    pump(stdout, tx.clone());
    pump(stderr, tx);
    Shell {
        stdin: Box::new(stdin),
        rx: Some(rx),
        master: None,
        killer: Killer::Pipes(child),
    }
}

/// OPTION B -- `bash -i` inside a real pty (portable-pty 0.9).
fn spawn_pty(bin: &str) -> Shell {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut cmd = CommandBuilder::new(bin);
    cmd.arg("-i");
    cmd.env("TERM", term_env());
    let child = pair.slave.spawn_command(cmd).expect("spawn bash -i (pty)");
    // Drop the slave so the master sees EOF when the child exits.
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().expect("pty reader");
    let writer = pair.master.take_writer().expect("pty writer");
    let (tx, rx) = std::sync::mpsc::channel();
    // One stream: stdout and stderr are the SAME tty, so ordering is the program's.
    pump(reader, tx);
    Shell {
        stdin: Box::new(writer),
        rx: Some(rx),
        master: Some(pair.master),
        killer: Killer::Pty(child),
    }
}

/// Decode `\xNN`, `\r`, `\n`, `\t`, `\\` in `#raw` lines.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'x' if i + 3 < b.len() => {
                    out.push(u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap_or(b'?'));
                    i += 4;
                    continue;
                }
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'\\' => out.push(b'\\'),
                c => out.push(c),
            }
            i += 2;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn steps() -> Vec<Step> {
    let path = env::var("LOOPRS_SPIKE_PROBES").unwrap_or_else(|_| PROBE_SCRIPT.to_string());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing {path}: {e}"));
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(a) = t.strip_prefix("#wait") {
            out.push(Step::Wait(a.trim().parse().unwrap_or(0)));
        } else if let Some(a) = t.strip_prefix("#raw") {
            out.push(Step::Raw(unescape(a.trim())));
        } else if let Some(a) = t.strip_prefix("#resize") {
            let mut it = a.split_whitespace();
            let rows = it.next().and_then(|v| v.parse().ok()).unwrap_or(24);
            let cols = it.next().and_then(|v| v.parse().ok()).unwrap_or(80);
            out.push(Step::Resize(rows, cols));
        } else if t.starts_with('#') {
            continue; // plain comment
        } else {
            out.push(Step::Line(format!("{line}\n").into_bytes()));
        }
    }
    out
}

#[derive(Default)]
struct Trace {
    raw: Vec<u8>,
    lines: Vec<(u128, String)>,
    leftover: Vec<u8>,
}

fn collect(rx: Receiver<Vec<u8>>, trace: Arc<Mutex<Trace>>, t0: Instant, sent: Arc<AtomicBool>) {
    loop {
        if t0.elapsed() > MAX_RUN {
            println!("NOTE MAX_RUN exceeded, stopping collection");
            break;
        }
        // Only start the "stream went idle" clock once every step has been written;
        // otherwise a slow command (sudo, vim) looks like the end of the run.
        let idle = if sent.load(Ordering::Relaxed) {
            QUIET
        } else {
            MAX_RUN
        };
        match rx.recv_timeout(idle) {
            Ok(chunk) => {
                let ms = t0.elapsed().as_millis();
                let mut tr = trace.lock().unwrap();
                tr.raw.extend_from_slice(&chunk);
                tr.leftover.extend_from_slice(&chunk);
                while let Some(pos) = tr.leftover.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = tr.leftover.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line).replace('\r', "");
                    let line = line.trim_end_matches('\n').to_string();
                    if line.trim_start().starts_with("PROBE done") {
                        return; // the probe script says it is finished
                    }
                    tr.lines.push((ms, line));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn marker(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn run(label: &str, mut shell: Shell) {
    println!("=== {label} ===");
    println!(
        "bash bin: {}",
        env::var("LOOPRS_SPIKE_BASH").unwrap_or_else(|_| "/bin/bash".into())
    );
    let trace = Arc::new(Mutex::new(Trace::default()));
    let rx = shell.rx.take().expect("receiver");
    let tr = Arc::clone(&trace);
    let sent = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let collector = thread::spawn({
        let sent = Arc::clone(&sent);
        move || collect(rx, tr, t0, sent)
    });

    for s in steps() {
        match s {
            Step::Line(b) => shell.send(&b),
            Step::Raw(b) => {
                println!(
                    "NOTE[{:>5}ms] harness writes raw bytes {:?}",
                    t0.elapsed().as_millis(),
                    String::from_utf8_lossy(&b)
                );
                shell.send(&b)
            }
            Step::Wait(ms) => thread::sleep(Duration::from_millis(ms)),
            Step::Resize(r, c) => println!(
                "NOTE[{:>5}ms] {}",
                t0.elapsed().as_millis(),
                shell.resize(r, c)
            ),
        }
    }
    sent.store(true, Ordering::Relaxed); // nothing more is being written: start the idle clock
    let _ = collector.join();

    let tr = trace.lock().unwrap();
    println!("\n--- timeline ([ms since spawn], every line the child emitted) ---");
    for (ms, line) in tr.lines.iter() {
        let line = line.replace('\u{1b}', "<ESC>");
        // Keep the log readable: long lines are summarised, not dumped.
        let shown: String = line.chars().take(96).collect();
        if shown.chars().count() < line.chars().count() {
            println!(
                "[{ms:>6}ms] {shown} … (+{} bytes)",
                line.chars().count() - 96
            );
        } else {
            println!("[{ms:>6}ms] {shown}");
        }
    }

    println!("\n--- PROBE results ---");
    for (_, line) in tr
        .lines
        .iter()
        .filter(|(_, l)| l.trim_start().starts_with("PROBE "))
    {
        println!("{}", line.trim());
    }

    println!("\n--- marker checks (against the raw byte stream) ---");
    let checks = [
        (
            "alt-screen entered (ESC[?1049h / ESC[?47h)",
            marker(&tr.raw, "\u{1b}[?1049h") || marker(&tr.raw, "\u{1b}[?47h"),
        ),
        (
            "vim painted typed text on its screen",
            marker(&tr.raw, "HELLO-FROM-VIM"),
        ),
        (
            "vim 'not to a terminal' warning",
            marker(&tr.raw, "is not to a terminal"),
        ),
        (
            "vim 'Error reading input' fatal",
            marker(&tr.raw, "Error reading input"),
        ),
        (
            "bash 'no job control in this shell'",
            marker(&tr.raw, "no job control in this shell"),
        ),
        (
            "Ctrl-C echoed as ^C by a line discipline",
            marker(&tr.raw, "^C"),
        ),
        (
            "macOS 'default interactive shell is now zsh' noise",
            marker(&tr.raw, "default interactive shell is now zsh"),
        ),
        ("bash prompt seen in stream (PS1)", marker(&tr.raw, "$ ")),
        (
            "colored/ANSI escapes present (count)",
            tr.raw.iter().filter(|&&b| b == 0x1b).count() > 0,
        ),
    ];
    for (name, hit) in checks {
        println!("{:<52} {}", name, if hit { "YES" } else { "no " });
    }

    // Ctrl-C latency measured off the child's own stream: ~1.5s means the tty line
    // discipline turned the byte into SIGINT; ~6s means the byte never did anything.
    let at = |want: &str| {
        tr.lines
            .iter()
            .find(|(_, l)| l.trim() == want)
            .map(|(ms, _)| *ms)
    };
    match (at("PROBE pre_interrupt"), at("PROBE post_interrupt")) {
        (Some(a), Some(b)) => println!(
            "{:<52} {:.2}s after `sleep 6` {}",
            "Ctrl-C gap",
            (b - a) as f64 / 1000.0,
            if b - a < 4000 {
                "=> INTERRUPTED"
            } else {
                "=> IGNORED (sleep ran to completion)"
            }
        ),
        _ => println!("{:<52} not observed", "Ctrl-C gap"),
    }

    // Did the merged stream preserve the program's stdout/stderr order?
    let observed: Vec<String> = tr
        .lines
        .iter()
        .filter_map(|(_, l)| {
            let l = l.trim();
            l.strip_prefix("PROBE ")
                .filter(|s| s.ends_with("-stdout") || s.ends_with("-stderr"))
                .map(|s| s.to_string())
        })
        .collect();
    let expected: Vec<String> = (1..=(observed.len() / 2))
        .flat_map(|i| [format!("o{i}-stdout"), format!("e{i}-stderr")])
        .collect();
    println!(
        "{:<52} {}",
        "stdout/stderr merged in program order",
        if observed == expected { "YES" } else { "NO" }
    );
    if observed != expected {
        println!("      observed: {}", observed.join(" "));
    }
}

fn main() {
    let mode = env::args().nth(1).unwrap_or_default();
    let bin = env::var("LOOPRS_SPIKE_BASH").unwrap_or_else(|_| "/bin/bash".into());
    match mode.as_str() {
        "pipes" => run("Option A - pipes (bash -i)", spawn_pipes(&bin)),
        "pty" => run("Option B - portable-pty 0.9", spawn_pty(&bin)),
        _ => {
            eprintln!("usage: spike_bash <pipes|pty>   (env LOOPRS_SPIKE_BASH=/path/to/bash)");
            std::process::exit(2);
        }
    }
}
