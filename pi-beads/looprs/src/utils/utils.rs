pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Turns a terminal byte stream into printable text for the transcript.
///
/// ADR-0001 rule 5: the Bash transcript copy keeps the *content* and drops the
/// presentation. Colours, cursor moves and alt-screen switching are the child's way
/// of driving a screen, not information to store — and storing them verbatim is
/// how escape sequences end up printed as literal `←[32m` noise.
///
/// **Stateful on purpose.** An escape sequence can straddle two reads, and a
/// per-chunk strip would drop the `\x1b[38;` half and print the `5;120m` half as
/// text. The unfinished tail is held until the rest arrives, so a sequence split at
/// any boundary is dropped whole.
#[derive(Default)]
pub struct ControlStripper {
    state: StripState,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum StripState {
    #[default]
    Text,
    /// Just saw `ESC` — the next byte decides what kind of sequence this is.
    Escaped,
    /// Inside `ESC [ … }`, until a final byte in `@`..`~`.
    Csi,
    /// Inside `ESC ] …`, until BEL or `ESC \`.
    Osc,
}

impl ControlStripper {
    /// Consume a chunk, return the printable text in it.
    pub fn strip(&mut self, src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        let mut chars = src.chars().peekable();
        while let Some(c) = chars.next() {
            match self.state {
                StripState::Text => match c {
                    '\n' | '\t' => out.push(c),
                    '\u{1b}' => self.state = StripState::Escaped,
                    // Other C0 controls (and DEL) are presentation, not content.
                    c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                    c => out.push(c),
                },
                StripState::Escaped => match c {
                    '[' => self.state = StripState::Csi,
                    ']' => self.state = StripState::Osc,
                    // `ESC <char>` is a complete (if archaic) sequence.
                    _ => self.state = StripState::Text,
                },
                StripState::Csi => {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        self.state = StripState::Text;
                    }
                }
                StripState::Osc => {
                    if c == '\u{7}' {
                        self.state = StripState::Text;
                    } else if c == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                            self.state = StripState::Text;
                        }
                    }
                }
            }
        }
        out
    }

    /// True when a sequence is half-parsed and the stripper is holding bytes back.
    ///
    /// Introspection, not behavior: it is how the tests assert that a sequence split
    /// across two chunks is being held rather than half-printed. Production code
    /// deliberately does not branch on it — holding is correct whatever the answer.
    #[allow(dead_code)] // consumer: tests only, by design
    pub fn mid_sequence(&self) -> bool {
        self.state != StripState::Text
    }

    /// Give up on any dangling sequence (end of stream).
    pub fn reset(&mut self) {
        self.state = StripState::Text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_all(chunks: &[&str]) -> String {
        let mut s = ControlStripper::default();
        let mut out = String::new();
        for c in chunks {
            out.push_str(&s.strip(c));
        }
        out
    }

    #[test]
    fn plain_text_and_newlines_survive() {
        assert_eq!(strip_all(&["hello\n", "world\n"]), "hello\nworld\n");
        assert_eq!(strip_all(&["tab\there"]), "tab\there");
    }

    #[test]
    fn sgr_color_is_dropped() {
        assert_eq!(strip_all(&["\u{1b}[32;1mgreen\u{1b}[0m"]), "green");
    }

    /// The reason the stripper is stateful rather than a regex over each chunk.
    #[test]
    fn a_sequence_split_across_chunks_is_dropped_whole() {
        assert_eq!(
            strip_all(&["red \u{1b}[38;5;12", "0m colored end"]),
            "red  colored end"
        );
        assert_eq!(strip_all(&["a\u{1b}", "[", "H", "b"]), "ab");
    }

    #[test]
    fn osc_with_bel_and_with_st_are_dropped() {
        assert_eq!(strip_all(&["\u{1b}]0;window title\u{7}done"]), "done");
        assert_eq!(strip_all(&["\u{1b}]0;title\u{1b}\\done"]), "done");
        // Split mid-OSC too.
        assert_eq!(strip_all(&["x\u{1b}]0;ti", "tle\u{1b}\\y"]), "xy");
    }

    #[test]
    fn cursor_moves_and_alt_screen_leave_nothing_behind() {
        assert_eq!(
            strip_all(&["\u{1b}[?1049h", "\u{1b}[2J", "screen", "\u{1b}[?1049l"]),
            "screen"
        );
    }

    #[test]
    fn control_characters_are_dropped_but_printable_text_is_not() {
        assert_eq!(strip_all(&["a\u{1}\u{7}\u{7f}b"]), "ab");
        assert_eq!(strip_all(&["café ✓"]), "café ✓");
    }

    /// A dangling sequence at the end of a chunk must not be shown as text, and
    /// must not stick: a later plain chunk still comes through.
    #[test]
    fn a_dangling_escape_holds_then_releases() {
        let mut s = ControlStripper::default();
        assert_eq!(s.strip("abc\u{1b}["), "abc");
        assert!(s.mid_sequence());
        assert_eq!(s.strip("2Jdef"), "def");
        assert!(!s.mid_sequence());
    }
}
