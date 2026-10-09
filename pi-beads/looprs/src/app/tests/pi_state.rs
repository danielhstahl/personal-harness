//! The Pi terminal state, from the UI side

use super::*;

/// **A settle never drives the beads machine from the App** (looprs-msj).
///
/// The bug as filed had two halves, and they are the same half twice: a Pi run
/// settling while the box happened to be on Beads drove the beads state
/// machine, and a beads worker settling while the box was on Pi left it
/// stalled. Both came from the App deciding what a settle *meant*.
///
/// So the assertion is the blunt one, run with the box in every mode and with
/// the settle from every session: handling a settle sends **nothing** to the
/// router. Not "the right command for the right mode" — nothing. The beads loop
/// is driven from inside `BeadsSession` off its own worker's stream, which is
/// pinned in `session::beads::tests`; all this side may do is paint.
#[test]
fn no_settle_ever_turns_into_a_command_from_the_app() {
    for ui_mode in TerminalType::ALL {
        for producer in [pi_id(), beads_id(), SessionId::new(TerminalType::Bash, 1)] {
            let (mut app, mut rx) = app_with(ui_mode);

            app.update(Msg::Agent {
                session: producer,
                event: PiEvent::AgentSettled,
            });

            assert!(
                rx.try_recv().is_err(),
                "box on {}, {} settled, and the App still sent a command",
                ui_mode.label(),
                producer
            );
            // It rendered, though — that part is the whole job.
            assert_eq!(
                app.view(producer.mode)
                    .expect("the producer has a view")
                    .chat,
                ChatState::Stopped,
                "settling must still stop {}'s live region",
                producer
            );
            // And no other view was touched by a session that never spoke to it.
            for other in TerminalType::ALL {
                if other != producer.mode {
                    assert!(
                        app.view(other).is_none(),
                        "{} settling created a {} view out of the UI's guesswork",
                        producer,
                        other.label()
                    );
                }
            }
        }
    }
}

/// The grep check the ticket asked for, kept as a test so it stays checked.
///
/// ADR-0002 Q2: the input mode is authoritative for *intent* — where the
/// user's keystrokes go — and never for *origin*. So here the field may only
/// ever be *assigned* (the one line in `App::new` that pins the box to the mode
/// the app opened in, which is the same fact the render pointer gets) and never
/// *read* to decide anything about an event. Reads by another name are caught
/// too, because every read of the box goes through this text.
///
/// It scans every file that holds `App` code rather than just `app.rs`: the copy
/// half now lives in [`crate::app::copy`], and a routing read that landed there
/// would be exactly as much of a looprs-msj as one left behind.
#[test]
fn the_ui_never_reads_the_input_mode_to_route_an_event() {
    // Assembled rather than written out, so this test's own source cannot match
    // the pattern it is looking for.
    let mode_field = concat!("input", ".mode");
    let assigned = concat!("input", ".mode = ");
    let src = concat!(
        include_str!("../../app.rs"),
        "\n",
        include_str!("../copy.rs")
    );
    let mentions: Vec<String> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//")) // prose is allowed to name the bug
        .filter(|l| l.contains(mode_field))
        .map(|l| l.to_string())
        .collect();
    let offenders: Vec<&String> = mentions
        .iter()
        // Assignments are not routing. `App::new` sets the box onto the mode it
        // opened in, and the tests set a scenario up; neither decides where an
        // event belongs.
        .filter(|l| !l.starts_with(assigned))
        .collect();
    assert!(
        offenders.is_empty(),
        "event handling must never consult the input mode (that is looprs-msj): {offenders:?}"
    );
    // And the checker is really looking at something: the allowed line must be
    // there, or this test would be green because the pattern rotted away.
    assert!(
        mentions.iter().any(|l| l.starts_with(assigned)),
        "no `{mode_field}` assignment found — the checker matched nothing: {mentions:?}"
    );
}

/// **Pi's copy of the user's own message stays out of the transcript** — the
/// echo made on submit is the one and only copy. Two copies of the same line is
/// worse than none, and pi sends one for every message we send.
#[test]
fn pi_copies_of_the_user_message_never_reach_the_transcript() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.echo_local(TerminalType::Pi, "my name is Dan".into());
    let after_echo = app.view(TerminalType::Pi).unwrap().transcript.entries.len();

    for role in ["user", "toolResult"] {
        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::MessageStart {
                message: WireMessage {
                    role: EntryRole::parse(role),
                    usage: None,
                },
            },
        });
        app.update(Msg::Agent {
            session: pi_id(),
            event: PiEvent::MessageEnd {
                message: WireMessage {
                    role: EntryRole::parse(role),
                    usage: None,
                },
            },
        });
    }

    let v = app.view(TerminalType::Pi).unwrap();
    assert_eq!(
        v.transcript.entries.len(),
        after_echo,
        "not one of pi's non-assistant messages added a line: {:?}",
        v.transcript
            .entries
            .iter()
            .map(|e| &e.text)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        text_of(&app, TerminalType::Pi),
        "my name is Dan",
        "the local echo is still the only copy"
    );

    // The assistant's own end-of-message is *not* suppressed: it closes the
    // streaming entry, which is what lets it flush at all.
    app.update(Msg::Agent {
        session: pi_id(),
        event: PiEvent::MessageUpdate {
            assistant_message_event: AssistantEvent::TextDelta {
                content_index: 0,
                delta: "hello".into(),
            },
        },
    });
    app.update(Msg::Agent {
        session: pi_id(),
        event: PiEvent::MessageEnd {
            message: WireMessage {
                role: EntryRole::Assistant,
                usage: None,
            },
        },
    });
    assert!(app.flush_active(60) != 0, "a closed answer must flush");
}

