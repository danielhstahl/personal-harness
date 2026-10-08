use crate::session::TerminalType;
use crate::state::board::Marker;
use crate::state::transcript::MessageKind;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

/// The app's blue: two steps up the same hue from `Color::Blue`.
///
/// `Color::Blue` is ANSI-4, and in most dark palettes that lands at about the
/// same luminance as the background — a link, a heading or a "running" verb
/// painted in it reads as a smudge you are meant to be able to read. `Indexed
/// 111` (#87afff) is the same colour with the lights on: unmistakably blue,
/// unmistakably legible. Indexed rather than RGB so a user's own 256-colour
/// palette still gets the last word, which `Rgb` would overrule.
///
/// Every blue in the app goes through here for the same reason every mode colour
/// goes through [`mode_color`]: two blues that drift apart is two answers to one
/// question.
pub const BLUE: Color = Color::Indexed(111);

/// The app's red: the same signal as `Color::Red`, taken off the shout.
///
/// The plain red was never a *visibility* problem — it was a comfort one. Full
/// saturation at terminal brightness is the harshest thing on the screen and it
/// pulls the eye off everything next to it, which is a real cost for a row that
/// also carries the error text. `Indexed 210` (#ff8787) keeps the luminance
/// (so it is still the loudest claim on a row of grey) and drops the
/// saturation, which is where the harshness actually lived.
pub const RED: Color = Color::Indexed(210);

/// The band a submitted user message sits on.
///
/// A **background**, not a glyph: the chevron this replaced said "mine" in one
/// character out of the thousands the message has, and was the only thing
/// between the user's words and the assistant's. A band says it for the whole
/// block, including the rows of a message that wrapped.
///
/// Blue-grey rather than a shade of the terminal's own background, because the
/// user's terminal background is not ours to know: a step in lightness vanishes
/// on a background darker or lighter than the one we pictured (and collides
/// with the `Rgb(45,45,45)` code-block grey on the warm palettes). A hue the
/// rest of the frame does not use is distinct whatever is behind it.
///
/// `Rgb` rather than an index for the same reason the code-block background is:
/// it is a chosen colour, not a palette member, and the transcript's syntax
/// highlighting is already truecolour wherever it lands.
pub const USER_BG: Color = Color::Rgb(44, 58, 82);

/// The user's text on that band, set explicitly rather than left to the
/// terminal's default.
///
/// "Default foreground" is only safe while the background is what we assumed.
/// The band is ours, so the pair is ours too: naming both colours makes the row
/// read on any terminal instead of on the one we pictured.
pub const USER_FG: Color = Color::White;

/// The colour of a mode, in the two places it is shown: the input box's border and
/// the status row (`components::status`).
///
/// One function because the ticket this replaces had the mode visible *only* as a
/// border colour, and a row that says `Beads` in a colour of its own would be a
/// second, contradicting answer to "which mode am I in".
pub fn mode_color(mode: TerminalType) -> Color {
    match mode {
        TerminalType::Bash => BLUE,
        TerminalType::Beeds => Color::DarkGray,
        TerminalType::Pi => Color::Yellow,
    }
}

pub fn content_width(term_width: u16) -> u16 {
    term_width.saturating_sub(2).max(20)
}
pub fn style_for(k: &MessageKind) -> Style {
    /* Thinking => italic+dim, User => the band, others => default */
    match k {
        MessageKind::Thinking => Style::new()
            .add_modifier(Modifier::ITALIC)
            .add_modifier(Modifier::DIM),
        MessageKind::User => Style::new().fg(USER_FG).bg(USER_BG),
        _ => Style::default(),
    }
}

/// The scrollback's trim marker (`looprs-pdl.7`).
///
/// Italic so it reads as *about* the transcript rather than as transcript — the
/// same trick `Thinking` uses, and the reason it works is that nothing else on
/// the band is italic except thinking, which is at least honest about being the
/// app's own voice. Dark-gray rather than a colour because the row is a loss
/// notice, not an answer, and it must not compete with the yellow the status row
/// spends on "the loop is working"; it stays readable on both palettes, which a
/// dim-only style is not.
pub fn trim_marker_style() -> Style {
    Style::new().dark_gray().add_modifier(Modifier::ITALIC)
}

// ─────────────────────────── the kanban band (looprs-5o4.4) ───────────────────────────
//
// One family on purpose. The band is chrome — it sits above the transcript and
// says something *about* the work rather than being the work — so every style in
// it is chosen to lose against the transcript. Concretely: no bold on the rows,
// no background anywhere, and colour spent on exactly three things (the two
// markers and an error footer). A board that competes with the answer the user is
// reading is a board that gets turned off.

