//! Keyboard copy and the chord table (looprs-pdl.13)
//!
//! These are the driven half of the table in
//! [`CHORD_TABLE`](crate::session::view::CHORD_TABLE): the view test checks
//! the table is internally consistent, these check that the keystrokes land
//! where the table says they do. Every one of them drives the real `App` with
//! real `KeyEvent`s and reads back real consequences — what the sink got,
//! what the router got, what the box holds, what the toast says.

use super::*;

fn recording_sink(app: &mut App) -> crate::testing::RecordingTranscriptSink {
    let rec = crate::testing::RecordingTranscriptSink::new();
    app.set_transcript_sink(Arc::new(rec.clone()));
    rec
}

fn stalling_sink(app: &mut App) -> crate::testing::StallTranscriptSink {
    let s = crate::testing::StallTranscriptSink::new();
    app.set_transcript_sink(Arc::new(s.clone()));
    s
}

/// Press `Ctrl-S`, then the target key, as the two key events they are.
fn chord(app: &mut App, target: KeyCode) {
    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    app.update(Msg::Term(key(target, KeyModifiers::NONE)));
}

/// Push shell output into the Bash view the way the pty pump does.
fn shell_out(app: &mut App, chunk: &str) {
    app.update(Msg::BashOutput {
        session: bash_id(),
        stream: ByteStream::Merged,
        chunk: chunk.into(),
    });
}

/// **`Ctrl-S a` copies the last answer** in the modes that have answers,
/// through the looprs-pdl.10 sink and with the looprs-pdl.10 toast.
#[test]
fn the_chord_copies_the_last_answer_through_the_same_sink_and_toast() {
    for mode in [TerminalType::Pi, TerminalType::Beeds] {
        let (mut app, _rx) = app_with(mode);
        let rec = recording_clipboard(&mut app);
        settle_answers(&mut app, 3);

        chord(&mut app, KeyCode::Char('a'));

        assert_eq!(
            rec.last().as_deref(),
            // The last *answer*, whole: the entire entry, not the visible
            // window of it and not the one before it.
            Some("LINE02 aaaaaaaaaa"),
            "{:?} mode must copy the last answer",
            mode
        );
        let toast = app.toast().expect("the copy is confirmed");
        assert_eq!(toast.tone(), crate::state::toast::Tone::Good);
        assert_eq!(toast.text(), "Copied 17 characters \u{b7} clipboard");
    }
}

/// In Bash there is no answer to copy, and the refusal says so **and names
/// the chord that does work here** — ADR-0004 R17's rule that the failure
/// text carries the way out.
#[test]
fn bash_has_no_answer_to_copy_and_says_which_chord_does() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    shell_out(&mut app, "bash$ make test\ncompiled\n");

    chord(&mut app, KeyCode::Char('a'));

    assert!(rec.is_empty(), "nothing was copied: {:?}", rec.copies());
    let toast = app.toast().expect("but it was said");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert!(
        toast.text().contains("Ctrl-S o"),
        "the refusal names the chord that works: {}",
        toast.text()
    );
}

/// **`Ctrl-S o` copies one command, not the session.** The boundary is made
/// at submit, so a second command's block does not come wrapped in the first
/// one's — which is the whole reason the seal exists, because a Bash stream
/// of one kind is one entry by design.
#[test]
fn the_chord_copies_the_last_command_and_not_the_ones_before_it() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    shell_out(&mut app, "one\n");

    // Type a command and submit it. That submit is the boundary.
    app.input.set_text("make\n".to_string());
    app.update(Msg::Term(key(KeyCode::Enter, KeyModifiers::NONE)));
    let submitted = rx.try_recv();
    assert!(
        matches!(&submitted, Ok(UiCommand::Submit { mode, .. }) if *mode == TerminalType::Bash),
        "the submit went out: {submitted:?}"
    );
    shell_out(&mut app, "two\n");

    chord(&mut app, KeyCode::Char('o'));

    let copied = rec.last().expect("something was copied");
    assert!(
        copied.contains("two"),
        "the last command's output is there: {copied:?}"
    );
    assert!(
        !copied.contains("one"),
        "and the previous command's is not — that is the boundary working: {copied:?}"
    );
}

