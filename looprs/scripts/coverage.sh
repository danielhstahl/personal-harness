#!/usr/bin/env bash
# Line coverage for the looprs crate. The `coverage` job of
# .github/workflows/quality.yml calls this; AGENTS.md requires a coverage job, and
# this repo's rule is that a workflow calls a script rather than carrying the logic.
#
#   ./scripts/coverage.sh                  # instrument, run the suite, print the percentage
#   ./scripts/coverage.sh --fail-under 60  # ...and fail if total line coverage is below 60
#   ./scripts/coverage.sh --self-test      # check this script's logic; no toolchain needed
#   ./scripts/coverage.sh --report-only    # re-read the last run's lcov; run no tests
#
# Outputs, under the crate root:
#
#   coverage/lcov.info              the instrumented run's export, in the format every
#                                   coverage tool reads (and what act's evidence scrape looks for)
#   coverage/coverage-summary.json  the same totals in the shape coverage tooling
#                                   expects to find a `total.lines` in
#
# **One instrumented run, and the percentage read back out of the lcov.** lcov's
# `LF:` (lines found) and `LH:` (lines hit) records, summed over the file, are exactly
# the two numbers a line-coverage percentage is made of; getting them from a second
# export format would mean either running the suite twice or asking llvm-cov for a
# summary it spells differently from the one half of tooling quotes. So one run, one
# export, and a derived summary file.
#
# The floor is a flag and not a constant. The first run of a coverage job is the run
# that learns what the number actually is, and a gate set from a desk either never
# fires or fires forever; with no flag this reports and passes.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate="$(dirname "$here")"
out="$crate/coverage"
# shellcheck source=lib/rust-target.sh
. "$here/lib/rust-target.sh"

fail_under=""
self_test=0
report_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    --fail-under)
      fail_under="${2:?--fail-under wants a percentage}"
      shift 2
      ;;
    --report-only) report_only=1; shift ;;
    --self-test) self_test=1; shift ;;
    *)
      echo "coverage.sh: unknown argument: $1 (--fail-under N, --report-only, --self-test)" >&2
      exit 2
      ;;
  esac
done

# ────────────────────── the parts that hold logic, testable apart ──────────────────────

# Total line coverage from an lcov file: `100 * ΣLH / ΣLF`.
#
# A file with no `LF:` record is an error rather than a 0%: an empty lcov means the
# instrumented run produced nothing, and "0% coverage" would be a true sentence about
# the wrong thing — the same distinction this crate makes between an empty board and
# an unreadable one.
total_pct() {
  python3 - "$1" <<'PY'
import sys
lf = lh = 0
try:
    for line in open(sys.argv[1], encoding="utf-8", errors="ignore"):
        if line.startswith("LF:"):
            lf += int(line[3:])
        elif line.startswith("LH:"):
            lh += int(line[3:])
except OSError as e:
    print(f"unreadable lcov: {e}", file=sys.stderr)
    sys.exit(3)
if lf <= 0:
    print("unreadable lcov: no LF: record, so nothing was instrumented", file=sys.stderr)
    sys.exit(3)
print(round(lh / lf * 100.0, 2))
PY
}

