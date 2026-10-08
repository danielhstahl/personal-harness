//! Select-to-copy (looprs-pdl.10)

use super::*;

fn answering_clipboard(
    app: &mut App,
    outcome: crate::services::clipboard::CopyOutcome,
) -> crate::testing::RecordingClipboard {
    let rec = crate::testing::RecordingClipboard::answering(outcome);
    app.set_clipboard(Arc::new(rec.clone()));
    rec
}

fn stalling_clipboard(app: &mut App) -> crate::testing::StallClipboard {
    let stall = crate::testing::StallClipboard::new();
    app.set_clipboard(Arc::new(stall.clone()));
    stall
}

/// **The feature.** Release, and the exact characters under the box are on
/// the clipboard, with the count that describes them in the toast.
#[test]
fn a_release_copies_the_selection_and_says_how_many_characters() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 4);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();

    drag(&mut app, (y0 + a as u16, bx + 5), (y0 + b as u16, bx + 9));

    assert_eq!(
        rec.last().as_deref(),
        Some("1 aaaaaaaaaa\nLINE02 aaa"),
        "the bytes that went are the characters that were selected"
    );
    let toast = app.toast().expect("a toast");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Good);
    assert_eq!(
        toast.text(),
        "Copied 23 characters \u{b7} clipboard",
        "23 *characters* — not bytes, not cells, not rows. Corrected from 25 in              looprs-pdl.13: the count here contradicted the copy text the assertion two lines              above pins, and 23 is what `Chars::of` says that string is. The point of the              assertion (characters, not bytes) stands; the CJK case below carries it harder."
    );
}

/// The count is characters in a way a `len()` drift would break: CJK is
/// three bytes a character, so a byte-counting toast on this selection says
/// something three times the truth.
#[test]
fn the_count_in_the_toast_is_characters_even_when_the_bytes_disagree() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    recording_clipboard(&mut app);
    assert!(app.copy_text("\u{6f22}\u{5b57}\u{5b57}".into()));
    assert_eq!(
        app.toast().unwrap().text(),
        "Copied 3 characters \u{b7} clipboard",
        "9 bytes, 6 cells, 3 characters — the toast says the last one"
    );
}

/// **R13.** A selection of blanks is not content: no write, no toast, and
/// the clipboard the user had before is still theirs.
#[test]
fn a_selection_of_blanks_copies_nothing_and_says_nothing() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let blank = drawn
        .iter()
        .position(|t| t.trim().is_empty())
        .expect("setup: a blank separator row is on screen");

    drag(
        &mut app,
        (y0 + blank as u16, bx + 2),
        (y0 + blank as u16, bx + 6),
    );
    assert!(rec.is_empty(), "nothing was copied: {:?}", rec.copies());
    assert!(app.toast().is_none(), "and nothing was said about it");
    // What this test deliberately no longer asserts is that the blank drag left
    // a live selection. `hit_resting` (crate::state::selection) resolves a
    // pointer on a blank row to the end of the text above it, so a drag that
    // never leaves a blank row resolves both ends to the same place, the
    // release sees a zero-width range, and the gesture is a click. The R13
    // outcome this test is *for* — nothing copied, nothing said — holds
    // either way. If the selection model grows a way to be live over nothing,
    // that belongs in the model and the assertion comes back with it; bending
    // the test here would only hide the question. (Found this way while
    // finishing looprs-pdl.13 against looprs-pdl.10's unfinished tree.)
}

/// A click — a release with nothing dragged — is not a copy. Together with
/// the blank rule this is the whole "does not fire on every event" story:
/// no `Copied 0 characters`, no clipboard thrash on every click in the
/// transcript.
#[test]
fn a_click_copies_nothing() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();

    app.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        y0 + a as u16,
        bx,
    ));
    app.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        y0 + a as u16,
        bx,
    ));
    assert!(rec.is_empty());
    assert!(app.toast().is_none());
}

