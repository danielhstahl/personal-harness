#!/usr/bin/env bash
# What does syntect's load cost, and what does the pure-Rust regex backend cost
# against the C one?  (looprs-00u.16)
#
# Two axes, and each one needs a separate process for a different reason:
#
#   cold / warm   — the laziness question. syntect compiles each regex on first
#                  use and caches the compiled form inside the loaded
#                  SyntaxSet, so a process is cold once and warm forever. One
#                  run cannot print both halves.
#   fancy / onig  — the Cargo.toml question. `default-fancy` and `default-onig`
#                  are compile-time; one build carries one engine.
#
# So: the two `fancy` blocks run in **this tree as it stands**, and the two
# `onig` blocks run in a copy of it with exactly one line changed —
#
#   syntect = { …, features = ["default-fancy"] }   →   ["default-onig"]
#
# built into its own target directory, nothing else touched: same sources, same
# profile, same corpus, minutes apart.
#
# Why a copy rather than a cargo feature: a feature that swaps the regex engine
# is a door left open in the shipped manifest, and what is being decided here is
# whether that door should be open. Pricing an alternative does not require
# shipping it — and a `--features onig` that nothing in CI builds is a feature
# that rots, which is the lesson `[features] notify` was written down with.
#
# Three kinds of block:
#
#   LATENCY   the four cold/warm × fancy/onig runs, over the real corpus, with
#             the counting allocator (`CountingAlloc` in src/measure.rs).
#   RSS       the same three programs — nothing highlighted / load only / load +
#             warm — with **no corpus**, each under `/usr/bin/time -l`.
#             These exist because the counting allocator cannot see a C
#             allocation: oniguruma's compiled patterns live in malloc'd
#             memory that `CountingAlloc` never counts, so the Rust-heap
#             comparison alone would flatter `onig`. Maximum resident set size
#             counts both sides. No corpus in these runs on purpose: reading
#             66 MB of session files into Strings would set the RSS and the
#             regex state would be noise inside it.
#   PRICE     the build-side cost of each backend: shipped-graph crate count and
#             release artifact size.
#
# Usage:
#   spikes/regex_backend_cost.sh
#   ./scripts/capture.sh regex_backend_cost.sh highlight-load-cost \
#       bash spikes/regex_backend_cost.sh
#
# Knobs:
#   LOOPRS_MEASURE_CORPUS   the real `pi` session .jsonl files, required for
#                           the LATENCY blocks (the RSS blocks must NOT have it)
#   LOOPRS_MEASURE_TICKETS  how many session files to replay (default 12, as
#                           in every other run in src/measure.rs)
#   KEEP_COPY=1             leave the ~0.5 GB onig copy and its target dir behind
#   SKIP_ONIG=1             only the fancy blocks (the onig legs build
#                           oniguruma from C; nothing else is different)

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

echo "# regex-backend-cost — looprs-00u.16"
echo "# date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "# host: $(uname -sr) $(uname -m)"
cargo --version | sed 's/^/# /'
rustc --version | sed 's/^/# /'
echo "# corpus: ${LOOPRS_MEASURE_CORPUS:-<unset — LATENCY blocks will refuse to run>}"
echo "#"
echo "# The ONLY manifest difference between the fancy blocks and the onig blocks:"
grep '^syntect = ' Cargo.toml | sed 's/^/#   fancy (shipped): /'

CORPUS_ENV=()
[ -n "${LOOPRS_MEASURE_CORPUS:-}" ] && CORPUS_ENV=(LOOPRS_MEASURE_CORPUS="$LOOPRS_MEASURE_CORPUS")
[ -n "${LOOPRS_MEASURE_TICKETS:-}" ] && CORPUS_ENV+=("LOOPRS_MEASURE_TICKETS=$LOOPRS_MEASURE_TICKETS")

run_block() {
  local backend="$1" dir="$2" test="$3"
  echo
  echo "════════════════════════════════════════════════════════════════════════"
  echo " block: $backend · $test"
  echo "════════════════════════════════════════════════════════════════════════"
  ( cd "$dir" && env LOOPRS_MEASURE_BACKEND="$backend" "${CORPUS_ENV[@]}" \
      cargo test --release -- --ignored --nocapture --test-threads=1 "$test" ) 2>&1
  echo " [exit $?]"
}

# A no-corpus run under `/usr/bin/time -l`, reported as max RSS only.
#
# The test **binary** is run directly, not through `cargo test`, because
# `ru_maxrss` is the maximum over the whole waited-for process tree and cargo
# itself peaks around 87 MB — bigger than anything this measurement is trying
# to see. Run through cargo the answer is "cargo", every time, for every
# configuration. Run the binary directly and the three programs differ only by
# what they load.
test_bin() {
  ( cd "$1" && cargo test --release --no-run --message-format=json 2>/dev/null ) | python3 -c '
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        o = json.loads(line)
    except Exception:
        continue
    if o.get("reason") != "compiler-artifact" or not o.get("executable"):
        continue
    # `test: true` is the part that matters. `cargo test --no-run` reports both
    # the ordinary bin (target/release/looprs, which is the *app* — it dies on
    # "Device not configured" without a tty and reports a plausible RSS for
    # having done nothing) and the test harness built from the same sources.
    # The first pass of this script picked the app and printed three identical
    # RSS numbers a mile apart in meaning.
    if o.get("target", {}).get("kind") != ["bin"]:
        continue
    if not o.get("profile", {}).get("test"):
        continue
    print(o["executable"])
' | tail -1
}