# The derived summary, in the shape that says `total.lines.pct`.
write_summary() {
  python3 - "$1" "$2" "$3" "$4" <<'PY'
import json, sys
pct, lf, lh = float(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
json.dump(
    {"total": {"lines": {"count": lf, "covered": lh, "missing": lf - lh, "pct": pct}},
     "derived_from": "lcov LF/LH sums (scripts/coverage.sh)"},
    open(sys.argv[1], "w"),
    indent=1,
)
PY
}

# The verdict. No floor is a pass; an empty percentage is a tooling failure, never a
# coverage one.
gate() {
  local pct="$1" floor="$2"
  if [ -z "$pct" ]; then
    echo "coverage: no percentage was produced; the export step failed" >&2
    return 2
  fi
  [ -z "$floor" ] && return 0
  python3 - "$pct" "$floor" <<'PY'
import sys
pct, floor = float(sys.argv[1]), float(sys.argv[2])
if pct < floor:
    print(f"coverage: {pct}% is below the {floor:g}% floor", file=sys.stderr)
    sys.exit(1)
print(f"coverage: {pct}% is at or above the {floor:g}% floor")
PY
}

# LF/LH sums, for the summary file.
lcov_sums() {
  python3 - "$1" <<'PY'
import sys
lf = lh = 0
for line in open(sys.argv[1], encoding="utf-8", errors="ignore"):
    if line.startswith("LF:"):
        lf += int(line[3:])
    elif line.startswith("LH:"):
        lh += int(line[3:])
print(lf, lh)
PY
}

# One metric line for a runner that asked for evidence, and nothing when it did not.
publish() {
  [ -z "${ACTMCP_EVIDENCE_OUT:-}" ] && return 0
  local line
  line=$(printf '{"metric":"line_coverage_pct","value":%s,"unit":"percent"}\n' "$1")
  # A sink that exists as a *directory* is the act runner's own evidence dir, which it
  # creates before the job starts and scrapes after; a file sink is a hand-set one. A
  # redirect cannot append into a directory, and the first act run of this job found out
  # with `Is a directory` after a successful build.
  if [ -d "$ACTMCP_EVIDENCE_OUT" ]; then
    printf '%s\n' "$line" >> "$ACTMCP_EVIDENCE_OUT/coverage.jsonl"
  else
    printf '%s\n' "$line" >> "$ACTMCP_EVIDENCE_OUT"
  fi
}

if [ "$self_test" = "1" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  # Two files, 1000 lines found / 650 hit overall, across two SF sections.
  printf 'SF:src/a.rs\nLF:600\nLH:400\nend_of_record\nSF:src/b.rs\nLF:400\nLH:250\nend_of_record\n' > "$tmp/a.info"
  got="$(total_pct "$tmp/a.info")"
  [ "$got" = "65.0" ] || { echo "self-test: total_pct read $got, wanted 65.0" >&2; exit 1; }

  # An empty export is a failure, not a 0%.
  printf 'TN:\n' > "$tmp/empty.info"
  if total_pct "$tmp/empty.info" >/dev/null 2>&1; then
    echo "self-test: an lcov with nothing instrumented must fail, not read as 0%" >&2
    exit 1
  fi

  # The gate's three verdicts.
  gate "65.0" "" || { echo "self-test: no floor must pass" >&2; exit 1; }
  gate "65.0" "65" || { echo "self-test: exactly at the floor must pass" >&2; exit 1; }
  if gate "64.99" "65" >/dev/null 2>&1; then
    echo "self-test: below the floor must fail" >&2
    exit 1
  fi
  if gate "" "65" >/dev/null 2>&1; then
    echo "self-test: a missing percentage must be a tooling failure, not a coverage one" >&2
    exit 1
  fi

  # The derived summary carries the same numbers it was built from.
  read -r lf lh <<<"$(lcov_sums "$tmp/a.info")"
  [ "$lf" = "1000" ] && [ "$lh" = "650" ] \
    || { echo "self-test: lcov_sums read $lf/$lh, wanted 650/1000" >&2; exit 1; }
  write_summary "$tmp/summary.json" "$got" "$lf" "$lh"
  [ "$(python3 -c "import json;print(json.load(open('$tmp/summary.json'))['total']['lines']['pct'])")" = "65.0" ] \
    || { echo "self-test: the derived summary did not carry the percentage" >&2; exit 1; }

  # Under a sink, the percentage is published as one JSON metric line.
  ACTMCP_EVIDENCE_OUT="$tmp/evidence.jsonl" publish "$got"
  [ "$(wc -l < "$tmp/evidence.jsonl" | tr -d ' ')" = "1" ] \
    || { echo "self-test: the metric must land as exactly one evidence line" >&2; exit 1; }
  # The runner hands a *directory* (`.evidence/incoming/<run id>`), not a file; the line
  # goes into one named file inside it rather than failing on the redirect.
  mkdir -p "$tmp/evidence-dir"
  ACTMCP_EVIDENCE_OUT="$tmp/evidence-dir" publish "$got"
  [ "$(wc -l < "$tmp/evidence-dir/coverage.jsonl" | tr -d ' ')" = "1" ] \
    || { echo "self-test: a directory sink must collect the metric in one named file" >&2; exit 1; }


  # The build-directory rule (lib/rust-target.sh), which is a real crash and not a
  # preference: under a runner that mounts the workspace, cargo must not be pointed at
  # the mount, and a choice the caller made must survive.
  got="$(ACT=true CARGO_TARGET_DIR= TMPDIR="$tmp" bash -c "cd '$crate' && . '$here/lib/rust-target.sh' && pick_cargo_target_dir && printf %s \"\$CARGO_TARGET_DIR\"")"
  case "$got" in
    "$tmp"/*) : ;;
    *) echo "self-test: under a mounted-workspace runner the build dir must leave the workspace, got: $got" >&2; exit 1 ;;
  esac
  got="$(CARGO_TARGET_DIR=/somewhere/else TMPDIR="$tmp" bash -c ". '$here/lib/rust-target.sh' && pick_cargo_target_dir && printf %s \"\$CARGO_TARGET_DIR\"")"
  [ "$got" = "/somewhere/else" ] \
    || { echo "self-test: an explicit CARGO_TARGET_DIR must not be overwritten, got: $got" >&2; exit 1; }

  echo "coverage self-test: ok (lcov read, empty export refused, three gate verdicts, derived summary, evidence line)"
  exit 0
fi

cd "$crate"

if [ "$report_only" = "1" ]; then
  [ -f "$out/lcov.info" ] || { echo "coverage.sh: no $out/lcov.info to re-read; run without --report-only" >&2; exit 2; }
  pct="$(total_pct "$out/lcov.info")"
  echo "coverage: ${pct}% (re-read from $out/lcov.info; nothing was run)"
  gate "$pct" "$fail_under"
  exit $?
fi

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  cat >&2 <<'MSG'
coverage.sh: `cargo-llvm-cov` is not installed.
  cargo install cargo-llvm-cov --locked        (CI: taiki-e/install-action)
It instruments through the `llvm-tools-preview` component of the toolchain.
MSG
  exit 3
fi

# Under a runner that mounts the workspace, the instrumented build goes on the
# container's own disk; see lib/rust-target.sh.
pick_cargo_target_dir

mkdir -p "$out"
echo "==> cargo llvm-cov: instrumented build, whole suite, lcov into $out/lcov.info"
# The fixture files under `tests/` are not production code and would otherwise be
# counted as 0%-covered source, which would make the percentage a function of how
# many fixtures the suite happens to carry.
if ! cargo llvm-cov \
  --all-features \
  --ignore-filename-regex '(^|/)tests/' \
  --lcov \
  --output-path "$out/lcov.info"; then
  echo "coverage.sh: the instrumented run failed; no coverage is reported for a red build" >&2
  exit 1
fi

pct="$(total_pct "$out/lcov.info")"
read -r lf lh <<<"$(lcov_sums "$out/lcov.info")"
write_summary "$out/coverage-summary.json" "$pct" "$lf" "$lh"
publish "$pct"

echo
echo "coverage: ${pct}% of ${lf} lines (${lh} hit, $((lf - lh)) missed)"
echo "  lcov:    $out/lcov.info"
echo "  summary: $out/coverage-summary.json"

gate "$pct" "$fail_under"
