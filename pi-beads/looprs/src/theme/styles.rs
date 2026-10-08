use crate::session::TerminalType;
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