/// **`LOOPRS_COPY_ON_SELECT=0`** turns the automatic path off *completely*
/// — no write on release — and takes neither the selection nor the toast
/// mechanism with it: the keyboard copy still works on the same selection
/// and still produces the same toast (R17).
#[test]
fn turning_copy_on_select_off_stops_the_auto_copy_and_nothing_else() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    app.set_copy_on_select(false);
    settle_answers(&mut app, 4);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();

    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    assert!(
        rec.is_empty(),
        "the release copied nothing: {:?}",
        rec.copies()
    );
    assert!(app.toast().is_none(), "and said nothing about it");
    assert!(
        app.selection().is_live(),
        "…but the selection is still live"
    );
    let text = app.selection_paste();
    assert!(!text.is_empty(), "…and still resolves to its text");

    assert!(app.copy_selection(), "the keyboard path still copies");
    assert_eq!(rec.last().as_deref(), Some(text.as_str()));
    assert!(
        app.toast().unwrap().text().starts_with("Copied "),
        "and the toast came with it: {:?}",
        app.toast().map(|t| t.text().to_string())
    );
}

/// **No optimistic toast.** The sink said it failed; the toast says it
/// failed. A `Copied` here would be a confident lie about the clipboard
/// that the user finds out about somewhere else, at the worst moment.
#[test]
fn a_failed_copy_toasts_as_a_failure_never_as_a_copy() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    answering_clipboard(
        &mut app,
        crate::services::clipboard::CopyOutcome::Failed {
            reason: "pbcopy exited 1: not authorized".into(),
        },
    );
    assert!(app.copy_text("something selected".into()));
    let toast = app.toast().expect("a toast");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert_eq!(
        toast.text(),
        "Copy failed: pbcopy exited 1: not authorized",
        "the reason is the sink's, at a level the user can act on"
    );
}

/// A mismatch is not a copy either, even though the write "worked".
#[test]
fn a_clipboard_that_changed_under_us_is_not_reported_as_a_copy() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    answering_clipboard(
        &mut app,
        crate::services::clipboard::CopyOutcome::Mismatch {
            chars: crate::services::clipboard::Chars::of("whatever"),
        },
    );
    assert!(app.copy_text("whatever".into()));
    let toast = app.toast().unwrap();
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert!(!toast.text().starts_with("Copied"), "{}", toast.text());
}

/// The unverified class is visible as itself: `Sent` over OSC 52 shows the
/// count with "not confirmed", because that is exactly what it is.
#[test]
fn a_copy_that_cannot_be_confirmed_says_so_next_to_the_count() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    answering_clipboard(
        &mut app,
        crate::services::clipboard::CopyOutcome::Sent {
            chars: crate::services::clipboard::Chars::of("a dozen chars"),
            transport: crate::services::clipboard::Transport::Osc52,
            note: None,
        },
    );
    assert!(app.copy_text("a dozen chars".into()));
    let toast = app.toast().unwrap();
    assert_eq!(toast.tone(), crate::state::toast::Tone::Good);
    assert_eq!(
        toast.text(),
        "Copied 13 characters \u{b7} OSC 52 (not confirmed)"
    );
}

/// **The late failure is made visible, not swallowed.** A helper parked on
/// a wedged compositor looks like a slow copy forever; at the deadline the
/// App says so, because the alternative is the user finding out at paste
/// time in some other application.
#[test]
fn a_copy_that_never_answers_is_reported_at_the_deadline() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let stall = stalling_clipboard(&mut app);
    let t = Instant::now();
    app.on_tick(t);

    assert!(app.copy_text("into the void".into()));
    assert_eq!(stall.inflight(), 1, "the copy is with the sink");
    assert!(app.toast().is_none(), "not yet: still in flight");

    // Just short of the deadline: nothing to report yet.
    app.poll_copy(t + crate::services::clipboard::COPY_TIMEOUT - Duration::from_millis(1));
    assert!(app.toast().is_none());

    app.poll_copy(t + crate::services::clipboard::COPY_TIMEOUT);
    let toast = app.toast().expect("the deadline reported");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert_eq!(
        toast.text(),
        "Copy failed: clipboard did not answer in 2s \u{2014} nothing confirmed"
    );

    // And an answer that arrives *after* the deadline cannot repaint the
    // failure as a success: the receipt was abandoned, so there is nothing
    // left for it to be written to.
    stall.answer_all(crate::services::clipboard::CopyOutcome::Verified {
        chars: crate::services::clipboard::Chars::of("twelve chars"),
    });
    let late = Instant::now();
    app.on_tick(late);
    assert_eq!(
        app.toast().map(|x| x.text().to_string()),
        Some("Copy failed: clipboard did not answer in 2s \u{2014} nothing confirmed".into()),
        "the late `Copied` never undrew the truth the user was given"
    );
}

