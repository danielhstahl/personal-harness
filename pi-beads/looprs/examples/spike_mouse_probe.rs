//! The thing that sits *inside* the pty for looprs-pdl.2 and answers the two
//! questions the selection/clipboard tickets are built on:
//!
//! * "did the mouse bytes the driver wrote reach me as the event kinds and grid
//!   coordinates I would act on?" — `events`
//! * "did the clipboard bytes I wrote actually leave the wire in the shape a
//!   terminal can use, and does anyone answer a read-back query?" — `osc52`,
//!   `osc52-query`
//!
//! It is a probe, not a feature: no ratatui, no app, no state beyond the event
//! counter it prints at the end. The alternative — asking the real TUI — would tie
//! every measurement in the spike to whatever `app.rs` happens to do with a
//! `MouseEvent`, which is exactly the thing not built yet. So the probe decodes
//! with the same library the app will use (`crossterm 0.29`, whose parser is the
//! thing under test) and reports one line per event, timestamped on its own clock.
//!
//! ## Output contract
//!
//! Everything is `KEY=value` lines on stdout, flushed as it is produced, so the
//! driver (`spikes/mouse_clipboard_e2e.py`) can read the wire without a parser:
//!
//! ```text
//! HELLO pid=4242 epoch_ns=1730000000000000000 modes=raw,mouse_report,...
//! EV t=12.345 mouse=down btn=left x=12 y=7 mods=NONE
//! EV t=13.900 mouse=drag btn=left x=13 y=7 mods=SHIFT
//! SUMMARY mouse=13 key=0 paste=0 resize=0 other=0 last_t=99.5
//! ```
//!
//! `t` is milliseconds since the probe started, which makes *spacings* between
//! events independent of any clock offset with the driver (the round-trip number
//! comes from the driver's own clock; the burst shape comes from here).
//!
//! ## Commands
//!
//! ```text
//! events [MS]                 read events for MS ms (default 1500) and report
//! events --decode-off [MS]    CONTROL: same tty, same raw mode, same modes
//!                             switched on, but the bytes are counted instead of
//!                             parsed. A "mouse event" that this run also reports
//!                             was not produced by the injected sequence.
//! osc52 SIZE [--frag N]       write an OSC 52 clipboard set of SIZE bytes of
//!                             payload, base64'd by crossterm's own
//!                             `CopyToClipboard`, in N chunks with 5ms gaps
//! osc52-file PATH [--frag N]  same, payload read from a file (exercises the
//!                             non-ASCII and newline cases argv cannot)
//! osc52-malformed SIZE        CONTROL: the same payload under OSC 51, not 52 —
//!                             a clipboard that changes on this run changed for
//!                             some reason other than our sequence
//! osc52-query [MS]            ask the terminal for the clipboard (OSC 52 ; c ;)
//!                             and report whether anything came back
//! mode-holder [MS]            switch the SPIKE_MODES set on and leave it on
//!                             (for the "what does the next process inherit" runs)
//! ```
//!
//! ## Environment
//!
//! `SPIKE_MODES` — the modes to switch on at startup, `LOOPRS_MODES`-shaped
//! (`raw`, `alt_screen`, `cursor_hidden`, `mouse_report`, `mouse_drag`,
//! `mouse_sgr`, `bracketed_paste`, `mouse`, `all`). Default `raw,mouse`.
//! Written as raw bytes rather than through crossterm's `Attribute` enum, so the
//! bytes are byte-for-byte the ones `src/teardown.rs` holds in its ledger and the
//! spike measures the same wire the app will put on it.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use crossterm::queue;

