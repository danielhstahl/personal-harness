//! The wheel and the trackpad (looprs-pdl.8)

use super::*;

/// The frame's five bands at this height, with the transcript's own band
/// first — the same call the draw makes, so a row classified against these
/// rectangles is classified against the pixels.
fn bands(app: &App, h: u16) -> viewport::FrameAreas {
    viewport::frame_areas(
        Rect::new(0, 0, W, h),
        app.live_card_rows(),
        app.input_band(W),
        viewport::KanbanBudget::Off,
    )
}

/// Paint with the input band the app is actually asking for — the value the
/// run loop hands `crate::view` — so the snapshot the draw publishes is
/// the geometry the test then classifies against. The `paint` helper above
/// pins `MIN_INPUT_ROWS`, which is a different frame whenever the box is
/// hidden or has grown, and a hit test run against a frame that was never
/// drawn is exactly the mistake the snapshot exists to prevent.
fn paint_real(app: &App, h: u16) {
    let backend = ratatui::backend::TestBackend::new(W, h);
    let mut term = ratatui::Terminal::new(backend).unwrap();
    let rows = app.input_band(W);
    term.draw(|f| crate::view(app, f, &[], rows)).unwrap();
}

/// **A notch is three whole rows, through the real door.** The report is
/// the `Msg` the run loop delivers; the store moves by
/// [`crate::state::wheel::WHEEL_ROWS_PER_STEP`]; and the view is off the
/// tail because the *store* says a view off the tail is off the tail. The
/// wheel invents no rule of its own.
#[test]
fn a_wheel_notch_over_the_band_moves_the_transcript_by_three_whole_rows() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    let h = 20u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: text, ..
    } = bands(&app, h);
    assert!(app.pinned(), "setup: open on the tail");
    let t = Instant::now();

    wheel(&mut app, WheelDir::Up, text.y + 1, t);
    assert_eq!(
        app.scrollback().offset(),
        crate::state::wheel::WHEEL_ROWS_PER_STEP as usize,
        "one notch, three rows, into the past"
    );
    assert!(!app.pinned(), "and the view left the tail");

    // A notch the other way is the same number with the other sign, and it
    // has to be spaced like a notch: two reports inside the throttle
    // interval are one flick, and the second of them moves nothing.
    wheel(
        &mut app,
        WheelDir::Down,
        text.y + 1,
        t + crate::state::wheel::WHEEL_STEP_INTERVAL,
    );
    assert_eq!(app.scrollback().offset(), 0);
    assert!(
        app.pinned(),
        "…and the tail re-pins, as the store always does"
    );
}

/// **Chrome keeps the cursor.** The status row, the input box and the card
/// band are not transcript; a wheel report that lands on one of them
/// belongs to the widget under the cursor and the transcript neither moves
/// nor unpins.
#[test]
fn a_wheel_over_the_input_box_or_the_status_row_does_not_scroll_the_transcript() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    let h = 24u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: text,
        cards: _cards,
        kanban: _board,
        status,
        input,
    } = bands(&app, h);
    assert!(
        input.height > 0 && status.height > 0,
        "setup: chrome rows exist"
    );

    let t = Instant::now();
    for (name, row) in [
        ("status row", status.y),
        ("input box", input.y),
        ("input box's last row", input.bottom() - 1),
    ] {
        wheel(&mut app, WheelDir::Up, row, t);
        assert_eq!(
            app.scrollback().offset(),
            0,
            "the wheel scrolled the transcript from over the {name} (row {row})"
        );
        assert!(app.pinned(), "and unpinned nothing from the {name}");
    }

    // The transcript's own rows do move, so the loop above is not passing
    // because nothing ever scrolls.
    wheel(
        &mut app,
        WheelDir::Up,
        text.y + 2,
        t + crate::state::wheel::WHEEL_STEP_INTERVAL,
    );
    assert_eq!(
        app.scrollback().offset(),
        3,
        "the band itself still answers"
    );
}

