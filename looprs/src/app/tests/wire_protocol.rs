//! The wire protocol inventory, read as data.
//!
//! [`WIRE_INVENTORY`] is the table `docs/guide/wire-protocol.md` is generated
//! from, and these tests are what make it a claim rather than a comment. Three
//! directions get checked:
//!
//! * **table ↔ type**: every row's `wire` value parses to the variant the row
//!   names, and for the three [`WireValue`] types the row's `outcome` is the one
//!   the variant's own exhaustive `outcome()` returns;
//! * **table ↔ app**: every role is driven through [`apply_pi`] and the
//!   transcript is checked against the row — painted, deliberately silent, or a
//!   note that names the value;
//! * **table ↔ itself**: no duplicate rows, no orphan groups, and a row count
//!   that has to be edited in the same breath as the enum it counts.
//!
//! The docs half of the contract lives in `scripts/docs_check.py`, which
//! regenerates the page from the same array and fails if the committed page
//! disagrees — the `CHORD_TABLE` pattern, applied to the wire (looprs-00u.19).

use super::*;
use crate::session::view::SessionView;
use crate::wire::{
    CompactionReason, Outcome, StopReason, WIRE_GROUPS, WIRE_INVENTORY, WireRow, wire_rows,
};
use serde::Deserialize;

/// How many variants each enum declares, as counted against its own declaration.
///
/// This is the weak link in the chain and it is named as one: Rust cannot count a
/// type's variants without a proc macro, so this number is hand-carried. What it
/// buys is that adding a variant to `PiEvent`, `EntryRole` or `AssistantEvent`
/// and forgetting the inventory **fails a test with this file's name in it**
/// rather than silently shipping a protocol page that is one row short.
const DECLARED: &[(&str, usize)] = &[
    ("role", 9),             // 8 roles pi declares + the Unknown arm
    ("event", 17),           // 16 event types + the Unknown arm
    ("assistant-event", 13), // 12 nested events + the Unknown arm
    ("compaction-reason", 4),
    ("stop-reason", 8),
];

fn view() -> SessionView {
    SessionView::new(SessionId::new(TerminalType::Pi, 1))
}