/// Same bytes as `Mode::on_bytes` in `src/teardown.rs`. Duplicated on purpose:
/// this example is not linked against the app, and a probe that imported the app's
/// table could not disagree with it, which is worth less than a probe that can.
const MODE_BYTES: &[(&str, &[u8], &[u8])] = &[
    ("alt_screen", b"\x1b[?1049h", b"\x1b[?1049l"),
    ("cursor_hidden", b"\x1b[?25l", b"\x1b[?25h"),
    ("mouse_report", b"\x1b[?1000h", b"\x1b[?1000l"),
    ("mouse_drag", b"\x1b[?1002h", b"\x1b[?1002l"),
    ("mouse_sgr", b"\x1b[?1006h", b"\x1b[?1006l"),
    ("bracketed_paste", b"\x1b[?2004h", b"\x1b[?2004l"),
];

fn stdout() -> io::Stdout {
    io::stdout()
}

fn line(args: impl std::fmt::Display) {
    let mut out = stdout();
    let _ = writeln!(out, "{args}");
    let _ = out.flush();
}

fn epoch_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Resolve a `LOOPRS_MODES`-shaped spec into the ordered list of (name, on, off).
fn resolve_modes(spec: &str) -> Vec<(&'static str, &'static [u8], &'static [u8])> {
    let mut names: Vec<&str> = Vec::new();
    for token in spec.split([',', ' ', '\t', '\n']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token {
            "all" => names.extend(MODE_BYTES.iter().map(|m| m.0)),
            "mouse" => names.extend(["mouse_report", "mouse_drag", "mouse_sgr"]),
            other => names.push(other),
        }
    }
    // BOOT_ORDER, so the enable order (and therefore the unwind order) does not
    // depend on how the spec was typed.
    let mut out: Vec<(&str, &[u8], &[u8])> = Vec::new();
    for m in MODE_BYTES {
        if names.contains(&m.0) && !out.iter().any(|o| o.0 == m.0) {
            out.push(*m);
        }
    }
    out
}