/// The artifact a real pty leaves, driven end to end. When a command
/// finishes, the shell comes back to its prompt and that prompt arrives as a
/// line of its own — a blank Bash line through the resolver — with the
/// `exit 0` note breaking the shell stream in between. Read as "the last
/// Bash entry", that blank *was* the command's output, and `Ctrl-S o`
/// refused to copy a screen full of the thing the user was pointing at.
/// Found by running the real binary in tmux (`spikes/tmux_keyboard_e2e.py`);
/// this is the test that keeps it fixed.
#[test]
fn the_chord_copies_the_command_past_the_shells_trailing_blank_line() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    app.input.set_text("echo MARKER\n".to_string());
    app.update(Msg::Term(key(KeyCode::Enter, KeyModifiers::NONE)));
    let _ = rx.try_recv();
    shell_out(&mut app, "bash-3.2$ echo MARKER\nMARKER\n");
    app.update(Msg::System {
        session: Some(bash_id()),
        text: "exit 0".into(),
    });
    // The prompt's carriage return: a Bash entry, and blank.
    shell_out(&mut app, "\r\n");

    chord(&mut app, KeyCode::Char('o'));

    let copied = rec.last().expect("a copy, not a refusal");
    assert!(copied.contains("MARKER"), "{copied:?}");
    let toast = app.toast().expect("a toast");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Good);
    assert!(toast.text().starts_with("Copied "), "{}", toast.text());
}

/// The other side of that same boundary. A command that ran and said nothing
/// gets a different sentence from one that was never asked, because they are
/// different facts about the shell and the user can only act on the right one.
#[test]
fn a_command_that_ran_and_said_nothing_says_produced_no_output() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    app.input.set_text("true\n".to_string());
    app.update(Msg::Term(key(KeyCode::Enter, KeyModifiers::NONE)));
    let _ = rx.try_recv();
    shell_out(&mut app, "\r\n");

    chord(&mut app, KeyCode::Char('o'));

    assert!(rec.is_empty(), "nothing was copied");
    assert!(
        app.toast().unwrap().text().contains("produced no output"),
        "{}",
        app.toast().unwrap().text()
    );
}

/// In the agentic modes the same chord answers the same question: what did
/// the last thing this session ran produce? There it is the last finished
/// tool card's result, not a shell block.
#[test]
fn the_chord_copies_the_last_tool_card_in_the_agentic_modes() {
    for mode in [TerminalType::Pi, TerminalType::Beeds] {
        let (mut app, _rx) = app_with(mode);
        let rec = recording_clipboard(&mut app);
        let id = SessionId::new(mode, 1);
        app.view_mut(id).transcript.start_tool(
            "t1".into(),
            "read".into(),
            "{\"path\":\"x\"}".into(),
        );
        app.view_mut(id)
            .transcript
            .finish_tool("t1".into(), "3 lines read".into(), false);

        chord(&mut app, KeyCode::Char('o'));

        assert_eq!(rec.last().as_deref(), Some("3 lines read"), "{mode:?}");
        assert!(
            app.toast()
                .expect("a toast")
                .text()
                .starts_with("Copied 12 characters"),
            "{:?}: {}",
            mode,
            app.toast().unwrap().text()
        );
    }
}

