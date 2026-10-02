//Visualize tool execution
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::state::state::{Entry, MessageKind};
use crate::utils::utils::FRAMES;

pub fn tool_line(e: &Entry, spinner: usize) -> Line<'static> {
    let MessageKind::Tool { name, state, .. } = &e.kind else {
        return Line::default();
    };
    let mut spans = vec![Span::styled(
        format!("{} {name} ", state.icon(spinner)),
        Style::new().fg(state.color()),
    )];
    if !e.text.is_empty() {
        spans.push(Span::styled(
            e.text.clone(),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

#[derive(Clone, Copy, PartialEq)]
pub enum ToolStateCategory {
    InProgress,
    Error,
    Success,
}

impl ToolStateCategory {
    fn color(self) -> Color {
        match self {
            Self::InProgress => Color::Blue,
            Self::Error => Color::Red,
            Self::Success => Color::Green,
        }
    }
    fn icon(self, spinner: usize) -> &'static str {
        match self {
            Self::InProgress => FRAMES[spinner % FRAMES.len()],
            Self::Error => "✗",
            Self::Success => "✓",
        }
    }
}
pub struct LiveToolPreview<'a> {
    entry: &'a Entry,
    spinner: usize,
}

impl<'a> LiveToolPreview<'a> {
    pub fn new(entry: &'a Entry, spinner: usize) -> Self {
        Self { entry, spinner }
    }
}
impl<'a> Widget for LiveToolPreview<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(tool_line(self.entry, self.spinner)).render(area, buf);
        /*let icon = self.mode.icon(self.spinner);
        let tool_name = &self.tool_name;
        Paragraph::new(Line::from(Span::styled(
            format!("{icon} {tool_name} "),
            Style::default().fg(self.mode.color()),
        )))
        .render(area, buf);*/
    }
}
