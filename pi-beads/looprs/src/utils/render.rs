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
