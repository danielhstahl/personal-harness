//! The token window (the row's cost facts)
//!
//! `↑in ↓out` is a *window*, not a global counter, and which window a mode
//! gets is the design (see `SessionView::tokens`). Every number here was handed
//! to the App by a `Msg`, so none of it depends on a child, a provider, or a
//! fixture's arithmetic.

use super::*;

/// Flush the active view and read back **what the store now holds**, as text.
///
/// `flush_active` used to hand the rows themselves back so tests could read
/// them. It returns a count now: the band draws from the store, the run loop
/// drops the return value, and the only reader of those cloned rows was the
/// test suite — a whole-batch clone every frame, bought for nobody. These
/// tests read the same store the display reads, which is the stronger claim.
fn flush_text(app: &mut App) -> String {
    app.flush_active(60);
    app.scrollback()
        .rows()
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn ended(session: SessionId, role: &str, u: Option<Usage>) -> Msg {
    Msg::Agent {
        session,
        event: PiEvent::MessageEnd {
            message: WireMessage {
                role: EntryRole::parse(role),
                usage: u,
            },
        },
    }
}

fn claim(id: &str) -> Msg {
    Msg::ActiveBead {
        session: beads_id(),
        bead: Some(ActiveBead {
            id: id.to_string(),
            title: format!("ticket {id}"),
        }),
    }
}

fn beads_tokens(app: &App) -> Tokens {
    app.view(TerminalType::Beads).unwrap().tokens
}

fn pi_tokens(app: &App) -> Tokens {
    app.view(TerminalType::Pi).unwrap().tokens
}

/// The window adds every assistant message's usage, and nothing else adds.
///
/// The two no-rows carry the weight. A `user` message's `message_end` is not
/// assistant work, and an assistant message that reported **nothing** is not a
/// message that cost nothing: `None` must never be counted as zero.
#[test]
fn every_assistant_message_adds_and_nothing_else_does() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(ended(pi_id(), "assistant", Some(usage(100, 40, 900, 25))));
    app.update(ended(pi_id(), "assistant", Some(usage(200, 80, 1800, 50))));
    let t = pi_tokens(&app);
    assert_eq!((t.input, t.output, t.cache), (300, 120, 2775), "{t:?}");

    app.update(ended(pi_id(), "user", Some(usage(999, 999, 999, 999))));
    app.update(ended(pi_id(), "toolResult", Some(usage(9, 9, 9, 9))));
    app.update(ended(pi_id(), "assistant", None));
    assert_eq!(pi_tokens(&app), t, "not one of those moved the total");
}

/// A claim opens a fresh window; releasing one deliberately does not close it.
///
/// "What is this ticket costing" is only answerable if the number restarts at
/// the claim, and it is only *worth* anything after the pass if it survives the
/// release that follows it.
#[test]
fn the_beads_window_opens_on_a_claim_and_survives_the_release() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    app.update(ended(beads_id(), "assistant", Some(usage(10, 5, 20, 1))));
    assert_eq!(
        beads_tokens(&app).input,
        10,
        "unclaimed spend sits in the window"
    );

    app.update(claim("A"));
    assert!(beads_tokens(&app).is_empty(), "ticket A starts at nothing");
    app.update(ended(beads_id(), "assistant", Some(usage(70, 30, 600, 10))));
    let a = beads_tokens(&app);
    assert_eq!((a.input, a.output, a.cache), (70, 30, 610), "{a:?}");

    // Released, still on show: the row answers what A cost.
    app.update(Msg::ActiveBead {
        session: beads_id(),
        bead: None,
    });
    assert_eq!(beads_tokens(&app), a, "a release is not a reset");

    // And B never inherits A's bill.
    app.update(claim("B"));
    assert!(beads_tokens(&app).is_empty(), "B is a fresh window");
}

