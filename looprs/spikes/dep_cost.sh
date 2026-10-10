#!/usr/bin/env bash
# What does the dependency and feature surface cost?  (looprs-00u.15)
#
# One clean `--release` build per configuration, each in its own target directory,
# with the four numbers the ticket asked for read off it:
#
#   * clean build wall time
#   * crates compiled, and crates in the *shipped* (non-dev, non-build) graph
#   * whether the feature under question is in that shipped graph at all
#   * final binary size, with and without its symbol table
#
# Usage:
#   spikes/dep_cost.sh <label> [cargo flags...]
#
#     spikes/dep_cost.sh notify-on
#     spikes/dep_cost.sh notify-off --no-default-features
#
# `spikes/dep_cost_pair.sh` runs a pair and tees the blocks into spikes/results/.
#
# Nothing here is incremental on purpose: an incremental build answers "what
# changed since the last build", and the question here is what a cold `cargo build`
# costs a person who has never built this crate. The target directory is under
# ${TMPDIR} and removed on exit unless KEEP_TARGET=1, because a run leaves ~0.5 GB
# behind.
#
# The graph questions are answered from `cargo tree -e normal` rather than from the
# build log: that edge set is the bin's own dependencies, which is what the shipped
# binary carries, while `cargo test`'s graph additionally carries the examples' and
# the test harness's. A feature that shows up under `-e normal` is in the binary;
# one that only shows up without that filter is a dev-time cost and nothing more.

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
label="${1:?usage: dep_cost.sh <label> [cargo flags...]}"
shift

dir="${TMPDIR:-/tmp}/looprs-dep-$label"
build_log="${TMPDIR:-/tmp}/dep-$label-build.log"
rm -rf "$dir"
mkdir -p "$dir"
cd "$here"
export CARGO_TARGET_DIR="$dir"

echo "# dep-cost / label=$label"
echo "# date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "# host: $(uname -sr) $(uname -m)"
echo "$*" | sed 's/^/# flags: /'
echo "# command: cargo build --release $*"
cargo --version | sed 's/^/# /'
rustc --version | sed 's/^/# /'

# --- the resolved graph, before anything is compiled ------------------------
shipped_crates() {
  cargo tree -e normal --prefix none "$@" 2>/dev/null | awk 'NF {print $1}' | sort -u | wc -l | tr -d ' '
}
echo "crates_in_shipped_graph: $(shipped_crates "$@")"
echo "crossterm_shipped_features: $(cargo tree -e normal -f '{p} {f}' -i crossterm "$@" 2>/dev/null | sed -n '1s/^crossterm v[0-9.9]* *//p' | sed 's/  *$//')"
echo "base64_in_shipped_graph: $(cargo tree -e normal -i base64 >/dev/null 2>&1 && echo yes || echo no)"
echo "reqwest_in_shipped_graph: $(cargo tree -e normal -i reqwest "$@" >/dev/null 2>&1 && echo yes || echo no)"
# base64 is looked up by string rather than with `-i` because two majors of it are
# in this lockfile and `cargo tree -i base64` refuses the ambiguous name.
echo "base64_0_22_in_shipped_graph: $(cargo tree -e normal --prefix none "$@" 2>/dev/null | grep -qE '(^| )base64 v0\.22' && echo yes || echo no)"

# --- the build itself ------------------------------------------------------
start=$(date +%s)
cargo build --release --timings "$@" >"$build_log" 2>&1
rc=$?
end=$(date +%s)

compiled=$(grep -c '^ *Compiling' "$build_log" || true)
echo "clean_release_build_wall_seconds: $((end - start))"
echo "crates_compiled: $compiled"
echo "build_exit: $rc"
grep -E 'Finished|error' "$build_log" | head -3 | sed 's/^/    /'

# --- the artifact ----------------------------------------------------------
bin="$dir/release/looprs"
if [[ -f "$bin" ]]; then
  bytes=$(stat -f%z "$bin" 2>/dev/null || stat -c%s "$bin")
  echo "binary_bytes: $bytes"
  cp "$bin" "$bin.stripped"
  strip -u -r "$bin.stripped" 2>/dev/null || strip "$bin.stripped" 2>/dev/null || true
  echo "binary_bytes_after_strip: $(stat -f%z "$bin.stripped" 2>/dev/null || stat -c%s "$bin.stripped")"
  echo "binary_segments:"
  size -m "$bin" 2>/dev/null | sed 's/^/    /'
else
  echo "binary_bytes: (no binary produced)"
fi

# --- the timing report -----------------------------------------------------
timing="$(ls -t "$dir"/cargo-timings/cargo-timing-*.html 2>/dev/null | head -1)"
if [[ -n "$timing" ]]; then
  cp "$timing" "${TMPDIR:-/tmp}/dep-$label-timing.html"
  echo "timing_html: ${TMPDIR:-/tmp}/dep-$label-timing.html ($(stat -f%z "${TMPDIR:-/tmp}/dep-$label-timing.html" 2>/dev/null || stat -c%s "${TMPDIR:-/tmp}/dep-$label-timing.html") bytes)"
fi

if [[ "${KEEP_TARGET:-0}" != "1" ]]; then
  rm -rf "$dir"
fi

exit "$rc"
