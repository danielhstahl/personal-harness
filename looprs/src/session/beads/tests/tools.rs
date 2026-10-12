//! The typed `bd` tool surface, seen from the process boundary (ADR-0010 #2).
//!
//! These are the tests that make "gated with `--tools`" a checked claim instead of
//! a paragraph. Everything they assert is observable in the fake child's own log:
//! the argv the worker was spawned with, and the prompt it was then given. Both
//! matter and neither implies the other — a `-e`/`--tools` pair with a prompt that
//! still says `bd close <id>` would pass an argv-only test, and a prompt that names
//! the tools while the child was never given them would pass a prompt-only one.
//!
//! The fake child answers no `bd` verb by itself; that is the point of
//! `LOOPRS_PI_BIN` staying the seam it is (ADR-0010 "What this forbids" 5). The one
//! thing these tests cannot prove is that the extension's *schema* validates what it
//! claims to — that needs `pi`, and it is `spikes/bd_tools_cost.py`'s leg 3 that
//! pays a real model to check it.

use crate::services::bd_tools::{DEFAULT_WORKER_TOOLS, Source, Surface};

use super::*;

/// A loop whose worker gets an explicit tool surface, so the test does not depend
/// on what happens to be in the developer's environment.
fn loop_with_surface(
    fakes: &Fakes,
    surface: Surface,
) -> (
    BeadsLoop,
    mpsc::UnboundedReceiver<SessionEvent>,
    mpsc::UnboundedReceiver<BeadsCmd>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<BeadsCmd>();
    let cfg = SessionConfig {
        bd_tools: surface,
        ..fakes_cfg(fakes)
    };
    let id = SessionId::new(TerminalType::Beads, 0);
    (BeadsLoop::new(id, tx, ctl_tx, cfg), rx, ctl_rx)
}

/// Start one pass and hand back the single argv the fake child recorded.
async fn one_worker_argv(fakes: &Fakes, mut l: BeadsLoop) -> Vec<String> {
    timeout(NO_HANG, l.next())
        .await
        .expect("the loop hung instead of starting a worker");
    assert_eq!(fakes.pi_spawns(), 1, "one worker for one ticket");
    let argv = fakes
        .pi_spawn_args()
        .pop()
        .expect("the fake pi recorded how it was spawned");
    // `--mode rpc` comes first: everything else is what this ticket is about.
    assert_eq!((argv[0].as_str(), argv[1].as_str()), ("--mode", "rpc"));
    argv
}

/// The shipped default: the extension is loaded, every tool the worker needs is
/// named in `--tools`, and the prompt speaks the tools' names.
#[tokio::test]
async fn the_default_pass_loads_the_extension_and_names_every_tool() {
    let fakes = Fakes::new(
        "tools-default",
        PiFake::Started,
        BdFake::Ok,
        ONE_BEADED_BOARD,
    );
    let (l, _rx, _ctl) =
        loop_with_surface(&fakes, Surface::new(DEFAULT_WORKER_TOOLS, Source::Embedded));
    let argv = one_worker_argv(&fakes, l).await;

    let ext = argv
        .iter()
        .position(|a| a == "-e")
        .expect("the typed surface passes the extension to load");
    let tools = argv
        .iter()
        .position(|a| a == "--tools")
        .expect("and the tool allowlist that gates it");
    let path = std::path::Path::new(&argv[ext + 1]);
    assert!(
        path.is_file(),
        "{path:?} was handed to `pi -e` but is not a file"
    );
    assert_eq!(
        argv[tools + 1],
        DEFAULT_WORKER_TOOLS,
        "the allowlist is the whole selection, named in full: pi 1.0.0's `+name` delta form \
         declares nothing at all"
    );
    assert!(
        argv[tools + 1].contains("bash"),
        "the default keeps the shell — a pass that ships code needs `git`"
    );

    let prompt = fakes.pi_prompts().pop().expect("the worker was prompted");
    for tool in ["bd_show", "bd_close", "bd_remember"] {
        assert!(
            prompt.contains(tool),
            "the prompt never named {tool}: {prompt}"
        );
    }
    for shell in ["bd show <id>", "bd close <id>", "bd remember"] {
        assert!(
            !prompt.contains(shell),
            "the prompt still tells the worker to shell out ({shell}) while the pass has the \
             tool for it"
        );
    }
}