/// **Esc's restore lands in the box** — the whole point of pulling the queued
/// text out of pi before aborting.
#[test]
fn esc_restore_puts_the_queued_text_back_in_the_box() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::RestoreInput {
        session: pi_id(),
        text: "and also this".into(),
    });
    assert_eq!(app.input.text(), "and also this");
}

/// The restore is asynchronous; the user's own typing is newer than it is, and
/// wins. The text is not thrown away either — it goes to the transcript, where
/// it can be re-typed, because silently dropping it is the very failure this
/// recipe exists to prevent.
#[test]
fn esc_restore_never_eats_what_the_user_typed_in_the_meantime() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    for c in "no wait".chars() {
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ))));
    }

    app.update(Msg::RestoreInput {
        session: pi_id(),
        text: "and also this".into(),
    });

    assert_eq!(app.input.text(), "no wait", "the user's text survives");
    assert!(
        text_of(&app, TerminalType::Pi).contains("and also this"),
        "and the restored text is visible rather than lost"
    );
}

/// **Ctrl-C in the Bash view belongs to the shell, not to looprs**
/// (ADR-0001 Q3). It has to arrive at the session as a cancel — the Bash
/// session turns that into `0x03` on the pty master, and the line discipline
/// SIGINTs the foreground process group — and it must not quit the app. A
/// Bash pane where `sleep 30` cannot be stopped is not a shell.
#[tokio::test]
async fn ctrl_c_in_bash_mode_cancels_the_shell_and_does_not_quit() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ))));

    assert!(!app.should_quit, "Ctrl-C must not quit Bash mode");
    match rx.recv().await {
        Some(UiCommand::Cancel) => {}
        other => panic!("Ctrl-C should reach the shell as Cancel, got {other:?}"),
    }
}

/// Outside Bash, Ctrl-C keeps the meaning it has today. Pinned rather than left
/// implicit so that looprs-5g7 changing it is a deliberate edit to this test
/// and not a regression nobody noticed.
#[tokio::test]
async fn ctrl_c_outside_bash_mode_still_quits_for_now() {
    for mode in [TerminalType::Beads, TerminalType::Pi] {
        let (mut app, mut rx) = app_with(mode);
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))));
        assert!(app.should_quit, "Ctrl-C in {} mode", mode.label());
        assert!(
            rx.try_recv().is_err(),
            "quitting is not a cancel; nothing was sent"
        );
    }
}

/// Ctrl-Q is the chord looprs owns in **every** mode, Bash included — the
/// way out when Ctrl-C has been handed to a shell.
#[test]
fn ctrl_q_quits_in_every_mode() {
    for mode in TerminalType::ALL {
        let (mut app, _rx) = app_with(mode);
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))));
        assert!(app.should_quit, "Ctrl-Q in {} mode", mode.label());
    }
}

/// **A `Msg::BashOutput` lands in the Bash view and never in the mode the input
/// box happens to be in** — and the child keeps its own framing: a line the
/// child ended is stored as one line, and a line it did not end stays live and
/// unfinished until the stream does (ADR-0001 rule 1, ADR-0005).
#[test]
fn bash_output_lands_in_its_own_view_verbatim() {
    let mut app = {
        let (tx, _rx) = mpsc::channel::<UiCommand>(16);
        App::new(tx, InputState::new(), TerminalType::Beads, 80, 24)
    };
    let bash = SessionId::new(TerminalType::Bash, 1);
    let chunk = "first line\nsecond line, with no trailing newline";
    app.update(Msg::BashOutput {
        session: bash,
        stream: ByteStream::Merged,
        chunk: chunk.into(),
    });

    // The finished line is in the store. The unterminated tail is *not*: ADR-0005
    // makes the store a list of complete lines so the line still being written
    // can be overwritten by the next `\r`, and the tail lives in the view's
    // resolver until the stream ends. Both halves are checked here, because
    // "arrived whole" is the same promise and it now has two addresses.
    assert_eq!(
        text_of(&app, TerminalType::Bash),
        "first line\n",
        "the line the child ended is in the store, as the child wrote it"
    );
    let live: String = app
        .view(TerminalType::Bash)
        .map(|v| v.preview(80).iter().map(|l| l.to_string()).collect())
        .unwrap_or_default();
    assert!(
        live.contains("second line, with no trailing newline"),
        "the line it did not end is live, not lost: {live:?}"
    );
    assert!(
        app.view(TerminalType::Beads).is_none(),
        "and it went nowhere near the mode the box was in"
    );

    // And ending the stream lands the tail rather than dropping it.
    let bash = SessionId::new(TerminalType::Bash, 1);
    app.view_mut(bash).seal();
    assert_eq!(
        text_of(&app, TerminalType::Bash),
        format!("{chunk}\n"),
        "the seal collected the live line; nothing was lost with the stream"
    );
}

/// A Pi cancel cannot type into the Beads box: the restore is tagged with the
/// session that made it, and only the mode on screen gets the keystroke.
#[test]
fn a_pi_cancel_cannot_type_into_another_modes_box() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    app.update(Msg::RestoreInput {
        session: pi_id(),
        text: "queued from pi".into(),
    });
    assert!(
        app.input.text().is_empty(),
        "the beads box is untouched by a Pi Esc"
    );
    assert!(
        text_of(&app, TerminalType::Pi).contains("queued from pi"),
        "and the text is still accounted for, in the session that owns it"
    );
}
