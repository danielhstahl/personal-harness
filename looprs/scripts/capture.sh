#!/usr/bin/env bash
# Commit a spike run as evidence (looprs-00u.23).
#
#   ./scripts/capture.sh <spike-script> <capture-name> [command…]
#
# writes `spikes/results/<capture-name>.log`: a provenance header, then the run's
# own stdout+stderr. The header is why this wrapper exists. A bare `tee` of a spike
# leaves a log that says what passed but not *what it passed on*, and a number in
# the docs that cannot be tied to a tree is exactly the drift this closes: three
# pages said "149 checks" while the tree printed 171 and no capture of the 171 run
# was ever committed.
#
#   ./scripts/capture.sh shutdown_e2e.py shutdown-e2e-00u23
#   ./scripts/capture.sh flash_e2e.py flash-e2e-control \
#       env LOOPRS_BIN=/tmp/base-target/debug/looprs
#
# The exit code is the spike's, so a failing run still leaves its log and still
# fails — a control run is *supposed* to fail, and you want the log either way.
# `scripts/docs_check.py` reads the `N/M checks passed` line out of these files;
# a count in prose that matches none of them is a gate failure.
#
# This header says *where* a number came from. A capture of a run that could have
# priced more than one thing has to say *what* it priced, and it has to come from
# the driver: owner/repo plus every artifact@version the run resolved, printed
# from what it queried, not from a literal in the source. Rule and who checks it:
# docs/guide/contributing.md, "Assessing a third-party dependency" (workspace-8jk.10).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

spike="${1:?usage: scripts/capture.sh <spike-script> <capture-name> [command…]}"
name="${2:?usage: scripts/capture.sh <spike-script> <capture-name> [command…]}"
shift 2
[ "$#" -eq 0 ] && set -- python3 "spikes/$spike"

out="spikes/results/$name.log"
if [ -e "$out" ]; then
    echo "capture.sh: $out already exists; pick a new capture name (logs are evidence, not scratch)" >&2
    exit 1
fi

rev="$(git rev-parse --short HEAD 2>/dev/null || echo 'unknown rev')"
case "$(git status --porcelain -- . 2>/dev/null)" in
    "") dirty="" ;;
    *)  dirty=" (dirty tree: uncommitted changes on top of that commit)" ;;
esac

head="$(mktemp)"
body="$(mktemp)"
trap 'rm -f "$head" "$body"' EXIT
{
    printf '# %s — captured %s, tree %s%s\n' "$name" "$(date -u +%FT%TZ)" "$rev" "$dirty"
    printf '# ran: %s\n' "$*"
} >"$head"

set +e
"$@" >"$body" 2>&1
rc=$?
set -e

cat "$head" "$body" >"$out"
rm -f "$head" "$body"
total="$(grep -Eo '[0-9]+/[0-9]+ checks passed' "$out" | tail -1 || true)"
echo "capture.sh: wrote $out (exit $rc)${total:+ — $total}"
exit $rc
