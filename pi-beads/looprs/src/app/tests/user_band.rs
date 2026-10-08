//! The user's own rows in the transcript (the "which of these did *I* say?" band).
//!
//! The distinction used to be one character: a `❯` at the head of the message.
//! That is a thin claim to put on a block that can be many rows long, and it is
//! invisible to everything downstream of the first column — a copy of the row
//! carried the chevron with it, and a message that wrapped put the marker two
//! rows behind the text it marked.
//!
//! The band replaces it, and the tests are about where the band *ends up*: on
//! every cell of every row of the message, on the user's rows only, and on no
//! character of the copy. A background that stops at the last character is an
//! underline; one that lands on the assistant's rows too is wallpaper; one that
//! reaches the clipboard is a marker wearing a costume.

use super::*;
use crate::theme::styles::USER_BG;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

/// Type into the box and press Enter, the way the keyboard does it, so what is
/// banded is the row a real submit made and not one assembled for the test.
fn submit(app: &mut App, text: &str) {
    for c in text.chars() {
        app.update(Msg::Term(key(KeyCode::Char(c), KeyModifiers::NONE)));
    }
    app.update(Msg::Term(key(KeyCode::Enter, KeyModifiers::NONE)));
}

/// One painted frame read back two ways: the characters on each row, and the
/// background of every cell on it.
///
/// Both are needed because the claim is about the pair — which text is sitting on
/// which colour, at which column.
struct Shot {
    lines: Vec<String>,
    bgs: Vec<Vec<Color>>,
}

impl Shot {
    fn of(app: &App, height: u16) -> Self {
        let mut term = ratatui::Terminal::new(TestBackend::new(W, height)).unwrap();
        term.draw(|f| crate::view(app, f, &[], viewport::MIN_INPUT_ROWS))
            .unwrap();
        let buf = term.backend().buffer();
        let w = buf.area.width as usize;
        Shot {
            lines: buf
                .content
                .chunks(w)
                .map(|r| r.iter().map(|c| c.symbol()).collect())
                .collect(),
            bgs: buf
                .content
                .chunks(w)
                .map(|r| r.iter().map(|c| c.bg).collect())
                .collect(),
        }
    }

    fn row_with(&self, needle: &str) -> usize {
        self.lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no row containing {needle:?}: {:?}", self.lines))
    }

    fn banded(&self, row: usize) -> bool {
        self.bgs[row].iter().all(|c| *c == USER_BG)
    }
}

/// The band, edge to edge, under the user's words — and no chevron in front of
/// them, because the band is the marker now.
#[test]
fn a_submitted_message_is_a_band_across_the_whole_row() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    submit(&mut app, "did the tests pass?");
    app.flush_active(W);

    let shot = Shot::of(&app, 20);
    let y = shot.row_with("did the tests pass?");

    assert_eq!(
        shot.lines[y].trim_end(),
        "did the tests pass?",
        "the user's words start at column 0 with no marker of any kind"
    );
    assert!(
        !shot.lines[y].contains('❯'),
        "the chevron is still being drawn: {:?}",
        shot.lines[y]
    );
    assert!(
        shot.banded(y),
        "row {y} is not banded across all {} cells: {:?}",
        W,
        shot.bgs[y]
    );
}

/// The band says "mine" and nothing else. An answer painted on the same shade is
/// the failure mode this guards: a band everywhere is no band at all, and the
/// user is back to counting glyphs to work out whose words these are.
#[test]
fn the_band_separates_the_two_voices_and_not_everything_else() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    submit(&mut app, "did the tests pass?");
    let id = pi_id();
    app.view_mut(id)
        .push_note(MessageKind::Answer, "all seven hundred of them".into());
    app.flush_active(W);

    let shot = Shot::of(&app, 20);
    let mine = shot.row_with("did the tests pass?");
    let theirs = shot.row_with("all seven hundred of them");
    assert_ne!(mine, theirs, "setup: two different rows are on screen");

    assert!(shot.banded(mine), "the user's row lost its band");
    assert!(
        shot.bgs[theirs].iter().all(|c| *c != USER_BG),
        "the answer got banded too, which makes the band meaningless: {:?}",
        shot.bgs[theirs]
    );
}

/// A message that wraps is one block, so it gets one band. The chevron could
/// only ever mark the first row of it; the band has to reach the rows below,
/// which is most of the point of moving the distinction to the background.
#[test]
fn a_wrapped_message_is_banded_on_every_row_of_it() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    // Long enough to wrap at the content width (W - 2 = 78 cells), and the
    // marker is at each end so both rows are identified by the test, not by
    // counting.
    submit(
        &mut app,
        "ZZTOP please explain this whole paragraph in detail because it is long enough that it has to wrap onto a second row at this terminal width ZZEND",
    );
    app.flush_active(W);

    let shot = Shot::of(&app, 20);
    let first = shot.row_with("ZZTOP");
    let last = shot.row_with("ZZEND");
    assert!(
        last > first,
        "setup: the message did not wrap (both markers are on row {first})"
    );
    for row in first..=last {
        assert!(
            !shot.lines[row].trim().is_empty(),
            "setup: row {row} is blank, so the range is not all message"
        );
        assert!(
            shot.banded(row),
            "row {row} of the wrapped message is not banded: {:?}",
            shot.bgs[row]
        );
    }
}

/// The band is styling, never a character. A copy of the row is exactly what
/// was typed: no chevron, no padding spaces from the band, nothing extra at the
/// end. The transcript is text the user copies, so this is the assertion that
/// keeps the cosmetics out of the content.
#[test]
fn copying_a_banded_row_copies_what_was_typed_and_nothing_else() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let rec = recording_clipboard(&mut app);
    submit(&mut app, "copy me exactly");
    app.flush_active(W);

    let h = 20u16;
    paint(&app, h, &[]);
    let (bx, y0, drawn) = geom(&app, h);
    let a = drawn
        .iter()
        .position(|t| t.contains("copy me exactly"))
        .expect("the submitted row is on the band");

    // Drag well past the end of the text: whatever the band covers that is not
    // text must not come along either.
    drag(&mut app, (y0 + a as u16, bx), (y0 + a as u16, bx + 40));

    assert_eq!(
        rec.last().as_deref(),
        Some("copy me exactly"),
        "the copy is the message, character for character"
    );
}