/// The card band is chrome with something live in it, which is the case a
/// "the card row is part of the transcript text" mistake would go and
/// scroll.
#[test]
fn the_live_card_band_is_not_transcript_and_does_not_scroll() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    app.update(Msg::Agent {
        session: SessionId::new(TerminalType::Pi, 1),
        event: PiEvent::CompactionStart {
            reason: "threshold".into(),
        },
    });
    let h = 26u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: _text,
        cards,
        kanban: _board,
        status: _status,
        input: _input,
    } = bands(&app, h);
    assert!(cards.height > 0, "setup: a card row was laid out");

    let before = app.scrollback().offset();
    let t = Instant::now();
    wheel(&mut app, WheelDir::Up, cards.y, t);
    wheel(
        &mut app,
        WheelDir::Up,
        cards.y,
        t + Duration::from_millis(500),
    );
    assert_eq!(
        app.scrollback().offset(),
        before,
        "the live card band is not transcript and does not scroll"
    );
}

/// **A wheel that moves nothing costs no frame.** At the end of the
/// transcript, in the direction of the end, there is nowhere to go: the
/// store is unchanged, the frame is not marked dirty, and the run loop
/// therefore draws nothing and writes no bytes. That is the whole of
/// "no frame cost when nothing is moving", and it starts here rather than
/// in the draw because the draw cannot be blamed for a `dirty` it was told
/// about.
#[test]
fn a_wheel_at_the_end_of_the_transcript_costs_no_frame() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    let h = 20u16;
    paint(&app, h, &[]);
    let band_row = 2u16;
    let t = Instant::now();

    // At the tail, rolled toward the tail: forty reports, no movement.
    app.dirty = false;
    for i in 0..40u32 {
        wheel(
            &mut app,
            WheelDir::Down,
            band_row,
            t + Duration::from_millis(100 * i as u64),
        );
    }
    assert_eq!(app.scrollback().offset(), 0, "still at the tail");
    assert!(
        !app.dirty,
        "forty reports that moved nothing did not ask for one frame"
    );

    // Same at the head: past the top of the content there is nothing, and
    // nothing is not a place a view can be taken to.
    app.top_active();
    let top = app.scrollback().offset();
    assert!(top > 0, "setup: there is a head to be at");
    app.dirty = false;
    for i in 0..40u32 {
        wheel(
            &mut app,
            WheelDir::Up,
            band_row,
            t + Duration::from_millis(100 * i as u64),
        );
    }
    assert_eq!(app.scrollback().offset(), top, "clamped at the head");
    assert!(!app.dirty, "and no frame was asked for at the head either");
}

/// **The flick, at the App's own door.** Thirty-seven reports at 5 ms —
/// a trackpad's opening, not a wheel's — move four steps, not thirty-
/// seven, and the thirty-three that were throttled leave nothing behind:
/// no queued scroll, no dirty frame, no byte.
#[test]
fn a_trackpad_burst_at_the_app_door_is_throttled_not_queued() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    let h = 20u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: text, ..
    } = bands(&app, h);
    let band_row = text.y + 1;
    let t = Instant::now();

    let mut moved = 0usize;
    for i in 0..37u64 {
        if app.on_wheel(WheelDir::Up, band_row, t + Duration::from_millis(5 * i)) {
            moved += 1;
        }
    }
    // One step at 0 ms, then one per interval: 0, 60, 120, 180 of a
    // 180 ms burst on a 60 ms clock.
    assert_eq!(
        moved, 4,
        "thirty-seven reports, four applied steps: the interval is the rate"
    );
    assert_eq!(app.scrollback().offset(), 12);
    assert!(
        app.scrollback().offset() < 37,
        "one row per report would have been 37 rows, and a heavy multiplier \
         would have made the first notch one row; this is neither"
    );

    // The reports inside the throttle ask for no repaint. Times are picked
    // from between two steps (185 ms … 230 ms against a 180 ms last step),
    // so this is the throttle being tested and not the clock keeping up.
    app.dirty = false;
    for i in 1..11u64 {
        assert!(
            !app.on_wheel(
                WheelDir::Up,
                band_row,
                t + Duration::from_millis(180 + 5 * i)
            ),
            "a report inside the throttle moved the view"
        );
    }
    assert!(
        !app.dirty,
        "a burst that is entirely inside the throttle asked for no repaint"
    );
}

