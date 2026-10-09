//! //! The full-screen handover (ADR-0001 Q2): who holds the terminal, and what
//! //! they owe when they give it back.
//! //!
//! //! A full-screen child is the app forwarding rather than painting. These are the
//! //! tests for the seams: the alt-screen pair passing through when the app is not
//! //! hosting, quitting mid-handover leaving the alt screen as the child left it
//! //! (or leaving no debt to pay, when the app *was* hosting), a repainting
//! //! program losing the screen with its command, ordinary output never claiming
//! //! the screen at all, and raw keys reaching the child verbatim.

use super::*;

/// **Acceptance: a full-screen program is handed the screen.**
///
/// Driven with `printf` rather than real vim because what is being proved is
/// the session's own behaviour — announce the takeover, then the paint, then
/// the release — and `printf` emits the same alt-screen bytes vim does without
/// depending on vim's timing. That vim itself reaches the screen is proved in
/// the real terminal by `spikes/vim_fullscreen.py`.
///
/// Since the frame owns the alternate screen (ADR-0004 R22), what is handed
/// over is *our* screen and a blank canvas, not a screen switch: the child's
/// `?1049h` and `?1049l` are cut out of the stream and never reach the
/// terminal. The takeover and the release are still reported to the UI
/// exactly as if the switch had happened, because the UI's question — "is
/// something else painting right now?" — is answered the same either way.
///
/// **Every byte check below runs on the tape ([`Tape`]), and that is the whole
/// of what made this test a flake.** It used to ask which *event* contained
/// `\u{1b}[?25lpainted`. A pty read returns whatever had arrived when it
/// returned — the same stream delivers this very command's echo one or two
/// characters at a time — so there is no promise that an escape and the bytes
/// after it land in the same event, and on a loaded runner they did not. The
/// needle was not whole, and a needle that does not exist cannot be found by
/// widening a tolerance. The transitions *are* events, so ordering against
/// them stays exact while the byte search stops depending on where the reads
/// happened to fall.
///
/// The "not on the wire" check moves for the same reason and keeps its scope:
/// the echo of the command line is full of the literal characters `?1049`, so
/// only output that went by **after** the takeover can say what reached the
/// terminal.
#[tokio::test]
async fn a_full_screen_program_is_handed_the_screen_and_gives_it_back() {
    let (mut s, mut rx) = bash(21);
    s.send_text("printf '\\033[?1049h\\033[?25lpainted\\033[?1049l'".into())
        .unwrap();
    let events = until_exit(&mut rx).await;
    let tape = Tape::of(&events);

    let took = events
        .iter()
        .position(|e| e == "screen true")
        .unwrap_or_else(|| panic!("the takeover was never reported: {events:?}"));
    // The *program's* paint, and not the two other things in this stream that
    // say "painted": the command's echo (the literal characters `\033[`, no
    // escape byte anywhere) and the shell's own prompt (a real escape of its
    // own, `\u{1b}[?1034h`, which used to satisfy "contains `painted` and
    // contains an escape" whenever the pty batched the prompt with the echo).
    // The needle is an escape exactly where the program put one.
    let painted = tape
        .event_carrying("\u{1b}[?25lpainted")
        .unwrap_or_else(|| panic!("the program's paint never went by: {events:?}"));
    let released = events
        .iter()
        .position(|e| e == "screen false")
        .unwrap_or_else(|| panic!("the release was never reported: {events:?}"));
    assert!(
        took < painted,
        "the UI must be teeing before the bytes that change the screen: {events:?}"
    );
    assert!(
        painted < released,
        "the release must come after the paint it follows: {events:?}"
    );
    // The switch itself never reached the wire: neither the enter nor the
    // leave, anywhere in the output that went by after the takeover.
    let after = tape.after_event(took);
    assert!(
        !after.contains("?1049"),
        "the child's alt-screen bytes reached the terminal; the frame owns \
             that screen now: {events:?}"
    );
    // …and in place of the enter, the canvas the child expected: a blank
    // screen, not our previous frame showing through wherever it did not paint.
    assert!(
        after.contains("\u{1b}[H\u{1b}[2J"),
        "no blank canvas was handed over: {events:?}"
    );
    assert_no_marker_bytes(&events[took..].join("|"));
    assert_eq!(s.status(), SessionStatus::Idle, "and the shell is fine");
}