rss_block() {
  local backend="$1" dir="$2" what="$3" test="$4"
  local bin rss
  bin="$(test_bin "$dir")"
  if [ -z "$bin" ]; then
    printf '  %-40s max RSS: (no test binary found)\n' "$backend · $what"
    return
  fi
  rss="$( env -u LOOPRS_MEASURE_CORPUS -u LOOPRS_MEASURE_TICKETS \
      LOOPRS_MEASURE_BACKEND="$backend" /usr/bin/time -l \
      "$bin" --include-ignored --exact --nocapture --test-threads=1 "$test" 2>&1 \
      | awk '/maximum resident set size/ {print $1}' | tail -1 )"
  printf '  %-40s max RSS: %s B\n' "$backend · $what" "${rss:-not reported}"
}

echo
echo "--- LATENCY (release, real corpus, counting allocator)"
echo "# every latency block is run twice. The absolutes in these numbers move by"
echo "# up to ~50% run to run on the same machine — the first process to touch"
echo "# the syntax dump pays the page cache for everyone after it — while the"
echo "# ratios hold. One sample would be quoted as a fact that its own next run"
echo "# contradicts; two samples is the spread, visible in the capture."
for sample in 1 2; do
  echo
  echo "############ sample $sample ############"
  run_block fancy "$here" a_cold_highlight_load_and_the_first_real_fence_measured
  run_block fancy "$here" a_warmed_highlighter_moves_the_load_off_the_draw_path
done

# ───────────────────────────── the alternative build (onig) ─────────────────────────────
copy="${TMPDIR:-/tmp}/looprs-onig"
if [ "${SKIP_ONIG:-0}" != "1" ]; then
  rm -rf "$copy"
  mkdir -p "$copy"
  # The crate is self-contained in this directory; the surrounding repo is not
  # needed, and `.git` would only make the copy slow and the tree look dirty.
  rsync -a --exclude target --exclude '.git' --exclude '.beads*' "$here"/ "$copy"/

  # The one line. If this does not match exactly, stop: a silent no-op here
  # would print two identical blocks and call the difference the backend.
  python3 - "$copy/Cargo.toml" <<'PY'
import re, sys
p = sys.argv[1]
src = open(p).read()
new, n = re.subn(r'(syntect = \{[^\n]*?)default-fancy', r'\1default-onig', src)
if n != 1:
    sys.exit(f"expected exactly one `default-fancy` in {p}, replaced {n} — refusing to report")
open(p, "w").write(new)
PY
  rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "regex_backend_cost.sh: could not rewrite the syntect line (rc=$rc)" >&2
    exit "$rc"
  fi
  echo "# and the copy's line, as built:"
  grep '^syntect = ' "$copy/Cargo.toml" | sed 's/^/#   onig (alternative): /'
  # Refresh the copy's lockfile (fancy-regex out, onig in) before anything runs.
  ( cd "$copy" && cargo tree -e normal --prefix none >/dev/null 2>&1 )
  for sample in 1 2; do
    echo
    echo "############ sample $sample ############"
    run_block onig "$copy" a_cold_highlight_load_and_the_first_real_fence_measured
    run_block onig "$copy" a_warmed_highlighter_moves_the_load_off_the_draw_path
  done
fi

echo
echo "--- RSS (release, NO corpus: the corpus's Strings would be the whole number)"
echo "  the three programs differ only in how far into the highlighter they get:"
echo "  · nothing highlighted = the baseline the other two are read against"
echo "  · load only          = + the syntax set and the theme"
echo "  · load + warm        = + the compiled regexes of rust/bash/python/sh"
rss_block fancy "$here" "nothing highlighted (baseline)" "utils::md::tests::a_small_heading_and_a_link_are_the_palettes_blue"
rss_block fancy "$here" "load only" "measure::a_cold_highlight_load_and_the_first_real_fence_measured"
rss_block fancy "$here" "load + warm(4 languages)" "measure::a_warmed_highlighter_moves_the_load_off_the_draw_path"
if [ "${SKIP_ONIG:-0}" != "1" ]; then
  rss_block onig "$copy" "nothing highlighted (baseline)" "utils::md::tests::a_small_heading_and_a_link_are_the_palettes_blue"
  rss_block onig "$copy" "load only" "measure::a_cold_highlight_load_and_the_first_real_fence_measured"
  rss_block onig "$copy" "load + warm(4 languages)" "measure::a_warmed_highlighter_moves_the_load_off_the_draw_path"
fi

# The build-side price of each backend: what the manifest choice adds to the
# shipped graph and to the artifact. Same queries `spikes/dep_cost.sh` uses, so
# the two logs can be read against each other.
ship_price() {
  local label="$1" dir="$2"
  local crates bin bytes="(no binary)"
  crates="$( cd "$dir" && cargo tree -e normal --prefix none 2>/dev/null | awk 'NF {print $1}' | sort -u | wc -l | tr -d ' ' )"
  bin="$dir/target/release/looprs"
  [ -f "$bin" ] && bytes="$(stat -f%z "$bin" 2>/dev/null || stat -c%s "$bin")"
  printf '  %-6s shipped-graph crates: %s · release binary: %s B\n' "$label" "$crates" "$bytes"
}

echo
echo "--- PRICE (the build-side cost of the manifest choice)"
ship_price fancy "$here"
[ "${SKIP_ONIG:-0}" != "1" ] && ship_price onig "$copy"

echo
if [ "${KEEP_COPY:-0}" = "1" ]; then
  echo "# the onig copy was left at $copy (KEEP_COPY=1)"
else
  rm -rf "$copy"
  echo "# the onig copy has been removed; KEEP_COPY=1 keeps it"
fi
echo "# note: every onig line above came from that copy — this tree with the single"
echo "#       manifest line changed, built into its own target dir. Sources, corpus,"
echo "#       profile and machine are identical to the fancy lines."