fn enable_modes() -> Vec<(&'static str, &'static [u8], &'static [u8])> {
    let spec = env::var("SPIKE_MODES").unwrap_or_else(|_| "raw,mouse".into());
    let raw = spec
        .split([',', ' ', '\t', '\n'])
        .any(|t| t.trim() == "raw");
    if raw {
        let _ = crossterm::terminal::enable_raw_mode();
    }
    let modes = resolve_modes(&spec);
    let mut out = stdout();
    for (_, on, _) in &modes {
        let _ = out.write_all(on);
    }
    let _ = out.flush();
    modes
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `str::find` for byte slices, which the standard library does not offer.
fn index(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The payload: an ASCII ruler with a line counter, ending in a marker that is
/// missing from any truncated copy. 60-column lines so a silent clamp shows up as
/// a round number of bytes short rather than as a mystery.
fn ruler(len: usize) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(len + 32);
    let mut n = 0usize;
    while buf.len() < len {
        let row = format!("{n:06} 0123456789 abcdefghijklmnopqrstuvwxyz ABCDE\n");
        buf.extend_from_slice(row.as_bytes());
        n += 1;
    }
    buf.truncate(len);
    buf.extend_from_slice(format!("<<END-PAYLOAD-{len}>>\n").as_bytes());
    buf
}

fn cmd_events(ms: u64, decode: bool) {
    let modes = enable_modes();
    let names: Vec<&str> = modes.iter().map(|m| m.0).collect();
    line(format!(
        "HELLO pid={} epoch_ns={} modes={} decode={}",
        std::process::id(),
        epoch_ns(),
        names.join(","),
        if decode { "on" } else { "off" }
    ));

    let t0 = Instant::now();
    let (mut mouse, mut keys, mut paste, mut resize, mut other) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut raw_bytes = 0usize;
    let mut last_t = 0f64;

    if !decode {
        // CONTROL: same tty, same raw mode, same modes switched on, no parser.
        // Anything this run "sees" is bytes, so a mouse event reported here would
        // have come from somewhere other than the injected sequence.
        //
        // The read runs on its own thread: `Stdin::read` blocks with no way to ask
        // it for a deadline, and a control that never prints its count is worse
        // than no control at all. The thread is left blocked when we return; the
        // process exiting takes it with it.
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(got) => {
                        n2.fetch_add(got, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        });
        thread::sleep(Duration::from_millis(ms));
        raw_bytes = n.fetch_add(0, std::sync::atomic::Ordering::SeqCst);
        last_t = t0.elapsed().as_secs_f64() * 1000.0;
        line(format!("RAWREAD bytes={raw_bytes} last_t={last_t:.3}"));
    } else {
        loop {
            let left = Duration::from_millis(ms).saturating_sub(t0.elapsed());
            if left.is_zero() {
                break;
            }
            if !event::poll(left).unwrap_or(false) {
                continue;
            }
            let Ok(ev) = event::read() else { break };
            let t = t0.elapsed().as_secs_f64() * 1000.0;
            last_t = t;
            match ev {
                Event::Mouse(m) => {
                    mouse += 1;
                    let (kind, btn) = match m.kind {
                        MouseEventKind::Down(b) => ("down", Some(b)),
                        MouseEventKind::Up(b) => ("up", Some(b)),
                        MouseEventKind::Drag(b) => ("drag", Some(b)),
                        MouseEventKind::Moved => ("moved", None),
                        MouseEventKind::ScrollUp => ("scroll_up", None),
                        MouseEventKind::ScrollDown => ("scroll_down", None),
                        MouseEventKind::ScrollLeft => ("scroll_left", None),
                        MouseEventKind::ScrollRight => ("scroll_right", None),
                    };
                    let btn = match btn {
                        Some(MouseButton::Left) => "left",
                        Some(MouseButton::Right) => "right",
                        Some(MouseButton::Middle) => "middle",
                        None => "-",
                    };
                    line(format!(
                        "EV t={t:.3} mouse={kind} btn={btn} x={} y={} mods={}",
                        m.column,
                        m.row,
                        m.modifiers.bits()
                    ));
                }
                Event::Key(k) => {
                    keys += 1;
                    line(format!(
                        "EV t={t:.3} key={:?} kind={:?} mods={}",
                        k.code,
                        k.kind,
                        k.modifiers.bits()
                    ));
                }
                Event::Paste(s) => {
                    paste += 1;
                    line(format!(
                        "EV t={t:.3} paste_len={} head={}",
                        s.chars().count(),
                        {
                            let h: String = s.chars().take(12).collect();
                            h.replace('\n', "\\n").replace('\r', "\\r")
                        }
                    ));
                }
                Event::Resize(c, r) => {
                    resize += 1;
                    line(format!("EV t={t:.3} resize cols={c} rows={r}"));
                }
                _ => other += 1,
            }
        }
    }

    line(format!(
        "SUMMARY mouse={mouse} key={keys} paste={paste} resize={resize} other={other} raw_bytes={raw_bytes} last_t={last_t:.3}"
    ));

    // Leave whatever we switched on, in the reverse of the order it went on — the
    // same promise `src/teardown.rs` makes. A probe that left the mouse reporting
    // on would poison the next run in the same tty.
    if env::var("SPIKE_LEAVE_MODES")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        let mut out = stdout();
        for (_, _, off) in modes.iter().rev() {
            let _ = out.write_all(off);
        }
        let _ = out.flush();
    }
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn write_osc52(payload: &[u8], frag: usize) {
    let mut out = stdout();
    // crossterm's own writer, so the bytes measured are the bytes the library
    // would put on the wire for us: OSC 52 ; "c" ; base64(payload) ST.
    let mut wire: Vec<u8> = Vec::new();
    let cmd = CopyToClipboard::to_clipboard_from(payload.to_vec());
    let _ = queue!(&mut wire, cmd);
    let _ = wire.flush();
    // The base64 body is everything after the second ';' up to the 2-byte ST.
    let first = index(&wire, b";").expect("crossterm wrote an OSC 52 sequence with no ';'");
    let b64_start = first + 1 + index(&wire[first + 1..], b";").expect("no second ';'");
    let b64_len = wire.len() - b64_start - 2; // minus the ST
    if frag > 1 {
        // The chunk log lines go *after* the sequence, not between the chunks:
        // an OSC string that has anything interleaved into it is no longer an OSC
        // string, and a logger that writes inside the payload is a real way to
        // break a copy. Measured, not assumed -- a version of this that logged
        // per chunk failed the reassembly check with the log text in the middle.
        let chunk = wire.len().div_ceil(frag);
        let mut sizes = Vec::new();
        for part in wire.chunks(chunk) {
            let _ = out.write_all(part);
            let _ = out.flush();
            sizes.push(part.len());
            thread::sleep(Duration::from_millis(5));
        }
        line(format!("FRAG chunks={} sizes={:?}", sizes.len(), sizes));
    } else {
        let _ = out.write_all(&wire);
        let _ = out.flush();
    }
    line(format!(
        "OSC52 payload_bytes={} wire_bytes={} b64_bytes={} head={}",
        payload.len(),
        wire.len(),
        b64_len,
        hex(&wire[..wire.len().min(16)])
    ));
}

fn cmd_osc52_query(ms: u64) {
    // Raw mode first: the reply arrives as bytes on stdin, and the line discipline
    // must not sit on them waiting for a newline that never comes.
    let _ = crossterm::terminal::enable_raw_mode();
    let mut out = stdout();
    line(format!("QUERY sent_at={}", epoch_ns()));
    let _ = out.write_all(b"\x1b]52;c?\x1b\\");
    let _ = out.flush();

    let acc = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let acc2 = acc.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => acc2.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });
    thread::sleep(Duration::from_millis(ms));
    let got = acc.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&got).to_string();
    let replied = text.contains("\x1b]52;");
    line(format!(
        "QUERY bytes={} osc52_reply={} head={}",
        got.len(),
        replied,
        hex(&got[..got.len().min(24)])
    ));
    let _ = crossterm::terminal::disable_raw_mode();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let flag = |name: &str, default: usize| -> usize {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("events");
    match cmd {
        "events" => {
            let ms = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1500u64);
            let decode = !args.iter().any(|a| a == "--decode-off");
            cmd_events(ms, decode);
        }
        "osc52" => {
            let size = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(64usize);
            write_osc52(&ruler(size), flag("--frag", 1));
        }
        "osc52-file" => {
            let path = args.get(1).cloned().unwrap_or_default();
            let body = fs::read(&path).unwrap_or_default();
            write_osc52(&body, flag("--frag", 1));
        }
        "osc52-malformed" => {
            // Same shape, wrong OSC number (51, not 52), same terminator. If a
            // clipboard round trip "passes" on this input it is passing on a
            // stale clipboard, not on our sequence.
            let size = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(64usize);
            let payload = ruler(size);
            let b64 = base64_std(&payload);
            let mut out = stdout();
            let _ = write!(out, "\x1b]51;c;{b64}\x1b\\");
            let _ = out.flush();
            line(format!("OSC51 payload_bytes={size}"));
        }
        "osc52-query" => {
            let ms = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1200u64);
            cmd_osc52_query(ms);
        }
        "mode-holder" => {
            let ms = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(300u64);
            let modes = enable_modes();
            line(format!(
                "HOLDER on={}",
                modes.iter().map(|m| m.0).collect::<Vec<_>>().join(",")
            ));
            thread::sleep(Duration::from_millis(ms));
            // Deliberately does NOT leave them: this is the process that hands a
            // dirty tty to whatever runs next in it.
            line("HOLDER held");
            std::mem::forget(modes);
        }
        other => {
            line(format!("PROBE_ERROR unknown_command={other}"));
            std::process::exit(2);
        }
    }
}

/// Base64 without pulling a second dependency into the example (crossterm's own
/// `base64` is not re-exported). Standard alphabet, padded — the same encoding
/// OSC 52 calls for.
fn base64_std(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        s.push(T[(n >> 18) as usize] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}