/// **`Ctrl-S s` copies whatever selection is live**, made by the mouse or by
/// anything else — the same paste, the same sink, the same toast as the drag
/// release that made it, which is the point of there being one path.
#[test]
fn the_chord_copies_the_live_selection_the_mouse_made() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 4);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    // The copy-on-release path is off, so the only way this clipboard gets
    // written is the chord — which is what makes this a test of the chord.
    app.set_copy_on_select(false);
    drag(&mut app, (y0 + a as u16, bx + 5), (y0 + b as u16, bx + 9));
    assert!(rec.is_empty(), "the release copied nothing, by request");
    assert!(app.selection().is_live());
    let pasted = app.selection_paste();

    chord(&mut app, KeyCode::Char('s'));

    assert_eq!(rec.last().as_deref(), Some(pasted.as_str()));
    assert!(
        app.toast()
            .expect("a toast")
            .text()
            .contains("characters \u{b7} clipboard"),
        "{}",
        app.toast().unwrap().text()
    );
}

/// **`Ctrl-S t` is the escape hatch**: the whole transcript, to a file, with
/// the count and the path both in the toast. The keyboard path for anything
/// too big for a paste buffer, on an injected sink exactly like the
/// clipboard's.
#[test]
fn the_chord_writes_the_whole_transcript_to_a_file_and_says_where() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let sink = recording_sink(&mut app);
    settle_answers(&mut app, 4);

    chord(&mut app, KeyCode::Char('t'));

    let dumped = sink.last().expect("the sink was handed the transcript");
    assert_eq!(
        dumped,
        app.active_view().unwrap().transcript.plain_text(),
        "the file's bytes are the whole transcript's bytes"
    );
    assert!(dumped.contains("LINE00") && dumped.contains("LINE03"));
    // The dump carries the mode it came from, so a pile of files from a
    // three-tab session can be told apart.
    assert_eq!(sink.dumps()[0].0, "pi", "the mode label went with the text");
    let toast = app.toast().expect("and said where");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Good);
    assert!(
        toast
            .text()
            .starts_with("Wrote 78 characters of transcript to "),
        "{}",
        toast.text()
    );
    assert!(
        toast.text().contains("fake-looprs-pi.txt"),
        "the path is in the toast: {}",
        toast.text()
    );
}

/// A dump goes through the sink and nowhere else: the clipboard is untouched,
/// because the verb in the toast is `Wrote` and a user who read `Copied`
/// would go looking for it in a paste buffer that never got it.
#[test]
fn a_dump_is_not_a_copy_and_says_wrote_not_copied() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let cb = recording_clipboard(&mut app);
    let sink = recording_sink(&mut app);
    settle_answers(&mut app, 2);

    chord(&mut app, KeyCode::Char('t'));

    assert_eq!(sink.count(), 1);
    assert!(cb.is_empty(), "the clipboard was not touched");
    assert!(
        !app.toast().unwrap().text().contains("Copied"),
        "{}",
        app.toast().unwrap().text()
    );
}

/// **`Esc` with the chord armed cancels the chord and nothing else** — not
/// the selection, not a cancel down the router. This is the branch the ticket
/// asks to be shown explicitly, and it is the one where getting it wrong is
/// the loudest: cancelling a running model call because the user changed
/// their mind about a copy is ADR-0003's named failure.
#[test]
fn esc_with_the_chord_armed_cancels_the_chord_only() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 4);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    app.set_copy_on_select(false);
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 3));
    assert!(app.selection().is_live(), "setup: a selection is live");

    // Arm, then back out.
    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));

    assert!(
        app.selection().is_live(),
        "Esc undid the prefix, not the selection: the most recent thing the user gave us was the chord"
    );
    assert!(rec.is_empty(), "and copied nothing");
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "and sent no cancel down the router"
    );

    // And the *next* Esc is the cancel the mode table already describes.
    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(!app.selection().is_live(), "that one is the selection's");
    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(
        matches!(rx.try_recv(), Ok(UiCommand::Cancel)),
        "and the third is the cancel"
    );
}

