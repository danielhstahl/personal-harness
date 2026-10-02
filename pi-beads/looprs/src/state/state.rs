use crate::components::tool::ToolStateCategory;

#[derive(Clone, PartialEq)]
pub enum MessageKind {
    User,
    Thinking,
    Answer,
    Tool {
        id: String,
        name: String,
        input: String,
        state: ToolStateCategory,
    },
    Error,
}
impl MessageKind {
    pub fn is_streamed(&self) -> bool {
        matches!(self, MessageKind::Thinking | MessageKind::Answer)
    }
}

pub struct Entry {
    pub kind: MessageKind,
    pub text: String,
    pub done: bool,
}

#[derive(Default)]
pub struct Transcript {
    pub entries: Vec<Entry>,
}

impl Transcript {
    pub fn new() -> Self {
        Self { entries: vec![] }
    }
    /// Streaming append. Same kind as the open entry extends it; a different kind
    /// closes it and starts a new one (thinking -> answer happens automatically).
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        match self.entries.last_mut() {
            Some(e) if e.kind == kind && !e.done => e.text.push_str(delta),
            _ => {
                self.finish_last();
                self.entries.push(Entry {
                    kind,
                    text: delta.to_string(),
                    done: false,
                });
            }
        }
    }

    /// Complete, one-shot entries (user message, tool result, error).
    pub fn push_done(&mut self, kind: MessageKind, text: String) {
        self.finish_last();
        self.entries.push(Entry {
            kind,
            text,
            done: true,
        });
    }

    /*pub fn finish_last(&mut self) {
        if let Some(e) = self.entries.last_mut() {
            e.done = true;
        }
    }*/

    //start of adding tools
    pub fn start_tool(&mut self, id: String, name: String, input: String) {
        self.finish_last();
        self.entries.push(Entry {
            kind: MessageKind::Tool {
                id,
                name,
                input,
                state: ToolStateCategory::InProgress,
            },
            text: String::new(), // result summary arrives later
            done: false,
        });
    }

    pub fn finish_tool(&mut self, id: String, summary: String, is_error: bool) {
        let found = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(&e.kind, MessageKind::Tool { id: i, .. } if *i == id));
        if let Some(e) = found {
            if let MessageKind::Tool { state, .. } = &mut e.kind {
                *state = if is_error {
                    ToolStateCategory::Error
                } else {
                    ToolStateCategory::Success
                };
            }
            e.text = summary;
            e.done = true;
        }
    }

    pub fn open_tools(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, MessageKind::Tool { .. }) && !e.done)
    }

    pub fn finish_last(&mut self) {
        // only streamed entries close implicitly; a running tool must not be closed
        // by whatever comes next (parallel tools, for example)
        // tools are often run in parallel, so this is required
        if let Some(e) = self.entries.last_mut().filter(|e| e.kind.is_streamed()) {
            e.done = true;
        }
    }
}
