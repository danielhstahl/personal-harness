//! The status row: the one row that answers, without scrolling, **which mode am I
//! in, is it working, on what, and did anything fail** (looprs-guh).
//!
//! Until now [`crate::viewport::frame_areas`] reserved the row and nothing was
//! drawn into it. Three very different things can be running in this harness at
//! once (ADR-0002 Q3: up to three live children, two of them Node), and the mode
//! was visible only as the input box's border colour. "beads is still working
//! while I am chatting in Pi" was invisible, and per that ADR that is a policy
//! failure rather than a cosmetic one.
//!
//! # One row is a priority list, not a sentence
//!
//! A sentence has to be cut somewhere, and cutting it arbitrarily means the thing
//! that survives depends on the string lengths. So the row is a list of segments,
//! each carrying a **keep-priority**, and the row drops the least-kept segment at a
//! time until it fits ([`fit`]). The ladder, from first gone to last:
//!
//! ```text
//! ^C quit · the bead's title · warm children · cache <tokens> · Tab switch
//! · which bead and how long a *background* run is on
//! · bg: <mode> <verb>            ← ADR-0002's reason for this row
//! · Esc cancel · how long this run has been going · what it cost (↑in ↓out)
//! · which bead · what it is doing · THE ERROR · which mode
//! ```
//!
//! Three deliberate calls inside that order:
//!
//! * **the error is never dropped, only shortened.** "Something failed" must
//!   survive a 40-column window even when "what failed" cannot. The full text is
//!   one scroll-up away; the marker is what stops a failed run looking like a
//!   finished one.
//! * **a busy background session outranks `Esc cancel`.** The hint describes a key
//!   you can re-learn in a second; the background entry describes money and a
//!   process you cannot otherwise see.
//! * **what it cost outranks how long it took, and the cache detail pays for that
//!   pair first.** `↑in ↓out` is the number a loop that spends money while nobody
//!   watches can actually act on; elapsed is nice to know. `cache …` only says
//!   where some of those tokens came from, so it is worth room when there is room
//!   and the first thing handed back when there is not — and it can never be the
//!   reason the in/out pair is missing.
//!
//! [`SEP`] costs three columns, which is why the row drops *whole* segments rather
//! than trimming each one: a fragment of everything tells you less than a whole of
//! something.
//!
//! # Purity
//!
//! [`Status::line`] reads no clock, no channel, no `App`, and no `bd`. Everything
//! time-shaped arrives as [`Sess::elapsed`], computed by the caller from its own
//! tick — so the row is a pure function of state, as the frame's contract
//! requires, and every branch of it is testable without a subprocess, a terminal
//! or a `sleep`.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::session::view::Tokens;
use crate::session::{ActiveBead, BeadStep, SessionStatus, TerminalType};
use crate::theme::styles::mode_color;
use crate::utils::render::FRAMES;

/// The gap between two segments.
const SEP: &str = " · ";

/// Keep-priorities. **Higher = keep longer**: when the row runs out of columns the
/// still-present segment with the lowest value is dropped, one at a time, until it
/// fits. Ties drop leftmost-first.
///
/// Numbered in one place so the whole ladder is readable at a glance; a new
/// segment means inserting a number here, not inventing one at the call site.
mod keep {
    pub const HINT_QUIT: u8 = 1;
    pub const BEAD_TITLE: u8 = 2;
    pub const WARM_CHILDREN: u8 = 3;
    /// `cache 1.2M` — the most expendable of the cost facts. Worth a look when
    /// the row has room; the first thing it gives back when it does not.
    pub const TOKEN_CACHE: u8 = 4;
    pub const HINT_SWITCH: u8 = 5;
    /// Which bead a background run is on, and for how long. Lower than the
    /// background head, so it goes first and the head can never lose its detail
    /// while keeping the detail would mean losing the head.
    pub const BG_DETAIL: u8 = 6;
    /// `bg: <mode> <verb>` — the load-bearing half of the background entry.
    pub const BACKGROUND: u8 = 7;
    pub const HINT_CANCEL: u8 = 8;
    pub const ELAPSED: u8 = 9;
    /// `↑ 12.3k ↓ 4.1k`. Outranks elapsed — for a loop that spends money while
    /// nobody watches, what it cost is the more actionable half of the pair — and
    /// ranks under the bead id, because "which ticket" beats "how much".
    pub const TOKENS: u8 = 10;
    pub const BEAD_ID: u8 = 11;
    pub const VERB: u8 = 12;
    /// Never dropped — shortened instead (`Cut::Short`).
    pub const ERROR: u8 = 13;
    /// Never cut at all, except by the last-resort clip.
    pub const MODE: u8 = 14;
}

/// How a segment gives up its columns when the row runs out of them.
///
/// `Drop` is the common answer. `Short` exists for exactly one segment, the error,
/// because "something failed" has to survive a narrow window even when the message
/// cannot: it is *flexible* rather than indestructible, and it absorbs whatever
/// width the ladder leaves behind. `Fixed` is the mode, which is the answer the row
/// is asked for first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cut {
    /// Never cut. The last-resort clip can still bite it at absurd widths.
    Fixed,
    /// Removed whole, least-kept first.
    Drop,
    /// Never removed; shortened instead, marker intact.
    Short,
}