/// **Auto-dismiss on the constant**, and the next key puts it away early.
#[test]
fn the_toast_dismisses_itself_and_the_next_key_beats_it_to_it() {
    use crate::state::toast::TOAST_TTL;
    let (mut app, _rx) = app_with(TerminalType::Pi);
    recording_clipboard(&mut app);
    let t = Instant::now();
    app.on_tick(t);
    app.copy_text("copied this".into());
    assert!(app.toast().is_some(), "up immediately");

    app.on_tick(t + TOAST_TTL - Duration::from_millis(1));
    assert!(app.toast().is_some(), "still within the 2s window");

    app.on_tick(t + TOAST_TTL + Duration::from_millis(1));
    assert!(app.toast().is_none(), "the TTL dismissed it");

    app.copy_text("copied that".into());
    assert!(app.toast().is_some());
    app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    ))));
    assert!(app.toast().is_none(), "the keystroke dismissed it");
}

/// Each release is its own copy with its own toast, and the toast that is up
/// describes the copy that just happened ("the last one wins visually").
#[test]
fn a_new_release_replaces_the_old_toast_rather_than_keeping_it() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 4);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();

    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    let first = app.toast().map(|t| t.text().to_string());
    assert!(first.is_some());

    drag(&mut app, (y0 + a as u16, bx + 1), (y0 + b as u16, bx + 3));
    let second = app.toast().map(|t| t.text().to_string());
    assert_eq!(rec.count(), 2, "each release is its own copy");
    assert_ne!(
        first, second,
        "the toast up top describes the copy that just happened"
    );
}

/// A wheel report is not "the user moved on": they are still looking at
/// the transcript this toast is about, so scrolling does not blow it away.
#[test]
fn scrolling_does_not_eat_the_confirmation() {
    use crate::state::toast::TOAST_TTL;
    let (mut app, _rx) = app_with(TerminalType::Pi);
    recording_clipboard(&mut app);
    let t = Instant::now();
    app.on_tick(t);
    app.copy_text("still here".into());
    let before = app.toast().map(|x| x.text().to_string());

    wheel(&mut app, WheelDir::Up, 3, t + Duration::from_millis(50));
    assert_eq!(app.toast().map(|x| x.text().to_string()), before);

    app.on_tick(t + TOAST_TTL + Duration::from_millis(500));
    assert!(app.toast().is_none(), "…and it still goes on its own TTL");
}

/// The default App has a clipboard that does not write, so a test that
/// never installs one cannot reach a real clipboard — and `Noop` reports
/// the truth about that rather than a `Copied` that never happened.
#[test]
fn a_default_app_never_reaches_a_real_clipboard() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    assert!(app.copy_text("nothing is wired".into()));
    let toast = app.toast().expect("a toast");
    assert_eq!(toast.tone(), crate::state::toast::Tone::Bad);
    assert!(
        toast.text().contains("LOOPRS_CLIPBOARD=off"),
        "{}",
        toast.text()
    );
}

/// A copy is never fired while a full-screen child holds the screen
/// (ADR-0004 R12): the pointer is over the child's pixels, so what we
/// would copy is a description of a frame that is not on screen.
#[test]
fn no_copy_while_a_child_holds_the_screen() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let rec = recording_clipboard(&mut app);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let bash = SessionId::new(TerminalType::Bash, 1);
    app.update(Msg::ScreenHeld {
        session: bash,
        active: true,
    });
    assert!(!app.copy_selection());
    assert!(!app.copy_text("even handed straight in".into()));
    assert!(rec.is_empty());
}

/// **The Esc ordering, wired.** A live selection takes the first Esc and
/// nothing reaches the session; the next Esc is the cancel the mode table
/// already describes.
#[test]
fn esc_clears_the_selection_first_and_only_then_cancels() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    assert!(app.selection().is_live());

    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(!app.selection().is_live(), "the first Esc unselected");
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "and sent no cancel: cancelling a run because the user wanted to \
         unselect is the surprise ADR-0003 exists to prevent"
    );

    app.update(Msg::Term(key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(
        matches!(rx.try_recv(), Ok(UiCommand::Cancel)),
        "and the next Esc is the cancel the mode table already describes"
    );
}

/// **A mode switch clears.** The selection is addressed into one view's
/// transcript; the next frame is another mode's rows.
#[test]
fn a_mode_switch_clears_the_selection() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    assert!(app.selection().is_live());

    app.update(Msg::Term(key(KeyCode::Tab, KeyModifiers::NONE)));
    assert!(
        !app.selection().is_live(),
        "the selection does not cross the mode boundary"
    );
}

