//! The kanban band: three columns, one bead per row, and an honest `+N more`
//! (looprs-5o4.4, under ADR-0007).
//!
//! [`crate::viewport`] decided how many rows the band gets;
//! [`crate::state::board`] decided which bead sits in which column. What is left
//! — and all this file is — is laying those rows out inside the [`Rect`] it is
//! handed, at a height somebody else already paid for.
//!
//! # Purity
//!
//! **The component is a pure function of snapshot + area.** No `Instant::now()`,
//! no `std::env::var`, no `bd` call, no channel, no remembered window size: the
//! age of the read arrives on [`BoardSnapshot::age`], computed upstream by
//! whoever holds the clock. That is the same rule
//! [`crate::components::status`] states for its row — *gather in App, decide in
//! the component, and the component is a pure function of what it is handed* —
//! and it is what lets every branch below be painted against a `TestBackend` with
//! no board, no subprocess and no `sleep` anywhere near it. ADR-0007 rule 10
//! ("do not put `bd` in the draw path") is a property of this signature, not of
//! good intentions.
//!
//! # The band's rows
//!
//! ```text
//! To-do 12         │ In progress 3     │ Complete 214   ← one header row, shared
//! ─────────────────┼───────────────────┼───────────────── ← the rule: granted, or not
//! ⊘ looprs-4 Fix … │ …                 │                ← one bead per row
//! +9 more          │                   │                ← the overflow marker
//! bd ok · 3s ago · ⏸ 2 deferred                        ← one footer row, never omitted
//! ```
//!
//! The header is **one row shared by three columns**, not three headers
//! ([`KANBAN_HEADER_ROWS`]), and the vertical rules are drawn in the *gutters*
//! rather than at a column's edge — which is why a column gets
//! `height − header − rule − footer` rows and not less.
//!
//! The rule row is the only one of the four that is **conditional**, and it is
//! conditional in one direction only: it is granted when the body would still
//! keep [`MIN_KANBAN_BODY_ROWS_WITH_RULE`] rows after paying for it, and is not
//! drawn at all otherwise. [`band_areas`] is the one place that decision is
//! made, so a band can never end up with a rule and nothing under it.
//!
//! # What each rule below is protecting
//!
//! * **A header count is the column's true total, never the drawn count**
//!   (ADR-0007 rule 7). It comes off the snapshot, so it cannot drift with what
//!   the frame happened to have room for. In a column too narrow to hold the
//!   count whole, **no count is drawn**: a truncated `12…` is not a smaller
//!   number, it is a wrong one.
//! * **The `+N more` row counts against the budget.** A column with 4 rows and
//!   more beads than that draws **3** beads and `+7 more` when the column holds
//!   ten: `3 + 7 = 10`, and that sum closing *is* ADR-0007's invariant I1 as
//!   seen from the frame. A column that drew 4 beads *and* `+7 more` would be
//!   claiming eleven. Getting this off by one is the single most likely bug in
//!   this widget, so it is tested at 1, 2, 3 and 5 rows rather than at one width
//!   and hoped for.
//! * **The marker is laid out before the title is truncated**, not appended
//!   after it. A marker the width cut could reach would leave a `blocked` bead
//!   looking exactly like pickable work: the bead dropped visually while still
//!   being counted. At the absolute floor of a one-column-wide row the marker is
//!   what survives, alone.
//! * **An empty column reads as empty, not as missing** — a dim `—`, and only
//!   when the read actually said so. A column that paints nothing is
//!   indistinguishable from a column that failed to render.
//! * **The failure states are drawn, not swallowed.** Never-loaded paints the
//!   header with `—` counts and `reading the board…`; an error keeps the last
//!   good rows, dims them, and shows the error's own words with the age of the
//!   read they came from. Empty, unread, stale and absent are four different
//!   paintings, because looprs-037 exists to keep them four different facts.
//! * **Narrow columns degrade rather than overlap.** The id outranks the title —
//!   the id is the handle a user types into `bd show`, and the title can be
//!   re-read from there — so below the width where both fit, the title goes
//!   first, and below the width where the id fits whole, the row says as much id
//!   as it honestly can and stops.
//!
//! # Style
//!
//! Everything comes from [`crate::theme::styles`]; there is no `Style` literal in
//! this file. The band is chrome sitting above the transcript, so it is
//! deliberately quieter than the thing the user is reading: no background, no
//! bold on the rows, and colour spent only on the two markers and a failed read.
//! The framing — the dividers and the rule — is `board_divider`, the same dark
//! gray as the footer, and it is **never** run through [`shade`]: the framing's
//! colour never changes, so `dim` goes on meaning one thing only, *these rows are
//! from the last good read*, rather than "some of this band is old".

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::components::status::fmt_elapsed;
use crate::services::bd::BD_TIMEOUT;
use crate::state::board::{BoardBead, BoardRead, BoardSnapshot, Column};
use crate::theme::styles::{
    board_divider, board_empty, board_footer_error, board_footer_ok, board_header, board_marker,
    board_overflow, board_row, board_staled,
};
use crate::utils::render::truncate_columns;
use crate::viewport::{KANBAN_FOOTER_ROWS, KANBAN_HEADER_ROWS};

/// The gutter between two columns, **including the divider drawn in it**.
///
/// Three columns, and the middle one is the `│`. It was two before the framing
/// existed, which left nowhere to put a divider without spending one of the two
/// cells on it: at two columns the line would have had to sit against a column's
/// last cell (touching the text it separates) or replace the gap entirely (no air
/// either side). Three gives the divider a cell of its own and a space of its own
/// on each side, and the columns keep every column they were given — the divider
/// is laid inside the gutter and never takes width from a row.
const COLUMN_GAP_COLS: u16 = 3;

/// The horizontal rule between the header and the body.
///
/// It exists because the header and the bead rows are the only two things on the
/// band that mean different categories of thing — *labels* above, *content* below
/// — and bold on the header alone did not say so: at a glance the header read as
/// a fourth bead row that happened to have no id in it.
const KANBAN_RULE_ROWS: u16 = 1;

/// What the body must be worth before the band will spend a row on the rule.
///
/// **This is the affordability rule, and it is the whole degradation order in one
/// number.** The rule is chrome, and every row of chrome on this band is bought
/// out of the body's surplus, so the rule is given up *before* a single bead row
/// is cut: a 3- or 4-row band has no rule at all and keeps every bead row it can
/// afford, and a 5-row band trades one of its four for the line. That makes the
/// framing the cheapest thing on the band to lose, which is the right ranking —
/// the rule makes the columns legible, and a bead row is the thing the user came
/// to read.
const MIN_KANBAN_BODY_ROWS_WITH_RULE: u16 = 2;

/// The least a column header must leave for its name before the name is dropped
/// in favour of the count.
///
/// Below this the name truncates to something that names nothing (`T…`), and a
/// label that stopped labelling is worth less than the number next to it — which
/// is the one number in the header a reader cannot reconstruct from anything
/// else on screen.
const MIN_HEADER_NAME_COLS: usize = 4;

/// The band, as painted from a snapshot.
///
/// Borrows the snapshot rather than owning it: the band is a *view* of the last
/// read, re-rendered as often as the frame runs, and cloning a few hundred beads
/// every frame to satisfy a widget signature is a cost with no name.
#[derive(Debug)]
pub struct Kanban<'a> {
    snapshot: &'a BoardSnapshot,
}

impl<'a> Kanban<'a> {
    pub fn new(snapshot: &'a BoardSnapshot) -> Self {
        Self { snapshot }
    }
}

impl Widget for Kanban<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        paint(self.snapshot, area, buf);
    }
}

/// Paint the whole band.
///
/// Split by split, top down. Everything here is total: an area of any size, down
/// to `0 × 0`, returns having painted nothing and without panicking, because a
/// small window must not be able to take the frame down with it.
pub fn paint(snapshot: &BoardSnapshot, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let band = band_areas(area);
    // Everything the last read did not answer for is drawn dimmed, decided once
    // here and carried down, so "stale" cannot be remembered in one column and
    // forgotten in another.
    let stale = snapshot.read.is_error();

    if let Some(header) = band.header {
        // The header row is split by the *same* column layout as the body, which
        // is the only way a column's name ends up over its rows.
        for (column, cell) in Column::ALL.iter().zip(column_areas(header)) {
            let line = header_line(
                *column,
                &snapshot.header_count(*column),
                cell.width as usize,
            );
            Paragraph::new(line).render(cell, buf);
        }
    }

    if let Some(body) = band.body {
        let rows = body.height as usize;
        for (column, cell) in Column::ALL.iter().zip(column_areas(body)) {
            let lines = column_lines(snapshot, *column, cell.width as usize, rows, stale);
            Paragraph::new(lines).render(cell, buf);
        }
    }

    if let Some(footer) = band.footer {
        let line = footer_line(snapshot, footer.width as usize);
        Paragraph::new(line).render(footer, buf);
    }

    // The framing goes on last. It is drawn into rects the content never touches,
    // so the order cannot matter in principle — and is put here anyway so that it
    // cannot matter in practice either: a divider rendered after the content
    // cannot be wiped by a paragraph that overran its cell, which is the one way
    // a framing bug could hide by being invisible rather than by being wrong.
    //
    // Both passes are outside `shade` on purpose. The framing is not stale-able:
    // a divider did not come from a read, so dimming it would split `dim` into
    // "old data" and "old decoration", and the first of those is the one the user
    // needs to be able to see.
    for chrome in [band.header, band.body].into_iter().flatten() {
        paint_dividers(chrome, buf);
    }
    if let Some(rule) = band.rule {
        let line = Line::styled(rule_line(rule.width as usize), board_divider());
        Paragraph::new(line).render(rule, buf);
    }
}