/// One segment of the row.
struct Seg {
    /// Painted immediately before `text`, inside the same segment, so the two can
    /// never be separated by a drop or a separator. The error marker rides here:
    /// `✗` has to survive together with the fact that there is an error.
    prefix: &'static str,
    text: String,
    style: Style,
    keep: u8,
    cut: Cut,
}

impl Seg {
    fn new(text: impl Into<String>, style: Style, keep: u8) -> Self {
        Self {
            prefix: "",
            text: text.into(),
            style,
            keep,
            cut: Cut::Drop,
        }
    }

    fn width(&self) -> usize {
        self.prefix.width() + self.text.width()
    }
}

/// One session, as the row sees it.
///
/// Plain data on purpose, not `&SessionView`: the row then cannot reach a
/// transcript, a flusher or a channel, and the whole table of
/// (mode × step × liveness × error) is constructible in a test with no session
/// anywhere near it.
#[derive(Debug)]
pub struct Sess<'a> {
    pub mode: TerminalType,
    pub status: SessionStatus,
    /// The beads machine's step. `None` for the two modes that have no loop.
    pub step: Option<BeadStep>,
    /// The ticket the beads loop holds a claim on.
    pub bead: Option<&'a ActiveBead>,
    /// How long the **current run** has been going. `None` = nothing is running.
    /// Computed by the caller: the row does not own a clock.
    pub elapsed: Option<Duration>,
    /// This session's last error, if any.
    pub error: Option<&'a str>,
}

impl<'a> Sess<'a> {
    pub fn busy(&self) -> bool {
        self.status.is_busy()
    }

    /// "What is it doing?" in the few words the row has room for.
    ///
    /// The beads step is the loop's own word for itself and beats process
    /// liveness; process liveness beats nothing having arrived yet. `Aborting`
    /// outranks both, because a run on its way out is not "working".
    pub fn verb(&self) -> &'static str {
        if self.status == SessionStatus::Aborting {
            return "cancelling";
        }
        if self.status.is_busy() {
            return match self.step {
                Some(BeadStep::CreateTickets) => "planning",
                Some(BeadStep::WorkTickets) => "working",
                // Busy with no step, or with a step that says "waiting for you":
                // believe the process, which is the one demonstrably busy.
                _ => "running",
            };
        }
        match self.step {
            Some(BeadStep::AwaitInput) if self.error.is_some() => "paused",
            Some(BeadStep::AwaitInput) => "awaiting input",
            // A step naming work that is not running is a stale step; the liveness
            // word is the honest one.
            _ => Self::liveness(self.status),
        }
    }

    fn liveness(status: SessionStatus) -> &'static str {
        match status {
            SessionStatus::Running => "running",
            SessionStatus::Aborting => "cancelling",
            SessionStatus::Idle => "idle",
            SessionStatus::Dead => "child gone",
            SessionStatus::NotStarted => "not started",
        }
    }

    /// The liveness glyph: animation means *a process doing something*, and the
    /// two static dots separate "warm and idle" from "nothing there".
    fn glyph(&self, spinner: usize) -> &'static str {
        match self.status {
            SessionStatus::Running | SessionStatus::Aborting => FRAMES[spinner % FRAMES.len()],
            SessionStatus::Idle => "●",
            SessionStatus::Dead | SessionStatus::NotStarted => "○",
        }
    }

    fn bead_id(&self) -> Option<&'a str> {
        self.bead.map(|b| b.id.as_str())
    }

    /// The head of a background entry: `bg: Beads working`.
    ///
    /// The `bg:` marker rides inside the head rather than heading a group, so the
    /// entry stays self-describing once the higher-priority segments next to it are
    /// gone — which is the whole premise of dropping by priority. The bead and the
    /// age are a separate, lower-priority segment ([`Sess::background_detail`]),
    /// because *"a mode you are not looking at is working"* is the fact ADR-0002
    /// makes this row load-bearing, and it must survive in a 40-column window even
    /// when which-bead and for-how-long cannot.
    fn background_head(&self) -> String {
        format!("bg: {} {}", self.mode.label(), self.verb())
    }

    /// The tail of a background entry: `looprs-2 12s`, or `None` if there is
    /// nothing to add.
    ///
    /// Kept whole and dropped whole. It is **not** shortenable: a truncated ticket
    /// id reads as a *different* ticket id, and a row that misnames the work is
    /// worse than a row that is less specific about it.
    fn background_detail(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(id) = self.bead_id() {
            parts.push(id.to_string());
        }
        if let Some(e) = self.elapsed {
            parts.push(fmt_elapsed(e));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// The row's whole input: the mode on screen, plus what the others are doing.
#[derive(Debug)]
pub struct Status<'a> {
    pub active: Sess<'a>,
    /// Live sessions that are **not** on screen and are **busy** — the thing
    /// ADR-0002 says must not be invisible.
    pub background: Vec<Sess<'a>>,
    /// Modes with a warm (alive, idle) child. One segment rather than one per
    /// mode: the honest content is "a process is resident here", which a list
    /// says once.
    pub warm: Vec<TerminalType>,
    /// Tokens this view's window has spent — see
    /// [`Tokens`] and [`SessionView::tokens`](crate::session::view::SessionView::tokens)
    /// for what the window covers per mode.
    pub tokens: Tokens,
    /// The row's own animation phase. `App` advances it off the frame tick, at
    /// the row's pace rather than the panel's.
    pub spinner: usize,
}

impl Status<'_> {
    /// Render the row. Pure, and total: any width, including 0, returns something
    /// a frame can draw.
    pub fn line(&self, width: u16) -> Line<'static> {
        if width == 0 {
            return Line::default();
        }
        clip(join(fit(segments(self), width as usize)), width as usize).into()
    }
}