/// An unknown second key is **swallowed and named**, not delivered. A
/// keystroke typed after a prefix is intended as part of the chord, and the
/// two readings of it ("copy the transcript" / "insert a letter into the
/// command I was about to run") are far enough apart that guessing silently
/// is the worse option.
#[test]
fn an_unknown_second_key_is_swallowed_and_named() {
    for mode in TerminalType::ALL {
        let (mut app, _rx) = app_with(mode);
        let rec = recording_clipboard(&mut app);
        settle_answers(&mut app, 2);
        app.input.set_text(String::new());

        app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        app.update(Msg::Term(key(KeyCode::Char('z'), KeyModifiers::NONE)));

        assert_eq!(
            app.input.text(),
            "",
            "{mode:?}: the letter did not reach the box"
        );
        assert!(rec.is_empty(), "{mode:?}: nothing was copied");
        let toast = app.toast().expect("{mode:?}: and it was said why");
        assert_eq!(toast.tone(), crate::state::toast::Tone::Bad, "{mode:?}");
        assert!(toast.text().contains("`z`"), "{}", toast.text());
        assert!(toast.text().contains("Ctrl-S ?"), "{}", toast.text());

        // …and the chord really is gone: the next `a` types, it does not copy.
        let before = rec.count();
        app.update(Msg::Term(key(KeyCode::Char('a'), KeyModifiers::NONE)));
        assert_eq!(rec.count(), before, "{mode:?}: the chord is disarmed");
        assert_eq!(
            app.input.text(),
            "a",
            "{mode:?}: the letter reached the box"
        );
    }
}

/// **Nothing new shadows the box.** `a`/`o`/`s`/`t`/`?` are ordinary typing
/// until `Ctrl-S` says otherwise, and the audit for "did the prefix cost the
/// user a key" is this test run in all three modes with no prefix out.
#[test]
fn the_chord_letters_are_still_typing_when_no_chord_is_armed() {
    for mode in TerminalType::ALL {
        let (mut app, _rx) = app_with(mode);
        let rec = recording_clipboard(&mut app);
        let sink = recording_sink(&mut app);
        settle_answers(&mut app, 2);
        app.input.set_text(String::new());

        for c in ['a', 'o', 's', 't', '?'] {
            app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
        }

        assert_eq!(app.input.text(), "aost?", "{mode:?}: all of them typed");
        assert!(rec.is_empty(), "{mode:?}: nothing was copied");
        assert!(sink.is_empty(), "{mode:?}: nothing was dumped");
    }
}

/// The prefix closes on the tick, and the hint that named it has the same
/// TTL — so the user watching the screen sees the window shut rather than
/// pressing `a` two minutes later into a chord they thought was live.
#[test]
fn the_armed_chord_closes_on_the_tick() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 2);
    let t = Instant::now();
    app.on_tick(t);

    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    assert!(app.toast().is_some(), "the hint went up");

    app.on_tick(t + crate::app::COPY_CHORD_WINDOW);
    assert!(
        app.toast().is_none(),
        "the hint expired with the window: they are the same TTL"
    );

    app.input.set_text(String::new());
    app.update(Msg::Term(key(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert!(rec.is_empty(), "`a` did not copy");
    assert_eq!(app.input.text(), "a", "and typed instead");
}

/// A second `Ctrl-S` backs out of the chord, so getting into the prefix is
/// never a state the user has to find a third key to escape.
#[test]
fn a_second_ctrl_s_lowers_the_chord() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 2);
    app.input.set_text(String::new());

    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    app.update(Msg::Term(key(KeyCode::Char('a'), KeyModifiers::NONE)));

    assert!(rec.is_empty(), "nothing copied");
    // The second `Ctrl-S` is consumed by the chord — that is what lowering
    // it *is* — and the `a` after it is ordinary typing again.
    assert_eq!(
        app.input.text(),
        "a",
        "the letter typed once the chord was down"
    );
}