/// Draw the `│` down the middle of every gutter of `area`.
///
/// A `Paragraph` per gutter rather than poking cells in the buffer: same reason the
/// rest of this file renders — the widget is checked for width, clipping and style
/// by ratatui, and a `buf[(x, y)].set_symbol(…)` is checked by nobody until a
/// narrow terminal finds out the hard way.
fn paint_dividers(area: Rect, buf: &mut Buffer) {
    for gutter in gutter_areas(area) {
        let Some(col) = divider_col(gutter.width) else {
            continue;
        };
        let mut line = String::new();
        for i in 0..gutter.width as usize {
            line.push(if i == col { '│' } else { ' ' });
        }
        // One line per row of the gutter: the vertical rule runs the whole height
        // of whatever band it was given, header rows and body rows alike, so the
        // columns read as *columns* and not as three separate lists.
        let style = board_divider();
        let lines = vec![Line::styled(line.clone(), style); gutter.height as usize];
        Paragraph::new(lines).render(gutter, buf);
    }
}

/// The rule under the header, built from the *same* [`column_split`] as the
/// dividers it crosses.
///
/// Not a repeated `─`: the junctions have to land on the divider columns, and a
/// string built without reference to the layout puts them wherever its own
/// rounding felt like. Every chunk of the split is answered with its own width in
/// its own glyph, so the result is exactly `width` columns wide at any width,
/// including the ones where the gutters have been rounded away.
fn rule_line(width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chunks = column_split(Rect::new(0, 0, width as u16, KANBAN_RULE_ROWS));
    let mut line = String::with_capacity(width);
    for (i, chunk) in chunks.iter().enumerate() {
        let w = chunk.width as usize;
        // A column chunk is all rule; a gutter chunk is rule up to the divider
        // column, the junction on it, and rule for what is left of the gutter.
        let junction = if i % 2 == 1 {
            divider_col(chunk.width)
        } else {
            None
        };
        match junction {
            Some(col) => {
                line.push_str(&"─".repeat(col));
                line.push('┼');
                line.push_str(&"─".repeat(w.saturating_sub(col + 1)));
            }
            None => line.push_str(&"─".repeat(w)),
        }
    }
    line
}

/// The band's three rows: header, body, footer.
///
/// Built from the bottom up, because that is the order the guarantees rank in:
/// the footer is the row that may never be omitted (it is the row that says
/// whether any of this is true), so it is taken first and the header is what has
/// room left over. Given one row the band paints a footer and no header, which is
/// the right way to be short — short in the chrome, not short in the state.
struct Band {
    header: Option<Rect>,
    /// The rule under the header — `None` when the body could not pay for it
    /// ([`MIN_KANBAN_BODY_ROWS_WITH_RULE`]), which is the only way this band has
    /// of being short in the chrome rather than in the content.
    rule: Option<Rect>,
    body: Option<Rect>,
    footer: Option<Rect>,
}

fn band_areas(area: Rect) -> Band {
    let footer = if area.height >= KANBAN_FOOTER_ROWS {
        Some(Rect {
            x: area.x,
            y: area.bottom().saturating_sub(KANBAN_FOOTER_ROWS),
            width: area.width,
            height: KANBAN_FOOTER_ROWS,
        })
    } else {
        None
    };

    let above_footer = area.height.saturating_sub(KANBAN_FOOTER_ROWS);
    let header = if above_footer >= KANBAN_HEADER_ROWS {
        Some(Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: KANBAN_HEADER_ROWS,
        })
    } else {
        None
    };
    // Everything below the header is laid out from a single cursor, because the
    // rule sits between two rows that both already had fixed offsets and a second
    // `area.y + …` in the middle of them is exactly where a row starts being
    // painted on top of its own neighbour.
    let used_by_header = if header.is_some() {
        KANBAN_HEADER_ROWS
    } else {
        0
    };
    let room = above_footer.saturating_sub(used_by_header);
    let mut y = area.y.saturating_add(used_by_header);

    // Granted only if the body still keeps its own minimum afterwards — read the
    // constant, not this line, for why it is phrased that way.
    let rule = if room >= KANBAN_RULE_ROWS + MIN_KANBAN_BODY_ROWS_WITH_RULE {
        Some(Rect {
            x: area.x,
            y,
            width: area.width,
            height: KANBAN_RULE_ROWS,
        })
    } else {
        None
    };
    let used_by_rule = if rule.is_some() { KANBAN_RULE_ROWS } else { 0 };
    y = y.saturating_add(used_by_rule);
    let body_rows = room.saturating_sub(used_by_rule);
    let body = if body_rows > 0 {
        Some(Rect {
            x: area.x,
            y,
            width: area.width,
            height: body_rows,
        })
    } else {
        None
    };
    Band {
        header,
        rule,
        body,
        footer,
    }
}

/// The three column rects inside `area`, near-equal width, split by
/// [`COLUMN_GAP_COLS`]-column gutters.
///
/// `Fill(1)` on each rather than the old sketch's `Percentage(33/34/33)`: the
/// percentages were one hardcoded answer to "how wide is a column", and that
/// answer quietly changes meaning with the window — percentages of a 47-column
/// band leave a column of slack unpainted at the right edge, while `Fill` hands
/// the remainder out with its own rounding and spends every column it was given.
/// Used for the header row and the body rows alike, so a column's name is always
/// over its rows.
fn column_split(area: Rect) -> [Rect; 5] {
    Layout::horizontal([
        Constraint::Fill(1),                 // To-do
        Constraint::Length(COLUMN_GAP_COLS), // gutter
        Constraint::Fill(1),                 // In progress
        Constraint::Length(COLUMN_GAP_COLS), // gutter
        Constraint::Fill(1),                 // Complete
    ])
    .areas(area)
}

/// The three column rects inside `area` — the even chunks of [`column_split`].
fn column_areas(area: Rect) -> [Rect; 3] {
    let chunks = column_split(area);
    std::array::from_fn(|i| chunks[i * 2])
}

/// The two gutter rects inside `area` — the odd chunks of [`column_split`], and
/// the only place a divider may be drawn.
///
/// Taken from the same split rather than computed as `column.right() + 1`: the
/// gutter's position is the *only* fact a divider needs, and the two ways of
/// naming it disagree as soon as a layout rounds, which is how a column ends up
/// with its rule one cell to the left of where the other two are.
fn gutter_areas(area: Rect) -> [Rect; 2] {
    let chunks = column_split(area);
    [chunks[1], chunks[3]]
}

/// The column *inside a gutter* that the vertical line occupies.
///
/// One function answers for the `│` and for the `┼` on the rule, because those
/// two have to be in the same cell at every width or the framing shows a crossing
/// that leads nowhere. `None` is a gutter too narrow to hold a line at all (a
/// band this narrow has no third column either) and the caller draws nothing.
fn divider_col(gutter_width: u16) -> Option<usize> {
    if gutter_width == 0 {
        return None;
    }
    Some((gutter_width as usize - 1) / 2)
}

/// One column header: the name, then its true total.
///
/// The count never gives way to the name, and is never itself cut. A column too
/// narrow for `name + space + count` keeps the name only if the name still has
/// [`MIN_HEADER_NAME_COLS`] to say itself with; below that the name goes and the
/// count stands alone, and below the width of the count itself the cell is left
/// empty rather than showing a truncated number.
fn header_line(column: Column, count: &str, width: usize) -> Line<'static> {
    let need = count.width() + 1;
    let text = if width >= need + MIN_HEADER_NAME_COLS {
        format!(
            "{} {}",
            truncate_columns(column.name(), width - need),
            count
        )
    } else if width >= count.width() {
        count.to_string()
    } else {
        String::new()
    };
    Line::styled(text, board_header())
}