/// The same program on the **non-hosting** path: nothing is cut, the pair
/// goes through as written, and the leave lands before the release is
/// reported — which is the shape that path had before the frame took the
/// screen, kept tested so a change to the cutter cannot silently widen.
///
/// Read on the tape ([`Tape`]) for the same reason as the hosted test, and
/// with a stronger reason: the check this replaces wanted **one event** to
/// carry the program's `\u{1b}[?1049h` *and* the `painted` that follows it
/// three escapes later, which is a demand about read coalescing that this path
/// never made any promise about.
#[tokio::test]
async fn a_childs_alt_screen_pair_passes_through_when_we_do_not_host_the_screen() {
    let (mut s, mut rx) = bash_not_hosting(21);
    s.send_text("printf '\\033[?1049h\\033[?25lpainted\\033[?1049l'".into())
        .unwrap();
    let events = until_exit(&mut rx).await;
    let tape = Tape::of(&events);

    let took = events
        .iter()
        .position(|e| e == "screen true")
        .unwrap_or_else(|| panic!("the takeover was never reported: {events:?}"));
    let released = events
        .iter()
        .position(|e| e == "screen false")
        .unwrap_or_else(|| panic!("the release was never reported: {events:?}"));
    let after = tape.after_event(took);
    assert!(
        after.contains("\u{1b}[?1049h"),
        "the enter was cut on the path that must cut nothing: {events:?}"
    );
    let painted = tape
        .event_carrying("\u{1b}[?25lpainted")
        .unwrap_or_else(|| panic!("the program's paint never went by: {events:?}"));
    // The order that matters here: the leave bytes reach the terminal *before*
    // the release is reported, so a terminal watching the wire is back on the
    // main screen by the time the UI is told it may draw again.
    let left = tape
        .event_carrying("\u{1b}[?1049l")
        .unwrap_or_else(|| panic!("the leave bytes never reached the terminal: {events:?}"));
    assert!(took < painted, "{events:?}");
    assert!(
        left < released,
        "the leave bytes must reach the terminal before the release: {events:?}"
    );
    assert_no_marker_bytes(&events.join("|"));
    assert_eq!(s.status(), SessionStatus::Idle);
}

/// **Acceptance: quitting while a full-screen program holds the screen pays the
/// alternate screen back.**
///
/// `printf` takes the alt screen and never returns it — the shape of vim
/// killed, `less` closed by a signal, a pager that died mid-paint. The command
/// boundary pays that debt for a command that finishes; this is the other way
/// out, the quit that arrives *while* the program still holds it. Nobody else
/// can pay it: this session is the only thing that watched the bytes go by,
/// and the moment `shutdown` returns the terminal belongs to whatever started
/// us. Left unpaid, the user is inside a dead program's screen with their prompt
/// gone, and the only way out is `reset` typed blind.
///
/// Driven on the **non-hosting** path, because that is the only shape where
/// the child's switch is real. With the frame hosting the alternate screen
/// this debt cannot exist at all — see
/// [`quitting_while_a_hosted_child_holds_the_screen_leaves_no_debt_to_pay`].
#[tokio::test]
async fn quitting_while_a_full_screen_program_holds_the_screen_leaves_the_alt_screen() {
    let (mut s, mut rx) = bash_not_hosting(24);
    s.send_text("printf '\\033[?1049hpainted and stuck'; sleep 30".into())
        .unwrap();

    // Wait for the takeover, then quit mid-hold. The `sleep 30` keeps a
    // command in front of the shell so nothing can end it politely for us: the
    // only thing that can pay the screen back here is the quit path itself.
    let deadline = tokio::time::Instant::now() + NO_HANG;
    loop {
        let line = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the session went silent before taking the screen")
            .expect("stream closed");
        if describe(&line) == "screen true" {
            break;
        }
    }

    s.shutdown().unwrap();
    let events = until_closed(&mut rx).await;
    let joined = events.join("|");
    assert!(
        joined.contains("\u{1b}[?1049l"),
        "the alt screen the program died holding must be paid back on the quit path: {events:?}"
    );
    assert!(
        events.contains(&"screen false".to_string()),
        "and the UI must be told the screen is free on the way out: {events:?}"
    );
}