/// **Rule 1, driven.** `Ctrl-C` never copies, in any mode, with or without a
/// chord outstanding; and in Bash it is still the shell's interrupt.
#[test]
fn ctrl_c_never_copies_and_is_still_sigint_in_bash() {
    // Bash: the shell gets 0x03 by way of Cancel, the clipboard is dark, and
    // an armed chord does not change any of it.
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    let sink = recording_sink(&mut app);
    shell_out(&mut app, "a command's output\n");
    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    app.update(Msg::Term(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(
        matches!(rx.try_recv(), Ok(UiCommand::Cancel)),
        "Ctrl-C is still the shell's cancel in Bash"
    );
    assert!(rec.is_empty() && sink.is_empty(), "and never a copy");
    assert!(!app.should_quit, "and never a quit in Bash either");

    // Pi / Beeds: quit, as the table says, and still no copy.
    for mode in [TerminalType::Pi, TerminalType::Beeds] {
        let (mut app, _rx) = app_with(mode);
        let rec = recording_clipboard(&mut app);
        settle_answers(&mut app, 2);
        app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        app.update(Msg::Term(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(app.should_quit, "{mode:?}: Ctrl-C still quits here");
        assert!(rec.is_empty(), "{mode:?}: Ctrl-C never copied");
    }
}

/// **A full-screen child never gets XOFF from us.** We own `Ctrl-Q`, so a
/// forwarded `Ctrl-S` would stop a child's output with the chord needed to
/// restart it already spent: the freeze would be permanent without killing
/// the app. So the byte is swallowed in the one state where it could reach a
/// pty, and the chord is not armed there because nothing is copied out from
/// under a held screen (R12).
#[test]
fn a_child_holding_the_screen_is_never_sent_xoff() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    assert!(app.passthrough(), "setup: the child owns the screen");

    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));

    match rx.try_recv() {
        Err(mpsc::error::TryRecvError::Empty) => {}
        Ok(UiCommand::Keys { ref bytes, .. }) if bytes.contains(&0x13) => {
            panic!("XOFF was forwarded to the child: {bytes:?}")
        }
        Ok(other) => panic!("unexpected command while handing Ctrl-S over: {other:?}"),
        Err(e) => panic!("the router channel is gone: {e:?}"),
    }

    // And the chord is not live on the other side of the handover: with a
    // target key now it must neither copy nor reach the child as a letter.
    app.update(Msg::Term(key(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert!(
        rec.is_empty(),
        "nothing was copied while the child held the screen"
    );
}

/// A chord armed **before** a child grabs the screen is cancelled by the
/// grab, rather than surviving to copy something the user can no longer see.
#[test]
fn a_child_taking_the_screen_kills_an_armed_chord() {
    let (mut app, mut rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    shell_out(&mut app, "output before the handover\n");
    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));

    app.update(Msg::ScreenHeld {
        session: bash_id(),
        active: true,
    });
    app.update(Msg::Term(key(KeyCode::Char('o'), KeyModifiers::NONE)));

    assert!(rec.is_empty(), "the chord did not survive the handover");
    // The letter went to the child instead, which is what owning the keyboard
    // means: the keystroke is the child's, not ours.
    match rx.try_recv() {
        Ok(UiCommand::Keys { ref bytes, .. }) => {
            assert_eq!(bytes, b"o", "the child got the keystroke it owns")
        }
        other => panic!("expected the keystroke forwarded, got {other:?}"),
    }
}

/// A mode switch clears an armed chord: the prefix was armed over one mode's
/// transcript, and the next frame is another mode's rows.
#[test]
fn a_mode_switch_lowers_the_chord() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 2);
    app.input.set_text(String::new());

    app.update(Msg::Term(key(KeyCode::Char('s'), KeyModifiers::CONTROL)));
    app.update(Msg::Term(key(KeyCode::Tab, KeyModifiers::NONE)));
    app.update(Msg::Term(key(KeyCode::Char('a'), KeyModifiers::NONE)));

    assert!(rec.is_empty(), "the chord did not cross the mode boundary");
    assert_eq!(app.input.text(), "a", "the letter typed instead");
}

/// **`Ctrl-S ?` is the help**, and it is reachable from inside the app: a
/// user who has found the prefix can find every target in it without opening
/// a docs file, which is the whole reason the chord list is a constant next
/// to the code that shows it.
#[test]
fn the_help_lists_every_target_in_the_family() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 2);
    chord(&mut app, KeyCode::Char('?'));
    let toast = app.toast().expect("a toast");
    for target in [
        "a answer",
        "o last output",
        "s selection",
        "t transcript",
        "Esc",
    ] {
        assert!(
            toast.text().contains(target),
            "the help is missing {target}: {}",
            toast.text()
        );
    }
    assert_eq!(toast.text(), crate::session::view::copy_chord_hint());
}