/// The gesture record is the measurement looprs-pdl.2 #4b never produced,
/// readable from the App after the fact: how many reports, how long, how
/// far — and closed by a gap rather than by a hook somebody has to
/// remember to call.
#[test]
fn the_app_keeps_the_shape_of_the_last_gesture() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    let h = 20u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: text, ..
    } = bands(&app, h);
    let band_row = text.y + 1;
    let t = Instant::now();

    assert!(app.wheel_gesture().is_none(), "nothing scrolled yet");
    for i in 0..37u64 {
        app.on_wheel(WheelDir::Up, band_row, t + Duration::from_millis(5 * i));
    }
    assert!(
        app.wheel_gesture().is_none(),
        "still inside the gesture: nothing has closed"
    );

    // The next report after the gap closes the previous one and opens
    // itself, so the two flicks are counted as two. The last report of
    // the burst landed at 180 ms, so this is the first one more than
    // WHEEL_GESTURE_GAP after it.
    app.on_wheel(
        WheelDir::Down,
        band_row,
        t + Duration::from_millis(180) + crate::state::wheel::WHEEL_GESTURE_GAP,
    );
    let g = app.wheel_gesture().expect("the gap closed the flick");
    assert_eq!(g.reports, 37, "every report counted, throttled or not");
    assert_eq!(g.rows, -12, "and only the applied ones moved");
    assert!(
        g.rows_per_report().abs() < 1.0,
        "a flick moves well under one row per report: {g:?}"
    );
}

/// **A child holding the screen owns the pointer, the wheel included.**
/// The same rule the drag has, reached from the other gesture: the pixels
/// under the cursor are the child's, so the snapshot the last draw
/// published is a fiction about them and our scroll state is not touched.
#[test]
fn no_wheel_state_changes_while_a_child_holds_the_screen() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    settle_deep(&mut app);
    let h = 20u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: text, ..
    } = bands(&app, h);
    let band_row = text.y + 1;

    // The wheel works before the handover, so the check below is not
    // passing because the wheel never works.
    wheel(&mut app, WheelDir::Up, band_row, Instant::now());
    assert_eq!(app.scrollback().offset(), 3);

    let bash = SessionId::new(TerminalType::Bash, 1);
    app.update(Msg::ScreenHeld {
        session: bash,
        active: true,
    });
    let held_at = app.scrollback().offset();
    app.dirty = false;
    let t = Instant::now();
    for i in 0..10u32 {
        assert!(
            !app.on_wheel(
                WheelDir::Up,
                band_row,
                t + Duration::from_millis(100 * i as u64)
            ),
            "the wheel moved the transcript while a child held the screen"
        );
    }
    assert_eq!(
        app.scrollback().offset(),
        held_at,
        "no scroll-state change while a child holds the screen"
    );
    assert!(!app.dirty, "and no frame was asked for either");

    // Back with us, it is the same wheel it always was.
    app.update(Msg::ScreenHeld {
        session: bash,
        active: false,
    });
    assert!(app.on_wheel(WheelDir::Up, band_row, t + Duration::from_secs(1)));
    assert_eq!(app.scrollback().offset(), held_at + 3);
}

