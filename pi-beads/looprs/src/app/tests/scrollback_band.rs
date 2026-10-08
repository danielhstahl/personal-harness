//! The scrollback store drives the band (looprs-pdl.6)

use super::*;

/// `n` settled lines in the active view, flushed at `width`.
///
/// Each `System` entry renders as itself plus its separator, so `n` entries
/// are `2n` store rows — which is what makes "is there history above the
/// band" a fact the tests can rely on rather than guess at.
fn settle(app: &mut App, n: usize, width: u16) {
    for i in 0..n {
        app.update(Msg::System {
            session: None,
            text: format!("settled {i}"),
        });
    }
    app.flush_active(width);
}

fn shown(app: &App, rows: usize) -> Vec<String> {
    app.transcript_window(rows)
        .iter()
        .map(|r| r.to_string())
        .collect()
}

/// A name for a command's shape. `UiCommand` is `Debug` but not `PartialEq`
/// — two actions are not "equal" in this crate — and a shape string is all
/// the comparison below needs.
fn shape(cmd: &UiCommand) -> String {
    match cmd {
        UiCommand::Resize { rows, cols } => format!("resize {rows}x{cols}"),
        other => format!("{other:?}"),
    }
}

/// Pinned, the band shows the tail; a page up shows older rows and hides it.
#[test]
fn a_page_up_pages_the_transcript_and_leaves_the_tail() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle(&mut app, 20, 80);
    let band = app.transcript_band_rows();
    assert!(band > 1, "the band has rows to show: {band}");
    assert!(
        app.scrollback().len() > band,
        "there is history above this band to scroll into: band={band} rows={}",
        app.scrollback().len()
    );

    let tail = shown(&app, band);
    assert!(
        tail.iter().any(|l| l.contains("settled 19")),
        "the newest line is not on screen while pinned: {tail:?}"
    );
    assert!(
        !tail.iter().any(|l| l.contains("settled 0")),
        "the head of the transcript is off the top while pinned: {tail:?}"
    );

    app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
    assert!(!app.pinned(), "one page up is off the tail");
    let hist = shown(&app, band);
    assert!(
        hist.iter().any(|l| l.contains("settled 0")),
        "a page of history reached the head of the transcript: {hist:?}"
    );
    assert!(
        !hist.iter().any(|l| l.contains("settled 19")),
        "and the tail is not on screen any more: {hist:?}"
    );
}

/// A page up with nothing above the band cannot scroll into blank space, so
/// it cannot unpin either: "pinned" is not a mode the app can be in without
/// the content agreeing.
#[test]
fn with_nothing_above_the_band_there_is_nowhere_to_scroll() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle(&mut app, 2, 80);
    app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
    assert!(
        app.pinned(),
        "a transcript shorter than the band cannot be scrolled up"
    );
    assert_eq!(app.scrollback().offset(), 0);
}

/// The half of the contract that is not about the keystroke at all: while the
/// user is off the tail, new output must not move what they are reading, and
/// must be *counted* so the UI can say the view is not up to date.
#[test]
fn output_that_arrives_while_scrolled_up_is_held_back_and_counted() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle(&mut app, 12, 80);
    let band = app.transcript_band_rows();
    app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
    let before = shown(&app, band);

    app.update(Msg::System {
        session: None,
        text: "arrived while you were reading".into(),
    });
    app.flush_active(80);

    assert_eq!(shown(&app, band), before, "the view held");
    assert_eq!(
        app.new_rows(),
        2,
        "the entry and its separator, both unseen"
    );
    assert!(!app.pinned());
}

/// `End` is the one action the pill names, and it answers the count: back at
/// the tail, nothing is pending, because the user is looking at it.
#[test]
fn end_returns_to_the_tail_and_answers_the_count() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle(&mut app, 12, 80);
    app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
    app.update(Msg::System {
        session: None,
        text: "arrived while you were reading".into(),
    });
    app.flush_active(80);
    assert_eq!(app.new_rows(), 2);

    app.update(Msg::Term(key(KeyCode::End, KeyModifiers::NONE)));
    assert!(app.pinned(), "the bottom of the content is the tail");
    assert_eq!(app.new_rows(), 0, "and nothing is unseen any more");
    let tail = shown(&app, app.transcript_band_rows());
    assert!(
        tail.iter()
            .any(|l| l.contains("arrived while you were reading")),
        "the tail is what the band shows: {tail:?}"
    );
}

