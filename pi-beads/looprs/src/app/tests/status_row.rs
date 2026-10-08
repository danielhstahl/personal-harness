//! The status row (looprs-guh)
//!
//! `App`'s half of the row: gather the mirrors, no more. The layout, the
//! truncation and the priority ladder are tested in `components::status`; what
//! is tested here is that the right thing reaches the row at all — which is the
//! half that can go wrong while every individual piece still looks correct.

use super::*;

fn row(app: &App, w: u16) -> String {
    app.status_line(w).to_string()
}

/// ADR-0002's whole reason for this row, end to end through the envelope: the
/// beads loop is running, the user is looking at Pi, and the row says so.
#[test]
fn the_row_names_a_busy_mode_you_are_not_looking_at() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::BeadStep {
        session: beads_id(),
        step: BeadStep::WorkTickets,
    });
    app.update(Msg::ActiveBead {
        session: beads_id(),
        bead: Some(ActiveBead {
            id: "looprs-guh".into(),
            title: "Status row is allocated but empty".into(),
        }),
    });
    app.update(Msg::SessionStatus {
        session: beads_id(),
        status: SessionStatus::Running,
    });
    let txt = row(&app, 100);
    assert!(txt.contains("Pi"), "{txt:?}");
    assert!(txt.contains("bg: Beeds working"), "{txt:?}");
    assert!(txt.contains("looprs-guh"), "{txt:?}");
    assert!(
        !txt.contains("warm: Beeds"),
        "a running mode is not a warm one: {txt:?}"
    );
}

/// The same mirrors are per-session, not global (ADR-0002 Q2 / Q5). A beads
/// edge must not make the Pi pane look busy, and vice versa.
#[test]
fn a_status_edge_lands_in_its_own_view_and_not_in_the_active_one() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::SessionStatus {
        session: beads_id(),
        status: SessionStatus::Running,
    });
    assert_eq!(
        app.view(TerminalType::Beeds).unwrap().status,
        SessionStatus::Running
    );
    assert_eq!(
        app.view(TerminalType::Pi).map(|v| v.status),
        None,
        "a beads edge must not have created or touched the Pi view's liveness"
    );

    // Now the Pi view exists, and it is idle while beads runs.
    app.update(Msg::SessionStatus {
        session: pi_id(),
        status: SessionStatus::Idle,
    });
    let txt = row(&app, 60);
    assert!(txt.contains("Pi") && txt.contains("idle"), "{txt:?}");
    assert_eq!(
        app.view(TerminalType::Pi).unwrap().status,
        SessionStatus::Idle
    );
    assert_eq!(
        app.view(TerminalType::Beeds).unwrap().status,
        SessionStatus::Running,
        "and beads is still running, unchanged by the Pi edge"
    );
}

/// A warm child (alive, idle, resident) is the cost ADR-0002 takes on for
/// instant mode switches. It shows up on the row, and only for modes that are
/// actually alive.
#[test]
fn warm_children_are_named_only_for_modes_with_a_live_child() {
    let (mut app, _rx) = app_with(TerminalType::Beeds);
    assert!(
        !row(&app, 100).contains("warm"),
        "nothing is alive yet: {:?}",
        row(&app, 100)
    );
    app.update(Msg::SessionStatus {
        session: pi_id(),
        status: SessionStatus::Idle,
    });
    assert!(row(&app, 100).contains("warm: Pi"), "{}", row(&app, 100));

    // Dead is not warm: the child is gone, and claiming otherwise would hide
    // the one thing the user would want to know.
    app.update(Msg::SessionStatus {
        session: pi_id(),
        status: SessionStatus::Dead,
    });
    assert!(
        !row(&app, 100).contains("warm"),
        "a dead child is not a warm one: {}",
        row(&app, 100)
    );
}

/// The active mode is never listed as background or warm. The row answers
/// "what is *this* pane doing", and the other lists are about the other panes.
#[test]
fn the_active_mode_is_never_listed_against_itself() {
    let (mut app, _rx) = app_with(TerminalType::Pi);
    app.update(Msg::SessionStatus {
        session: pi_id(),
        status: SessionStatus::Idle,
    });
    let txt = row(&app, 100);
    assert!(!txt.contains("bg: Pi"), "{txt:?}");
    assert!(!txt.contains("warm: Pi"), "{txt:?}");
}

/// The row is a pure function of state, and the state includes *when* it was
/// sampled. Advancing the app's clock advances the age the row shows; nothing
/// in the render path reads a clock of its own.
#[test]
fn the_run_age_comes_from_the_app_clock_not_from_a_read_of_the_wall() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let t0 = Instant::now();
    app.on_tick(t0);
    app.update(Msg::SessionStatus {
        session: bash_id(),
        status: SessionStatus::Running,
    });
    assert!(row(&app, 100).contains("0s"), "{}", row(&app, 100));

    app.on_tick(t0 + Duration::from_secs(45));
    let txt = row(&app, 100);
    assert!(txt.contains("45s"), "{txt:?}");

    app.on_tick(t0 + Duration::from_secs(3600));
    assert!(row(&app, 100).contains("1h00m"), "{}", row(&app, 100));
}