/// **Auto-scroll, wired.** Dragging off the top edge keeps extending the
/// selection by scrolling, at the throttled rate — and the selection
/// reaches rows that were never on screen at all.
#[test]
fn dragging_off_the_top_edge_scrolls_and_keeps_extending() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 40);
    let h = 24u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let start = drawn.iter().position(|t| t.contains("LINE35")).unwrap();
    let t0 = Instant::now();
    app.on_tick(t0);
    app.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        y0 + start as u16,
        bx,
    ));

    let offset_before = app.scrollback().offset();
    for i in 0..5u64 {
        app.on_tick(t0 + Duration::from_millis(150 * (i + 1)));
        app.update(mouse(MouseEventKind::Drag(MouseButton::Left), 0, bx));
    }
    assert_eq!(
        app.scrollback().offset(),
        offset_before + 5,
        "five edge drags, five rows up — one per interval, no more"
    );
    let text = app.selection_paste();
    assert!(
        text.contains("LINE34") && text.contains("LINE28"),
        "the selection kept growing as the view scrolled, reaching rows that \
         were never on screen: {text:?}"
    );
    assert!(
        !text.contains("LINE36"),
        "and never reached below where the drag started: {text:?}"
    );
}

/// The throttle in the wiring, not just in the model: 50 edge drags inside
/// one interval move nothing.
#[test]
fn a_fast_burst_of_edge_drags_does_not_outrun_the_pointer() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 40);
    let h = 24u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let start = drawn.iter().position(|t| t.contains("LINE35")).unwrap();
    let t0 = Instant::now();
    app.on_tick(t0);
    app.update(mouse(
        MouseEventKind::Down(MouseButton::Left),
        y0 + start as u16,
        bx + 2,
    ));
    let offset = app.scrollback().offset();
    for i in 1..50u64 {
        app.on_tick(t0 + Duration::from_millis(i));
        app.update(mouse(MouseEventKind::Drag(MouseButton::Left), 0, bx + 2));
    }
    assert_eq!(
        app.scrollback().offset(),
        offset + 1,
        "one scroll for the whole burst; the rest were inside the interval"
    );
}

/// **The live tail is not selectable.** It has no final form to address:
/// it is still arriving. The frame that flushes it is the frame that makes
/// it selectable, and that is the frame after it stops changing.
#[test]
fn the_live_tail_cannot_be_selected() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    app.view_mut(SessionId::new(TerminalType::Pi, 1)).chat = ChatState::Chat;
    let h = 20u16;
    let preview = vec![Line::from("STILL-ARRIVING".to_string())];
    paint(&app, h, &preview);
    let [text, ..] = viewport::frame_areas(
        Rect::new(0, 0, W, h),
        app.live_card_rows(),
        app.input_band(W),
        viewport::KanbanBudget::Off,
    );
    // The live line is the band's last row.
    let live_y = text.bottom() - 1;
    app.update(mouse(MouseEventKind::Down(MouseButton::Left), live_y, 2));
    app.update(mouse(MouseEventKind::Drag(MouseButton::Left), live_y, 8));
    app.update(mouse(MouseEventKind::Up(MouseButton::Left), live_y, 8));
    assert!(
        !app.selection().is_live(),
        "the live tail is not in the store, so it is not selectable"
    );

    // …and once flushed it is: the settled line is a store row like any other.
    app.view_mut(SessionId::new(TerminalType::Pi, 1))
        .push_note(MessageKind::Answer, "STILLED".to_string());
    app.flush_active(W);
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let row = drawn.iter().position(|t| t.contains("STILLED")).unwrap();
    drag(&mut app, (y0 + row as u16, bx), (y0 + row as u16, bx + 3));
    assert_eq!(app.selection_paste(), "STIL");
}