/// A respawned child starts at nothing.
///
/// The row means "this session", and a new generation *is* a new session —
/// carrying the dead child's total over would make an ordinary respawn read
/// like a runaway on the one number the user watches for that.
#[test]
fn a_new_generation_starts_the_window_at_nothing() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(ended(pi_id(), "assistant", Some(usage(500, 200, 4000, 90))));
    assert!(!pi_tokens(&app).is_empty());

    let gen2 = SessionId::new(TerminalType::Pi, 2);
    app.update(Msg::SessionStatus {
        session: gen2,
        status: SessionStatus::Idle,
    });
    assert!(
        pi_tokens(&app).is_empty(),
        "the new incarnation owes nothing for the old one's run"
    );
}

/// The wire shape, parsed from bytes shaped like pi's own.
///
/// camelCase on the wire; every field independently optional. `message_update`
/// carries a **cumulative** `usage` that this harness deliberately does not
/// model — the test pins that it still parses, and that nothing on the
/// streaming path can fold it into a total twice.
#[test]
fn the_wire_usage_record_parses_as_pi_writes_it() {
    let full: PiEvent = serde_json::from_str(
        r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":1234,"output":56,"cacheRead":9000,"cacheWrite":25}}}"#,
    )
    .expect("a full usage record parses");
    let PiEvent::MessageEnd { message } = full else {
        panic!("expected message_end");
    };
    let u = message.usage.expect("usage present");
    assert_eq!(
        (u.input, u.output, u.cache_read, u.cache_write),
        (1234, 56, 9000, 25),
        "camelCase mapped: {u:?}"
    );

    // A provider that reports only two of the four still lands; the rest are
    // zero-because-absent, which the window treats as "nothing to add".
    let part: PiEvent = serde_json::from_str(
        r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":7,"output":3}}}"#,
    )
    .expect("a partial usage record parses");
    let PiEvent::MessageEnd { message } = part else {
        panic!("expected message_end");
    };
    let u = message.usage.expect("usage present");
    assert_eq!(
        (u.input, u.output, u.cache_read, u.cache_write),
        (7, 3, 0, 0)
    );

    // No usage at all is `None` — never a zero-cost message.
    let none: PiEvent =
        serde_json::from_str(r#"{"type":"message_end","message":{"role":"user","content":"hi"}}"#)
            .expect("a usage-less message_end parses");
    let PiEvent::MessageEnd { message } = none else {
        panic!("expected message_end");
    };
    assert!(
        message.usage.is_none(),
        "absent is absent: {:?}",
        message.usage
    );

    // The streaming event's cumulative `usage` is not a modelled field, and
    // must not become one silently: it parses, and lands in the same variant.
    let upd: PiEvent = serde_json::from_str(concat!(
        r#"{"type":"message_update","usage":{"input":100,"output":20,"cacheRead":900,"cacheWrite":25},"#,
        r#""assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hi"}}"#,
    ))
    .expect("message_update parses with its cumulative usage alongside");
    assert!(matches!(upd, PiEvent::MessageUpdate { .. }));
}

/// The row's cost facts come out of the view that owns them — and the empty
/// window prints nothing rather than `↑0 ↓0`.
#[test]
fn the_row_prints_the_window_it_is_handed() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    let empty = app.status_line(120).to_string();
    assert!(
        !empty.contains('\u{2191}'),
        "nothing reported, nothing printed: {empty:?}"
    );

    app.update(claim("A"));
    app.update(ended(
        beads_id(),
        "assistant",
        Some(usage(12_345, 678, 90_000, 0)),
    ));
    let row = app.status_line(120).to_string();
    assert!(row.contains("\u{2191}12.3k \u{2193}678"), "{row:?}");
    assert!(row.contains("cache 90.0k"), "{row:?}");
}

/// The fix for the whole "I tabbed away and the other session wrecked this
/// view" class: an event lands in the view that *owns* the session.
#[test]
fn events_land_in_the_view_that_owns_them() {
    let (mut app, _rx) = app_with(TerminalType::Beads);

    app.update(Msg::Agent {
        session: pi_id(),
        event: PiEvent::MessageUpdate {
            assistant_message_event: AssistantEvent::TextDelta {
                content_index: 0,
                delta: "pi answer".into(),
            },
        },
    });
    app.update(Msg::Agent {
        session: beads_id(),
        event: PiEvent::MessageUpdate {
            assistant_message_event: AssistantEvent::TextDelta {
                content_index: 0,
                delta: "beads answer".into(),
            },
        },
    });

    assert!(text_of(&app, TerminalType::Beads).contains("beads answer"));
    assert!(!text_of(&app, TerminalType::Beads).contains("pi answer"));
    assert!(text_of(&app, TerminalType::Pi).contains("pi answer"));
    assert!(!text_of(&app, TerminalType::Pi).contains("beads answer"));
}