/// A refusal is not silence. Every target has a "not there" case, and every
/// one of them says which target was asked for and why it was empty — the
/// keyboard asked a question and the answer was no, which R13's silence (for
/// a mouse gesture that was never a request) does not cover.
#[test]
fn every_missing_target_is_named_in_its_refusal() {
    // No answer at all.
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    chord(&mut app, KeyCode::Char('a'));
    assert!(rec.is_empty());
    assert_eq!(
        app.toast().expect("a refusal is said").tone(),
        crate::state::toast::Tone::Bad
    );
    assert!(app.toast().unwrap().text().contains("Nothing copied"));

    // A shell that is up but has not run anything yet: the view exists, and
    // what it does not have is a command block.
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    app.view_mut(bash_id())
        .push_note(MessageKind::System, "shell started".into());
    chord(&mut app, KeyCode::Char('o'));
    assert!(rec.is_empty());
    assert!(
        app.toast().unwrap().text().contains("no command has run"),
        "{}",
        app.toast().unwrap().text()
    );

    // A transcript with nothing selected in it.
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 2);
    assert!(!app.selection().is_live(), "setup: nothing selected");
    chord(&mut app, KeyCode::Char('s'));
    assert!(rec.is_empty());
    assert!(
        app.toast().unwrap().text().contains("nothing is selected"),
        "{}",
        app.toast().unwrap().text()
    );

    // A view that exists and holds nothing, for the dump: the other half of
    // "the transcript is empty" is "there is no transcript at all", and
    // both refuse rather than writing an empty file.
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let sink = recording_sink(&mut app);
    chord(&mut app, KeyCode::Char('t'));
    assert!(sink.is_empty(), "nothing was written");
    assert!(
        app.toast().unwrap().text().contains("Nothing written"),
        "{}",
        app.toast().unwrap().text()
    );
}

/// The dump can wedge like any other sink, and the user is owed a *late*
/// failure rather than a belief that their transcript is on disk.
#[test]
fn a_wedged_volume_reports_a_late_failure_not_silence() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let stall = stalling_sink(&mut app);
    settle_answers(&mut app, 2);
    let t = Instant::now();
    app.on_tick(t);

    chord(&mut app, KeyCode::Char('t'));
    assert_eq!(stall.inflight(), 1, "the dump is in the sink's hands");
    assert!(
        stall.dumps()[0].1.contains("LINE00"),
        "the text was handed over before the wedge: what is missing is the disk, not the transcript"
    );
    // Nothing has been promised yet: the only thing on screen is the chord's
    // own hint, and that is not a `Wrote`.
    if let Some(t) = app.toast() {
        assert!(!t.text().contains("Wrote"), "{}", t.text());
    }

    app.on_tick(t + crate::services::transcript_file::DUMP_TIMEOUT);
    let toast = app.toast().expect("a late failure is a toast");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert!(toast.text().contains("did not answer"), "{}", toast.text());
    assert_eq!(
        stall.inflight(),
        1,
        "the dump itself is still in flight: we abandoned the receipt, not the write"
    );
}