/// Every segment that has something to say, in display order.
fn segments(s: &Status<'_>) -> Vec<Seg> {
    let a = &s.active;
    let busy = a.busy();
    let mut segs = Vec::new();

    // Which mode. Bold, and in the input box's own mode colour, so the row and the
    // border can never disagree about what mode this is.
    segs.push(Seg {
        prefix: "",
        text: format!("{} {}", a.glyph(s.spinner), a.mode.label()),
        style: Style::new()
            .fg(mode_color(a.mode))
            .add_modifier(Modifier::BOLD),
        keep: keep::MODE,
        cut: Cut::Fixed,
    });

    // What it is doing.
    segs.push(Seg::new(
        a.verb(),
        if busy {
            Style::new().fg(Color::Blue)
        } else if a.status.is_alive() {
            Style::default()
        } else {
            Style::new().fg(Color::DarkGray)
        },
        keep::VERB,
    ));

    // Which bead.
    if let Some(id) = a.bead_id() {
        segs.push(Seg::new(id, Style::default(), keep::BEAD_ID));
    }

    // How long this run has been going.
    if let Some(e) = a.elapsed {
        segs.push(Seg::new(
            fmt_elapsed(e),
            Style::new().fg(Color::DarkGray),
            keep::ELAPSED,
        ));
    }

    // What failed. Marker is part of the segment so shortening cannot eat it, and
    // `shortenable` so dropping cannot remove it either.
    if let Some(err) = a.error {
        segs.push(Seg {
            prefix: "✗ ",
            text: err.to_string(),
            style: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            keep: keep::ERROR,
            cut: Cut::Short,
        });
    }

    // What the modes you are not looking at are doing — head first (the fact),
    // then the detail, which goes earlier under pressure. Both are whole segments:
    // neither is ever cut mid-word, and the detail can never outlive its head
    // because its keep is strictly lower.
    for bg in &s.background {
        segs.push(Seg::new(
            bg.background_head(),
            Style::new().fg(mode_color(bg.mode)),
            keep::BACKGROUND,
        ));
        if let Some(detail) = bg.background_detail() {
            segs.push(Seg::new(
                detail,
                Style::new().fg(mode_color(bg.mode)),
                keep::BG_DETAIL,
            ));
        }
    }

    // What this session has spent. Tokens rather than dollars: the number a loop
    // that runs unwatched can act on is how much it burned, and the pair reads at
    // a glance. `is_empty` covers "nothing reported yet" — an absent `usage` must
    // not come out as `↑0 ↓0`, which would read as a free run rather than an
    // unmeasured one.
    if !s.tokens.is_empty() {
        segs.push(Seg::new(
            format!(
                "\u{2191}{} \u{2193}{}",
                fmt_tokens(s.tokens.input),
                fmt_tokens(s.tokens.output)
            ),
            Style::new().fg(Color::Yellow),
            keep::TOKENS,
        ));
        // Cache tokens get their own segment because `input` does not include them,
        // and on a long run they are most of the total. Showing `input` alone is
        // how a run of almost-all cache reads looks cheap.
        if s.tokens.cache > 0 {
            segs.push(Seg::new(
                format!("cache {}", fmt_tokens(s.tokens.cache)),
                dim(),
                keep::TOKEN_CACHE,
            ));
        }
    }

    // Warm children: resident, idle, and invisible without this.
    if !s.warm.is_empty() {
        let names: Vec<&str> = s.warm.iter().map(|m| m.label()).collect();
        segs.push(Seg::new(
            format!("warm: {}", names.join(", ")),
            Style::new().fg(Color::DarkGray),
            keep::WARM_CHILDREN,
        ));
    }

    // The keys that matter right now. `Esc` is only a live key while something is
    // running to cancel; showing it over an idle pane teaches the wrong thing.
    if busy {
        segs.push(Seg::new("Esc cancel", dim(), keep::HINT_CANCEL));
    }
    segs.push(Seg::new("Tab switch", dim(), keep::HINT_SWITCH));
    segs.push(Seg::new("^C quit", dim(), keep::HINT_QUIT));

    // The bead's title: the human-readable version of the work, and the first
    // thing here to go, because it is the longest thing here.
    if let Some(t) = a.bead.map(|b| b.title.as_str())
        && !t.trim().is_empty()
    {
        segs.push(Seg::new(format!("“{}”", t.trim()), dim(), keep::BEAD_TITLE));
    }

    segs
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// The least a [`Cut::Short`] segment may be handed and still be worth showing:
/// its marker plus four columns of message.
///
/// This is the number that decides *whether anything else gets dropped*. Cost the
/// flexible segment at its natural size instead and the row drops the bead id and
/// the verb to keep a long error uncut — the ladder read backwards, trading two
/// acceptance-critical answers for text that was going to be truncated anyway.
const FLEX_FLOOR: usize = 6;

/// Cut the row down to `width`.
///
/// Drop the least-kept `Cut::Drop` segment until the row would fit **with the
/// flexible segment at its floor**, then hand that segment every column the ladder
/// left over. That ordering is the whole point: the error costs the row nothing it
/// was not going to pay anyway, so the work answers survive it.
fn fit(mut segs: Vec<Seg>, width: usize) -> Vec<Seg> {
    let mut flex = segs.iter().position(|s| s.cut == Cut::Short);
    loop {
        if cost(&segs, flex) <= width {
            break;
        }
        // The least-kept droppable survivor goes. `min_by_key` takes the first
        // minimum, so equal priorities fall leftmost-first.
        let Some(i) = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.cut == Cut::Drop)
            .min_by_key(|(_, s)| s.keep)
            .map(|(i, _)| i)
        else {
            break;
        };
        segs.remove(i);
        // Removing a segment shifts everything after it left, and the flexible
        // index is held across the loop, so it has to follow. It can never *be*
        // the removed one: the filter only drops `Cut::Drop`.
        if let Some(f) = flex.as_mut()
            && *f > i
        {
            *f -= 1;
        }
    }
    // The flexible segment takes all the room the ladder left.
    if let Some(i) = flex {
        let others = total_without(&segs, i);
        // Putting the flexible segment *back* costs one more separator than
        // `total_without` accounted for — forgetting it leaves the row three
        // columns over, and the last-resort clip then cuts a hint mid-word, which
        // is exactly the ugly this whole module exists to avoid.
        let join = if segs.len() > 1 { SEP.width() } else { 0 };
        let avail = width
            .saturating_sub(others)
            .saturating_sub(join)
            .saturating_sub(segs[i].prefix.width());
        let cut = shorten(&segs[i].text, avail);
        segs[i].text = cut;
    }
    segs
}