/// `need_input` follows the ACTIVE view, not the beads machine: a beads pass
/// working off-screen must not hide the Pi input box, and vice versa. The box
/// opens and closes on the *view's* derived state, never on whichever mode is
/// making noise.
#[test]
fn need_input_is_derived_from_the_active_view() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    assert!(app.need_input(), "no view yet => ask the human");

    app.update(Msg::SessionStatus {
        session: beads_id(),
        status: SessionStatus::Running,
    });
    assert!(!app.need_input(), "beads is working, so the box is hidden");

    // Same beads state; look at Pi instead. The Pi box is untouched.
    app.active = TerminalType::Pi;
    assert!(
        app.need_input(),
        "a beads pass running off-screen must not hide the Pi input box"
    );

    app.active = TerminalType::Beads;
    assert!(
        !app.need_input(),
        "and coming back must not resurrect a box the beads run already closed"
    );
}

/// The other half of the keyboard rule as the App sees it: no matter how many
/// agents are working, the Bash view answers yes. This is the whole reason the
/// user is never locked out of the harness.
#[test]
fn bash_takes_input_while_every_agent_is_working() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::SessionStatus {
        session: pi_id(),
        status: SessionStatus::Running,
    });
    app.update(Msg::BeadStep {
        session: beads_id(),
        step: BeadStep::WorkTickets,
    });
    app.update(Msg::SessionStatus {
        session: beads_id(),
        status: SessionStatus::Running,
    });
    app.update(Msg::BashOutput {
        session: SessionId::new(TerminalType::Bash, 1),
        stream: ByteStream::Merged,
        chunk: "$ ".into(),
    });

    assert!(!app.need_input(), "Pi is mid-run");
    app.active = TerminalType::Beads;
    assert!(!app.need_input(), "and so is beads");
    app.active = TerminalType::Bash;
    assert!(
        app.need_input(),
        "and the shell is open regardless of what the agents are doing"
    );
}

/// `chat_state` (spinner / live preview) is per session too: the tick must not
/// animate over a session that is not streaming, and one session's stream must
/// not animate another's preview.
#[test]
fn chat_state_is_per_session() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    assert_eq!(app.chat_state(), ChatState::Stopped);

    app.update(Msg::Agent {
        session: beads_id(),
        event: PiEvent::ToolExecutionStart {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            args: Value::Null,
        },
    });
    assert_eq!(
        app.chat_state(),
        ChatState::Stopped,
        "a beads tool going live must not animate the Pi view"
    );
    assert_eq!(app.view(TerminalType::Beads).unwrap().chat, ChatState::Tool);

    app.active = TerminalType::Beads;
    assert_eq!(app.chat_state(), ChatState::Tool);
}

/// A compaction is a separate LLM call that pauses the run and prints nothing
/// of its own — ten to sixty seconds of a transcript that has stopped moving,
/// which is exactly the shape of "is it hung?". So the start event must put a
/// row on the screen by itself.
#[test]
fn a_compaction_shows_a_live_card_while_it_runs() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::Agent {
        session: pi_id(),
        event: PiEvent::CompactionStart {
            reason: CompactionReason::Threshold,
        },
    });

    assert_eq!(
        app.chat_state(),
        ChatState::Compacting,
        "the live region is animating for it, not for stale prose"
    );
    assert_eq!(
        app.live_card_rows(),
        1,
        "and the height policy is told to budget its row"
    );

    let card = app
        .view(TerminalType::Pi)
        .unwrap()
        .transcript
        .open_cards()
        .last()
        .expect("the card is open");
    let line = crate::components::card::card_line(card, 0).to_string();
    assert!(line.contains("compacting context"), "{line}");
    assert!(line.contains("threshold"), "and why: {line}");
}

