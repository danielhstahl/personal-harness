//! `view`'s tests, split along the responsibilities the split of the module
//! itself made visible. Each file in here is one of them:
//!
//! * [`retention`](retention) — the store is a window, the journal is the
//!   document, and the ceiling is the sum of its stated halves;
//! * [`chord_rules`](chord_rules) — the rules that read `CHORD_TABLE` as data:
//!   one owner per key per state, every mode covered, the table a table;
//! * [`flush`](flush) — one cursor, one transcript, each finalized line exactly
//!   once;
//! * [`answers`](answers) — the step label, the input gate, the run clock;
//! * [`evict`](evict) — over budget: what a view drops and what it must not do
//!   while dropping it;
//! * [`rewrap`](rewrap) — resize: rebuild the rows, and count what the rebuild
//!   cannot reach.
//!
//! What more than one section drives lives here: the `SessionView` builder, the
//! recording journal that stands in for the file, and the flush/store readers a
//! test asserts through. They are private to this suite on purpose — every file
//! above reaches them through the glob below.

use crate::session::view::SessionView;
use crate::session::{SessionId, TerminalType};
use crate::state::scrollback::ROW_STRUCT_BYTES;
use crate::state::transcript::MessageKind;

mod answers;
mod chord_rules;
mod evict;
mod flush;
mod retention;
mod rewrap;

fn view(mode: TerminalType) -> SessionView {
    SessionView::new(SessionId::new(mode, 0))
}

use std::sync::{Arc, Mutex};

/// A journal that remembers what it was handed, for the tests that care about
/// what reached the file rather than what is on disk. The real writer has its
/// own tests (`services::journal`); duplicating its thread here would test
/// the scheduler.
#[derive(Default, Debug)]
struct RecordingJournal {
    got: Mutex<Vec<(&'static str, String)>>,
}

impl crate::services::journal::Journal for RecordingJournal {
    fn append(&self, mode: &'static str, text: String) {
        self.got.lock().unwrap().push((mode, text));
    }
    fn display_path(&self, mode: &str) -> Option<String> {
        Some(format!("/tmp/last-{mode}"))
    }
    fn close(&self) {}
    fn describe(&self) -> &'static str {
        "recording"
    }
}

impl RecordingJournal {
    fn text(&self) -> String {
        self.got
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<String>()
    }
}

fn with_journal(v: &mut SessionView, j: Arc<RecordingJournal>) {
    v.set_journal(j);
}

/// The flush invariant, stated as a test: monotonic. Note what the flusher's
/// own contract is — a prose block is rendered when it *closes* (blank line or
/// `done`), not on every newline — so these assertions are about the cursor,
/// not about line counts.
/// Flush and return **exactly the rows this flush appended**, as text.
///
/// `flush` hands back a count now rather than the batch: the batch was cloned
/// every real frame for a caller that did not exist (the band reads the
/// store), so the only reader of those rows was the test suite. The count
/// names the same rows precisely — the store's tail of that length *is* what
/// was just pushed, since the trim marker, if there is one, sits at the head
/// and never at the tail.
fn flush_new(v: &mut SessionView, w: u16) -> Vec<String> {
    let added = v.flush(w);
    let rows = v.scrollback().rows();
    rows[rows.len() - added..]
        .iter()
        .map(|r| r.to_string())
        .collect()
}

/// Every row the store holds, as text — for assertions that do not care
/// which flush produced the row.
fn store_text(v: &SessionView) -> String {
    v.scrollback()
        .rows()
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ────────── the re-render source is bounded by the cap (looprs-zie) ──────────
//
// A resize used to re-render every entry the view holds and let the store
// trim afterwards: 30,000 rows materialised to keep 5,787. The tests here
// are the shape of that difference at a size a unit test can hold — a cap of
// tens of rows, a transcript of hundreds — with the transcript buffer off
// (`with_buffer(.., 0)`) so the store cap is the only cap in the room.

/// A view of `entries` answers, each a few lines of prose, flushed at 80
/// against a store capped at `cap_rows` rows' worth of charge.
fn filled(cap_rows: usize, entries: usize) -> SessionView {
    let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Pi, 0), 0);
    v.set_store_cap(cap_rows * ROW_STRUCT_BYTES);
    for i in 0..entries {
        v.push_note(
                MessageKind::Answer,
                format!(
                    "answer {i:03}: the first line is long enough to be a whole row of prose at eighty columns.\n\
                     the second line keeps the paragraph going with ordinary words.\n\
                     and the third one closes it out."
                ),
            );
    }
    v
}

fn all_text(v: &SessionView) -> String {
    v.scrollback()
        .rows()
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