/// The row's cost with segment `flex` counted at [`FLEX_FLOOR`] instead of at its
/// natural width: what the row costs if the error gives up as much as it may.
fn cost(segs: &[Seg], flex: Option<usize>) -> usize {
    let Some(i) = flex else {
        return total(segs);
    };
    total_at(segs, Some(i), FLEX_FLOOR.max(segs[i].prefix.width()))
}

fn total(segs: &[Seg]) -> usize {
    total_at(segs, None, 0)
}

/// Sum of the segments plus the separators between them, with segment `flex`
/// replaced by a substitute width.
fn total_at(segs: &[Seg], flex: Option<usize>, flex_width: usize) -> usize {
    if segs.is_empty() {
        return 0;
    }
    let body: usize = segs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if Some(i) == flex {
                flex_width
            } else {
                s.width()
            }
        })
        .sum();
    body + SEP.width() * (segs.len() - 1)
}

/// What the row costs without segment `skip`, separator included.
fn total_without(segs: &[Seg], skip: usize) -> usize {
    if segs.len() <= 1 {
        return 0;
    }
    let body: usize = segs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != skip)
        .map(|(_, s)| s.width())
        .sum();
    body + SEP.width() * (segs.len() - 2)
}

/// Lay the segments out with separators between them.
///
/// `prefix` and `text` come out as **one** span, so a segment's marker can never
/// end up on one side of a separator (or of a cut) and its message on the other.
fn join(segs: Vec<Seg>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, seg) in segs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP.to_string(), dim()));
        }
        spans.push(Span::styled(
            format!("{}{}", seg.prefix, seg.text),
            seg.style,
        ));
    }
    spans
}

/// Clip a run of spans to `avail` display columns, dropping whole spans from the
/// front of the tail once the budget is gone. Last-resort: [`fit`] should have made
/// this a no-op, and it exists so a bug in the ladder cannot write past the window.
fn clip(spans: Vec<Span<'static>>, avail: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= avail {
            break;
        }
        let mut kept = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > avail {
                break;
            }
            used += w;
            kept.push(ch);
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
    }
    out
}

/// Shorten `text` to `avail` display columns, marking the cut with `…`.
///
/// Cut by display width, not by `chars().count()` or by bytes: a box-drawing
/// character, an emoji or a combining mark all break the latter two, and a cut
/// that lands where the reader cannot see it is a cut that silently loses text.
fn shorten(text: &str, avail: usize) -> String {
    if text.width() <= avail {
        return text.to_string();
    }
    if avail == 0 {
        return String::new();
    }
    // `…` is part of the budget. Below two columns there is nothing worth keeping
    // but the first column, so say that much rather than nothing.
    let body = take(text, avail.saturating_sub(1));
    format!("{body}…")
}

/// The first `avail` display columns of `text`.
fn take(text: &str, avail: usize) -> String {
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

/// A run's age, in the fewest columns that still read: `9s`, `59s`, `1m03s`,
/// `1h04m`. Past a day it stops being a run age and says so.
///
/// Note the `secs % 60` in the minutes arm: printing the *total* seconds there is
/// the classic `1m63s`, and a status field nobody can read is a status field that
/// trains the user to ignore it.
pub fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..=86_399 => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
        _ => "1d+".to_string(),
    }
}