/// `compaction_start` on its own is only half the feedback. The end is the
/// half that says whether it worked, and pi says three different things there
/// — freed something, cancelled, failed — each of which has to reach the
/// scrollback as its own line once the card closes.
#[test]
fn a_finished_compaction_reaches_the_scrollback_with_what_it_freed() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.on_pi(
        pi_id(),
        PiEvent::CompactionStart {
            reason: CompactionReason::Threshold,
        },
    );
    app.on_pi(
        pi_id(),
        PiEvent::CompactionEnd {
            reason: Some(CompactionReason::Threshold),
            aborted: false,
            error_message: None,
            result: Some(CompactionResult {
                tokens_before: Some(150_000),
                estimated_tokens_after: Some(32_000),
            }),
        },
    );

    assert_eq!(
        app.live_card_rows(),
        0,
        "the card is closed, so it stops taking live rows"
    );
    assert_eq!(
        app.chat_state(),
        ChatState::Stopped,
        "and nothing is left spinning over work that has finished"
    );

    let flushed: String = flush_text(&mut app);
    assert!(flushed.contains("context compacted"), "{flushed:?}");
    assert!(
        flushed.contains("150.0k → 32.0k"),
        "the numbers pi reported are the point of the row: {flushed:?}"
    );
}

/// Cancelled and failed are the two endings a user must not have to guess
/// about, and they are not the same sentence: `aborted: true` means the user
/// pressed Esc, `errorMessage` means the summarisation call broke.
#[test]
fn an_aborted_compaction_says_aborted_and_a_failed_one_says_why() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    app.on_pi(
        beads_id(),
        PiEvent::CompactionEnd {
            reason: Some(CompactionReason::Manual),
            aborted: true,
            error_message: None,
            result: None,
        },
    );
    let aborted: String = flush_text(&mut app);
    assert!(aborted.contains("compaction aborted"), "{aborted:?}");
    assert!(aborted.contains("manual"), "{aborted:?}");

    app.on_pi(
        beads_id(),
        PiEvent::CompactionEnd {
            reason: Some(CompactionReason::Overflow),
            aborted: false,
            error_message: Some("provider refused the summary".into()),
            result: None,
        },
    );
    let failed: String = flush_text(&mut app);
    assert!(
        failed.contains("provider refused the summary"),
        "pi's own words, not a shrug: {failed:?}"
    );
    assert!(
        !aborted.contains("provider refused"),
        "and the two endings are two separate rows"
    );
}

/// An `end` whose `start` never arrived (a reconnect, a dropped line) is still
/// an event that happened. Swallowing it because there was nothing to close is
/// the same silence this whole path exists to remove.
#[test]
fn a_compaction_end_with_no_open_card_is_recorded_anyway() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.on_pi(
        pi_id(),
        PiEvent::CompactionEnd {
            reason: Some(CompactionReason::Overflow),
            aborted: false,
            error_message: None,
            result: None,
        },
    );
    let flushed: String = flush_text(&mut app);
    assert!(
        flushed.contains("context compacted · overflow"),
        "the reason comes off the end record when there was no start to say it: {flushed:?}"
    );
}

/// The card must not hold the transcript hostage — but while it is open it
/// legitimately owns the flush cursor, the same way a running tool does. What
/// matters is that closing it releases everything queued behind it.
#[test]
fn closing_the_compaction_card_releases_what_came_behind_it() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.on_pi(
        pi_id(),
        PiEvent::CompactionStart {
            reason: CompactionReason::Threshold,
        },
    );
    app.on_pi(
        pi_id(),
        PiEvent::MessageUpdate {
            assistant_message_event: AssistantEvent::TextDelta {
                content_index: 0,
                delta: "text after the summary\n\n".into(),
            },
        },
    );
    assert!(
        app.flush_active(60) == 0,
        "the open card is the cursor, and nothing past it goes out yet"
    );

    app.on_pi(
        pi_id(),
        PiEvent::CompactionEnd {
            reason: Some(CompactionReason::Threshold),
            aborted: false,
            error_message: None,
            result: None,
        },
    );
    let flushed: String = flush_text(&mut app);
    assert!(flushed.contains("context compacted"), "{flushed:?}");
    assert!(
        flushed.contains("text after the summary"),
        "and the run's own text follows it into the scrollback: {flushed:?}"
    );
}