/// The hosted shape of the same quit. The child asks for the alternate screen
/// while the **frame** owns it, so its `?1049h` is cut (ADR-0004 R22) and
/// nothing is switched — which means there is no debt for the quit path to
/// pay, and the session must write **no** leave of its own. A second
/// `?1049l` after the ledger's one would be aimed at the user's real main
/// screen.
///
/// What the quit still owes is the *report*: `screen false`, so nothing
/// downstream keeps waiting for a screen that was never switched away. Proved
/// end to end by `spikes/fullscreen_e2e.py` ("a SIGKILLed child owes no
/// leave"); this is the same promise at unit speed.
#[tokio::test]
async fn quitting_while_a_hosted_child_holds_the_screen_leaves_no_debt_to_pay() {
    let (mut s, mut rx) = bash(25);
    let mut seen: Vec<String> = Vec::new();
    s.send_text("printf '\\033[?1049hpainted and stuck'; sleep 30".into())
        .unwrap();

    let deadline = tokio::time::Instant::now() + NO_HANG;
    loop {
        let line = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("silent before taking the screen, got: {seen:?}"))
            .expect("stream closed");
        let d = describe(&line);
        seen.push(d.clone());
        if d == "screen true" {
            break;
        }
    }

    s.shutdown().unwrap();
    let mut events = until_closed(&mut rx).await;
    events.splice(0..0, seen);
    // Measured from the takeover on: everything before it is the echo of the
    // command line, whose literal `?1049` characters are text, not wire.
    let took = events
        .iter()
        .position(|e| e == "screen true")
        .expect("no takeover in the merged stream");
    assert!(
        !events[took..].join("|").contains("?1049"),
        "the session charged or repaid a screen it had already cut: {events:?}",
    );
    assert!(
        events.contains(&"screen false".to_string()),
        "the UI must still be told the screen is free on the way out: {events:?}",
    );
}

/// The non-alt-screen case: a program that repaints in place without ever
/// switching screens (the shape ADR-0001's measurement found — cursor
/// addressing in a chunk with no linefeeds). The screen comes back at the
/// command boundary, because that program has no leave sequence to wait for.
#[tokio::test]
async fn a_repainting_program_takes_the_screen_and_loses_it_with_the_command() {
    let (mut s, mut rx) = bash(22);
    // Two separate writes: the first is a line (the transcript's business), the
    // second moves the cursor back up over it, which is the paint.
    s.send_text("printf 'first-line\\n'; sleep 0.3; printf '\\033[2Aover-the-top'".into())
        .unwrap();
    let events = until_exit(&mut rx).await;

    let took = events
        .iter()
        .position(|e| e == "screen true")
        .expect("the repaint should take the screen");
    let painted = events
        .iter()
        .rposition(|e| e.starts_with("out ") && e.contains("over-the-top"))
        .expect("the repaint output never arrived");
    let released = events
        .iter()
        .position(|e| e == "screen false")
        .expect("the command boundary must release the screen");
    assert!(took < painted, "announced before the paint: {events:?}");
    assert!(
        painted <= released,
        "and released only once the command is over: {events:?}"
    );
}

/// **Ordinary output must never trip the screen path.** If it did, `ls` would
/// suspend the UI and stop the transcript, which is a much worse bug than a
/// full-screen program that does not show.
#[tokio::test]
async fn ordinary_output_never_claims_the_screen() {
    let (mut s, mut rx) = bash(23);
    s.send_text("printf 'one\\ntwo\\nthree\\n'".into()).unwrap();
    let events = until_exit(&mut rx).await;
    assert!(
        events.iter().all(|e| !e.starts_with("screen")),
        "a plain three-line command took the screen: {events:?}"
    );
    // Colour is the common false-positive risk: `ls --color`, `git log`.
    s.send_text("printf '\\033[31mred\\033[0m text with no newline'".into())
        .unwrap();
    let events = until_exit(&mut rx).await;
    assert!(
        events.iter().all(|e| !e.starts_with("screen")),
        "an SGR-coloured chunk is still a line of output: {events:?}"
    );
}

