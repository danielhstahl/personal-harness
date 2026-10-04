#!/usr/bin/env bash
# The warning gate (looprs-6ol).
#
# One command that says whether this crate is in a state to merge:
#
#   1. `cargo fmt --check`   — formatted, or not
#   2. `cargo clippy -D warnings` — no lint left warning, and no blanket `allow`
#   3. `cargo test`          — everything green, with no network and no model calls
#
# The order is deliberate: fmt and clippy are seconds-long and explain themselves, so
# they run before the ~20s test suite rather than after it.
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

echo
echo "gate: clean (fmt, clippy -D warnings, tests)"