/// The wire shape, pinned: `reason` is required on the start, and optional on
/// the end, where `result` is absent unless the compaction succeeded.
#[test]
fn the_compaction_wire_format_parses() {
    let s = parse(&serde_json::json!({"type":"compaction_start","reason":"overflow"})).unwrap();
    assert!(
        matches!(s, PiEvent::CompactionStart { reason } if reason == CompactionReason::Overflow)
    );

    let e = parse(&serde_json::json!({
        "type": "compaction_end",
        "reason": "threshold",
        "result": { "summary": "...", "firstKeptEntryId": "abc", "tokensBefore": 150000, "estimatedTokensAfter": 32000 },
        "aborted": false,
        "willRetry": true
    }))
    .unwrap();
    match e {
        PiEvent::CompactionEnd {
            reason,
            aborted,
            error_message,
            result,
        } => {
            assert_eq!(reason, Some(CompactionReason::Threshold));
            assert!(!aborted);
            assert!(error_message.is_none());
            let r = result.expect("the result was parsed");
            assert_eq!(r.tokens_before, Some(150_000));
            assert_eq!(r.estimated_tokens_after, Some(32_000));
        }
        other => panic!("wrong event: {other:?}"),
    }

    // Aborted: no `result` on the wire at all, and `aborted: true`.
    assert!(matches!(
        parse(&serde_json::json!({"type":"compaction_end","aborted":true})).unwrap(),
        PiEvent::CompactionEnd {
            aborted: true,
            result: None,
            ..
        }
    ));
}

/// Death must seal: the text a dead session left un-flushed has to reach the
/// scrollback, not stall the cursor forever.
#[test]
fn session_down_seals_the_view_and_gives_the_input_box_back() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    app.update(Msg::Agent {
        session: beads_id(),
        event: PiEvent::MessageUpdate {
            assistant_message_event: AssistantEvent::TextDelta {
                content_index: 0,
                delta: "unterminated answer".into(),
            },
        },
    });
    assert!(
        app.flush_active(60) == 0,
        "an open entry is not flushed while it is still open"
    );

    app.update(Msg::SessionDown {
        session: beads_id(),
        reason: ExitReason::Crashed { code: Some(2) },
    });
    let lines = flush_text(&mut app);
    assert!(
        lines.contains("unterminated answer"),
        "sealing on death must release the tail: {lines:?}"
    );
    let v = app.view(TerminalType::Beads).unwrap();
    assert_eq!(v.status, SessionStatus::Dead);
    assert!(
        v.accepts_input(),
        "a dead session must not hide the input box"
    );
}

/// A new generation of a mode cannot strand the previous one's output.
#[test]
fn adopting_a_new_generation_seals_the_old_one() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let old = SessionId::new(TerminalType::Pi, 1);
    let new = SessionId::new(TerminalType::Pi, 2);

    app.update(Msg::Agent {
        session: old,
        event: PiEvent::MessageUpdate {
            assistant_message_event: AssistantEvent::TextDelta {
                content_index: 0,
                delta: "from the corpse".into(),
            },
        },
    });
    app.view_mut(new); // the respawn arrives without any SessionDown
    let lines = flush_text(&mut app);
    assert!(
        lines.contains("from the corpse"),
        "adoption must seal what it replaces: {lines:?}"
    );
    assert_eq!(app.view(TerminalType::Pi).unwrap().session, new);
}

