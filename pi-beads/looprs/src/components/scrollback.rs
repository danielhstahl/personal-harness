// scrollback.rs
use crate::{
    components::card::card_line,
    state::transcript::{Entry, MessageKind, Transcript},
    theme::styles::{content_width, restyle, style_for},
    utils::md,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;

fn render_simple(e: &Entry, w: u16) -> Vec<Line<'static>> {
    let mut lines = match &e.kind {
        MessageKind::User => md::wrap(
            vec![Span::raw(e.text.clone())],
            w as usize,
            "❯ ".into(),
            "  ".into(),
        ),
        MessageKind::Error => vec![Line::styled(
            format!("error: {}", e.text),
            Style::new().red(),
        )],
        MessageKind::System => vec![Line::styled(format!("• {}", e.text), Style::new().yellow())],
        _ => unreachable!("streamed kinds use drain_stream"),
    };
    lines.push(Line::default()); // blank line after each entry
    lines
}

pub struct Flusher {
    first: usize, // entries[..first] are fully written to scrollback
    cur: Cursor,  // progress within entries[first]
}

#[derive(Default)]
struct Cursor {
    scan: usize,  // start of the first unconsumed line
    block: usize, // start of the open prose block (<= scan)
    fence: Option<(String, HighlightLines<'static>)>,
}

impl Flusher {
    pub fn new() -> Self {
        Self {
            first: 0,
            cur: Cursor::default(),
        }
    }

    /// Move the cursor to `first` and discard its in-progress per-entry state.
    ///
    /// Mandatory whenever the index moves rather than advancing normally: `cur`
    /// (scan/block/fence) is only meaningful for the entry `first` was reading, so
    /// pointing it somewhere else without resetting would make `drain_stream` slice
    /// the wrong text — the worst kind of bug, because it only shows up as garbled
    /// scrollback long after the cause. Used by the per-view buffer cap.
    pub fn reseat(&mut self, first: usize) {
        self.first = first;
        self.cur = Cursor::default();
    }

    /// Everything written to scrollback: `entries[..consumed]`. The per-view buffer
    /// cap asks this so it can tell a line the terminal already has from one it has
    /// not emitted yet — the first may be dropped, the second never may.
    pub fn consumed(&self) -> usize {
        self.first
    }

    /// Everything that became final since the last call. Call once per frame, before drawing.
    pub fn drain(&mut self, t: &Transcript, term_width: u16) -> Vec<Line<'static>> {
        let w = content_width(term_width);
        let mut out = Vec::new();
        while let Some(e) = t.entries.get(self.first) {
            let lines = if e.kind.is_raw() {
                // Shell output: verbatim lines, no markdown, no re-wrap
                // (ADR-0001 rule 1). Complete lines go out as they arrive; the
                // partial tail stays in the live region.
                self.cur.drain_raw(e)
            } else if e.kind.is_streamed() {
                self.cur.drain_stream(e, w)
            } else if !e.done {
                break; // an open card (tool or compaction): the viewport owns it
            } else if matches!(
                e.kind,
                MessageKind::Tool { .. } | MessageKind::Compaction { .. }
            ) {
                // A finished card: one row, drawn by the same renderer the live
                // band uses, so the scrollback copy and the live copy cannot
                // disagree about what the card said. Spinner frame is irrelevant
                // once finished.
                vec![card_line(e, 0)]
            } else {
                render_simple(e, w) // User / Error
            };
            out.extend(lines.into_iter().map(|l| restyle(l, style_for(&e.kind))));
            if !e.done {
                break;
            }
            self.first += 1;
            self.cur = Cursor::default();
        }
        out
    }

    /// The not-yet-final tail of the active entry. A pure read: same data, same cursor.
    pub fn preview(&self, t: &Transcript, term_width: u16) -> Vec<Line<'static>> {
        let Some(e) = t.entries.get(self.first).filter(|e| !e.done) else {
            return vec![];
        };
        let c = &self.cur;
        let lines = if e.kind.is_raw() {
            // The unfinished last line of shell output: verbatim, and never
            // re-flowed through the markdown path.
            vec![Line::from(e.text[c.scan..].to_string())]
        } else if c.fence.is_some() {
            vec![Line::from(e.text[c.scan..].to_string())]
        } else {
            md::render_markdown(&e.text[c.block..], content_width(term_width))
        };
        lines
            .into_iter()
            .map(|l| restyle(l, style_for(&e.kind)))
            .collect()
    }
}

impl Cursor {
    /// Raw, line-at-a-time rendering for shell output.
    ///
    /// Deliberately *not* `drain_stream`: that one accumulates a prose block and
    /// renders it through markdown when the block closes. Shell output must reach
    /// the scrollback as the exact lines the child wrote, as soon as each line is
    /// complete — waiting for a blank line would hold back everything a shell does
    /// between prompts, and markdown would reinterpret `# comment` as a heading.
    fn drain_raw(&mut self, e: &Entry) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        while let Some(nl) = e.text[self.scan..].find('\n') {
            let line = e.text[self.scan..self.scan + nl].trim_end_matches('\r');
            out.push(Line::from(line.to_string()));
            self.scan += nl + 1;
        }
        if e.done && self.scan < e.text.len() {
            out.push(Line::from(
                e.text[self.scan..].trim_end_matches('\r').to_string(),
            ));
            self.scan = e.text.len();
        }
        out
    }

    fn drain_stream(&mut self, e: &Entry, w: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        while let Some(nl) = e.text[self.scan..].find('\n') {
            let (start, end) = (self.scan, self.scan + nl);
            self.on_line(&e.text, start, end, end + 1, w, &mut out);
            self.scan = end + 1;
        }
        if e.done {
            if self.scan < e.text.len() {
                let len = e.text.len();
                self.on_line(&e.text, self.scan, len, len, w, &mut out);
                self.scan = len;
            }
            self.flush_block(&e.text, e.text.len(), w, &mut out);
            self.fence = None;
        }
        out
    }

    fn on_line(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        next: usize,
        w: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let line = text[start..end].trim_end_matches('\r');
        if let Some((marker, hl)) = self.fence.as_mut() {
            if line.trim_start().starts_with(marker.as_str()) {
                self.fence = None;
                out.push(Line::default());
            } else {
                out.push(md::code_line(hl, line));
            }
        } else if let Some(marker) = fence_marker(line) {
            self.flush_block(text, start, w, out);
            out.push(md::code_header(lang_of(line)));
            self.fence = Some((marker.into(), md::highlighter().start(lang_of(line))));
        } else if line.trim().is_empty() {
            self.flush_block(text, start, w, out);
        } else {
            return; // stays part of the open prose block
        }
        self.block = next;
    }

    fn flush_block(&self, text: &str, end: usize, w: u16, out: &mut Vec<Line<'static>>) {
        let src = &text[self.block..end];
        if !src.trim().is_empty() {
            out.extend(md::render_markdown(src, w));
            out.push(Line::default());
        }
    }
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    ["```", "~~~"].into_iter().find(|m| t.starts_with(m))
}
fn lang_of(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches(['`', '~'])
        .trim()
        .split([' ', ','])
        .next()
        .unwrap_or("")
}