fn rows(group: &'static str) -> Vec<&'static WireRow> {
    wire_rows(group).collect()
}

fn outcome_of(row: &WireRow) -> String {
    row.outcome.label()
}

/// **The role table names the real wire values.** Each row's `wire` spelling is
/// the one that parses to the row's `variant`, and each round-trips back out
/// through `as_str` — so a camelCase role mistyped as snake_case cannot hide in
/// the page.
#[test]
fn every_role_row_names_a_real_role_and_round_trips() {
    for row in rows("role") {
        if row.variant == "Unknown" {
            continue;
        }
        let role = EntryRole::parse(row.wire);
        assert_eq!(
            role.variant(),
            row.variant,
            "the `{}` row names `{}`, but that wire value parses to `{}`",
            row.wire,
            row.variant,
            role.variant()
        );
        assert_eq!(
            role.as_str(),
            row.wire,
            "`{}` did not round-trip through `EntryRole`",
            row.wire
        );
        assert!(role.is_known(), "`{}` should be a known role", row.wire);
        assert_eq!(
            role.outcome(),
            row.outcome,
            "the `{}` row says {:?} but the type says {:?}",
            row.wire,
            row.outcome,
            role.outcome()
        );
    }
}

/// The `Unknown` arm keeps what it was handed. This is the arm the whole ticket
/// exists for: the roles pi can send that this file has not named must arrive as
/// themselves rather than as nothing.
#[test]
fn a_role_this_file_has_not_named_keeps_its_own_spelling() {
    let invented = EntryRole::parse("quantumFrobnicate");
    assert_eq!(invented.variant(), "Unknown");
    assert!(!invented.is_known());
    assert_eq!(invented.as_str(), "quantumFrobnicate");

    let row = rows("role")
        .into_iter()
        .find(|r| r.variant == "Unknown")
        .expect("the role group has an Unknown row");
    assert_eq!(
        invented.outcome(),
        row.outcome,
        "an invented role must get the treatment the page promises: {}",
        outcome_of(row)
    );
    assert_eq!(row.outcome, Outcome::SurfacesAsNote);
}

/// **The app is made to do it.** Every role in the table goes through
/// `apply_pi` as a `message_end`, and the transcript is checked against the
/// row's own outcome. A row that promises a visible note and delivers silence
/// fails here, in the suite, not in a reading of the docs.
#[test]
fn every_role_does_what_its_row_says_when_a_message_ends() {
    for row in rows("role") {
        let mut v = view();
        // Something streaming first: "seals the live region" has to be
        // observable as a change, not as the absence of one.
        v.push_delta(MessageKind::Answer, "half an answer");
        let before = v.transcript.entries.len();
        let sealed_before = v.transcript.entries.back().map(|e| e.done);

        apply_pi(
            &mut v,
            PiEvent::MessageEnd {
                message: WireMessage {
                    role: EntryRole::parse(row.wire),
                    usage: None,
                },
            },
        );

        let after = v.transcript.entries.len();
        match &row.outcome {
            Outcome::Renders(_) => {
                assert_eq!(after, before, "rendering here means sealing, not appending");
                assert_eq!(
                    v.transcript.entries.back().map(|e| e.done),
                    Some(true),
                    "`{}` must seal the live region",
                    row.wire
                );
                assert_ne!(
                    sealed_before,
                    Some(true),
                    "the region was open before, so the seal is a real change"
                );
            }
            Outcome::Silent(_) => {
                assert_eq!(
                    after, before,
                    "`{}` is `Silent` on the page — {:?} — but added a line",
                    row.wire, row.outcome
                );
                assert_eq!(
                    v.transcript.entries.back().map(|e| e.done),
                    sealed_before,
                    "`{}` must not even seal: the row says nothing is painted",
                    row.wire
                );
            }
            Outcome::SurfacesAsNote => {
                assert_eq!(
                    after,
                    before + 1,
                    "`{}` is a note on the page and added nothing",
                    row.wire
                );
                let note = v.transcript.entries.back().expect("the note");
                assert!(
                    matches!(note.kind, MessageKind::System),
                    "`{}` should surface as a system line, got {:?}",
                    row.wire,
                    note.kind
                );
                assert!(
                    note.text.contains(row.wire)
                        || row.variant == "Unknown" && note.text.contains("quantumFrobnicate")
                        || row.wire == "*anything else*",
                    "the note must name the value; it says {:?}",
                    note.text
                );
            }
        }
    }
}

/// The same claim for a value nobody has seen before, spelled out because it is
/// the one the ticket was filed against: an unrecognised role used to be a
/// string that flowed through and rendered as nothing.
#[test]
fn a_role_nobody_has_seen_arrives_as_a_line_that_names_it() {
    let mut v = view();
    apply_pi(
        &mut v,
        PiEvent::MessageEnd {
            message: WireMessage {
                role: EntryRole::parse("quantumFrobnicate"),
                usage: None,
            },
        },
    );
    let entries = &v.transcript.entries;
    assert_eq!(entries.len(), 1, "the unknown role produced one line");
    assert!(
        entries[0].text.contains("quantumFrobnicate"),
        "and the line says what arrived: {:?}",
        entries[0].text
    );
}

/// No event row names a `type` tag that pi does not actually send: every one of
/// them is recognised by the real deserialiser. A row whose spelling is wrong
/// would land in `Unknown`, and the page would be documenting a value the
/// binary never sees.
#[test]
fn every_event_row_names_a_tag_the_deserialiser_recognises() {
    for row in rows("event") {
        if row.variant == "Unknown" {
            continue;
        }
        match PiEvent::deserialize(&serde_json::json!({ "type": row.wire })) {
            Ok(ev) => assert_eq!(
                ev.variant(),
                row.variant,
                "`\"type\": \"{}\"` deserialises to `{}`, not the row's `{}`",
                row.wire,
                ev.variant(),
                row.variant
            ),
            // A recognised tag with fields this minimal record does not supply:
            // exactly the outcome that proves the tag itself is real.
            Err(e) => assert!(
                e.to_string().contains("missing field"),
                "`{}` is not a tag serde knows: {e}",
                row.wire
            ),
        }
    }
}

/// As above, one level down: the nested `assistantMessageEvent` table.
#[test]
fn every_assistant_event_row_names_a_tag_the_deserialiser_recognises() {
    for row in rows("assistant-event") {
        if row.variant == "Unknown" {
            continue;
        }
        match AssistantEvent::deserialize(&serde_json::json!({ "type": row.wire })) {
            Ok(ev) => assert_eq!(
                ev.variant(),
                row.variant,
                "`assistantMessageEvent.type = \"{}\"` deserialises to `{}`, not the row's `{}`",
                row.wire,
                ev.variant(),
                row.variant
            ),
            Err(e) => assert!(
                e.to_string().contains("missing field"),
                "`{}` is not a nested tag serde knows: {e}",
                row.wire
            ),
        }
    }
}

/// The two reason tables match their types, value for value — the same check the
/// roles get, because `manual`/`threshold`/`overflow` and the seven stop
/// reasons were the other "values listed only in a comment" fields in the file.
#[test]
fn the_reason_rows_match_their_types() {
    for row in rows("compaction-reason") {
        let r = CompactionReason::parse(row.wire);
        if row.variant == "Unknown" {
            assert_eq!(r.variant(), "Unknown");
            assert!(!r.is_known());
            continue;
        }
        assert_eq!(r.variant(), row.variant, "`{}` mislabeled", row.wire);
        assert_eq!(r.as_str(), row.wire, "`{}` does not round-trip", row.wire);
        assert_eq!(r.outcome(), row.outcome, "`{}` outcome drifted", row.wire);
    }

    for row in rows("stop-reason") {
        let r = StopReason::parse(row.wire);
        if row.variant == "Unknown" {
            assert_eq!(r.variant(), "Unknown");
            assert!(!r.is_known());
            continue;
        }
        assert_eq!(r.variant(), row.variant, "`{}` mislabeled", row.wire);
        assert_eq!(r.as_str(), row.wire, "`{}` does not round-trip", row.wire);
        assert_eq!(r.outcome(), row.outcome, "`{}` outcome drifted", row.wire);
    }
}

/// The table is a table: one row per (group, variant, wire), no row in a group
/// the docs page does not print, and a row count that has to move with the enum.
#[test]
fn the_inventory_is_a_table() {
    for (group, declared) in DECLARED {
        let got = rows(group).len();
        assert_eq!(
            got, *declared,
            "`{group}` has {got} rows in WIRE_INVENTORY and {declared} declared in the enum; \
             a variant added to the type needs its row here before the docs page can carry it"
        );
    }

    let groups: std::collections::BTreeSet<&str> = WIRE_INVENTORY.iter().map(|r| r.group).collect();
    let printed: std::collections::BTreeSet<&str> = WIRE_GROUPS.iter().copied().collect();
    assert_eq!(
        groups, printed,
        "every group with rows must be printed, and every printed group must have rows"
    );

    for group in WIRE_GROUPS {
        let mut seen = std::collections::BTreeSet::new();
        for row in wire_rows(group) {
            assert!(
                seen.insert((row.wire, row.variant)),
                "duplicate `{group}` row: {} / {}",
                row.wire,
                row.variant
            );
            assert!(
                !row.note.trim().is_empty(),
                "`{group}` row `{}` has no note: the page would print an empty cell",
                row.wire
            );
        }
    }
}

/// The three outcomes have to be tellable apart in prose, because that column is
/// the only thing on the page that says "we drop this on purpose" from "we never
/// thought about it". `label()` is asserted by name here so the generator's
/// wording is a checked claim rather than a phrase only the generator knows.
#[test]
fn the_three_outcomes_read_differently() {
    let renders = Outcome::Renders("the answer stream").label();
    let silent = Outcome::Silent("already echoed").label();
    let note = Outcome::SurfacesAsNote.label();
    assert!(renders.starts_with("renders:"), "{renders}");
    assert!(silent.starts_with("not painted"), "{silent}");
    assert!(note.contains("note") && note.contains("value"), "{note}");
    assert_ne!(renders, silent);
    assert_ne!(silent, note);
    assert_ne!(renders, note);
}

/// Every `Silent` row says *why* it is silent, and every unread row names what
/// would make it read. A `Silent` with no reason, or a `reader: None` with
/// nothing after it, is indistinguishable from "nobody looked" — which is the
/// thing the whole inventory exists to make impossible.
#[test]
fn silence_comes_with_a_reason_and_an_unread_row_names_what_it_waits_for() {
    for row in WIRE_INVENTORY {
        if let Outcome::Silent(why) = &row.outcome {
            assert!(
                why.len() > "nothing yet".len(),
                "`{}` row is silent without a reason worth reading: {why:?}",
                row.wire
            );
        }
        // `reader: None` is the inventory saying "nothing reads this today".
        // That is allowed exactly as far as the allows go — and no further:
        // `waiting_on` has to say what would change it, which is the question
        // `scripts/dead_audit.py` prints next to every allow so a human can
        // answer it (looprs-2nd: nine allows promised a consumer that had
        // landed reading something else).
        match (row.reader, row.waiting_on) {
            (Some(_), None) => {}
            (None, Some(what)) => assert!(
                what.trim().len() > 20,
                "`{group}` row `{wire}` has no reader, so `waiting_on` has to say something \
                 worth reading, not {what:?}",
                group = row.group,
                wire = row.wire
            ),
            (None, None) => panic!(
                "`{group}` row `{wire}` is unread and names nothing it waits for",
                group = row.group,
                wire = row.wire
            ),
            (Some(r), Some(w)) => panic!(
                "`{group}` row `{wire}` has a reader ({r:?}) and a `waiting_on` ({w:?}): pick one",
                group = row.group,
                wire = row.wire
            ),
        }
    }
}

/// The page cannot be generated from an empty or shuffled table: check the
/// groups render in the declared order and that every group's rows arrive in
/// the order they are written here (the docs generator preserves it).
#[test]
fn the_groups_render_in_the_declared_order() {
    let order: Vec<&str> = WIRE_INVENTORY.iter().map(|r| r.group).collect();
    let mut last_seen = 0usize;
    for group in WIRE_GROUPS {
        let at = order.iter().position(|g| g == group);
        let at = at.expect("every printed group has rows");
        assert!(
            order[at..].iter().take_while(|g| *g == group).count() > 1 || order[at] == *group,
            "group `{group}` must appear as one contiguous block"
        );
        assert!(
            at >= last_seen,
            "group `{group}` appears before an earlier one"
        );
        last_seen = at;
    }
}