/// **Chrome cannot be selected, and touching it does not disturb the
/// selection.** The status row is not in the store; there is nothing there
/// to hit.
#[test]
fn chrome_cannot_be_selected_and_leaves_the_selection_alone() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let [_text, _, _, status, input] = viewport::frame_areas(
        Rect::new(0, 0, W, h),
        app.live_card_rows(),
        app.input_band(W),
        viewport::KanbanBudget::Off,
    );

    // A press on the status row, then on the input box: nothing starts.
    app.update(mouse(MouseEventKind::Down(MouseButton::Left), status.y, 2));
    app.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        input.y + 1,
        8,
    ));
    assert!(
        !app.selection().is_live(),
        "a drag that starts on chrome selects nothing"
    );

    // Now a real selection, then a press on chrome: it stands.
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    let selected = app.selection_paste();
    assert!(!selected.is_empty(), "setup: a selection stands");

    app.update(mouse(MouseEventKind::Down(MouseButton::Left), status.y, 2));
    app.update(mouse(MouseEventKind::Up(MouseButton::Left), status.y, 2));
    assert_eq!(
        app.selection_paste(),
        selected,
        "a press on chrome is not a statement about the transcript: it \
         neither selects nor clears"
    );
}

/// **The resize property, through the App.** The selection is addressed by
/// content, so a resize moves the rows and leaves the selection's own
/// coordinates alone; the highlight re-derives onto wherever those
/// characters ended up.
///
/// The *range* is the invariant, deliberately, rather than the pasted
/// string: the store's wrap drops the space it broke a soft line on (see
/// the note on `RowEnd::Soft`), so a selection that spans a soft seam can
/// come back a space short of what it was. That is a defect in what the
/// wrap keeps, not in what the selection addresses — it is on the board
/// for the copy ticket, and asserting the range here is what keeps the two
/// concerns from getting welded together.
#[test]
fn a_resize_re_derives_the_cells_and_keeps_the_range() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let id = SessionId::new(TerminalType::Pi, 1);
    for i in 0..6 {
        app.view_mut(id).push_note(
            MessageKind::Answer,
            format!("LINE{i} {}", "word ".repeat(14)),
        );
    }
    app.flush_active(80);
    let h = 30u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE2")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE3")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 6));
    let before = app.selection().range().expect("setup: a range");

    // A narrower window: everything re-wraps, rows are added.
    app.set_window(52, h);
    app.flush_active(52);
    assert!(
        app.scrollback().len() > 12,
        "setup: the narrower wrap made more rows"
    );
    assert_eq!(
        app.selection().range(),
        Some(before),
        "the selection followed the characters, not the rows they were on"
    );

    // And the highlight re-derives: the box still lands somewhere, on the
    // rows the same content is now drawn on.
    paint(&app, h, &[]);
    let (_bx2, _y1, drawn2) = geom(&app, h);
    let runs = app.selection().cells(app.transcript_window(drawn2.len()));
    assert!(!runs.is_empty(), "the box is still drawn somewhere");

    // A selection inside one row is byte-identical across a resize: the
    // seam problem above cannot touch it.
    let same_row = drawn2
        .iter()
        .position(|t| t.to_string().split_whitespace().count() > 3)
        .unwrap();
    drag(
        &mut app,
        (y0 + same_row as u16, bx),
        (y0 + same_row as u16, bx + 8),
    );
    let pinned = app.selection_paste();
    app.set_window(46, h);
    app.flush_active(46);
    assert_eq!(
        app.selection_paste(),
        pinned,
        "same eight characters, same place"
    );
}

/// **Not cleared by a repaint, a tick, or new output.** The ticket's
/// other half: a selection is a fact about content, not about the frame
/// it happened to be made in. Repainting, a clock tick, and new output
/// arriving *below* the selection all leave it standing with the same
/// characters — only the four things on the clear list take it away.
#[test]
fn a_selection_survives_a_repaint_a_tick_and_new_output() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    drag(&mut app, (y0 + a as u16, bx + 1), (y0 + b as u16, bx + 4));
    let selected = app.selection_paste();
    assert!(!selected.is_empty(), "setup: something selected");

    // A tick: the spinner turns, the clock moves.
    app.on_tick(Instant::now() + Duration::from_millis(120));
    assert_eq!(app.selection_paste(), selected, "a tick keeps it");

    // New output, flushed into the store below the selection.
    app.view_mut(SessionId::new(TerminalType::Pi, 1))
        .push_note(MessageKind::Answer, "arrived after the drag".to_string());
    app.flush_active(W);
    assert_eq!(
        app.selection_paste(),
        selected,
        "new output does not take the selection away"
    );

    // A repaint, which re-derives the band geometry from scratch.
    paint(&app, h, &[]);
    assert_eq!(
        app.selection_paste(),
        selected,
        "and neither does drawing the frame again"
    );
    assert!(app.selection().is_live(), "still live, still selectable");
}