/// The age is *this run's*, not this session's: a second run restarts it.
#[test]
fn a_second_run_starts_a_new_clock() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let t0 = Instant::now();
    app.on_tick(t0);
    app.update(Msg::SessionStatus {
        session: bash_id(),
        status: SessionStatus::Running,
    });
    app.on_tick(t0 + Duration::from_secs(100));
    assert!(row(&app, 100).contains("1m40s"), "{}", row(&app, 100));

    app.update(Msg::SessionStatus {
        session: bash_id(),
        status: SessionStatus::Idle,
    });
    // The idle row has no age at all...
    assert!(
        !row(&app, 100).contains("1m40s"),
        "an idle session must not keep showing the last run's age: {}",
        row(&app, 100)
    );
    // ...and the next run starts from zero, not from the old one.
    app.update(Msg::SessionStatus {
        session: bash_id(),
        status: SessionStatus::Running,
    });
    app.on_tick(t0 + Duration::from_secs(105));
    let txt = row(&app, 100);
    assert!(txt.contains("5s"), "{txt:?}");
}

/// A new generation of a mode is a different process. Its row must not carry
/// the previous one's age or its claim.
#[test]
fn a_new_generation_carries_no_age_and_no_claim() {
    let (mut app, _rx) = app_with(TerminalType::Beeds);
    let t0 = Instant::now();
    app.on_tick(t0);
    app.update(Msg::ActiveBead {
        session: beads_id(),
        bead: Some(ActiveBead {
            id: "looprs-old".into(),
            title: "old".into(),
        }),
    });
    app.update(Msg::SessionStatus {
        session: beads_id(),
        status: SessionStatus::Running,
    });
    app.on_tick(t0 + Duration::from_secs(60));
    assert!(row(&app, 100).contains("looprs-old"));

    let newer = SessionId::new(TerminalType::Beeds, 2);
    app.view_mut(newer);
    let txt = row(&app, 100);
    assert!(!txt.contains("looprs-old"), "{txt:?}");
    assert!(!txt.contains("1m00s"), "stale run age survived: {txt:?}");
}

/// `on_tick` decides when the row repaints. With nothing busy it must not mark
/// the app dirty, or an idle app redraws at 60fps for no reason at all; with
/// something busy it repaints at the row's own pace.
#[test]
fn an_idle_app_is_not_repainted_by_the_row_and_a_busy_one_is_repainted_at_eight_fps() {
    let (mut app, _rx) = app_with(TerminalType::Bash);
    let t0 = Instant::now();

    app.dirty = false;
    app.on_tick(t0 + Duration::from_millis(16));
    assert!(
        !app.dirty,
        "nothing is busy; the row must not ask for a frame"
    );

    app.update(Msg::SessionStatus {
        session: bash_id(),
        status: SessionStatus::Running,
    });
    app.dirty = false;
    // Under the animation interval: nothing to show yet that changed.
    app.on_tick(t0 + Duration::from_millis(60));
    assert!(!app.dirty, "still inside the same animation frame");
    // Past it: the spinner moved, so the row needs painting.
    app.on_tick(t0 + Duration::from_millis(200));
    assert!(
        app.dirty,
        "the spinner advanced and the row was not repainted"
    );
}

/// A mode with no view at all still gets a row. `App` is created before any
/// session exists, and the very first frame is exactly the frame where "what is
/// happening?" has no answer but the honest one.
#[test]
fn a_fresh_app_with_no_sessions_at_all_renders_a_sane_row() {
    let (tx, _rx) = mpsc::channel::<UiCommand>(1);
    let app = App::new(tx, InputState::new(), TerminalType::Beeds, 40, 24);
    let txt = row(&app, 40);
    assert!(txt.contains("Beeds"), "{txt:?}");
    assert!(txt.contains("not started"), "{txt:?}");
    assert!(!txt.contains('{') && !txt.contains('}'), "{txt:?}");
}

/// The last error must not need scrolling to see — that is the ticket. It also
/// has to stay put when the mode is switched away from and back.
#[test]
fn the_last_error_shows_in_its_own_mode_and_does_not_follow_the_user_around() {
    let (mut app, _rx) = app_with(TerminalType::Beeds);
    app.update(Msg::Error {
        session: Some(beads_id()),
        text: "bd: database is locked".into(),
    });
    let txt = row(&app, 100);
    assert!(txt.contains("✗"), "{txt:?}");
    assert!(txt.contains("database is locked"), "{txt:?}");

    // The Pi pane's row is not showing the beads pane's failure.
    let other = App {
        active: TerminalType::Pi,
        ..app
    };
    let txt = row(&other, 100);
    assert!(!txt.contains("database is locked"), "{txt:?}");
}