/// The wheel and the page keys are two doors on one store. What a notch
/// costs and what a page costs differ; unpinning, holding, counting the
/// rows that arrive while the view is away and re-pinning on the way back
/// are one set of rules, because it is one `Scrollback`.
#[test]
fn the_wheel_and_the_page_keys_agree_because_they_are_one_store() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_deep(&mut app);
    let h = 20u16;
    paint(&app, h, &[]);
    let viewport::FrameAreas {
        transcript: text, ..
    } = bands(&app, h);
    let band_row = text.y + 1;
    let page = app.transcript_band_rows() as isize;
    let t = Instant::now();

    assert!(app.on_wheel(WheelDir::Up, band_row, t));
    assert_eq!(app.scrollback().offset(), 3, "one notch");

    // A page further into the past, by the keyboard's door.
    app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    ))));
    assert_eq!(
        app.scrollback().offset(),
        (3 + page) as usize,
        "the notch and the page added up in the same store"
    );

    // Output arriving while the view is away is counted, not shown — the
    // store's rule, reached by a wheel-shaped route to it.
    let id = SessionId::new(TerminalType::Pi, 1);
    app.view_mut(id)
        .push_note(MessageKind::Answer, "ARRIVED-WHILE-AWAY".into());
    app.flush_active(W);
    assert_eq!(app.new_rows(), 2, "the line and its separator");
    assert_eq!(
        app.scrollback().offset(),
        (3 + page + 2) as usize,
        "the view held while the tail ran away"
    );

    // And the wheel's own way back to the tail is the store's rule too:
    // enough notches down and the view is pinned again, count cleared.
    let mut at = t + Duration::from_millis(1000);
    for _ in 0..(page + 5) {
        app.on_wheel(WheelDir::Down, band_row, at);
        at += Duration::from_millis(100);
    }
    assert!(app.pinned(), "the wheel got back to the tail");
    assert_eq!(app.new_rows(), 0, "and the count is answered");
}

/// **All three modes, one wheel.** The handler does not know which mode it
/// is in — it scrolls the *active view*'s store and nothing else — so a
/// notch over the band has to move the transcript in Beeds, in Pi and in
/// Bash alike. This is the ticket's title read literally, and it is cheap to
/// prove because there is nothing mode-specific to prove: one door, three
/// views behind it.
#[test]
fn the_wheel_scrolls_the_transcript_in_every_mode() {
    for mode in TerminalType::ALL {
        let (mut app, _rx) = app_with(mode);
        settle_deep(&mut app);
        let h = 20u16;
        paint(&app, h, &[]);
        let viewport::FrameAreas {
            transcript: text, ..
        } = bands(&app, h);
        let t = Instant::now();

        wheel(&mut app, WheelDir::Up, text.y + 1, t);
        assert_eq!(
            app.scrollback().offset(),
            crate::state::wheel::WHEEL_ROWS_PER_STEP as usize,
            "{mode:?}: the notch did not move this mode's transcript"
        );
        assert!(
            !app.pinned(),
            "{mode:?}: and the store unpinned, the same way it does everywhere"
        );
    }
}

/// The same claim with the frame in its other shape: a beads pass that has
/// taken the keyboard has no input box, so the band is taller and the row
/// that *was* the box is now something else. The wheel follows the band,
/// not the row numbers — which is the whole reason the gate reads the
/// published snapshot instead of counting from the bottom of the screen.
#[test]
fn the_wheel_follows_the_band_when_the_input_box_is_gone() {
    let (mut app, _rx) = app_with(TerminalType::Beeds);
    settle_deep(&mut app);
    let id = SessionId::new(TerminalType::Beeds, 1);
    app.view_mut(id)
        .set_status(SessionStatus::Running, Instant::now());
    assert_eq!(
        app.input_band(W),
        viewport::NO_INPUT_ROWS,
        "setup: the box is not taking rows"
    );

    let h = 24u16;
    paint_real(&app, h);
    let viewport::FrameAreas {
        transcript: text,
        cards: _cards,
        kanban: _board,
        status,
        input: _input,
    } = bands(&app, h);
    assert_eq!(status.bottom(), h, "the status row is the last row now");
    assert_eq!(
        text.bottom(),
        status.y,
        "setup: the band ends where the chrome begins: {text:?} vs {status:?}"
    );

    let t = Instant::now();
    // The row that used to hold the box: chrome, whatever height it has.
    wheel(&mut app, WheelDir::Up, status.y, t);
    assert_eq!(app.scrollback().offset(), 0, "the status row is not band");

    // The band's own last row is band, and it scrolls.
    wheel(
        &mut app,
        WheelDir::Up,
        text.bottom() - 1,
        t + crate::state::wheel::WHEEL_STEP_INTERVAL,
    );
    assert_eq!(
        app.scrollback().offset(),
        3,
        "the band's last row is inside the band, at whatever height the frame put it"
    );
}
