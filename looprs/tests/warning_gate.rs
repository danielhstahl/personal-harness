//! The warning gate, visible from `cargo test` (looprs-6ol).
//!
//! Three files describe this gate, and each is the entry point for somebody:
//!
//! * `./scripts/check.sh` — everything: fmt, clippy, tests. What a human runs and
//!   what CI runs (`.github/workflows/looprs-gate.yml` at the repo root).
//! * this file — the same two *static* checks, reachable from cargo, for the case
//!   where "is the lint clean" is the only question you came here to ask.
//! * `Cargo.toml` `[lints]` — so `cargo clippy` denies by default rather than
//!   relying on everyone remembering `-D warnings`.
//!
//! `#[ignore]`d by default, deliberately, for two reasons that are both about the
//! word "nested": this runs `cargo` from inside `cargo test`.
//!
//! 1. **Lock.** Cargo takes a lock on its build directory. This test sets
//!    `CARGO_TARGET_DIR` to its own directory so the inner clippy cannot queue
//!    behind the outer test run — but that also means it cannot reuse the outer
//!    build, so it compiles the world from scratch the first time.
//! 2. **Cost.** `cargo test` is the fast inner loop (~20s here). A gate that
//!    recompiles everything on every run gets skipped, and a skipped gate is worse
//!    than no gate because it reads as coverage.
//!
//! So: `cargo test` for the behaviour, `cargo test -- --ignored` (or
//! `./scripts/check.sh`) for the gate.

use std::path::Path;
use std::process::Command;

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Run one cargo subcommand against this package, in a target dir of its own.
///
/// A separate `CARGO_TARGET_DIR` is the whole trick to nesting cargo: the outer
/// `cargo test` holds the lock on `target/`, so an inner clippy aimed at the same
/// directory would block until the outer command finished — which is never, because
/// the outer command is waiting for this test.
fn cargo(args: &[&str]) {
    let gate_target = manifest().join("target/warning-gate");
    let status = Command::new(env!("CARGO"))
        .current_dir(manifest())
        .env("CARGO_TARGET_DIR", &gate_target)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("could not run `cargo {}`: {e}", args.join(" ")));
    assert!(
        status.success(),
        "`cargo {}` failed with {status}; run ./scripts/check.sh for the full output",
        args.join(" ")
    );
}

#[test]
#[ignore = "nested cargo (own target dir, cold build); use ./scripts/check.sh"]
fn formatted() {
    cargo(&["fmt", "--check"]);
}

#[test]
#[ignore = "nested cargo (own target dir, cold build); use ./scripts/check.sh"]
fn lints_clean() {
    cargo(&["clippy", "--all-targets", "--", "-D", "warnings"]);
}
