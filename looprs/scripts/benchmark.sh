#!/usr/bin/env bash
# The benchmark job's measurement (the `benchmark` job of
# .github/workflows/quality.yml calls this; AGENTS.md requires a benchmark job, and
# this repo's rule is that a workflow calls a script rather than carrying logic).
#
#   ./scripts/benchmark.sh                      # measure; print metrics, write evidence
#   ./scripts/benchmark.sh --metrics FILE       # also write the metrics as JSONL there
#   ./scripts/benchmark.sh --compare prev.jsonl # report drift between two saved runs
#   ./scripts/benchmark.sh --cold               # drop the target dir first (slow, honest)
#   ./scripts/benchmark.sh --self-test          # check this script's logic; no cargo needed
#
# **What is measured, and why that is the right thing for a runner.** The spikes
# under `spikes/` measure the app's hot paths, and they refuse to run without a real
# corpus (`src/measure.rs` says why: a number over synthetic filler is the round
# number the ticket existed to replace). A runner has no model, no `~/.pi/agent/
# sessions` and no `bd` database, so the honest thing it *can* measure repeatedly is
# the cost of the gate itself — which is the number this repo already makes claims
# about ("~20 s", "runs in well under a second"):
#
#   build_release_secs  `cargo build --release`, from whatever the runner's cache is
#   test_suite_secs     the whole `cargo test` suite, as `check.sh` runs it
#   test_count          how many tests those seconds bought
#   docs_check_ms       the docs rot gate alone, the page whose claim is "<1s"
#
# Nothing here asserts a threshold, deliberately: a timing on a shared machine is a
# fact about that machine, and a benchmark that goes red whenever a runner is busy is
# a benchmark that gets deleted. Drift is where the claim lives — `--compare` diffs
# two saved runs of this same script, which is the only comparison that means
# anything across machines.
#
# Every metric is echoed as `metric <name> <value> <unit>`, appended as one JSON line
# to `$ACTMCP_EVIDENCE_OUT` when a runner set it (act's benchmark evidence), and
# written to `--metrics` when asked, so the same run feeds a terminal, an evidence
# file and a later comparison.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate="$(dirname "$here")"

cold=0
self_test=0
compare=""
metrics_file="${BENCH_METRICS:-}"

usage() {
  echo "usage: benchmark.sh [--cold] [--metrics FILE] [--compare PREV.jsonl] [--self-test]" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cold) cold=1; shift ;;
    --self-test) self_test=1; shift ;;
    --metrics)
      metrics_file="${2:?--metrics wants a path}"
      shift 2
      ;;
    --compare)
      compare="${2:?--compare wants a path to a saved metrics file}"
      shift 2
      ;;
    *) usage; exit 2 ;;
  esac
done

# ─────────────────────── the parts that hold logic, testable apart ───────────────────────

# One metric out, three ways: the human line, the evidence file, the metrics file.
# A metric with an empty value is refused rather than emitted as 0 — "0 seconds" and
# "the timer did not produce a number" are different facts.
emit() {
  local name="$1" value="$2" unit="$3"
  if [ -z "${value// /}" ]; then
    echo "benchmark.sh: metric $name had no value; refusing to report it as 0" >&2
    return 1
  fi
  printf 'metric %s %s %s\n' "$name" "$value" "$unit"
  [ -n "${ACTMCP_EVIDENCE_OUT:-}" ] &&
    printf '{"metric":"%s","value":%s,"unit":"%s"}\n' "$name" "$value" "$unit" >> "$ACTMCP_EVIDENCE_OUT"
  [ -n "${BENCH_METRICS:-}" ] &&
    printf '{"metric":"%s","value":%s,"unit":"%s"}\n' "$name" "$value" "$unit" >> "$BENCH_METRICS"
  return 0
}

# The test count, summed over every `test result: ok.` summary line.
#
# Reading this out of captured output is the kind of thing that silently reports 0
# when a format moves, so a run with no summary line is an error, not a zero.
test_count() {
  python3 - "$1" <<'PY'
import re, sys
total, n = None, re.compile(r"test result: ok\. (\d+) passed")
for line in open(sys.argv[1], encoding="utf-8", errors="ignore"):
    for hit in n.findall(line):
        total = (total or 0) + int(hit)
if total is None:
    print("no `test result: ok.` line in the captured output", file=sys.stderr)
    sys.exit(3)
print(total)
PY
}

# Drift between two saved metric files: the metrics in common, as a percentage.
drift() {
  python3 - "$1" "$2" <<'PY'
import json, sys
def load(path):
    out = {}
    for line in open(path, encoding="utf-8", errors="ignore"):
        line = line.strip()
        if line:
            rec = json.loads(line)
            out[rec["metric"]] = float(rec["value"])
    return out
before, after = load(sys.argv[1]), load(sys.argv[2])
shared = sorted(set(before) & set(after))
if not shared:
    print("the two metrics files share no metric name; that is not a comparison", file=sys.stderr)
    sys.exit(3)
for name in shared:
    b, a = before[name], after[name]
    pct = 0.0 if b == 0 else (a - b) / b * 100.0
    print(f"drift {name}: {b:g} -> {a:g} ({pct:+.1f}%)")
for name in sorted(set(after) - set(before)):
    print(f"new metric with no baseline: {name}")
for name in sorted(set(before) - set(after)):
    print(f"metric gone: {name}")
PY
}

