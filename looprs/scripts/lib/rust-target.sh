# shellcheck shell=bash
# Where cargo puts its build artifacts, decided once and shared by every script that
# runs cargo (check.sh, coverage.sh, benchmark.sh). Sourced, not executed.
#
# The workspace's own `target/` is the right answer on a real runner: it is what makes
# `Swatinem/rust-cache` and `cargo toolchain install --profile minimal` pay off, and
# nothing else is proposed on GitHub Actions.
#
# It is the wrong answer when the workspace is a *mounted* filesystem. Observed while
# running `.github/workflows/quality.yml` through the act runner: rustc died of
# `SIGBUS: access to undefined memory` compiling `shell-words` into
# `<WORKSPACE>/looprs/target/release/...` — a memory-mapped write landing on a
# filesystem that cannot carry it (Docker Desktop's bind mount, arm64 linux). The
# identical crash appears in this container when building into the mounted checkout
# with no `CARGO_TARGET_DIR`, and building into a container-local path instead makes it
# go away. A timer that measures a build which cannot finish measures nothing, so the
# choice of directory is part of the measurement's honesty, not an optimization.
#
# Sets CARGO_TARGET_DIR when neither the caller nor the environment has picked one, and
# only under a runner that mounts the workspace (`ACT=true`, which act sets in every
# container it starts). The notice goes to stderr: these scripts' stdout carries
# metrics, and a build directory they chose on someone's behalf is a fact about the
# environment, not a measurement.

pick_cargo_target_dir() {
  [ -n "${CARGO_TARGET_DIR:-}" ] && return 0
  if [ "${ACT:-}" = "true" ]; then
    CARGO_TARGET_DIR="${TMPDIR:-/tmp}/looprs-target-${PWD##*/}"
    export CARGO_TARGET_DIR
    echo "build dir: CARGO_TARGET_DIR=$CARGO_TARGET_DIR (the workspace is a mounted filesystem here; rustc faults writing into it)" >&2
  fi
}