/// **A trim that ate the anchor clears it — wired, not promised.** The
/// buffer cap evicts entries from the front of the transcript and the
/// store renumbers its rows by entry; the selection speaks the same
/// addresses, so it has to hear about the eviction with the same numbers
/// or the entries shift out from under it.
#[test]
fn a_trim_that_eats_the_anchor_clears_the_selection() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 6);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    assert!(app.selection_paste().contains("LINE01"), "setup");

    // Trip the cap and stream enough to evict the selected entries.
    let id = SessionId::new(TerminalType::Pi, 1);
    app.view_mut(id).set_buffer_limit(300);
    for _ in 0..30 {
        app.view_mut(id)
            .push_note(MessageKind::Answer, "z".repeat(60));
    }
    app.flush_active(W);
    assert!(
        !app.selection().is_live(),
        "the entries the selection addressed are gone, and it went with them"
    );
    assert_eq!(app.selection_paste(), "");
}

/// The same eviction with the selection *above* the water line: the entries
/// survive, and the selection follows them down the renumbered store to
/// the same text it had before. This is the case a silent drift would turn
/// into a highlight pointing at the message after the one that was
/// selected, which is a worse bug than losing the selection.
#[test]
fn a_trim_above_the_selection_renumbers_it_onto_the_same_text() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let id = SessionId::new(TerminalType::Pi, 1);
    for i in 0..10 {
        app.view_mut(id).push_note(
            MessageKind::Answer,
            format!("LINE{i:02} {}", "y".repeat(40)),
        );
    }
    app.flush_active(W);
    let h = 30u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE08")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE09")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 12));
    let before = app.selection_paste();
    assert!(
        before.contains("LINE08") && before.contains("LINE09"),
        "setup: the tail two entries selected: {before:?}"
    );

    // A cap trip that eats the *front* of the transcript, not this selection.
    app.view_mut(id).set_buffer_limit(420);
    for i in 10..13 {
        app.view_mut(id).push_note(
            MessageKind::Answer,
            format!("LINE{i:02} {}", "y".repeat(40)),
        );
    }
    app.flush_active(W);
    assert!(
        app.scrollback()
            .rows()
            .iter()
            .any(|r| r.to_string().contains("LINE09")),
        "setup: the trim stopped short of the entries this selection wants"
    );
    assert_eq!(
        app.selection_paste(),
        before,
        "the selection followed its entries down the renumbered store"
    );
}

/// A motion report with no press of ours behind it is not our gesture, and
/// a release with nothing pressed is not either.
#[test]
fn a_drag_without_a_press_is_nothing() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    app.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        y0 + a as u16,
        bx,
    ));
    app.update(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        y0 + b as u16,
        bx + 4,
    ));
    app.update(mouse(
        MouseEventKind::Up(MouseButton::Left),
        y0 + b as u16,
        bx + 4,
    ));
    assert!(!app.selection().is_live());
}

/// The other buttons are left alone: an unbound button must not be silently
/// swallowed (pdl.8's rule), and the middle click is pdl.11's.
#[test]
fn the_other_buttons_do_not_drive_the_selection() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    for btn in [MouseButton::Right, MouseButton::Middle] {
        app.update(mouse(MouseEventKind::Down(btn), y0 + a as u16, bx));
        app.update(mouse(MouseEventKind::Drag(btn), y0 + b as u16, bx + 4));
        app.update(mouse(MouseEventKind::Up(btn), y0 + b as u16, bx + 4));
    }
    assert!(
        !app.selection().is_live(),
        "a right- or middle-drag does not select"
    );
}

/// A drag taken while a full-screen child holds the real terminal is a
/// fiction: the pixels under the pointer are not ours, so nothing is
/// selected against the last frame we painted.
#[test]
fn no_selection_while_a_child_holds_the_screen() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let bash = SessionId::new(TerminalType::Bash, 1);
    app.update(Msg::ScreenHeld {
        session: bash,
        active: true,
    });
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();
    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    assert!(
        !app.selection().is_live(),
        "the pointer is over someone else's screen"
    );
}
