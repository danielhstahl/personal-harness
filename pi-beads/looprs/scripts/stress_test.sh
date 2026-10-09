#!/usr/bin/env bash
# Stress a cargo test suite under CPU contention, round by round, with a watchdog.
#
#   ./scripts/stress_test.sh <crate-dir> <rounds> [test-threads] [extra cargo test args]
#
# Environment:
#   HOGS     number of `yes > /dev/null` processes to run over the box (default 12)
#   ROUND_TIMEOUT  wall-clock seconds a round may take before it is killed and
#                 counted as a HANG (default 180)
#   LOG      where to append the same lines (default /tmp/stress.log)
#
# One line per round: PASS/FAIL/HANG, the wall clock, and the names of the tests
# that failed. A round whose binary never came back is killed by process group —
# the whole point of the watchdog is that a test that hangs forever is reported as
# the failure it is instead of ending the run silently.
#
# Example — 20 rounds of the full suite, 8 test threads, 12 CPU hogs:
#   HOGS=12 ./scripts/stress_test.sh "$(pwd)" 20 8
set -uo pipefail

CRATE="${1:?usage: stress.sh <crate-dir> <rounds> [threads] [extra args]}"
ROUNDS="${2:-20}"
THREADS="${3:-8}"
if [ "$#" -gt 3 ]; then shift 3; else shift "$#"; fi
EXTRA="$*"

HOGS="${HOGS:-12}"
ROUND_TIMEOUT="${ROUND_TIMEOUT:-180}"
LOG="${LOG:-/tmp/stress.log}"

pids=()
cleanup() { [ "${#pids[@]}" -gt 0 ] && kill "${pids[@]}" 2>/dev/null; }
trap cleanup EXIT
if [ "$HOGS" -gt 0 ]; then
  for _ in $(seq 1 "$HOGS"); do
    yes > /dev/null &
    pids+=($!)
  done
fi

echo "=== stress: $CRATE rounds=$ROUNDS threads=$THREADS hogs=$HOGS timeout=${ROUND_TIMEOUT}s extra='$EXTRA'" | tee -a "$LOG"
fails=0
for r in $(seq 1 "$ROUNDS"); do
  line=$(CRATE="$CRATE" ROUND_TIMEOUT="$ROUND_TIMEOUT" THREADS="$THREADS" EXTRA="$EXTRA" python3 <<'PY'
import os, signal, subprocess, time, re

crate = os.environ["CRATE"]
timeout = float(os.environ["ROUND_TIMEOUT"])
threads = os.environ["THREADS"]
extra = os.environ.get("EXTRA", "").split()
cmd = ["cargo", "test", "--", f"--test-threads={threads}"] + extra
start = time.time()
p = subprocess.Popen(cmd, cwd=crate, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                     text=True, start_new_session=True)
out = ""
try:
    out, _ = p.communicate(timeout=timeout)
except subprocess.TimeoutExpired:
    os.killpg(os.getpgid(p.pid), signal.SIGKILL)
    out, _ = p.communicate()
    dur = time.time() - start
    running = [l for l in out.splitlines() if l.startswith("test ") and " ... ok" not in l]
    print(f"HANG {dur:.1f}s  [killed at {timeout:.0f}s; last: {running[-3:]}]")
    raise SystemExit(0)
dur = time.time() - start
if "test result: FAILED" in out:
    names = sorted(set(re.findall(r"^    (\S+)$", out.split("failures:")[-1], re.M)))
    print(f"FAIL {dur:.1f}s  [{' '.join(names)}]")
elif "test result: ok" in out:
    n = re.search(r"(\d+) passed", out)
    print(f"PASS {dur:.1f}s  ({n.group(0) if n else 'ok'})")
else:
    print(f"NO RESULT {dur:.1f}s  [tail: {out.strip().splitlines()[-3:]}]")
PY
)
  echo "round $r: $line" | tee -a "$LOG"
  case "$line" in
    PASS*) ;;
    *) fails=$((fails + 1)) ;;
  esac
done
echo "=== total failing rounds: $fails of $ROUNDS" | tee -a "$LOG"
[ "$fails" -eq 0 ]