/// The escape hatch, proven as a command line and not as a promise: `off` spawns
/// the child exactly the way this loop always did, and the prompt goes back to the
/// shell cheat sheet with it.
#[tokio::test]
async fn turning_the_surface_off_spawns_the_child_the_way_it_was_always_spawned() {
    let fakes = Fakes::new("tools-off", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let (l, _rx, _ctl) = loop_with_surface(&fakes, Surface::off());
    let argv = one_worker_argv(&fakes, l).await;

    assert_eq!(
        argv,
        vec!["--mode".to_string(), "rpc".to_string()],
        "an off surface must add nothing at all to the argv — no extension, no allowlist"
    );
    let prompt = fakes.pi_prompts().pop().expect("the worker was prompted");
    assert!(prompt.contains("bd close <id>"), "{prompt}");
    assert!(
        !prompt.contains("bd_close"),
        "the prompt named a tool this pass was not given"
    );
}

/// What the prompt *does* say tracks the allowlist, verb by verb: give a pass
/// `bd_claim` and it hears about `bd_claim`; withhold `bd_remember` and it does not.
#[tokio::test]
async fn the_prompt_names_the_verbs_this_pass_was_given_and_no_others() {
    let fakes = Fakes::new("tools-list", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let (l, _rx, _ctl) = loop_with_surface(
        &fakes,
        Surface::new("read,bash,bd_show,bd_claim", Source::Embedded),
    );
    let argv = one_worker_argv(&fakes, l).await;
    assert_eq!(
        argv[argv.iter().position(|a| a == "--tools").unwrap() + 1],
        "read,bash,bd_show,bd_claim"
    );

    let prompt = fakes.pi_prompts().pop().unwrap();
    assert!(prompt.contains("bd_claim"), "{prompt}");
    assert!(prompt.contains("bd_show"), "{prompt}");
    assert!(
        !prompt.contains("bd_remember"),
        "the pass has no note tool, so the prompt must not order it to take notes: {prompt}"
    );
    assert!(
        !prompt.contains("bd_close"),
        "and no close tool, so it must not order a close either: {prompt}"
    );
}

/// A pass that could not be given the tools its prompt promises is refused before
/// anything is spawned. Paying for a worker that settles with the ticket still open
/// — and then tripping the re-work guard — is the expensive version of this failure.
#[tokio::test]
async fn an_extension_that_is_not_there_refuses_the_pass_by_name() {
    let fakes = Fakes::new(
        "tools-missing-ext",
        PiFake::Started,
        BdFake::Ok,
        ONE_BEADED_BOARD,
    );
    let (mut l, mut rx, _ctl) = loop_with_surface(
        &fakes,
        Surface::new(
            DEFAULT_WORKER_TOOLS,
            Source::Path(std::path::PathBuf::from("/no/such/bd_tools.ts")),
        ),
    );

    timeout(NO_HANG, l.next())
        .await
        .expect("refusing a pass is an answer, not a hang");
    assert_eq!(
        fakes.pi_spawns(),
        0,
        "refusing the pass means never spawning it"
    );
    let events = drain(&mut rx);
    let err = last_error(&events).expect("the refusal is said out loud, not swallowed");
    assert!(
        err.contains("LOOPRS_BD_TOOLS_EXT") && err.contains("/no/such/bd_tools.ts"),
        "the refusal has to name the knob and the path it looked at: {err}"
    );
    assert!(
        events.contains(&"step:await".to_string()),
        "and it has to leave the loop parked for a human, not spinning: {events:?}"
    );
}

/// The planner keeps its own gating. The typed surface is the *worker's* five
/// verbs, and `bd create` — the verb a planner lives on — is not among them, so a
/// planner pass still gets `read,bash` and nothing else.
#[tokio::test]
async fn the_planner_is_untouched_by_the_typed_surface() {
    let fakes = Fakes::new("tools-planner", PiFake::Started, BdFake::Ok, EMPTY_BOARD);
    let (mut l, _rx, _ctl) =
        loop_with_surface(&fakes, Surface::new(DEFAULT_WORKER_TOOLS, Source::Embedded));

    assert!(
        timeout(NO_HANG, l.launch_create_tickets("one ticket, please"))
            .await
            .is_ok(),
        "launching the planner hung"
    );
    let argv = fakes
        .pi_spawn_args()
        .pop()
        .expect("the planner child recorded its argv");
    assert_eq!(
        argv,
        vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--tools".to_string(),
            "read,bash".to_string()
        ],
        "the planner still gets read,bash and no extension"
    );
}

/// The other half of the seam: the fake `bd` answers the verbs the typed tools
/// actually run. `remember` is the one that arrived with this surface — the real
/// `bd` refuses an unknown verb with exit 2, so a fixture that fell through to that
/// branch would make `bd_remember` untestable at the process boundary, which is
/// exactly what ADR-0010's third obligation is about keeping working.
#[tokio::test]
async fn the_fake_bd_answers_every_verb_the_typed_surface_runs() {
    let fakes = Fakes::new("tools-verbs", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let bd = fakes.bd_bin();

    // The five, spelled the way `extensions/bd_tools.ts` builds them.
    for argv in [
        vec!["ready", "--limit", "5"],
        vec!["show", "looprs-26r"],
        vec!["update", "looprs-26r", "--claim"],
        vec!["close", "looprs-26r", "--reason", "done"],
    ] {
        let out = tokio::process::Command::new(bd)
            .args(&argv)
            .output()
            .await
            .expect("the fake bd runs");
        assert!(
            out.status.success(),
            "`bd {}` was refused by the fake: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = tokio::process::Command::new(bd)
        .args([
            "remember",
            "the fake seam answers remember",
            "--key",
            "typed-tools",
        ])
        .output()
        .await
        .expect("the fake bd runs");
    assert!(
        out.status.success(),
        "`bd remember` has to be answered, not an unsupported verb: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fakes.memories(),
        vec![(
            "typed-tools".to_string(),
            "the fake seam answers remember".to_string()
        )],
        "the note has to land somewhere a test can read back"
    );
    assert!(
        fakes
            .bd_log()
            .iter()
            .any(|l| l.contains("remember the fake seam answers remember --key typed-tools")),
        "and it has to be on the log the way every other verb is: {:?}",
        fakes.bd_log()
    );

    // A key is optional in `bd`'s own CLI, and in the tool.
    let out = tokio::process::Command::new(bd)
        .args(["remember", "a note with no key at all"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let memories = fakes.memories();
    assert_eq!(memories.len(), 2);
    assert_eq!(memories[1].1, "a note with no key at all");
    assert!(
        !memories[1].0.is_empty(),
        "a slug was synthesised, as `bd` does"
    );
}