/// The lines one column paints, capped at `rows`, overflow marker included.
///
/// **The marker row is one of the `rows`, not an extra one.** With `rows = 4`
/// and ten beads the column draws three beads and `+7 more`. A column that drew
/// four *and* said `+7 more` would be claiming eleven.
fn column_lines(
    snapshot: &BoardSnapshot,
    column: Column,
    width: usize,
    rows: usize,
    stale: bool,
) -> Vec<Line<'static>> {
    if rows == 0 || width == 0 {
        return Vec::new();
    }
    let beads = snapshot.beads_in(column);

    if beads.is_empty() {
        // `—` claims *"the read counted this column and it holds nothing"*. Only
        // a read that answered may say that: after an error the column is simply
        // not drawn, because what we would be asserting is the state of a board
        // we failed to read.
        if snapshot.read.is_ok() {
            return vec![Line::styled("—", board_empty())];
        }
        return Vec::new();
    }

    let total = beads.len();
    if total <= rows {
        return beads.iter().map(|b| bead_line(b, width, stale)).collect();
    }
    let shown = rows - 1;
    let mut lines: Vec<Line<'static>> = beads
        .iter()
        .take(shown)
        .map(|b| bead_line(b, width, stale))
        .collect();
    lines.push(overflow_line(total - shown, width));
    lines
}

/// One bead row: marker, id, title — in that order of priority.
///
/// The order is not aesthetic, it is the order things get *paid for* out of a
/// narrow column's width. The marker buys first because a row that loses its `⊘`
/// stops saying "not pickable work" and starts looking like everything else: the
/// bead still counted, now lying about what it is. The id buys next because it is
/// the handle the user acts on. The title — prose, and re-readable with one
/// `bd show` — is what gives way.
fn bead_line(bead: &BoardBead, width: usize, stale: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if width == 0 {
        return Line::from(spans);
    }
    let mut used = 0usize;

    if let Some(marker) = bead.marker {
        let glyph = marker.glyph();
        if width >= glyph.width() {
            // The separating space is paid for only if the row can spare a whole
            // column on top of the glyph. At the floor — one column, marker
            // alone — the marker riding alone is still the correct painting: it
            // is the last thing here that gets cut, because it is the thing that
            // says what the row means.
            let sep = if width > glyph.width() { " " } else { "" };
            spans.push(Span::styled(
                format!("{glyph}{sep}"),
                shade(board_marker(marker), stale),
            ));
            used += glyph.width() + sep.width();
        }
    }

    let avail = width.saturating_sub(used);
    let id_w = bead.id.width();
    if id_w > avail {
        // The id is the last thing standing, and it may not be cut *silently*: a
        // truncated ticket id that looked whole would read as a *different* ticket
        // id, and a row that misnames the work is worse than a row that says less
        // about it. So the row fills what is left with as much id as fits, wears
        // the ellipsis so the cut is visible, and ends there.
        spans.push(Span::styled(
            truncate_columns(&bead.id, avail),
            shade(board_row(), stale),
        ));
        return Line::from(spans);
    }
    spans.push(Span::styled(bead.id.clone(), shade(board_row(), stale)));
    used += id_w;

    // The title takes what is left, minus the space in front of it. `>= 2` so a
    // row never spends a column on a space that leads nowhere.
    let rest = width.saturating_sub(used);
    if rest >= 2 && !bead.title.trim().is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            truncate_columns(bead.title.trim(), rest - 1),
            shade(board_row(), stale),
        ));
    }
    Line::from(spans)
}

/// The overflow row: `+N more`, where `N` is every bead in this column this
/// frame did not draw — the whole and only remainder, never rounded, never
/// counting the marker row itself.
///
/// `+N` rather than `+N more` if the width cannot hold the words, and truncated
/// if it cannot hold even that: by the point the marker is being cut the column is
/// three cells wide, and an ellipsis saying "…and more than this" is still the
/// truest thing available.
fn overflow_line(hidden: usize, width: usize) -> Line<'static> {
    let full = format!("+{hidden} more");
    let text = if full.width() > width {
        format!("+{hidden}")
    } else {
        full
    };
    Line::styled(truncate_columns(&text, width), board_overflow())
}

/// The footer: the band's one row of *about itself*.
///
/// Composed as a head and tails so the row can be cut in the right place. The
/// head is the sentence ("bd ok", or the error's own words); the tails are the
/// freshness facts (`· ⏸ 2 deferred`, `· stale 42s`). When the row runs short
/// **the head gives and the tails stand**: "how out of date is this" is the
/// question the footer exists to answer, and an error that reads as current news
/// is worse than an error whose message was cut short.
fn footer_line(snapshot: &BoardSnapshot, width: usize) -> Line<'static> {
    let (head, tails, style) = footer_parts(snapshot);
    let head_avail = width.saturating_sub(tails.width());
    Line::styled(
        format!("{}{}", truncate_columns(&head, head_avail), tails),
        style,
    )
}

/// The footer's sentence, its freshness tails, and its colour — one row of the
/// ADR §4 table, picked by the snapshot's own state rather than by re-reading an
/// error string.
fn footer_parts(snapshot: &BoardSnapshot) -> (String, String, Style) {
    let stale_tail = stale_suffix(snapshot);
    match &snapshot.read {
        // Never a blank band. Before the first snapshot lands the band says so,
        // because a blank band reads as "no beads" and that conflation is
        // looprs-037's whole reason for existing.
        BoardRead::Never => (
            "reading the board…".to_string(),
            String::new(),
            board_footer_ok(),
        ),
        BoardRead::Ok => {
            // `age == None` on an `Ok` read means the caller had no timestamp to
            // give. Say `bd ok` and claim nothing about when: `0s ago` would be
            // inventing a reading.
            let head = match snapshot.age {
                Some(age) => format!("bd ok · {} ago", fmt_elapsed(age)),
                None => "bd ok".to_string(),
            };
            // The deferred count rides here and nowhere else: it is not a row
            // (ADR-0007 §1), so this is the only way it reaches the eye. Shown
            // only when non-zero — `⏸ 0 deferred` is noise about an absence.
            let deferred = if snapshot.deferred > 0 {
                format!(" · ⏸ {} deferred", snapshot.deferred)
            } else {
                String::new()
            };
            (head, deferred, board_footer_ok())
        }
        BoardRead::Unavailable { reason } => (
            format!("bd unavailable: {reason}"),
            stale_tail,
            board_footer_error(),
        ),
        BoardRead::Failed { code, message } => {
            let code = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let head = match message {
                Some(msg) => format!("bd failed (exit {code}): {msg}"),
                None => format!("bd failed (exit {code})"),
            };
            (head, stale_tail, board_footer_error())
        }
        BoardRead::Malformed => (
            "bd answered unreadably — see the log".to_string(),
            stale_tail,
            board_footer_error(),
        ),
        // The number comes off `BD_TIMEOUT` rather than being written out, so the
        // row cannot promise a patience the service does not have.
        BoardRead::Timeout => (
            format!("bd did not answer in {}", fmt_elapsed(BD_TIMEOUT)),
            stale_tail,
            board_footer_error(),
        ),
    }
}

/// ` · stale 42s`, when there is a last-good read left to be stale about.
///
/// No age means there is nothing behind the band to date, and `stale —` would be
/// an adjective about nothing.
fn stale_suffix(snapshot: &BoardSnapshot) -> String {
    snapshot
        .age
        .map(|age| format!(" · stale {}", fmt_elapsed(age)))
        .unwrap_or_default()
}

