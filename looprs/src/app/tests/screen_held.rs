//! The full-screen (ADR-0001 Q2) seam

use super::*;

/// **While a full-screen child holds the screen, the transcript is not the
/// display path.** The bytes go to the real terminal instead; keeping a copy
/// in the transcript as well is the same frame twice — once as the child drew
/// it, once re-rendered by us above the viewport.
#[test]
fn a_held_screen_goes_to_the_terminal_and_not_to_the_transcript() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    assert!(app.passthrough(), "the active Bash session owns the screen");

    app.update(Msg::BashOutput {
        session: bash_id(),
        stream: ByteStream::Merged,
        chunk: "\u{1b}[?1049h\u{1b}[24;1H--INSERT--".into(),
    });

    let held_in_transcript = app
        .view(TerminalType::Bash)
        .map(|v| v.transcript.entries.len())
        .unwrap_or(0);
    assert_eq!(
        held_in_transcript, 0,
        "a program's screen is not scrollback material"
    );
}

/// The holder is Bash, but the **user is looking at Pi**. Teeing Bash's paint
/// over the mode on screen would be worse than not showing it, so the bytes go
/// to the Bash view and are kept rather than smeared.
#[test]
fn a_held_screen_is_never_painted_over_the_mode_that_is_showing() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    assert!(
        !app.passthrough(),
        "Bash owns the screen but is not the mode on it"
    );

    app.update(Msg::BashOutput {
        session: bash_id(),
        stream: ByteStream::Merged,
        chunk: "painted while hidden".into(),
    });
    let live: String = app
        .view(TerminalType::Bash)
        .map(|v| v.preview(80).iter().map(|l| l.to_string()).collect())
        .unwrap_or_default();
    assert!(
        live.contains("painted while hidden"),
        "kept in its own view instead of overwriting Pi"
    );
    assert_eq!(
        text_of(&app, TerminalType::Bash),
        "",
        "and it is still the live line it was: nothing was ended, so the              scrollback got nothing — while hidden, the bytes are held, not smeared"
    );
    assert!(app.view(TerminalType::Pi).is_none(), "Pi was not touched");
}

/// **The tee is where the alternate-screen debt gets booked**, because the
/// tee is the only thing this app uses to tell the real terminal anything.
/// A child that switched the screen through us and died there leaves us
/// holding it, and the exit path reads that off this handle (`looprs-pdl.3`).
#[test]
fn a_teed_alt_screen_is_a_screen_we_owe_the_terminal_back() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    assert_eq!(
        app.screen_debt().outstanding(),
        None,
        "nothing was switched on yet"
    );

    app.update(Msg::BashOutput {
        session: bash_id(),
        stream: ByteStream::Merged,
        chunk: "\u{1b}[?1049h--INSERT--".into(),
    });
    assert_eq!(
        app.screen_debt().outstanding(),
        Some(1049),
        "that screen is now ours to give back"
    );

    app.update(Msg::BashOutput {
        session: bash_id(),
        stream: ByteStream::Merged,
        chunk: ":wq\r\n\u{1b}[?1049l".into(),
    });
    assert_eq!(
        app.screen_debt().outstanding(),
        None,
        "the child paid its own leave, so nobody owes it twice"
    );
}

/// The other half: bytes that never reached the terminal cannot have switched
/// anything on it. With the user looking at Pi, Bash's paint is transcripted,
/// and a transcript of `?1049h` is text, not a mode switch.
#[test]
fn a_screen_switch_that_was_never_teed_owes_nothing() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    app.update(Msg::BashOutput {
        session: bash_id(),
        stream: ByteStream::Merged,
        chunk: "\u{1b}[?1049hpaint that stayed in the transcript".into(),
    });
    assert_eq!(
        app.screen_debt().outstanding(),
        None,
        "the real terminal was never told anything, so it is owed nothing"
    );
}

/// **Acceptance: Esc reaches a full-screen program as Esc.** In vim `Esc` is
/// the key that leaves insert mode; `0x03` is an interrupt, and sending that
/// instead is exactly why vim was unusable. The keystroke has to go down as
/// raw bytes and must not type into the input box.
#[tokio::test]
async fn esc_is_esc_inside_a_full_screen_program() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));

    match rx.recv().await {
        Some(UiCommand::Keys { mode, bytes }) => {
            assert_eq!(mode, TerminalType::Bash);
            assert_eq!(bytes, vec![0x1b], "one byte: Esc");
        }
        other => {
            panic!("Esc inside a held screen must reach the child as a keystroke, got {other:?}")
        }
    }
    assert!(
        app.input.text().is_empty(),
        "and it did not go into the input box"
    );
}

/// …and with no program holding the screen, Esc is still the interrupt a line
/// command needs. Both halves of the acceptance line, in the same app type,
/// separated by exactly one thing: who owns the screen.
#[tokio::test]
async fn esc_is_still_cancel_at_a_line_prompt() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
    match rx.recv().await {
        Some(UiCommand::Cancel) => {}
        other => panic!("Esc at a prompt should still cancel, got {other:?}"),
    }
}

/// **Acceptance: Ctrl-C interrupts in both cases.** Inside a held screen it
/// still routes as Cancel — which the Bash session writes to the master as
/// `0x03`, and the line discipline does the rest. It still must not quit.
#[tokio::test]
async fn ctrl_c_still_interrupts_inside_a_full_screen_program() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    app.update(Msg::Term(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));

    assert!(!app.should_quit, "Ctrl-C must not quit while vim is up");
    match rx.recv().await {
        Some(UiCommand::Cancel) => {}
        other => panic!("Ctrl-C should still reach the shell as Cancel, got {other:?}"),
    }
}

/// The way out of a program that took the keyboard: Ctrl-Q is ours even while
/// the screen is handed over. Without it a grabbed keyboard is a locked app.
#[test]
fn ctrl_q_quits_even_while_the_screen_is_held() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    app.update(Msg::Term(key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
    assert!(app.should_quit, "Ctrl-Q stays ours");
}

/// A release asks for the viewport to be re-anchored; a takeover must not, or
/// the run loop would resize our viewport onto the child's screen.
#[test]
fn a_release_asks_to_re_anchor_and_a_takeover_does_not() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    assert!(
        !app.repaint_all,
        "taking the screen over is not a reason to resize"
    );
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: false,
    });
    assert!(
        app.repaint_all,
        "getting it back is: ratatui's diff no longer describes the screen"
    );
}

/// A child that dies holding the screen still gives it up. Nothing can wait for
/// the release event here — the death is what cancelled the program that would
/// have sent it — so the death itself has to close the seam.
#[test]
fn a_child_dying_while_it_holds_the_screen_lets_go_of_it() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    assert!(app.passthrough());

    app.update(Msg::SessionDown {
        session: bash_id(),
        reason: ExitReason::Crashed { code: Some(137) },
    });
    assert!(
        !app.passthrough(),
        "a dead child cannot hold a screen; not releasing here wedges the UI dark"
    );
    assert!(app.repaint_all, "and the viewport has to be rebuilt");
}

/// A release from some *other* session must not disturb a held screen. The
/// pairing is by session id, not by "somebody said inactive".
#[test]
fn a_release_from_another_session_does_not_take_the_screen_back() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let other = SessionId::new(TerminalType::Bash, 2);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    app.update(Msg::ScreenHeld {
        session: other,
        active: false,
    });
    assert!(
        app.passthrough(),
        "a generation that never held the screen cannot release it"
    );
    assert!(!app.repaint_all);
}