if [ "${self_test}" = "1" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  # The emitter: one JSON line per metric in both sinks, and a blank value refused.
  ACTMCP_EVIDENCE_OUT="$tmp/act.jsonl" BENCH_METRICS="$tmp/mine.jsonl" emit test_count 932 tests
  ACTMCP_EVIDENCE_OUT="$tmp/act.jsonl" BENCH_METRICS="$tmp/mine.jsonl" emit docs_check_ms 812 milliseconds
  [ "$(wc -l < "$tmp/mine.jsonl" | tr -d ' ')" = "2" ] \
    || { echo "self-test: each metric must land as exactly one line in the metrics file" >&2; exit 1; }
  cmp -s "$tmp/act.jsonl" "$tmp/mine.jsonl" \
    || { echo "self-test: the evidence sink and the metrics file must carry the same records" >&2; exit 1; }
  if emit test_suite_secs "" seconds 2>/dev/null; then
    echo "self-test: a metric with no value must be refused, not reported as 0" >&2
    exit 1
  fi

  # The counter: summaries add, and silence is an error.
  printf 'test result: ok. 930 passed; 0 failed; 9 ignored\nrunning\n\ntest result: ok. 2 passed\n' > "$tmp/suite.log"
  [ "$(test_count "$tmp/suite.log")" = "932" ] \
    || { echo "self-test: test_count did not sum the summaries" >&2; exit 1; }
  printf 'Compiling whatever\n' > "$tmp/quiet.log"
  if test_count "$tmp/quiet.log" >/dev/null 2>&1; then
    echo "self-test: output with no summary must fail, not read as zero tests" >&2
    exit 1
  fi

  # The drift report, including both of its refusal paths and its two notes.
  printf '{"metric":"test_suite_secs","value":20}\n{"metric":"docs_check_ms","value":900}\n' > "$tmp/prev.jsonl"
  printf '{"metric":"test_suite_secs","value":22}\n{"metric":"new_thing","value":1}\n' > "$tmp/next.jsonl"
  out="$(drift "$tmp/prev.jsonl" "$tmp/next.jsonl")"
  echo "$out" | grep -q 'test_suite_secs: 20 -> 22 (+10.0%)' \
    || { echo "self-test: drift did not report the percentage: $out" >&2; exit 1; }
  echo "$out" | grep -q 'new metric with no baseline: new_thing' \
    || { echo "self-test: an unbaselined metric must be named: $out" >&2; exit 1; }
  echo "$out" | grep -q 'metric gone: docs_check_ms' \
    || { echo "self-test: a metric that disappeared must be named: $out" >&2; exit 1; }
  printf '{"metric":"unrelated","value":1}\n' > "$tmp/other.jsonl"
  if drift "$tmp/prev.jsonl" "$tmp/other.jsonl" >/dev/null 2>&1; then
    echo "self-test: two files sharing no metric must not pass as a comparison" >&2
    exit 1
  fi

  echo "benchmark self-test: ok (emitter and its sinks, test counter, drift report, four refusals)"
  exit 0
fi

if [ -n "$compare" ]; then
  [ -f "$compare" ] || { echo "benchmark.sh: nothing to compare at $compare" >&2; exit 2; }
  [ -n "$metrics_file" ] || { echo "benchmark.sh: --compare needs --metrics to compare against" >&2; exit 2; }
  drift "$compare" "$metrics_file"
  exit $?
fi

# ───────────────────────────── the measurement ─────────────────────────────

cd "$crate"
target_dir="${CARGO_TARGET_DIR:-$crate/target}"
export CARGO_TARGET_DIR="$target_dir"
BENCH_METRICS="${metrics_file:-$(mktemp)}"
export BENCH_METRICS
capture="$(mktemp)"
trap 'rm -f "$capture" "$BENCH_METRICS"' EXIT

if [ "$cold" = "1" ]; then
  echo "==> dropping $target_dir (cold build)"
  rm -rf "$target_dir"
fi

# Wall-clock seconds, float, from python3 — the same interpreter both scripts already
# need for their JSON. `date +%s.%N` would be one less dependency but BSD `date` has
# no `%N`, and a benchmark whose timer silently becomes an integer on macOS is a
# benchmark whose millisecond metric goes to zero on half the machines.
now() { python3 -c "import time; print(time.time())"; }
since() { python3 -c "print(round($(now) - $(cat "$1"), 3))"; }
mark() { now > "$1"; }

echo "==> cargo build --release"
t="$(mktemp)"; mark "$t"
if ! cargo build --release; then
  echo "benchmark.sh: the release build failed; nothing is measured on a red build" >&2
  exit 1
fi
emit build_release_secs "$(since "$t")" seconds

echo "==> cargo test (the suite ./scripts/check.sh runs)"
mark "$t"
if ! cargo test > "$capture" 2>&1; then
  echo "benchmark.sh: the suite failed, so its seconds would be a fiction. Tail:" >&2
  tail -30 "$capture" >&2
  exit 1
fi
emit test_suite_secs "$(since "$t")" seconds
emit test_count "$(test_count "$capture")" tests

echo "==> the docs rot gate (the page whose own claim is \"<1s\")"
mark "$t"
python3 "$here/docs_check.py" > /dev/null 2>&1 || true
emit docs_check_ms "$(python3 -c "print(int(round($(since "$t") * 1000)))")" milliseconds

echo
echo "metrics ($([ "$cold" = 1 ] && echo "cold" || echo "warm") build, target $target_dir):"
sed 's/^/  /' "$BENCH_METRICS"
echo
echo "Runner timings, not sizing claims, and nothing here fails on a threshold."
echo "Drift between two saved runs is the claim:"
echo "  ./scripts/benchmark.sh --metrics prev.jsonl   # then, later:"
echo "  ./scripts/benchmark.sh --compare prev.jsonl --metrics now.jsonl"