/// Only the active view flushes; a hidden one keeps buffering.
#[test]
fn only_the_active_view_flushes() {
    let (mut app, _rx) = app_with(TerminalType::Beads);
    app.update(Msg::System {
        session: Some(pi_id()),
        text: "buffered while hidden".into(),
    });
    assert!(
        app.flush_active(60) == 0,
        "nothing has been said to the beads view"
    );
    assert!(
        text_of(&app, TerminalType::Pi).contains("buffered while hidden"),
        "the hidden view kept its text instead of dropping it"
    );

    app.active = TerminalType::Pi;
    let lines = flush_text(&mut app);
    assert!(
        lines.contains("buffered while hidden"),
        "switching in flushes the backlog as one burst: {lines:?}"
    );
}

/// A harness-level message (no session) goes where the user can see it.
#[test]
fn harness_messages_go_to_the_active_view() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::Error {
        session: None,
        text: "could not open Bash".into(),
    });
    assert_eq!(
        app.view(TerminalType::Pi).unwrap().last_error.as_deref(),
        Some("could not open Bash")
    );
    assert!(app.view(TerminalType::Beads).is_none());
}

/// Switching modes: the render pointer moves immediately, and the Router is
/// the one told to do the lifecycle. The App never touches a session itself.
#[tokio::test]
async fn a_tab_is_addressed_to_the_router_not_to_a_session() {
    let (mut app, mut rx) = app_with(TerminalType::Beads);
    let tab = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyModifiers::NONE,
    );
    app.update(Msg::Term(Event::Key(tab)));

    assert_eq!(app.active, TerminalType::Pi, "the view moved");
    match rx.recv().await.unwrap() {
        UiCommand::SwitchMode { from, to } => {
            assert_eq!(from, TerminalType::Beads);
            assert_eq!(to, TerminalType::Pi);
        }
        other => panic!("expected SwitchMode, got {other:?}"),
    }
}

/// A submit is addressed to the mode it was typed into, and echoed there.
#[tokio::test]
async fn submitting_is_addressed_and_echoed() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    for c in "hi".chars() {
        app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ))));
    }
    app.update(Msg::Term(Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ))));

    match rx.recv().await.unwrap() {
        UiCommand::Submit { mode, text } => {
            assert_eq!(mode, TerminalType::Pi);
            assert_eq!(text, "hi");
        }
        other => panic!("expected Submit, got {other:?}"),
    }
    assert!(text_of(&app, TerminalType::Pi).contains("hi"));
}

/// The exit signal is a command on the same channel as every other one, not a
/// `drop` of that channel — so the App that sent it is still alive and still
/// holding the reader the exit drain needs. Asserting the round trip is what
/// pins that choice down; the alternative (dropping `App`) looks the same from
/// here and loses the tail of the answer.
#[tokio::test]
async fn asking_for_shutdown_puts_quit_on_the_command_channel() {
    let (mut app, mut rx) = app_with(TerminalType::Pi);
    app.request_shutdown().await;
    assert!(
        matches!(rx.recv().await.unwrap(), UiCommand::Quit),
        "the exit instruction reached the router"
    );
    // …and the App is still a working reader afterwards.
    app.update(Msg::SessionDown {
        session: pi_id(),
        reason: ExitReason::Shutdown,
    });
    assert!(text_of(&app, TerminalType::Pi).contains("ended"));
}

/// **A local echo does not steal the view's session identity.** It matters
/// because `view_mut` seals on adoption: if echoing used a placeholder id, a
/// submit into a live session would seal that session's open entry (and flip its
/// status) from the *user's* keystroke rather than from the session's death.
#[test]
fn a_local_echo_does_not_steal_the_views_session_identity() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    let before = {
        let v = app.view_mut(pi_id());
        v.push_delta(MessageKind::Answer, "streaming");
        v.status = SessionStatus::Running;
        (pi_id(), v.transcript.entries.len())
    };

    app.echo_local(TerminalType::Pi, "typed".into());

    let v = app.view(TerminalType::Pi).unwrap();
    assert_eq!(
        v.session, before.0,
        "the echo used the existing generation, not HARNESS_GENERATION"
    );
    assert_eq!(
        v.status,
        SessionStatus::Running,
        "an echo is not a lifecycle event"
    );
    assert_eq!(v.transcript.entries.len(), before.1 + 1);
    assert_eq!(v.transcript.entries[0].text, "streaming");
}