/// Scrolling is local: no command goes to the Router for a wheel, a page or
/// a `Home`, and nothing goes to the input box either.
#[test]
fn scrolling_is_not_a_round_trip_and_not_a_keystroke_anyones_else() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    settle(&mut app, 12, 80);
    for code in [
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
    ] {
        app.update(Msg::Term(key(code, KeyModifiers::NONE)));
        assert!(
            rx.try_recv().is_err(),
            "{code:?} sent a command; the scrollback is the app\'s own state"
        );
    }
    assert!(
        app.input.text().is_empty(),
        "the box took none of them either"
    );
}

/// **The size poll and the `Resize` event are one adoption, not two
/// implementations of it** (looprs-pdl.15).
///
/// The run loop takes a resize from either source, so the two must leave
/// the same state behind: same width and height, a frame asked for, and —
/// the one worth pinning — the same `UiCommand::Resize` out to the
/// children. A poll that set `App::width` locally would repaint the frame
/// and leave every child pty wrapping for a window that no longer exists
/// (ADR-0001 rule 6), which is the bug this ticket exists to remove in
/// one place and would quietly reintroduce in another.
#[test]
fn the_size_poll_adopts_what_the_resize_event_adopts() {
    let (mut by_event, mut ev_rx) = app_with(TerminalType::Bash);
    let (mut by_poll, mut poll_rx) = app_with(TerminalType::Bash);

    by_event.update(Msg::Term(Event::Resize(132, 43)));
    // …what `main.rs`'s tick arm does with the poll's answer.
    by_poll.set_window(132, 43);

    assert_eq!((by_event.width, by_event.height), (132, 43));
    assert_eq!((by_poll.width, by_poll.height), (132, 43));
    assert!(by_event.dirty, "the event asks for a frame");
    assert!(by_poll.dirty, "and so does the poll");

    let from_event = ev_rx.try_recv().ok().map(|c| shape(&c));
    let from_poll = poll_rx.try_recv().ok().map(|c| shape(&c));
    assert_eq!(
        from_event.as_deref(),
        Some("resize 43x132"),
        "rows first, as ADR-0001 rule 6 spells it"
    );
    assert_eq!(
        from_event, from_poll,
        "the children were told the same thing by both paths"
    );
}

/// A resize re-wraps the store rather than re-adding to it: the same content,
/// once, at the new width.
#[test]
fn a_resize_rewraps_the_store_without_doubling_or_losing_content() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let id = SessionId::new(TerminalType::Pi, 1);
    app.view_mut(id)
        .push_delta(MessageKind::Answer, "MARKER-1 alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau. MARKER-2 the quick brown fox jumps over the lazy dog again and again until it needs a second row at eighty columns and a third at forty.

");
    app.flush_active(80);
    let wide = app.scrollback().len();
    assert!(wide > 1, "the answer rendered: {wide} rows");

    app.set_window(40, 24);
    app.flush_active(40);
    let narrow = app.scrollback().len();
    assert_eq!(app.scrollback().width(), 40, "the store is wrapped for 40");
    assert!(
        narrow > wide,
        "narrower window, more rows: {wide} -> {narrow}"
    );

    let all: String = app
        .scrollback()
        .rows()
        .iter()
        .map(|r| r.to_string())
        .collect();
    assert_eq!(all.matches("MARKER-1").count(), 1, "{all:?}");
    assert_eq!(all.matches("MARKER-2").count(), 1, "{all:?}");
}

/// The page a scroll key moves is the band the frame lays out — the same
/// number, from the same function, rather than two arithmetic that can drift.
#[test]
fn a_page_is_the_band_the_frame_lays_out() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle(&mut app, 40, 80);
    let band = app.transcript_band_rows();
    let max = app.scrollback().max_scroll(band);
    assert!(
        max > band,
        "enough history that a page is not the whole way"
    );

    app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
    assert_eq!(
        app.scrollback().offset(),
        band.min(max),
        "one page is exactly one band of transcript"
    );

    // Walked up page by page, the head of the content is where it stops.
    for _ in 0..(max / band.max(1) + 2) {
        app.update(Msg::Term(key(KeyCode::PageUp, KeyModifiers::NONE)));
    }
    assert_eq!(app.scrollback().offset(), max, "and no page goes past it");

    app.update(Msg::Term(key(KeyCode::End, KeyModifiers::NONE)));
    assert_eq!(
        app.scrollback().offset(),
        0,
        "and `End` is all the way back"
    );
    assert!(app.pinned());
}