/// The last-good pass: dim whatever style a row came in with, or leave it alone.
///
/// A function rather than an `if` at each call site so that "everything stale is
/// dimmed the same way" is one decision rather than four that can disagree.
fn shade(style: Style, stale: bool) -> Style {
    if stale { board_staled(style) } else { style }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::bd::{Bead, BeadIssueType, BeadStatus, BeadStatusFallback};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use std::time::Duration;

    // ─────────────────────────── the painting harness ───────────────────────────
    //
    // Every assertion below is made against a real `TestBackend`: what a row
    // *is* is the cells it painted, including their widths and their colours, so
    // "it fits inside the column" is measured rather than argued. No board, no
    // `bd`, no clock — the purity of the component is what makes painting a
    // hundred states here cost nothing.

    fn bead(id: &str, title: &str, status: BeadStatus) -> Bead {
        Bead {
            id: id.to_string(),
            title: title.to_string(),
            status: BeadStatusFallback::Known(status),
            issue_type: BeadIssueType::Task,
        }
    }

    /// A good read, three seconds old.
    fn board(beads: &[Bead]) -> BoardSnapshot {
        BoardSnapshot::from_beads(beads, Some(Duration::from_secs(3)))
    }

    fn buffer(snap: &BoardSnapshot, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| f.render_widget(Kanban::new(snap), f.area()))
            .unwrap();
        term.backend().buffer().clone()
    }

    /// The band as it landed: whole rows, plus each column's own body rows.
    struct Painted {
        /// Every row of the band, right-trimmed.
        rows: Vec<String>,
        /// Each column's body rows, right-trimmed, indexed by `Column::ALL`.
        cols: [Vec<String>; 3],
        band: Band,
        buf: ratatui::buffer::Buffer,
    }

    impl Painted {
        /// The gutters between the columns, across the body rows.
        fn gutters(&self) -> Vec<Rect> {
            let Some(body) = self.band.body else {
                return Vec::new();
            };
            let c = column_areas(body);
            [(c[0], c[1]), (c[1], c[2])]
                .into_iter()
                .filter_map(|(left, right)| {
                    let width = right.x.saturating_sub(left.right());
                    (width > 0).then_some(Rect::new(left.right(), body.y, width, body.height))
                })
                .collect()
        }

        /// The framing's own vocabulary: what a gutter is allowed to contain.
        ///
        /// `""` is in there because a wide glyph leaves its continuation cell
        /// blank — folded by `region_text`, not here, but a cell the backend
        /// never wrote to reads as `""` when asked directly.
        const FRAMING: [&'static str; 5] = [" ", "", "│", "─", "┼"];

        /// Every symbol in `area` that is neither blank nor one of the framing's
        /// own glyphs. Empty means the area holds framing and nothing else.
        ///
        /// This replaces the `untouched` helper that used to guard the gutters —
        /// "is this area blank" stopped being the right question the moment the
        /// band started drawing in them, and a boolean was the worse answer to the
        /// new one anyway: this names the cell that got in.
        fn intrusions(&self, area: Rect) -> Vec<String> {
            let mut out = Vec::new();
            for dy in 0..area.height {
                for dx in 0..area.width {
                    let sym = self.buf[(area.x + dx, area.y + dy)].symbol();
                    if !Self::FRAMING.contains(&sym) {
                        out.push(format!("({dx},{dy})={sym:?}"));
                    }
                }
            }
            out
        }

        /// The `x` positions in `area` whose **top row** carries `glyph` — the way
        /// to ask "where is the divider" of the painted buffer rather than of the
        /// code that drew it.
        fn glyph_xs(&self, area: Rect, glyph: &str) -> Vec<u16> {
            (0..area.width)
                .filter(|&dx| self.buf[(area.x + dx, area.y)].symbol() == glyph)
                .map(|dx| area.x + dx)
                .collect()
        }

        fn to_do(&self) -> &[String] {
            &self.cols[0]
        }
        fn in_progress(&self) -> &[String] {
            &self.cols[1]
        }
        fn complete(&self) -> &[String] {
            &self.cols[2]
        }
        fn header(&self) -> &str {
            self.rows.first().map(String::as_str).unwrap_or("")
        }
        fn footer(&self) -> &str {
            self.rows.last().map(String::as_str).unwrap_or("")
        }
    }

    /// The component's own code: every line of this file that is neither a
    /// comment nor part of this test module.
    ///
    /// Both exclusions carry weight. The module doc *names* the things these
    /// tests forbid (`Instant::now()`, `std::env::var`, a `bd` call), and the
    /// forbid-list below is itself code that contains every banned string — so
    /// "scan the file for clock reads" has to mean "scan the component", or it
    /// fails on its own prose.
    fn component_code() -> String {
        let src = include_str!("kanban.rs");
        let body = src.split("#[cfg(test)]\nmod tests").next().unwrap_or(src);
        body.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The non-empty rows of a column, i.e. the rows it actually painted.
    fn drawn(col: &[String]) -> Vec<&str> {
        col.iter()
            .map(String::as_str)
            .filter(|r| !r.is_empty())
            .collect()
    }

    /// The text a rect of the buffer holds, one string per row, with
    /// wide-character continuation cells folded out.
    ///
    /// `TestBackend` gives a double-width glyph one cell and leaves the cell it
    /// visually occupies blank; read back verbatim, every 漢字 in the string
    /// carries a space of its own and a column reads wider than the terminal was
    /// painted. Folding the continuation cell away makes a row's display width
    /// the number of cells the widget actually consumed, which is the quantity
    /// every width assertion here is about.
    fn region_text(buf: &ratatui::buffer::Buffer, area: Rect) -> Vec<String> {
        (0..area.height)
            .map(|dy| {
                let mut line = String::new();
                let mut skip = 0usize;
                for dx in 0..area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = buf[(area.x + dx, area.y + dy)].symbol();
                    line.push_str(sym);
                    skip = sym.width().saturating_sub(1);
                }
                line.trim_end().to_string()
            })
            .collect()
    }

    fn paint_snap(snap: &BoardSnapshot, w: u16, h: u16) -> Painted {
        let buf = buffer(snap, w, h);
        let band = band_areas(Rect::new(0, 0, w, h));
        let rows = region_text(&buf, Rect::new(0, 0, w, h));
        let mut cols: [Vec<String>; 3] = Default::default();
        if let Some(body) = band.body {
            for (i, cell) in column_areas(body).iter().enumerate() {
                cols[i] = region_text(&buf, *cell);
            }
        }
        Painted {
            rows,
            cols,
            band,
            buf,
        }
    }

    /// The band height that renders a body of exactly `want` rows at width `w`
    /// (the smallest such height, since the affordability rule makes `want`
    /// reachable with or without the rule row).
    ///
    /// **Derived from [`band_areas`] rather than computed as `want + 2`.** That
    /// arithmetic was true when the band was header + body + footer; with a rule
    /// row that the band may spend, `want + 2` silently paints a *shorter* body
    /// than the helper's own name promises, and every overflow assertion built on
    /// it quietly becomes a test of `want − 1` — failing loudly at best and, if
    /// the expected numbers were copied from the same wrong formula, passing while
    /// proving nothing. Asking the layout what it would render is the only version
    /// that stays true when the framing changes again.
    fn height_for_body(w: u16, want: usize) -> u16 {
        (1u16..=64)
            .find(|&h| {
                band_areas(Rect::new(0, 0, w, h))
                    .body
                    .is_some_and(|b| b.height as usize == want)
            })
            .unwrap_or_else(|| panic!("no band height renders a body of {want} rows at {w} cols"))
    }

    fn todo_beads(total: usize) -> Vec<Bead> {
        (0..total)
            .map(|i| {
                bead(
                    &format!("looprs-{:02}", i),
                    &format!("title {i}"),
                    BeadStatus::Open,
                )
            })
            .collect()
    }

    /// Paint a board whose To-do column holds `total` plain beads and whose other
    /// two columns are empty, at a band height that buys `body` rows per column.
    fn todo_only(total: usize, body: usize) -> Painted {
        paint_snap(&board(&todo_beads(total)), 90, height_for_body(90, body))
    }

    // ─────────────────────────── the full board ───────────────────────────

    #[test]
    fn a_full_board_paints_three_named_columns_over_one_shared_header_row() {
        let snap = board(&[
            bead("looprs-01", "Rewrap the transcript", BeadStatus::Open),
            bead("looprs-02", "Blocked on Dolt", BeadStatus::Blocked),
            bead("looprs-03", "Painting the band", BeadStatus::InProgress),
            bead("looprs-04", "The old sketch", BeadStatus::Closed),
            bead("looprs-05", "An unknown status", BeadStatus::Unknown),
        ]);
        let p = paint_snap(&snap, 90, 6);

        // One header row, all three names and their true totals on it.
        assert!(p.header().contains("To-do 3"), "{}", p.header());
        assert!(p.header().contains("In progress 1"), "{}", p.header());
        assert!(p.header().contains("Complete 1"), "{}", p.header());
        // ...and no second header row: the names do not repeat down the band.
        for row in &p.rows[1..p.rows.len() - 1] {
            assert!(
                !row.contains("To-do") && !row.contains("Complete"),
                "a header row leaked into the body: {row:?}"
            );
        }
        // Body: three in To-do, one each in the other two. (`drawn`, not the raw
        // per-row vector — that one is as long as the column is tall, blanks
        // included, which is a different question.)
        assert_eq!(drawn(p.to_do()).len(), 3, "{:?}", p.to_do());
        assert_eq!(drawn(p.in_progress()).len(), 1);
        assert_eq!(drawn(p.complete()).len(), 1);
        // The unknown bead is *in* To-do, marked — not dropped.
        assert!(
            p.to_do().iter().any(|r| r.starts_with('?')),
            "Unknown must show up in To-do marked ?: {:?}",
            p.to_do()
        );
        assert!(
            p.to_do().iter().any(|r| r.starts_with("⊘")),
            "blocked must show up in To-do marked ⊘: {:?}",
            p.to_do()
        );
    }

    #[test]
    fn one_bead_per_row_and_no_row_holds_two_of_them() {
        let beads: Vec<Bead> = (0..4)
            .map(|i| bead(&format!("looprs-{i}"), "t", BeadStatus::Open))
            .collect();
        // Height derived, so "four body rows" is the layout's answer and not a
        // guess about how many rows the framing will eat.
        let p = paint_snap(&board(&beads), 90, height_for_body(90, 4));
        let drawn = drawn(p.to_do());
        assert_eq!(drawn.len(), 4, "one per row, four rows: {drawn:?}");
        for row in drawn {
            // Two ids on one row would be the column overlapping its neighbour.
            assert_eq!(
                row.matches("looprs-").count(),
                1,
                "two beads on one row: {row:?}"
            );
        }
    }

    // ─────────────────────────── the +N arithmetic ───────────────────────────

    /// The ticket's own test: the overflow arithmetic at **1, 2, 3 and 5 rows**.
    ///
    /// `N` must be every bead the column did not draw — with the marker itself
    /// eating one of the available rows. Asserted against an independently
    /// computed expectation rather than against a literal transcription of the
    /// same arithmetic, so a bug in the formula cannot hide by being written
    /// into the test too.
    #[test]
    fn the_overflow_marker_is_honest_at_one_two_three_and_five_rows() {
        for body in [1usize, 2, 3, 5] {
            for total in [body, body + 1, body + 3, body + 10] {
                let p = todo_only(total, body);
                let drawn = drawn(p.to_do());
                if total <= body {
                    assert_eq!(
                        drawn.len(),
                        total,
                        "body {body}, total {total}: every bead should be drawn"
                    );
                    assert!(
                        !p.header().contains('+') && !drawn.iter().any(|r| r.contains("more")),
                        "an overflow marker on a column that overflowed nothing: {drawn:?}"
                    );
                    continue;
                }
                // Expected, computed without reference to the widget:
                // the marker eats a row, so `shown = body - 1` and `N = total - shown`.
                let expect_shown = body - 1;
                let expect_n = total - expect_shown;
                assert_eq!(
                    drawn.len(),
                    body,
                    "body {body}, total {total}: the column should fill every row"
                );
                let last = drawn[drawn.len() - 1];
                assert_eq!(
                    last,
                    format!("+{expect_n} more"),
                    "body {body}, total {total}"
                );
                // The header still counts the whole column, marker included.
                assert_eq!(
                    expect_shown + expect_n,
                    total,
                    "drawn + N must equal the read, body {body} total {total}"
                );
                assert!(
                    p.header().contains(&format!("To-do {total}")),
                    "the header must show the true total, not the drawn count: {}",
                    p.header()
                );
                // And the marker row is the marker's own: no bead on it.
                assert_eq!(
                    drawn[..expect_shown]
                        .iter()
                        .filter(|r| r.contains("more"))
                        .count(),
                    0
                );
            }
        }
    }

    /// Exactly one bead per row, at the boundary: a column of exactly `rows`
    /// beads fills the column with beads and paints no marker at all.
    #[test]
    fn a_column_exactly_full_paints_every_bead_and_no_marker() {
        let p = todo_only(4, 4);
        assert_eq!(drawn(p.to_do()).len(), 4);
        assert!(
            !p.rows.iter().any(|r| r.contains("more")),
            "no overflow to report: {:?}",
            p.rows
        );
    }

    /// A one-row column with more than one bead shows the marker, not a bead:
    /// choosing one bead and hiding the rest silently is the alternative, and it
    /// is worse.
    #[test]
    fn a_single_body_row_column_reports_the_whole_column_as_more() {
        let p = todo_only(3, 1);
        assert_eq!(drawn(p.to_do()), vec!["+3 more"]);
    }

    #[test]
    fn all_three_columns_carry_their_own_overflow_marker_at_once() {
        let mut beads = Vec::new();
        for i in 0..6 {
            beads.push(bead(&format!("todo-{i}"), "t", BeadStatus::Open));
        }
        for i in 0..5 {
            beads.push(bead(&format!("prog-{i}"), "t", BeadStatus::InProgress));
        }
        for i in 0..7 {
            beads.push(bead(&format!("done-{i}"), "t", BeadStatus::Closed));
        }
        // body = 3 rows per column → shown 2, N = total - 2 in every column.
        let p = paint_snap(&board(&beads), 90, height_for_body(90, 3));
        for (col, total) in [
            (p.to_do(), 6usize),
            (p.in_progress(), 5usize),
            (p.complete(), 7usize),
        ] {
            let drawn = drawn(col);
            assert_eq!(drawn.len(), 3, "every row of the column is used");
            assert_eq!(drawn[2], format!("+{} more", total - 2), "{drawn:?}");
            assert_eq!(drawn[..2].iter().filter(|r| r.contains("more")).count(), 0);
        }
        // `+N more` counts only that column's undrawn rows, so the three markers
        // are three different numbers, not one number copied around.
        assert_eq!(p.to_do()[2], "+4 more");
        assert_eq!(p.in_progress()[2], "+3 more");
        assert_eq!(p.complete()[2], "+5 more");
    }

    // ─────────────────────────── the empty board ───────────────────────────

    /// An empty board reads as empty: three zeros and a dim `—` in each column.
    /// Not a missing band, and not the not-loaded band.
    #[test]
    fn an_empty_board_reads_as_three_zeros_and_a_dash_in_every_column() {
        let p = paint_snap(&board(&[]), 90, 5);
        assert!(p.header().contains("To-do 0"), "{}", p.header());
        assert!(p.header().contains("In progress 0"), "{}", p.header());
        assert!(p.header().contains("Complete 0"), "{}", p.header());
        for col in &p.cols {
            assert_eq!(
                drawn(col),
                vec!["—"],
                "an empty column still says something"
            );
        }
        assert!(p.footer().contains("bd ok"), "{}", p.footer());
    }

    /// ...and the `—` is *dim*, so three of them read as chrome rather than as
    /// content the user might have to act on.
    #[test]
    fn the_empty_marker_is_dimmed() {
        let snap = board(&[]);
        let buf = buffer(&snap, 90, 5);
        let body = band_areas(Rect::new(0, 0, 90, 5)).body.expect("a body");
        let first = column_areas(body)[0];
        let cell = &buf[(first.x, first.y)];
        assert_eq!(cell.symbol(), "—");
        assert_eq!(
            cell.style().fg,
            Some(Color::DarkGray),
            "the empty marker should be a dim dash, not a bright one"
        );
    }

    // ─────────────────────────── the never-loaded band ───────────────────────────

    /// The state looprs-037 was filed for, painted: **never loaded** must not
    /// look like **empty board**.
    #[test]
    fn a_band_that_has_never_loaded_says_it_is_reading_and_never_looks_empty() {
        let loading = paint_snap(&BoardSnapshot::loading(), 90, 5);
        let empty = paint_snap(&board(&[]), 90, 5);

        assert_eq!(loading.footer(), "reading the board…");
        assert_ne!(
            loading.footer(),
            empty.footer(),
            "not-loaded and empty-board must not share a rendering"
        );
        // Counts are `—`, not 0: nothing has been counted.
        for name in ["To-do", "In progress", "Complete"] {
            assert!(
                loading.header().contains(&format!("{name} —")),
                "{} missing its unknown count: {}",
                name,
                loading.header()
            );
            assert!(
                !loading.header().contains(&format!("{name} 0")),
                "not-loaded must not read as an empty board: {}",
                loading.header()
            );
        }
        // ...and no rows of beads, but the band itself is painted: header and
        // footer both carry content.
        assert!(!loading.rows[0].trim().is_empty());
        assert!(!loading.rows[4].trim().is_empty());
    }

    // ─────────────────────────── the error states ───────────────────────────

    /// An error keeps the last good rows, dims them, and says its own words.
    #[test]
    fn a_failed_read_keeps_the_last_good_rows_and_says_its_own_words() {
        let good = board(&[
            bead("looprs-keep", "Still on the board", BeadStatus::Open),
            bead("looprs-blocked", "Waiting on a human", BeadStatus::Blocked),
        ]);
        let err = err_unavailable();
        let after = good.with_error(&err, Some(Duration::from_secs(42)));
        let p = paint_snap(&after, 90, 5);

        // The rows survived the error.
        assert!(p.to_do().iter().any(|r| r.contains("looprs-keep")));
        assert!(p.to_do().iter().any(|r| r.starts_with("⊘")));
        // The header stopped counting, because the read did not answer.
        assert!(p.header().contains("To-do —"), "{}", p.header());
        // The footer carries the error's own words and the age of what is drawn.
        assert!(
            p.footer()
                .contains("bd unavailable: No such file or directory"),
            "{}",
            p.footer()
        );
        assert!(p.footer().contains("stale 42s"), "{}", p.footer());
        // ...and `bd unavailable` is tellable apart from `board empty` from this
        // row alone — which is the whole looprs-037 requirement, in a widget.
        assert_ne!(p.footer(), paint_snap(&board(&[]), 90, 5).footer());
    }

    /// The dim pass actually landed on the painted cells, not just on the string.
    #[test]
    fn stale_rows_are_painted_dim_and_a_fresh_read_is_not() {
        let beads = [bead("looprs-a", "A title", BeadStatus::Open)];
        let fresh = buffer(&board(&beads), 90, 5);
        let stale_snap = board(&beads).with_error(
            &err_failed(3, "repository lock held\nmore\n"),
            Some(Duration::from_secs(61)),
        );
        let stale = buffer(&stale_snap, 90, 5);

        fn first_cell(buf: &ratatui::buffer::Buffer) -> &ratatui::buffer::Cell {
            let body = band_areas(Rect::new(0, 0, 90, 5)).body.expect("body");
            let col = column_areas(body)[0];
            &buf[(col.x, col.y)]
        }
        let cell_of = first_cell;
        assert_eq!(cell_of(&fresh).symbol(), "l", "a fresh read paints the id");
        assert_eq!(
            cell_of(&stale).style().fg,
            Some(Color::DarkGray),
            "the last good rows are dimmed"
        );
        assert_ne!(
            cell_of(&fresh).style().fg,
            Some(Color::DarkGray),
            "a fresh row is not dim"
        );
    }

    /// Every failure state says its own sentence. Four failures, four strings —
    /// none of them "0", none of them blank.
    #[test]
    fn every_read_state_says_its_own_thing_in_the_footer() {
        let empty_ok = board(&[]);
        let cases: Vec<(BoardSnapshot, &str)> = vec![
            (BoardSnapshot::loading(), "reading the board…"),
            (empty_ok.clone(), "bd ok · 3s ago"),
            (
                empty_ok.with_error(&err_unavailable(), Some(Duration::from_secs(9))),
                "bd unavailable: No such file or directory · stale 9s",
            ),
            (
                empty_ok.with_error(
                    &err_failed(3, "bd: no .beads repo here\n"),
                    Some(Duration::from_secs(9)),
                ),
                "bd failed (exit 3): bd: no .beads repo here · stale 9s",
            ),
            (
                empty_ok.with_error(&err_malformed(), Some(Duration::from_secs(9))),
                "bd answered unreadably — see the log · stale 9s",
            ),
            (
                empty_ok.with_error(&err_timeout(), Some(Duration::from_secs(9))),
                "bd did not answer in 30s · stale 9s",
            ),
        ];
        let mut said: Vec<String> = Vec::new();
        for (snap, want) in &cases {
            let p = paint_snap(snap, 120, 4);
            assert_eq!(p.footer(), *want);
            said.push(p.footer().to_string());
        }
        let mut uniq = said.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            said.len(),
            "every state has its own footer: {said:?}"
        );
    }

    #[test]
    fn a_failed_read_with_no_message_still_says_the_exit_code() {
        let snap = board(&[]).with_error(&err_failed(1, "   \n"), Some(Duration::from_secs(2)));
        assert_eq!(
            footer_line(&snap, 120).to_string(),
            "bd failed (exit 1) · stale 2s"
        );
        let signalled = board(&[]).with_error(&err_killed(), Some(Duration::from_secs(2)));
        assert!(
            footer_line(&signalled, 120)
                .to_string()
                .contains("exit signal")
        );
    }

    /// The deferred count is the footer's business, not a row's — and it shows
    /// only when there is some.
    #[test]
    fn deferred_is_a_footer_count_and_never_a_row() {
        let with_deferred = BoardSnapshot::from_beads(
            &[
                bead("looprs-a", "open", BeadStatus::Open),
                bead("looprs-d1", "postponed", BeadStatus::Deferred),
                bead("looprs-d2", "postponed", BeadStatus::Deferred),
            ],
            Some(Duration::from_secs(1)),
        );
        let p = paint_snap(&with_deferred, 120, 5);
        assert!(p.footer().contains("⏸ 2 deferred"), "{}", p.footer());
        let all_rows = p.rows.join("|");
        assert!(
            !all_rows.replace("⏸ 2 deferred", "").contains("postponed"),
            "a deferred bead painted a row: {all_rows:?}"
        );

        let none_deferred = board(&[bead("looprs-a", "open", BeadStatus::Open)]);
        let p = paint_snap(&none_deferred, 120, 5);
        assert!(!p.footer().contains("deferred"), "{}", p.footer());
    }

    /// The deferred tail is *after* the age, and the age is never cut off to
    /// make room for it.
    #[test]
    fn the_footer_gives_up_its_sentence_before_its_freshness() {
        let snap = BoardSnapshot::from_beads(
            &[bead("looprs-a", "open", BeadStatus::Deferred)],
            Some(Duration::from_secs(90)),
        );
        // Wide: everything.
        assert_eq!(
            footer_line(&snap, 120).to_string(),
            "bd ok · 1m30s ago · ⏸ 1 deferred"
        );
        // Narrow: the sentence is cut, the tails survive.
        let narrow = footer_line(&snap, 24).to_string();
        assert!(narrow.ends_with("⏸ 1 deferred"), "{narrow:?}");
        assert!(narrow.chars().count() <= 24, "{narrow:?}");
    }

    // ─────────────────────────── width and truncation ───────────────────────────

    /// The ADR's rule made concrete with a wide-character title: the row is laid
    /// out marker-first, so a narrow column still says `⊘`, and the CJK title is
    /// cut inside its column rather than overrunning into its neighbour.
    ///
    /// Swept across every width rather than sampled, because the interesting part
    /// is the crossing: a two-column glyph arriving at a one-column boundary is
    /// exactly where a cut that counts characters instead of columns goes wrong.
    #[test]
    fn a_wide_title_is_cut_inside_its_column_and_the_marker_survives() {
        let snap = board(&[bead(
            "looprs-cjk",
            "返回的标题非常長需要截斷顯示",
            BeadStatus::Blocked,
        )]);
        for w in 10u16..=90 {
            let p = paint_snap(&snap, w, 5);
            let Some(body) = p.band.body else { continue };
            let col = column_areas(body)[0];
            let rows = drawn(p.to_do());
            assert!(!rows.is_empty(), "width {w}: the row vanished");
            let row = rows[0];
            assert!(
                row.starts_with("⊘"),
                "width {w}: the marker did not survive truncation: {row:?}"
            );
            assert!(
                row.width() <= col.width as usize,
                "width {w}: row is {} columns wide in a {} column: {row:?}",
                row.width(),
                col.width,
            );
            // The gutters hold the framing and nothing else at every width: a row
            // that ran past its own column lands in the gutter first, so "the
            // gutter contains only what the framing put there" is still the
            // overflow check it always was — it just stopped being "the gutter is
            // blank" the day the band started drawing its own rules in it.
            for gutter in p.gutters() {
                assert!(
                    p.intrusions(gutter).is_empty(),
                    "width {w}: a row spilled into {gutter:?}: {:?}",
                    p.intrusions(gutter)
                );
            }
        }
    }

    /// A wide title that cannot fit whole still shows what fits. Cutting it is
    /// required; vanishing it is not, and a column that dropped every CJK title
    /// for being wide would be dropping the work.
    #[test]
    fn a_wide_title_that_cannot_fit_whole_still_shows_what_fits() {
        let snap = board(&[bead(
            "looprs-cjk",
            "返回的标题非常長需要截斷顯示",
            BeadStatus::Blocked,
        )]);
        let p = paint_snap(&snap, 60, 5);
        let row = drawn(p.to_do())[0];
        assert!(
            row.chars().any(|c| (c as u32) >= 0x4e00),
            "the CJK title went missing entirely: {row:?}"
        );
        assert!(row.ends_with('…'), "the cut should be marked: {row:?}");
    }

    /// And when the column cannot hold the marker *and* the space *and* the id,
    /// the marker keeps its column and the space is what goes — the marker is
    /// never the thing that gets cut.
    #[test]
    fn at_one_column_wide_the_marker_is_what_survives() {
        let line = bead_line(
            &BoardBead {
                id: "looprs-x".into(),
                title: "a title".into(),
                marker: Some(crate::state::board::Marker::Blocked),
            },
            1,
            false,
        );
        assert_eq!(line.to_string(), "⊘");
    }

    /// Below the width where both fit, the id stays and the title goes: the id is
    /// the thing a user types into `bd show`, and the title is re-readable from
    /// there.
    #[test]
    fn a_narrow_column_keeps_the_id_and_drops_the_title() {
        let snap = board(&[bead(
            "looprs-5o4-4",
            "a title far too long for this column",
            BeadStatus::Open,
        )]);
        // 42 columns → three 12-column cells plus two 3-column gutters: exactly
        // the id, and nothing else. (40 was the old number for the same shape,
        // before the gutter grew the cell the divider lives in.)
        let p = paint_snap(&snap, 42, 5);
        let rows = drawn(p.to_do());
        assert_eq!(rows, vec!["looprs-5o4-4"], "{rows:?}");
        assert_eq!(
            rows[0].width(),
            column_areas(p.band.body.unwrap())[0].width as usize,
            "the row should fill the column it was given"
        );
    }

    /// One column narrower still, where not even the id fits: the title is gone
    /// and the id goes out wearing an ellipsis. A cut id that *looked* whole
    /// would be the worse failure — it would read as some other, shorter ticket.
    #[test]
    fn when_even_the_id_does_not_fit_the_cut_is_visible() {
        let snap = board(&[bead(
            "looprs-5o4-4",
            "a title far too long for this column",
            BeadStatus::Open,
        )]);
        let p = paint_snap(&snap, 30, 5);
        let rows = drawn(p.to_do());
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].ends_with('…'), "the cut must be visible: {rows:?}");
        assert!(!rows[0].contains(' '), "no title rode along: {rows:?}");
    }

    /// A header too narrow for name + count never shows a *cut* count: `12…` is
    /// not a smaller number, it is a wrong one.
    #[test]
    fn a_narrow_header_drops_the_name_rather_than_the_count() {
        for (name_width, count) in [(2usize, "42"), (3, "42"), (4, "42")] {
            let line = header_line(Column::Complete, count, name_width);
            let text = line.to_string();
            assert!(
                !text.contains('…'),
                "a truncated count would be a lie: {text:?} at width {name_width}"
            );
            if name_width >= count.width() {
                assert!(text.contains(count), "{text:?}");
            }
        }
        // Wide enough for both, and both are there.
        let wide = header_line(Column::Complete, "42", 20).to_string();
        assert_eq!(wide, "Complete 42");
    }

    /// Nothing the band paints can spill outside the area it was handed, at any
    /// width: every row is within the band, and every column's rows are within
    /// that column.
    #[test]
    fn nothing_is_painted_outside_the_area_it_was_handed() {
        let beads: Vec<Bead> = (0..7)
            .map(|i| {
                bead(
                    &format!("looprs-{}", i),
                    &format!("a fairly long title for bead {i} indeed"),
                    BeadStatus::Open,
                )
            })
            .collect();
        let snap = board(&beads);
        for w in 1u16..=90 {
            for h in 1u16..=8 {
                let p = paint_snap(&snap, w, h);
                for row in &p.rows {
                    assert!(
                        row.width() <= w as usize,
                        "{w}×{h}: a row is {} wide: {row:?}",
                        row.width()
                    );
                }
                if let Some(body) = p.band.body {
                    let cols = column_areas(body);
                    for (col, area) in p.cols.iter().zip(cols.iter()) {
                        for line in col {
                            assert!(
                                line.width() <= area.width as usize,
                                "{w}×{h}: a column row is {} wide in {} cols: {line:?}",
                                line.width(),
                                area.width
                            );
                        }
                    }
                }
            }
        }
    }

    // ─────────────────────────── degenerate areas ───────────────────────────

    /// Zero by zero: nothing painted, nothing panicked.
    #[test]
    fn a_zero_area_is_answered_with_nothing_rather_than_a_panic() {
        let snap = board(&[bead("looprs-a", "t", BeadStatus::Open)]);
        let buf = buffer(&snap, 1, 1); // backend minimum sanity
        let _ = buf;
        let mut term = Terminal::new(TestBackend::new(4, 4)).unwrap();
        term.draw(|f| f.render_widget(Kanban::new(&snap), Rect::ZERO))
            .unwrap();
        assert!(
            term.backend()
                .buffer()
                .content
                .iter()
                .all(|c| c.symbol() == " "),
            "a zero rect paints nothing at all"
        );
    }

    /// Given a single row the band spends it on the footer, not on a header: a
    /// header alone would be chrome claiming to be a board, and the footer is the
    /// row that says whether anything here is true.
    #[test]
    fn with_one_row_the_band_buys_the_footer_not_the_header() {
        let p = paint_snap(&board(&[]), 60, 1);
        assert_eq!(
            p.footer(),
            "bd ok · 3s ago",
            "one row, and it is the footer: {:?}",
            p.rows
        );
        assert!(p.band.header.is_none());
        assert!(p.band.body.is_none());
    }

    #[test]
    fn with_two_rows_there_is_a_header_and_a_footer_and_no_body() {
        let p = paint_snap(&BoardSnapshot::loading(), 60, 2);
        assert!(p.rows[0].contains("To-do —"), "{}", p.rows[0]);
        assert_eq!(p.rows[1], "reading the board…");
        assert!(p.band.body.is_none(), "two rows is not a board");
    }

    /// The smallest thing that is still a board: header, one body row, footer.
    #[test]
    fn three_rows_is_a_board() {
        let p = paint_snap(&board(&[bead("looprs-a", "t", BeadStatus::Open)]), 60, 3);
        assert!(p.band.header.is_some() && p.band.body.is_some() && p.band.footer.is_some());
        assert_eq!(drawn(p.to_do()), vec!["looprs-a t"]);
    }

    // ─────────────────────────── the purity rule, made mechanical ───────────────────────────

    /// The module doc asserts the component reads no clock, no environment and no
    /// `bd`. That is the kind of claim a later edit quietly breaks, so it is
    /// checked against the file rather than left to inspection.
    ///
    /// Comment lines are skipped: the doc prose *names* the things this rule
    /// forbids, and naming a thing is not calling it.
    #[test]
    fn the_component_reads_no_clock_no_env_and_no_bd() {
        let code = component_code();
        let banned = [
            "Instant",
            "SystemTime",
            "env::var",
            "std::env",
            "Command::new",
            "services::bd::run",
            "bd::beads",
            "tokio::time",
            "sleep(",
        ];
        for needle in banned {
            assert!(
                !code.lines().any(|l| l.contains(needle)),
                "the component is not a pure function of snapshot + area: `{needle}` \
                 (the only `bd` import allowed is the BD_TIMEOUT constant named in the footer)"
            );
        }
        // The one `services::bd` import that is allowed, asserted so that the
        // ban above cannot be satisfied by quietly removing it too.
        assert!(code.lines().any(|l| l.contains("BD_TIMEOUT")));
    }

    /// "Titles are truncated with the shared width-aware helper; no new
    /// hand-rolled truncation in the file."
    #[test]
    fn truncation_comes_from_the_shared_helper_not_from_here() {
        let code = component_code();
        assert!(
            code.lines()
                .any(|l| l.contains("use crate::utils::render::truncate_columns;")),
            "the shared helper should be imported, not reimplemented"
        );
        for needle in [
            "fn take",
            "fn shorten",
            "fn truncate",
            "fn clip",
            "fn width_of",
        ] {
            assert!(
                !code.lines().any(|l| l.contains(needle)),
                "a hand-rolled truncation appeared in the widget: `{needle}`"
            );
        }
    }

    /// The header rows of the header and the body come from the *same* column
    /// layout — the thing that keeps a column's name over its rows.
    #[test]
    fn the_header_is_split_exactly_like_the_body() {
        let area = Rect::new(0, 0, 91, 1);
        let split = column_areas(area);
        // Near-equal, and the gutters are accounted for: 3w + 2g = 91.
        let total: u16 = split.iter().map(|r| r.width).sum::<u16>() + 2 * COLUMN_GAP_COLS;
        assert_eq!(total, 91, "the split spends every column it was given");
        let widths: Vec<u16> = split.iter().map(|r| r.width).collect();
        let spread = widths.iter().max().unwrap() - widths.iter().min().unwrap();
        assert!(
            spread <= 1,
            "columns should be near-equal, spread {spread}: {widths:?}"
        );
    }

    // ─────────────────── the framing: dividers and the rule ───────────────────

    /// The affordability rule read straight off the layout: the rule row exists
    /// from 5 rows up and never below it.
    ///
    /// Asserted against the *heights* rather than against the constant, because
    /// the constant is the decision and these are its consequences: a 3-row band
    /// keeps its one bead row and goes without the line, and the line arrives the
    /// moment there are two bead rows to spare. If someone moves the threshold
    /// this test moves with it; if someone makes the rule unconditional, or lets
    /// it be granted when the body cannot pay, it does not.
    #[test]
    fn the_rule_row_is_granted_only_when_the_body_can_pay_for_it() {
        for h in 1u16..=12 {
            let band = band_areas(Rect::new(0, 0, 90, h));
            assert_eq!(
                band.rule.is_some(),
                h >= KANBAN_RULE_ROWS
                    + MIN_KANBAN_BODY_ROWS_WITH_RULE
                    + KANBAN_HEADER_ROWS
                    + KANBAN_FOOTER_ROWS,
                "h {h}: the rule was {}",
                if band.rule.is_some() {
                    "granted"
                } else {
                    "refused"
                }
            );
            // The floor is a condition *of granting the rule*, not of having a
            // body: a 3-row band has one body row and no rule, and that is the
            // refusal working, not the floor being breached.
            if band.rule.is_some() {
                let body = band.body.expect("a granted rule leaves a body behind");
                assert!(
                    body.height >= MIN_KANBAN_BODY_ROWS_WITH_RULE,
                    "h {h}: the rule was granted and left the body {} rows",
                    body.height
                );
            }
        }
    }

    /// The four rows tile the band: nothing spent twice, nothing left unspent, and
    /// no gap between one row and the next.
    ///
    /// The contiguity check is the point. `band_areas` now lays the rows out from
    /// a running cursor, which is exactly the shape of code that puts the body one
    /// row too low and paints it over the footer — a bug that shows up as
    /// half a footer on screen and as nothing at all in any single row's
    /// contents.
    #[test]
    fn the_four_rows_tile_the_band_with_no_overlap_and_no_gap() {
        for h in 0u16..=12 {
            let band = band_areas(Rect::new(0, 0, 90, h));
            let chain: Vec<Rect> = [band.header, band.rule, band.body, band.footer]
                .into_iter()
                .flatten()
                .collect();
            let spent: u16 = chain.iter().map(|r| r.height).sum();
            assert!(spent <= h, "h {h}: spent {spent} rows out of {h}");
            for pair in chain.windows(2) {
                assert_eq!(
                    pair[0].bottom(),
                    pair[1].y,
                    "h {h}: rows are not contiguous: {chain:?}"
                );
            }
            if band.header.is_some() && band.footer.is_some() {
                assert_eq!(spent, h, "h {h}: a row of the band went unspent: {chain:?}");
            }
        }
    }

    /// The `┼` on the rule sits on the `│` it crosses, measured off the painted
    /// buffer rather than off the code that drew it.
    ///
    /// The two are produced by different functions (`paint_dividers` and
    /// `rule_line`) out of the same split, which is the design; a drift between
    /// them is invisible in review and obvious on screen, so it is checked here at
    /// widths where the gutters round differently.
    #[test]
    fn the_rule_junctions_sit_on_the_dividers_they_cross() {
        let snap = board(&[bead("looprs-a", "a title", BeadStatus::Open)]);
        for w in [90u16, 78, 61, 45, 33] {
            let p = paint_snap(&snap, w, 6);
            let header = p.band.header.expect("a header at 6 rows");
            let body = p.band.body.expect("a body at 6 rows");
            let rule = p.band.rule.expect("the rule is granted at 6 rows");
            for area in [header, body] {
                let expected: Vec<u16> = gutter_areas(area)
                    .iter()
                    .filter_map(|g| divider_col(g.width).map(|c| g.x + c as u16))
                    .collect();
                assert_eq!(
                    p.glyph_xs(area, "│"),
                    expected,
                    "{w}×6 rows: the dividers are not where the layout put them"
                );
            }
            let expected_junctions: Vec<u16> = gutter_areas(rule)
                .iter()
                .filter_map(|g| divider_col(g.width).map(|c| g.x + c as u16))
                .collect();
            assert_eq!(
                p.glyph_xs(rule, "┼"),
                expected_junctions,
                "{w}×6 rows: the rule's junctions drifted off the dividers"
            );
        }
    }

    /// The rule is neither short nor long for the width it was asked for, at every
    /// width — a `─` is one column, so the line the widget renders and the line
    /// that fits the area have to be the same number of columns, and at the
    /// widths where the layout squeezes the gutters that is not obvious.
    #[test]
    fn the_rule_is_exactly_the_width_it_was_asked_for() {
        for w in 0usize..=120 {
            let line = rule_line(w);
            assert_eq!(line.width(), w, "width {w}: {line:?}");
        }
    }

    /// The framing does not go stale, so it is not dimmed.
    ///
    /// Tested with the row dimming asserted *first*: without the control, this
    /// test would pass just as happily if the stale pass had stopped working
    /// altogether, which is the opposite of what it is for.
    #[test]
    fn the_framing_is_never_dimmed_by_a_stale_read() {
        let beads = [bead("looprs-a", "A title", BeadStatus::Open)];
        let fresh = buffer(&board(&beads), 90, 6);
        let stale = buffer(
            &board(&beads).with_error(
                &err_failed(3, "repository lock held\n"),
                Some(Duration::from_secs(61)),
            ),
            90,
            6,
        );
        let band = band_areas(Rect::new(0, 0, 90, 6));
        let body = band.body.expect("a body at 6 rows");
        let rule = band.rule.expect("the rule at 6 rows");

        // The control: the row itself is dim.
        let row = column_areas(body)[0];
        assert_ne!(
            fresh[(row.x, row.y)].style().fg,
            Some(Color::DarkGray),
            "a fresh row should not be dimmed"
        );
        assert_eq!(
            stale[(row.x, row.y)].style().fg,
            Some(Color::DarkGray),
            "the stale pass did not reach the row"
        );

        // ...and the framing is identical either way, down the gutters and across
        // the rule.
        for area in [band.header.expect("a header"), body, rule] {
            for g in gutter_areas(area) {
                let Some(col) = divider_col(g.width) else {
                    continue;
                };
                let x = g.x + col as u16;
                assert_eq!(
                    fresh[(x, g.y)].style().fg,
                    stale[(x, g.y)].style().fg,
                    "the framing changed colour because of a read: ({x}, {})",
                    g.y
                );
                assert_eq!(
                    stale[(x, g.y)].style().fg,
                    Some(Color::DarkGray),
                    "the framing is the footer's gray, not a fourth colour"
                );
            }
        }
    }

    /// A gutter the layout has squeezed to nothing is left alone rather than given
    /// a line that lands outside the area, and nothing but framing is ever painted
    /// into a gutter at any width down there.
    #[test]
    fn a_gutter_too_narrow_to_hold_a_line_draws_nothing() {
        assert_eq!(
            divider_col(0),
            None,
            "a zero-width gutter has no divider column"
        );
        assert_eq!(
            divider_col(1),
            Some(0),
            "a one-column gutter puts the line at 0"
        );
        let snap = board(&[bead(
            "looprs-a",
            "a title wide enough to want truncating",
            BeadStatus::Open,
        )]);
        for w in 1u16..=24 {
            let p = paint_snap(&snap, w, 6);
            for area in [p.band.header, p.band.rule, p.band.body]
                .into_iter()
                .flatten()
            {
                for g in gutter_areas(area) {
                    assert!(
                        p.intrusions(g).is_empty(),
                        "{w} cols, gutter {g:?}: {:?}",
                        p.intrusions(g)
                    );
                }
            }
        }
    }

    // ─────────────────────────── a look, not an assertion ───────────────────────────

    #[test]
    #[ignore = "visual: run with --ignored --nocapture to see the band painted"]
    fn look() {
        let beads = vec![
            bead("looprs-5o4.4", "Kanban component", BeadStatus::Open),
            bead("looprs-5o4.5", "Frame wiring", BeadStatus::Blocked),
            bead("looprs-037", "bd honesty", BeadStatus::Ready),
            bead("looprs-pdl.4", "全屏的框", BeadStatus::Unknown),
            bead("looprs-guh", "Status row", BeadStatus::InProgress),
            bead("looprs-6ol", "Warning gate", BeadStatus::Deferred),
            bead("looprs-2nd", "Dead allows", BeadStatus::Closed),
            bead("looprs-3ka", "Split app.rs", BeadStatus::Done),
        ];
        for (name, snap) in [
            ("a full board", board(&beads)),
            ("an empty board", board(&[])),
            ("never loaded", BoardSnapshot::loading()),
            (
                "bd blew up, last good kept",
                board(&beads).with_error(
                    &err_failed(3, "repository lock held by pid 4211"),
                    Some(Duration::from_secs(73)),
                ),
            ),
        ] {
            println!("\n{name}");
            // Swept over height as well as width, because the rule row turns on
            // partway up the ladder and a visual test that only ever looks at one
            // height never sees both halves of the affordability rule.
            for (w, h) in [(90u16, 8u16), (90, 6), (90, 5), (90, 4), (40, 8), (40, 4)] {
                let p = paint_snap(&snap, w, h);
                println!(
                    "  {w}×{h} — rule {}",
                    if p.band.rule.is_some() {
                        "drawn"
                    } else {
                        "refused"
                    }
                );
                for row in &p.rows {
                    println!("  {w:>3} │{row}│");
                }
                // Printed rows are checked rows: nothing outside the band.
                assert!(p.rows.iter().all(|r| r.width() <= w as usize));
            }
        }
    }

    // Small fixtures, so a test says the failure it means instead of rebuilding
    // the service's whole error vocabulary inline.
    use crate::services::bd::BdError;

    fn err_unavailable() -> BdError {
        BdError::Unavailable {
            bin: "bd".into(),
            reason: "No such file or directory".into(),
        }
    }

    fn err_failed(code: i32, stderr: &str) -> BdError {
        BdError::Failed {
            bin: "bd".into(),
            args: "--readonly list --all --limit 0 --json".into(),
            code: Some(code),
            stderr: stderr.into(),
        }
    }

    /// Killed by a signal: no exit code at all.
    fn err_killed() -> BdError {
        BdError::Failed {
            bin: "bd".into(),
            args: "list".into(),
            code: None,
            stderr: String::new(),
        }
    }

    fn err_malformed() -> BdError {
        BdError::Malformed {
            bin: "bd".into(),
            args: "list".into(),
            reason: "expected a JSON array".into(),
            raw: "not json".into(),
        }
    }

    fn err_timeout() -> BdError {
        BdError::Timeout {
            bin: "bd".into(),
            args: "list".into(),
        }
    }
}