/// **Acceptance: raw keystrokes reach the child with nothing added.**
///
/// `Esc` has to arrive as one `0x1b` byte, not `0x03` and not with a newline
/// on it: this is what lets a program in the alt screen read the keyboard.
///
/// **The witness is the child's own report of the bytes, not the line
/// discipline's echo.** The old form read the echo, and the echo turned out not
/// to be a witness at all: whether it fires depends on whether readline had
/// the tty in raw mode at the instant of the write, and whether `ESC :` is
/// echoed as `^[:` or swallowed by readline as an escape prefix. Three runs of
/// the old form through the same pty: two showed `^[:wq!` before the command
/// finished, one showed nothing until `bash: wq!: command not found` — which is
/// not `:wq!`, so that run failed the assertion it was supposed to be
/// guaranteeing (looprs-00u.17). `head -c 6 | od -An -tx1` answers the
/// question by value instead: exactly these bytes arrived, in this order, and
/// nothing else arrived with them.
///
/// The command is put in flight through the seam ([`in_flight`]) rather than by
/// polling `status()` until it reads `Running`, which made "is it safe to type
/// at it yet?" a question with a timer for an answer.
#[tokio::test]
async fn raw_keys_reach_the_child_verbatim_and_the_shell_still_survives() {
    let (mut s, mut rx) = bash(24);
    warm_shell(&mut s, &mut rx).await;
    // `GOT[` … `]` brackets the report so it can be picked out of the stream
    // without guessing at which other bytes are hex-looking; `stty -echo` on
    // the way in and back out again so the run neither reads the echo as the
    // report nor leaves the tty muted for whoever inherits it.
    in_flight(
            &mut s,
            "printf 'first-line\\n'; stty -echo; printf 'GOT['; head -c 6 | od -An -tx1 | tr -d ' \\n'; stty echo; printf ']\\n'",
        )
        .await;

    // Type a keystroke sequence at the running command: Esc, ':', 'w', 'q',
    // '!' and CR — the shape of `:wq!`.
    s.send_bytes(vec![0x1b, b':', b'w', b'q', b'!', 0x0d])
        .unwrap();
    let ran = run_logged(&mut rx).await;
    assert!(ran.out.contains("first-line"), "{:?}", ran.out);
    let reported = between(&ran.out, "GOT[", "]").unwrap_or_else(|| {
        panic!(
            "the child never reported the bytes it was handed, so they never reached \
                 the pty: {:?}",
            ran.out
        )
    });
    // The ESC came through as `0x1b` — not translated to `0x03`, not eaten —
    // and the five typed bytes are the five reported bytes, in order.
    assert!(
        reported.starts_with("1b3a777121"),
        "the child got {reported:?}, not the ESC : w q ! we typed"
    );
    // Six bytes and not seven: the "nothing added" half. The sixth is the line
    // terminator we typed — CR, which the line discipline's own ICRNL turns
    // into NL on the way in. A `\\n` appended by the session would make seven.
    assert_eq!(
        reported.len(),
        12,
        "typed six bytes, the child reported {reported:?}: a byte added or lost"
    );
    assert!(
        reported.ends_with("0a") || reported.ends_with("0d"),
        "the terminator is not the CR we typed (possibly ICRNL-translated): {reported:?}"
    );
    assert_eq!(s.status(), SessionStatus::Idle);
}

/// A held screen must not swallow the command boundary: the `exit 0` still has
/// to reach the transcript after the release, or the user sees no result at all.
#[tokio::test]
async fn the_command_result_is_still_said_after_a_screen_session() {
    let (mut s, mut rx) = bash(25);
    s.send_text("printf '\\033[?1049hscreen\\033[?1049l'; echo done-marker".into())
        .unwrap();
    let ran = run_logged(&mut rx).await;
    assert_eq!(ran.code, Some(0), "{:?}", ran.notes);
    assert!(ran.out.contains("done-marker"), "{:?}", ran.out);
    assert!(
        ran.notes.iter().any(|n| n.contains("exit 0")),
        "{:?}",
        ran.notes
    );
}