/// Byte counts the way the row has room for them: `512B`, `9.1KiB`, `1.4MiB`.
/// Token counts, compact and decimal (tokens are not powers of 1024).
///
/// Bounded width is the whole point: `999`, `12.3k`, `1.24M` keep the row's
/// arithmetic stable at every order of magnitude, so a nine-figure run cannot push
/// the bead id off the screen by itself.
fn fmt_tokens(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    if n < K {
        format!("{n}")
    } else if n < M {
        format!("{}.{:01}k", n / K, (n % K) * 10 / K)
    } else {
        format!("{}.{:02}M", n / M, (n % M) * 100 / M)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead() -> ActiveBead {
        ActiveBead {
            id: "looprs-guh".to_string(),
            title: "Status row is allocated but empty".to_string(),
        }
    }

    fn sess(mode: TerminalType, status: SessionStatus) -> Sess<'static> {
        Sess {
            mode,
            status,
            step: None,
            bead: None,
            elapsed: None,
            error: None,
        }
    }

    /// Render and measure, because "it fits" is a claim about display columns, not
    /// about bytes or `chars()`.
    fn render(s: &Status<'_>, width: u16) -> (String, usize) {
        let line = s.line(width);
        let w = line.spans.iter().map(|sp| sp.content.width()).sum();
        (line.to_string(), w)
    }

    fn plain(mode: TerminalType, status: SessionStatus) -> Status<'static> {
        Status {
            active: sess(mode, status),
            background: vec![],
            warm: vec![],
            tokens: Tokens::default(),
            spinner: 0,
        }
    }

    // ────────────────────────── the verb table ──────────────────────────
    //
    // Every (mode, step, liveness, error) row that the three sessions can actually
    // produce, said out loud once. `App` renders from this, so a miss here is a
    // miss in the row — and this needs no session, no channel and no sleep.
    #[test]
    fn the_verb_table_says_what_every_state_means() {
        /// (mode, step, liveness, error, expected verb)
        type VerbRow = (
            TerminalType,
            Option<BeadStep>,
            SessionStatus,
            Option<&'static str>,
            &'static str,
        );
        let rows: Vec<VerbRow> = vec![
            // beads: the loop's own step wins over the process word
            (
                TerminalType::Beeds,
                Some(BeadStep::AwaitInput),
                SessionStatus::Idle,
                None,
                "awaiting input",
            ),
            (
                TerminalType::Beeds,
                Some(BeadStep::CreateTickets),
                SessionStatus::Running,
                None,
                "planning",
            ),
            (
                TerminalType::Beeds,
                Some(BeadStep::WorkTickets),
                SessionStatus::Running,
                None,
                "working",
            ),
            // ...and a paused loop says so, because "awaiting input" after a
            // failure reads as an invitation rather than a consequence.
            (
                TerminalType::Beeds,
                Some(BeadStep::AwaitInput),
                SessionStatus::Idle,
                Some("bd refused the claim"),
                "paused",
            ),
            // a cancel in flight is never "working"
            (
                TerminalType::Beeds,
                Some(BeadStep::WorkTickets),
                SessionStatus::Aborting,
                None,
                "cancelling",
            ),
            // a stale step naming work that is not running believes the process
            (
                TerminalType::Beeds,
                Some(BeadStep::WorkTickets),
                SessionStatus::Idle,
                None,
                "idle",
            ),
            // beads with no step yet (the first frame after boot) still answers
            (
                TerminalType::Beeds,
                None,
                SessionStatus::NotStarted,
                None,
                "not started",
            ),
            // pi and bash have no loop: liveness is the whole story
            (
                TerminalType::Pi,
                None,
                SessionStatus::Running,
                None,
                "running",
            ),
            (TerminalType::Pi, None, SessionStatus::Idle, None, "idle"),
            (
                TerminalType::Pi,
                None,
                SessionStatus::Aborting,
                None,
                "cancelling",
            ),
            (
                TerminalType::Pi,
                None,
                SessionStatus::Dead,
                None,
                "child gone",
            ),
            (
                TerminalType::Bash,
                None,
                SessionStatus::Running,
                None,
                "running",
            ),
            (
                TerminalType::Bash,
                None,
                SessionStatus::NotStarted,
                None,
                "not started",
            ),
            // busy beats a step that says the human is wanted: believe the process
            (
                TerminalType::Beeds,
                Some(BeadStep::AwaitInput),
                SessionStatus::Running,
                None,
                "running",
            ),
        ];
        for (mode, step, status, error, want) in rows {
            let s = Sess {
                mode,
                status,
                step,
                bead: None,
                elapsed: None,
                error,
            };
            assert_eq!(
                s.verb(),
                want,
                "mode {mode:?} step {step:?} status {status:?} error {error:?}"
            );
        }
    }

    #[test]
    fn every_status_has_a_verb_in_every_mode() {
        let all = [
            SessionStatus::NotStarted,
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::Aborting,
            SessionStatus::Dead,
        ];
        for mode in TerminalType::ALL {
            for st in all {
                let v = sess(mode, st).verb();
                assert!(!v.is_empty(), "{mode:?} {st:?} said nothing");
                assert!(!v.contains('{') && !v.contains('}'), "{v:?}");
            }
        }
    }

    // ────────────────────────── the empty state ──────────────────────────

    /// The state the ticket was filed against: nothing there, and the row must say
    /// so in words rather than render nothing, a hole, or a format-string leak.
    #[test]
    fn an_empty_row_still_answers_which_mode_and_whether_anything_started() {
        let (txt, w) = render(&plain(TerminalType::Beeds, SessionStatus::NotStarted), 40);
        assert!(txt.contains("Beeds"), "{txt:?}");
        assert!(txt.contains("not started"), "{txt:?}");
        assert!(w <= 40, "{w} > 40: {txt:?}");
        assert!(
            !txt.contains('{') && !txt.contains('}'),
            "format leak: {txt:?}"
        );
    }

    #[test]
    fn the_three_modes_are_distinguishable_at_the_narrowest_useful_width() {
        for mode in TerminalType::ALL {
            let (txt, _) = render(&plain(mode, SessionStatus::Idle), 40);
            assert!(txt.contains(mode.label()), "{mode:?}: {txt:?}");
        }
    }

    // ────────────────────────── never wider than the window ──────────────────────────

    /// The row is cut by the ladder, not by luck: at **every** width from 1 to 120
    /// the rendered text fits, with the fattest possible input in every slot.
    #[test]
    fn the_row_never_overflows_its_width_at_any_width() {
        let b = bead();
        let long_err = "bd show looprs-guh failed: repository lock held by another process for 30s";
        let active = Sess {
            mode: TerminalType::Beeds,
            status: SessionStatus::Running,
            step: Some(BeadStep::WorkTickets),
            bead: Some(&b),
            elapsed: Some(Duration::from_secs(4 * 3600 + 42 * 60)),
            error: Some(long_err),
        };
        let bg = Sess {
            mode: TerminalType::Pi,
            status: SessionStatus::Aborting,
            step: None,
            bead: None,
            elapsed: Some(Duration::from_secs(63)),
            error: None,
        };
        let s = Status {
            active,
            background: vec![bg],
            warm: vec![TerminalType::Bash],
            tokens: Tokens {
                input: 1_234_567,
                output: 42_100,
                cache: 8_400_000,
            },
            spinner: 3,
        };
        for width in 1u16..=120 {
            let (txt, w) = render(&s, width);
            assert!(w <= width as usize, "width {width} got {w}: {txt:?}");
        }
    }

    /// Zero- and one-column windows are the ones that turn a `subtract` into a
    /// panic. The row answers with a clipped fragment rather than taking the frame
    /// down with it.
    #[test]
    fn a_window_of_zero_or_one_column_is_answered_not_panicked() {
        let s = plain(TerminalType::Bash, SessionStatus::Running);
        let (txt, w) = render(&s, 0);
        assert_eq!(w, 0);
        assert_eq!(txt, "");
        let (_, w) = render(&s, 1);
        assert!(w <= 1);
    }

    // ────────────────────────── the priority ladder ──────────────────────────

    /// The 40-column case from the acceptance criteria, with an error present: the
    /// four answers come out, and the hints are what pays for them.
    #[test]
    fn at_forty_columns_with_an_error_the_work_and_the_failure_survive() {
        let b = bead();
        let active = Sess {
            mode: TerminalType::Beeds,
            status: SessionStatus::Running,
            step: Some(BeadStep::WorkTickets),
            bead: Some(&b),
            elapsed: Some(Duration::from_secs(12)),
            error: Some("bd refused the claim: owned by another owner"),
        };
        let s = Status {
            active,
            background: vec![sess(TerminalType::Pi, SessionStatus::Running)],
            warm: vec![],
            tokens: Tokens {
                input: 4_096,
                output: 512,
                cache: 900_000,
            },
            spinner: 0,
        };
        let (txt, w) = render(&s, 40);
        assert!(w <= 40, "{w}: {txt:?}");
        assert!(txt.contains("Beeds"), "which mode: {txt:?}");
        assert!(txt.contains("working"), "is it working: {txt:?}");
        assert!(txt.contains("looprs-guh"), "which bead: {txt:?}");
        assert!(txt.contains("✗"), "did anything fail: {txt:?}");
        // ...and the hints, which are the cheapest things here, went first.
        assert!(!txt.contains("Tab switch"), "{txt:?}");
        assert!(!txt.contains("bg: Pi"), "{txt:?}");
    }

    /// The marker is not optional. A shortened error still says a thing failed;
    /// a truncated-to-nothing error looks exactly like a clean run.
    #[test]
    fn a_shortened_error_keeps_its_marker() {
        let mut s = plain(TerminalType::Pi, SessionStatus::Idle);
        s.active.error = Some("a very long failure message that will not fit anywhere");
        for width in 12u16..=48 {
            let (txt, _) = render(&s, width);
            assert!(txt.contains("✗"), "width {width}: {txt:?}");
        }
    }

    #[test]
    fn the_widest_segment_goes_first_and_the_mode_last() {
        let b = bead();
        let mut s = plain(TerminalType::Beeds, SessionStatus::Idle);
        s.active.bead = Some(&b);
        // Wide enough for everything, including the title.
        let (wide, _) = render(&s, 120);
        assert!(
            wide.contains("Status row is allocated but empty"),
            "{wide:?}"
        );
        // Not enough for the title, which is the longest thing here.
        let (tight, _) = render(&s, 46);
        assert!(tight.contains("looprs-guh"), "{tight:?}");
        assert!(
            !tight.contains("Status row is allocated"),
            "the title outlived the room for it: {tight:?}"
        );
    }

    /// ADR-0002's reason for this whole row: the mode you are looking at is not
    /// the only one that can be spending money.
    #[test]
    fn a_busy_mode_you_are_not_looking_at_is_named_and_the_idle_one_is_not() {
        let b = bead();
        let mut s = plain(TerminalType::Pi, SessionStatus::Idle);
        s.background.push(Sess {
            mode: TerminalType::Beeds,
            status: SessionStatus::Running,
            step: Some(BeadStep::WorkTickets),
            bead: Some(&b),
            elapsed: Some(Duration::from_secs(9)),
            error: None,
        });
        let (txt, _) = render(&s, 100);
        assert!(txt.contains("bg: Beeds working"), "{txt:?}");
        assert!(txt.contains("looprs-guh"), "{txt:?}");
        // The active mode never lists itself as background.
        assert!(!txt.contains("bg: Pi"), "{txt:?}");
    }

    /// The ADR-0002 fact at the ticket's floor: the head of the background entry
    /// stays, the detail is what pays. A truncated bead id would be worse than an
    /// absent one, so nothing here is cut mid-token.
    #[test]
    fn at_forty_columns_a_background_run_still_says_which_mode_and_that_it_works() {
        let b = bead();
        let mut s = plain(TerminalType::Pi, SessionStatus::Idle);
        s.background.push(Sess {
            mode: TerminalType::Beeds,
            status: SessionStatus::Running,
            step: Some(BeadStep::WorkTickets),
            bead: Some(&b),
            elapsed: Some(Duration::from_secs(95)),
            error: None,
        });
        let (txt, w) = render(&s, 40);
        assert!(w <= 40, "{w}: {txt:?}");
        assert!(txt.contains("bg: Beeds working"), "{txt:?}");
        assert!(
            !txt.contains("looprs-"),
            "the detail outlived the room for it: {txt:?}"
        );
    }

    /// Warm = resident and idle. Not the urgent case, but the one ADR-0002 names
    /// as a cost of the warm-child policy, so it gets said.
    #[test]
    fn warm_children_are_listed_once_and_busy_ones_are_not_double_counted() {
        let mut s = plain(TerminalType::Beeds, SessionStatus::Idle);
        s.warm = vec![TerminalType::Pi, TerminalType::Bash];
        let (txt, _) = render(&s, 100);
        assert!(txt.contains("warm: Pi, Bash"), "{txt:?}");

        // A busy background mode rides its own, higher-priority entry.
        let mut s = plain(TerminalType::Beeds, SessionStatus::Idle);
        s.background
            .push(sess(TerminalType::Pi, SessionStatus::Running));
        let (txt, _) = render(&s, 100);
        assert!(txt.contains("bg: Pi running"), "{txt:?}");
        assert!(!txt.contains("warm: Pi"), "{txt:?}");
    }

    /// The hint list is about *right now*. Offering `Esc` over an idle pane teaches
    /// the user that it does something when it does not.
    #[test]
    fn esc_is_offered_only_while_there_is_something_to_cancel() {
        let (idle, _) = render(&plain(TerminalType::Bash, SessionStatus::Idle), 100);
        assert!(!idle.contains("Esc"), "{idle:?}");
        let (busy, _) = render(&plain(TerminalType::Bash, SessionStatus::Running), 100);
        assert!(busy.contains("Esc cancel"), "{busy:?}");
        let (aborting, _) = render(&plain(TerminalType::Bash, SessionStatus::Aborting), 100);
        assert!(aborting.contains("Esc cancel"), "{aborting:?}");
    }

    /// **Tokens appear only when they were reported.**
    ///
    /// The "stays out" half is the load-bearing one. pi's `usage` is an optional
    /// record, so `↑0 ↓0` would read as "this run was free" when the truth is
    /// "this run was never measured". The row keeps silent instead of guessing.
    #[test]
    fn tokens_are_shown_only_when_they_were_reported() {
        let mut s = plain(TerminalType::Pi, SessionStatus::Running);
        let (txt, _) = render(&s, 100);
        assert!(!txt.contains('\u{2191}'), "{txt:?}");
        assert!(!txt.contains("cache"), "{txt:?}");

        s.tokens = Tokens {
            input: 4096 + 205,
            output: 812,
            cache: 0,
        };
        let (txt, _) = render(&s, 100);
        assert!(txt.contains("\u{2191}4.3k \u{2193}812"), "{txt:?}");
        assert!(!txt.contains("cache"), "no cache reported: {txt:?}");

        s.tokens.cache = 1_800_000;
        let (txt, _) = render(&s, 100);
        assert!(txt.contains("cache 1.80M"), "{txt:?}");
    }

    /// The two cost facts surrender at different widths, by design: the cache
    /// detail is paid for the in/out pair before the pair gives anything up.
    ///
    /// Asserted by *finding* the surrender width rather than hard-coding one, so
    /// the test says what it means ("cache went first") and does not rot the next
    /// time a segment changes length.
    #[test]
    fn the_cache_detail_gives_way_before_the_in_out_pair() {
        let b = bead();
        let active = Sess {
            mode: TerminalType::Beeds,
            status: SessionStatus::Running,
            step: Some(BeadStep::WorkTickets),
            bead: Some(&b),
            elapsed: Some(Duration::from_secs(42)),
            error: None,
        };
        let s = Status {
            active,
            background: vec![sess(TerminalType::Pi, SessionStatus::Running)],
            warm: vec![TerminalType::Bash],
            tokens: Tokens {
                input: 1_234_567,
                output: 42_100,
                cache: 9_100_000,
            },
            spinner: 0,
        };

        // The row's natural width: everything fits at that and above.
        let full = render(&s, 300).1 as u16;
        assert!(
            render(&s, 300).0.contains("cache 9.10M"),
            "and the cache detail is in it: {:?}",
            render(&s, 300).0
        );

        // The first width at which the cache segment is gone.
        let cache_gone = (1..=full)
            .rev()
            .find(|w| !render(&s, *w).0.contains("cache"))
            .expect("the cache segment survived every width");

        // …and at that width the pair is still on screen: it is strictly better
        // kept than the cache detail, which is the ordering this test exists to pin.
        let (txt, w) = render(&s, cache_gone);
        assert!(w <= cache_gone as usize, "{w} > {cache_gone}");
        assert!(
            txt.contains('\u{2191}') && txt.contains('\u{2193}'),
            "in/out should outlive the cache detail: {txt:?}"
        );
        // And of course the answers the row exists to give are all still there.
        assert!(
            txt.contains("Beeds") && txt.contains("working") && txt.contains("looprs-guh"),
            "{txt:?}"
        );
    }

    // ────────────────────────── the number formats ──────────────────────────

    #[test]
    fn elapsed_reads_as_a_duration_at_every_order_of_magnitude() {
        let cases: [(u64, &str); 8] = [
            (0, "0s"),
            (9, "9s"),
            (59, "59s"),
            (60, "1m00s"),
            (63, "1m03s"),
            (3599, "59m59s"),
            (3600, "1h00m"),
            (90 * 60, "1h30m"),
        ];
        for (secs, want) in cases {
            assert_eq!(fmt_elapsed(Duration::from_secs(secs)), want, "{secs}s");
        }
        assert_eq!(fmt_elapsed(Duration::from_secs(86_400)), "1d+");
    }

    #[test]
    fn token_counts_stay_compact_at_every_order_of_magnitude() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(7), "7");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(999_999), "999.9k");
        assert_eq!(fmt_tokens(1_000_000), "1.00M");
        assert_eq!(fmt_tokens(8_400_000), "8.40M");
        assert_eq!(fmt_tokens(123_456_789), "123.45M");
    }
}