/// The column header: name and true total.
///
/// Bold and uncoloured. It is the one row in the band that is *labels* rather
/// than *content*, and bold is enough to separate label from content without a
/// colour that would then have to be reserved for something more important.
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_header() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// A bead row. The quietest thing on the band: default foreground, no modifiers.
///
/// The row's own information is the id and the title; the *style* has nothing to
/// add, and any colour here would be competing with the marker two cells to its
/// left.
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_row() -> Style {
    Style::default()
}

/// The marker's style, by marker (ADR-0007 §1: `⊘` blocked, `?` unknown).
///
/// Yellow for `⊘`: it is the band's one "a person should look at this", and it
/// is not the app's red, because a blocked bead is not a *failure* — the board
/// is telling the truth about it. Dark gray for `?`: nothing is wrong with the
/// bead, it is our own classifier that came up short, and making `?` shout would
/// read as "this ticket is broken" rather than "this build has not met this
/// status".
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_marker(marker: Marker) -> Style {
    match marker {
        Marker::Blocked => Style::new().fg(Color::Yellow),
        Marker::Unknown => Style::new().dark_gray(),
    }
}

/// The `+N more` overflow row.
///
/// Italic and dark gray, the same treatment the scrollback's trim marker uses:
/// the row is a statement *about* the column — "there are more rows than this"
/// — not a row of the column, and italic is how this app marks its own voice.
/// Italic rather than dim alone because a dim-only style is unreliable across
/// palettes (see [`trim_marker_style`] for the same call, made for the same
/// reason).
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_overflow() -> Style {
    Style::new().dark_gray().add_modifier(Modifier::ITALIC)
}

/// The `—` an empty column draws in its body.
///
/// Dim but upright: the column answered, and what it said was "nothing". A
/// column that draws nothing at all is a column that failed to render, which is
/// the reading this character exists to prevent.
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_empty() -> Style {
    Style::new().dark_gray()
}

/// The footer when the read was good.
///
/// Dark gray, because "bd ok · 3s ago" is the boring state and the boring state
/// should not be the one that catches the eye.
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_footer_ok() -> Style {
    Style::new().dark_gray()
}

/// The footer when the read was not good: the band's one loud thing.
///
/// Not bold — the status row already spends bold red on failures, and two red
/// bold rows stacked one above the other is one red shout rather than two facts.
/// The word order and the app's red carry it on their own.
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_footer_error() -> Style {
    Style::new().fg(RED)
}

/// The stale pass over the last good rows: whatever a row's style was, it is now
/// painted in the colour of *the previous read*.
///
/// `patch`, not replace, so a marker keeps its modifiers and only its colour
/// comes back dim: the marker is still the row's own fact, while the dim says the
/// band has not been able to vouch for any of it since the last read answered.
/// Dark gray rather than the `DIM` modifier, for the palette reason given on
/// [`trim_marker_style`] — a modifier alone disappears on terminals that render
/// it as a no-op, and "stale" is the one state that must not be invisible.
#[allow(dead_code)] // consumer: looprs-5o4.5, the kanban band
pub fn board_staled(base: Style) -> Style {
    base.patch(Style::new().fg(Color::DarkGray))
}

/// Apply an entry's base style to a rendered line, at **both** levels.
///
/// The span pass is the one that was always here: colour where the characters
/// are, with the span's own style winning over the base.
///
/// The line pass is what a background needs and the spans cannot give it. A
/// span only paints the cells its own text covers, so spans alone turn a band
/// into an underline of the message — stopping at the last character and
/// leaving every wrapped row shorter than the row above it. ratatui's `Line`
/// widget paints `line.style` across the *whole* area handed to it
/// (`Buffer::set_style`) before drawing the spans over it, which is the one
/// way a background reaches edge to edge. Patched in the same order as the
/// spans (`base.patch(line.style)`), so a line that already carries a style of
/// its own still outranks the base, exactly as a span does.
///
/// Only the user's band uses a background today; for every other kind the base
/// has no `bg`, so the line pass writes nothing visible.
pub fn restyle(mut line: Line<'static>, base: Style) -> Line<'static> {
    line.style = base.patch(line.style);
    for span in &mut line.spans {
        span.style = base.patch(span.style); // span's own style wins
    }
    line
}
