//! The input box: its own height policy, and the keys it has to swallow rather
//! than forward.
//!
//! The box and the frame's height policy are the same fact measured twice, so most
//! of what is asserted here is the two agreeing — `input_rows` is what the box's
//! own wrapping says it needs, capped. The other half is the keys that stop at the
//! box: arrows, Home/End, Shift-Tab. A `Left` that leaked down the command channel
//! arrives in a live shell as `ESC [ D` — a history search, or half an escape
//! sequence handed to whatever the child happens to be running.

use super::*;

/// The wiring between the box and the height policy: what the app asks the frame
/// for *is* what the box's own wrapping needs, capped. If those two drift the
/// box gets cut off, or the pane grows a band of blank space nobody can
/// explain from a screenshot.
#[test]
fn the_app_asks_for_the_rows_the_box_actually_wraps_into() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let width = 22u16; // inner width 20
    for n in 0usize..=200 {
        app.input.set_text("x".repeat(n));
        let inner = inner_width(width);
        let wrapped = app.input.display_lines(inner).len() as u16;
        assert_eq!(
            app.input_rows(width),
            viewport::input_rows(wrapped),
            "{n} characters typed"
        );
    }
    // Grows with the text, then stops at the cap.
    app.input.set_text("x".to_string());
    assert_eq!(app.input_rows(width), viewport::MIN_INPUT_ROWS);
    app.input.set_text("x".repeat(40)); // two rows of 20 cells
    assert_eq!(app.input_rows(width), viewport::MIN_INPUT_ROWS + 1);
    app.input.set_text("x".repeat(200)); // ten rows wanted, the cap wins
    assert_eq!(app.input_rows(width), viewport::MAX_INPUT_ROWS);
}

/// **The arrow keys belong to the box, not to a session.** A `Left` that
/// leaked down the command channel would arrive in the shell as `ESC [ D` — a
/// history search, or half an escape sequence handed to whatever program is
/// running. Nothing goes out; the caret moves and the text is untouched.
#[test]
fn arrow_keys_are_consumed_by_the_box_and_reach_no_session() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    for c in "ab cd".chars() {
        app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
    }
    for code in [
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::BackTab,
        KeyCode::Delete,
    ] {
        app.update(Msg::Term(key(code, KeyModifiers::NONE)));
    }
    assert_eq!(
        app.input.text(),
        "ab cd\n",
        "the keys edited the box (Shift-Tab broke the line) and lost nothing"
    );
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "not one keystroke went out to a session"
    );
}

/// The multi-line box is still one message on the wire: a newline in the
/// middle changes nothing about what `Enter` means.
#[test]
fn a_two_line_box_submits_one_command_with_both_lines_in_it() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    for c in "first".chars() {
        app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
    }
    app.update(Msg::Term(key(KeyCode::BackTab, KeyModifiers::NONE)));
    for c in "second".chars() {
        app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
    }
    app.update(Msg::Term(key(KeyCode::Enter, KeyModifiers::NONE)));
    match rx.try_recv() {
        Ok(UiCommand::Submit { mode, text }) => {
            assert_eq!(mode, TerminalType::Pi);
            assert_eq!(text, "first\nsecond");
        }
        other => panic!("expected exactly one Submit, got {other:?}"),
    }
}

/// The one place "is the box showing?" and "how tall is it?" become a single
/// number, so the height policy and the drawn box cannot each make their own
/// mind up. Hidden means *zero rows granted*, not a box drawn somewhere else:
/// that is what lets the status row sit on the bottom edge of the live region
/// instead of hanging above a band nothing is drawn into.
#[test]
fn the_input_band_is_the_box_when_it_shows_and_nothing_when_it_is_hidden() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.input.set_text("a question to type".to_string());
    assert!(app.need_input(), "nothing running: the box is open");
    assert_eq!(
        app.input_band(60),
        app.input_rows(60),
        "a showing box is budgeted its measured height"
    );

    app.view_mut(pi_id())
        .set_status(SessionStatus::Running, Instant::now());
    assert!(!app.need_input(), "Pi is mid-run: the box is hidden");
    assert_eq!(
        app.input_band(60),
        viewport::NO_INPUT_ROWS,
        "a hidden box must be granted nothing, not its old height"
    );

    // Bash always takes the keyboard, so its band is always the box — even
    // while a command is running (it is the human's shell).
    let (mut bash, _rx) = app_with(TerminalType::Bash);
    bash.view_mut(crate::session::SessionId::new(TerminalType::Bash, 1))
        .set_status(SessionStatus::Running, Instant::now());
    assert!(bash.need_input(), "bash is always open");
    assert_eq!(bash.input_band(60), bash.input_rows(60));
}
