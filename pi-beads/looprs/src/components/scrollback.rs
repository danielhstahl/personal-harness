// scrollback.rs
use crate::{
    components::tool::tool_line,
    state::state::{Entry, MessageKind, Transcript},
    theme::theme::{content_width, restyle, style_for},
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
            /*let lines = match e.kind {
                MessageKind::Thinking | MessageKind::Answer => self.cur.drain_stream(e, w),
                MessageKind::Tool { .. } => vec![tool_line(e, 0)],
                _ => render_simple(e, w), // always created `done`
            };*/
            let lines = if e.kind.is_streamed() {
                self.cur.drain_stream(e, w)
            } else if !e.done {
                break; // open tool: the viewport owns it
            } else if matches!(e.kind, MessageKind::Tool { .. }) {
                vec![tool_line(e, 0)] // spinner frame is irrelevant once finished
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
        let lines = if c.fence.is_some() {
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