/// A dump that answers late — after the deadline was reported — cannot come
/// back and paint `Wrote` over the failure the user was already given.
#[test]
fn a_late_dump_answer_cannot_undraw_the_failure() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let stall = stalling_sink(&mut app);
    settle_answers(&mut app, 2);
    let t = Instant::now();
    app.on_tick(t);

    chord(&mut app, KeyCode::Char('t'));
    app.on_tick(t + crate::services::transcript_file::DUMP_TIMEOUT);
    let failed = app.toast().unwrap().text().to_string();

    stall.answer_all(crate::services::transcript_file::DumpOutcome::Written {
        path: std::path::PathBuf::from("/tmp/late.txt"),
        chars: crate::services::clipboard::Chars::of("x"),
    });
    // One breath after the deadline, while the failure is still on screen:
    // this is the moment a late `Wrote` would do its damage.
    app.on_tick(t + crate::services::transcript_file::DUMP_TIMEOUT + Duration::from_millis(1));
    assert_eq!(
        app.toast().expect("the failure toast is still up").text(),
        failed,
        "the late answer was dropped rather than redrawn over the failure"
    );
    assert!(
        !app.toast().unwrap().text().contains("Wrote"),
        "{}",
        app.toast().unwrap().text()
    );
}

/// **The scroll keys in all three modes**, which is the acceptance line, and
/// with the store's own pin semantics rather than a second set: a page up
/// unpinning, `End` re-pinning, `Home` the top of the transcript, and
/// none of them ever reaching across to another mode's store.
#[test]
fn the_page_keys_scroll_every_mode_with_the_wheels_semantics() {
    for mode in TerminalType::ALL {
        let (mut app, _rx) = app_with(mode);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let page = app.transcript_band_rows() as isize;
        assert!(app.pinned(), "{mode:?}: starts pinned");

        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert_eq!(
            app.scrollback().offset(),
            page as usize,
            "{mode:?}: one page up"
        );
        assert!(!app.pinned(), "{mode:?}: a page up unpins");

        app.update(Msg::Term(key(KeyCode::PageDown, KeyModifiers::NONE)));
        assert_eq!(
            app.scrollback().offset(),
            0,
            "{mode:?}: and back down again"
        );
        assert!(app.pinned(), "{mode:?}: reaching the bottom re-pins");

        app.update(Msg::Term(key(KeyCode::Home, KeyModifiers::NONE)));
        let band = app.transcript_band_rows();
        assert_eq!(
            app.transcript_window(band).first().map(|r| r.entry),
            Some(0),
            "{mode:?}: Home is the top of the transcript, not the top of the screen"
        );

        app.update(Msg::Term(key(KeyCode::End, KeyModifiers::NONE)));
        assert!(app.pinned(), "{mode:?}: End re-pins");

        // The other mode's store did not move: three views, three offsets.
        let other = mode.next();
        let (mut app2, _rx2) = app_with(other);
        settle_deep(&mut app2);
        app2.update(Msg::Term(key(KeyCode::Home, KeyModifiers::NONE)));
        assert_eq!(
            app.scrollback().offset(),
            0,
            "{mode:?}: End put us back at the tail, and the other mode was never touched"
        );
        assert!(
            app2.scrollback().offset() > 0,
            "{other:?}: its own Home moved its own store"
        );
    }
}

/// The other failure that is not a timeout: the sink answered, and the answer
/// was no. The sink's own reason is what the user reads — a `Failed` that
/// arrived and produced no toast is the same silence as a wedge, and worse,
/// because something did have time to say what went wrong.
#[test]
fn a_dump_the_sink_refuses_reports_the_sinks_own_reason() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let sink = crate::testing::RecordingTranscriptSink::answering(
        crate::services::transcript_file::DumpOutcome::Failed {
            reason: "/tmp: read-only file system".into(),
        },
    );
    app.set_transcript_sink(Arc::new(sink.clone()));
    settle_answers(&mut app, 2);

    chord(&mut app, KeyCode::Char('t'));

    assert_eq!(sink.count(), 1, "the dump was attempted");
    let toast = app.toast().expect("and reported");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert_eq!(
        toast.text(),
        "Nothing written: /tmp: read-only file system",
        "the reason is passed through, not wrapped in a guess"
    );
}
