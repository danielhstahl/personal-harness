use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The spinner. Ten braille frames, one per tick.
///
/// Shared by every band that spins (the live text preview, a tool card, a
/// compaction card, the status row) so that two things on one screen cannot be
/// turning out of step with each other.
pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// `ControlStripper` lived here until ADR-0005. It is gone rather than deprecated
// because it was not merely a worse version of something better: on its own it was
// *wrong* for shell output. Deleting `\x1b[…` byte by byte is fine for a string
// nobody re-lays out, but once looprs owns the wrapping, a `\r`-repainted progress
// bar loses its frames without losing their concatenation — every frame arrives as
// one long line of `###---###---###---`. The resolver that replaced it is
// [`shelltext::LineResolver`](crate::utils::shelltext::LineResolver); the
// sequences this file used to drop are covered there, in `CORPUS`, so retiring
// this one does not retire a single test of "no escape bytes reach the store".

/// A rendered line's text with the styling dropped.
///
/// The one definition of "the string this line is", used by every `Display` impl
/// on the wrapped types (`RenderedRow`, `DisplayRow`) so that reading a line out
/// of the flush and reading it out of the store give the same characters — and so
/// the copy path's "drop the presentation on the way out" (ADR-0004 R14,
/// ADR-0005 Q4) is one function rather than a convention.
pub fn plain(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The first `avail` **display columns** of `text`, by `unicode-width`.
///
/// The unit is the column, not the `char` and not the byte, and that distinction
/// is the whole reason this exists instead of `&text[..n]`: a CJK character is one
/// `char` and two columns, a combining accent is one `char` and zero columns, and
/// both of the latter cut a slice in a place the reader cannot see. A cut nobody
/// can see is a cut that loses text silently, which is the failure class every
/// width-aware thing in this crate refuses.
///
/// Nothing is ever split: the loop stops *before* the character that would
/// overflow the budget, so a double-width glyph is dropped whole rather than
/// leaving half of it (and its replacement glyph) behind.
pub fn take_columns(text: &str, avail: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > avail {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// Shorten `text` to at most `avail` display columns, marking the cut with `…`.
///
/// **The marker is inside the budget, not added to it**: the result of
/// `truncate_columns(s, w)` is never wider than `w` columns, which is the only
/// contract that makes a truncated string safe to drop into a fixed-width cell.
/// A helper that returned `w + 1` columns would be worse than no helper at all —
/// the caller would have to re-check its own truncation.
///
/// `avail == 0` returns nothing (there is no cell to fill); `avail == 1` returns
/// the marker alone, which is the honest maximum in one column: *something was
/// here*.
///
/// Moved here from `components::status` (looprs-5o4.4), where it was the row's
/// private `shorten`, so that the kanban band takes its truncation from the same
/// width-aware place instead of growing a second one. Every truncation in the
/// crate goes through these two functions.
pub fn truncate_columns(text: &str, avail: usize) -> String {
    if text.width() <= avail {
        return text.to_string();
    }
    if avail == 0 {
        return String::new();
    }
    format!("{}…", take_columns(text, avail - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_string_comes_back_untouched_and_unmarked() {
        assert_eq!(truncate_columns("id", 10), "id");
        assert_eq!(truncate_columns("", 5), "");
        // Exactly the budget is not over the budget: no marker.
        assert_eq!(truncate_columns("012345678", 9), "012345678");
        // One over, and the marker is inside the nine.
        assert_eq!(truncate_columns("0123456789", 9), "01234567…");
    }

    /// The unit is columns, so the two-width scripts cost what they cost —
    /// 返回的标题 is five `char`s and ten columns.
    #[test]
    fn wide_characters_are_counted_as_two_columns() {
        assert_eq!("返回的标题".width(), 10);
        assert_eq!(take_columns("返回的标题", 5), "返回");
        assert_eq!(take_columns("返回的标题", 6).width(), 6);
        // An odd budget lands a column short rather than a column over: the
        // helper never splits a wide glyph to make the number come out exact.
        assert_eq!(truncate_columns("返回的标题", 6), "返回…");
    }

    /// A cut never lands in the middle of a character: the wide glyph that would
    /// overflow is dropped whole rather than leaving a hole for the terminal's own
    /// replacement glyph.
    #[test]
    fn an_odd_budget_drops_the_wide_glyph_whole() {
        // 3 columns cannot hold 返回 (4) nor a marker plus 返 (3) without the
        // marker costing one of them: the marker is the budget's, so one glyph fits.
        assert_eq!(truncate_columns("返回的", 3), "返…");
    }

    #[test]
    fn zero_and_one_column_budgets_answer_instead_of_panicking() {
        assert_eq!(truncate_columns("anything", 0), "");
        assert_eq!(truncate_columns("anything", 1), "…");
        assert_eq!(take_columns("anything", 0), "");
    }

    /// The property the callers rely on: whatever the input, the output is never
    /// wider than the budget it was given.
    #[test]
    fn the_result_never_exceeds_the_budget() {
        let cases = [
            "plain ascii title",
            "返回的标题 wide CJK",
            "é accents and a 漢字",
            "🎉 emoji is two columns",
        ];
        for text in cases {
            for avail in 0..=24usize {
                let cut = truncate_columns(text, avail);
                assert!(
                    cut.width() <= avail,
                    "{text:?} @ {avail} columns came back {} wide: {cut:?}",
                    cut.width()
                );
            }
        }
    }

    #[test]
    fn zero_width_combining_marks_cost_nothing_and_survive() {
        // "e" + U+0301 is one grapheme, one column wide.
        let s = "e\u{0301}x";
        assert_eq!(s.width(), 2);
        assert_eq!(take_columns(s, 2), s);
    }
}