#[cfg(test)]
mod peek {
    use super::*;

    /// Not an assertion — a look. `cargo test peek -- --nocapture` prints the row
    /// at a spread of widths with a full state in every slot, which is the fastest
    /// way to tell whether the ladder reads the way it was designed to.
    #[test]
    fn look() {
        // Every printed row is also checked: within the width it was asked for, and
        // naming its mode. A print-only test rots silently; this one cannot.
        let b = ActiveBead {
            id: "looprs-guh".into(),
            title: "Status row is allocated but empty".into(),
        };
        let cases: Vec<(&str, Status)> = vec![
            (
                "beads idle, nothing started",
                Status {
                    active: Sess {
                        mode: TerminalType::Beeds,
                        status: SessionStatus::NotStarted,
                        step: Some(BeadStep::AwaitInput),
                        bead: None,
                        elapsed: None,
                        error: None,
                    },
                    background: vec![],
                    warm: vec![],
                    tokens: Tokens::default(),
                    spinner: 0,
                },
            ),
            (
                "beads working a bead, warm pi",
                Status {
                    active: Sess {
                        mode: TerminalType::Beeds,
                        status: SessionStatus::Running,
                        step: Some(BeadStep::WorkTickets),
                        bead: Some(&b),
                        elapsed: Some(Duration::from_secs(7)),
                        error: None,
                    },
                    background: vec![],
                    warm: vec![TerminalType::Pi],
                    tokens: Tokens::default(),
                    spinner: 4,
                },
            ),
            (
                "pi idle, beads still working (adr-0002)",
                Status {
                    active: Sess {
                        mode: TerminalType::Pi,
                        status: SessionStatus::Idle,
                        step: None,
                        bead: None,
                        elapsed: None,
                        error: None,
                    },
                    background: vec![Sess {
                        mode: TerminalType::Beeds,
                        status: SessionStatus::Running,
                        step: Some(BeadStep::WorkTickets),
                        bead: Some(&b),
                        elapsed: Some(Duration::from_secs(95)),
                        error: None,
                    }],
                    warm: vec![TerminalType::Bash],
                    tokens: Tokens::default(),
                    spinner: 2,
                },
            ),
            (
                "bash running, an error on file",
                Status {
                    active: Sess {
                        mode: TerminalType::Bash,
                        status: SessionStatus::Running,
                        step: None,
                        bead: None,
                        elapsed: Some(Duration::from_secs(41)),
                        error: Some("spawn: no such file or directory"),
                    },
                    background: vec![],
                    warm: vec![TerminalType::Pi, TerminalType::Beeds],
                    tokens: Tokens {
                        input: 12_900,
                        output: 3_100,
                        cache: 1_800_000,
                    },
                    spinner: 7,
                },
            ),
            (
                "beads paused after an error",
                Status {
                    active: Sess {
                        mode: TerminalType::Beeds,
                        status: SessionStatus::Idle,
                        step: Some(BeadStep::AwaitInput),
                        bead: None,
                        elapsed: None,
                        error: Some("bd claim refused: looprs-guh is owned by another owner"),
                    },
                    background: vec![],
                    warm: vec![],
                    tokens: Tokens::default(),
                    spinner: 0,
                },
            ),
        ];
        for (name, s) in cases {
            println!("\n{name}");
            for w in [40u16, 60, 100] {
                let l = s.line(w);
                println!("  {w:>3} |{l}|  ({} cols)", l.width());
                assert!(l.width() <= w as usize, "{name} @ {w}");
                assert!(
                    l.to_string().contains(s.active.mode.label()),
                    "{name} @ {w} lost its mode: {l}"
                );
            }
        }
    }
}
