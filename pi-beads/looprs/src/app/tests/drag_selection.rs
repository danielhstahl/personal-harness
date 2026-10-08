//! The drag selection, through the real frame (pdl.9)

use super::*;

/// The whole gesture end to end: a press and a release around a drag put
/// the characters under the box into `selection_paste`, across a row
/// boundary, with the blank separator row the flusher adds counted as the
/// blank line the user saw.
#[test]
fn a_drag_across_two_rows_selects_the_characters_under_the_box() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 4);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn
        .iter()
        .position(|t| t.contains("LINE01"))
        .expect("setup: LINE01 is on screen");
    let b = drawn
        .iter()
        .position(|t| t.contains("LINE02"))
        .expect("setup: LINE02 is on screen");

    drag(&mut app, (y0 + a as u16, bx + 5), (y0 + b as u16, bx + 9));
    assert!(app.selection().is_live());
    // Cell 5 of "LINE01 aaaaaaaaaa" is the `1`, cell 9 of the LINE02 row
    // the third `a`, and the blank separator row the flusher adds lands in
    // between as the one newline that keeps the two messages apart: the
    // paste is the characters, with the seam the user saw between them.
    assert_eq!(
        app.selection_paste(),
        "1 aaaaaaaaaa\nLINE02 aaa",
        "the selection is the characters under the box, across the row seam"
    );
}

/// **A click clears.** Press and release with nothing between them is not a
/// selection, and it takes away the one that was standing.
#[test]
fn a_click_selects_nothing_and_clears_what_was_selected() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    settle_answers(&mut app, 3);
    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn.iter().position(|t| t.contains("LINE01")).unwrap();
    let b = drawn.iter().position(|t| t.contains("LINE02")).unwrap();

    drag(&mut app, (y0 + a as u16, bx), (y0 + b as u16, bx + 4));
    assert!(!app.selection_paste().is_empty(), "setup: a selection");

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
    assert!(
        !app.selection().is_live(),
        "the click cleared the standing selection"
    );
    assert_eq!(app.selection_paste(), "");
}
