#!/usr/bin/env bash
# The warning gate (looprs-6ol).
#
# One command that says whether this crate is in a state to merge:
#
#   1. `cargo fmt --check`   — formatted, or not
#   2. `cargo clippy -D warnings` — no lint left warning, and no blanket `allow`
#   3. `cargo test`          — everything green, with no network and no model calls
#   4. `clippy` + `test` again over **the other build** — `--features notify`. The
#         default binary is built *without* it (see `[features]` in `Cargo.toml`,
#         looprs-00u.15), and a feature half that only ever gets compiled by the
#         person who last touched it is a feature half that rots. Running both is
#         what makes "the feature does not change the shape of the seam" a checked
#         claim rather than an intention: the same 840-odd tests pass in both
#         configurations, and the ones that differ are the ones that need the wire.
#   5. `scripts/dead_audit.py --gate` — every `#[allow(dead_code)]` still covers
#         dead code
#   6. `scripts/dep_audit.py --gate` — every `[dependencies]` entry is named in
#         `src/`, or answered at its own line in the manifest. This is the
#         counterpart of step 4 for the other half of the tree, and the reason the
#         `unused_dependencies = "allow"` blanket could be deleted rather than
#         argued about.
#   7. `scripts/docs_check.sh` — no dead link or orphan page, every `LOOPRS_*` the
#         code reads is documented, no two defaults disagree, the keymap tables
#         still match `CHORD_TABLE`, the wire protocol page still matches
#         `WIRE_INVENTORY` (and no `#[allow(dead_code)]` in `src/wire.rs` is
#         missing the reason it carries), and every spike check count written into
#         prose matches a committed capture in `spikes/results/` (looprs-00u.23:
#         three pages said "149 checks" while the tree printed 171, and the 171
#         run was never captured, so nothing in the repo could contradict the page),
#         and every test name a page quotes in backticks answers to a real `fn` of
#         that name in the tree (looprs-00u.25: a page whose point is that the test
#         list is a specification quoted a name that could not be grepped for),
#         and every `*.py`/`*.sh` under `spikes/` has a row in `spikes/README.md`
#         while no row points at a driver that is gone (looprs-00u.26: two pages
#         call that file *the* index of what each spike measures, and it was
#         missing three drivers — two of them linked from those pages by name, so
#         the index said a cited file did not exist), and no table row that says it
#         *includes* another row quotes a smaller line count than the row it
#         includes, nor a difference that does not equal the subtraction it claims
#         (looprs-00u.27: the table whose whole job is to be the measurement —
#         ADR-0008's corpus table — said 7,071 lines under `docs/` and 7,056
#         *including* `spikes/`, a superset fifteen lines smaller than its own
#         part, typed from two different moments and contradicted by nothing)
#
# The order is deliberate: fmt and clippy are seconds-long and explain themselves, so
# they run before the ~20s test suite rather than after it. Step 7 runs last for the
# same reason in reverse: it is the fastest step in the gate (<1s, no cargo, no
# network) and its failures are prose failures, which are the ones you want to see
# after the code has stopped shouting — but it is not optional, because a docs check
# that only runs when someone remembers is a docs check that has already rotpped.
#
# Step 4 is not covered by step 2. `-D warnings` stops a *new* unjustified allow
# from being added; it says nothing about the ones already in the tree, and an
# allow whose stated reason has gone stale keeps warning nobody while reading as
# coverage (looprs-2nd: nine allows promised pdl.9 as their consumer, and pdl.9
# landed reading something else). The audit asks the compiler — through the
# allow, with `--force-warn=dead_code` — whether the guarded item is still dead.
# A redundant one fails here. Whether a *still-dead* item's reason is true is a
# question about prose, so that part prints rather than fails; run without
# `--gate` for the full list.
#
# CI runs exactly this file (.github/workflows/looprs-gate.yml at the repo root), so
# "passes locally" and "passes in CI" are the same statement. `cargo test --test
# warning_gate -- --ignored` runs steps 1 and 2 from inside cargo, for when the
# tests are already what you wanted.
#
# KNOWN NOISE, NOT OURS: the build prints
#   "warning: the following packages contain code that will be rejected by a future
#    version of Rust: nix v0.28.0"
# That is inside `portable-pty`'s transitive dependency, it is a `note` rather than
# a failure, and it is tracked in docs/adr/0001-bash-terminal-state-pty.md
# ("Costs / follow-ups"). Do not "fix" it by silencing the whole report: the point
# of this gate is that the *rest* is clean, which is only meaningful while the one
# exception is named.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test

echo "==> clippy + test, the other build (--features notify)"
cargo clippy --all-targets --features notify -- -D warnings
cargo test --features notify

echo "==> dead-code allow audit"
python3 "$(dirname "${BASH_SOURCE[0]}")/dead_audit.py" --gate

echo "==> dependency-surface audit"
python3 "$(dirname "${BASH_SOURCE[0]}")/dep_audit.py" --gate

echo "==> docs rot gate (links, knob coverage, keymap coverage, wire protocol coverage, measurement claims)"
"$(dirname "${BASH_SOURCE[0]}")/docs_check.sh"

echo
echo "gate: clean (fmt, clippy -D warnings in both feature configurations, tests in both,"
echo "        dead-code audit, dependency audit, docs)"
