#!/usr/bin/env bash
# Dirty the machine the way an abandoned looprs run does (looprs-00u.24).
#
#   ./scripts/plant_stale_debris.sh [count] [owner-pid]
#
# writes `looprs-bash-integration-<owner>-<n>.sh` into $TMPDIR and starts a shell
# carrying each one, detached so it outlives this script. The owner pid defaults to
# 99999, which nothing owns — that is exactly what makes the planted shell a
# leftover by the same test the reaper applies: the app named in the rc file's name
# is not running, so nothing will ever come back for this shell.
#
# It is the deliberate "dirty machine" half of the looprs-00u.24 evidence. The
# leak checks in `spikes/shutdown_e2e.py` are scoped to the pids that run spawns,
# so planting this debris must not move the pass count by one; the reaper's line at
# the top of the log has to say how much of it there was. `ps` afterwards shows
# what a run of the old, global-`pgrep` spike read as its own failure:
#
#   ./scripts/plant_stale_debris.sh 2
#   ps -eo pid,ppid,etime,command | grep looprs-bash-integration
#   python3 spikes/shutdown_e2e.py | tail -1     # still 188/188
set -euo pipefail

count="${1:-2}"
owner="${2:-99999}"
tmp="${TMPDIR:-/tmp}"

if ps -p "$owner" >/dev/null 2>&1; then
    echo "plant_stale_debris.sh: pid $owner is alive; pick one nobody owns" >&2
    exit 1
fi

for ((n = 1; n <= count; n++)); do
    f="$tmp/looprs-bash-integration-$owner-$n.sh"
    # The content does not matter to the test — the *name* is the evidence, because
    # the name is what carries the owner pid. Written to look like the real thing so
    # `ps`/`ls` output is indistinguishable from a run that really made it.
    printf '# looprs bash integration (generated file, safe to delete).\n# planted by scripts/plant_stale_debris.sh\ntrue\n' >"$f"
    # `trap '' HUP` first, and that is not decoration: `bash -c 'sleep 300'` is a
    # single simple command, so bash `exec`s straight into it and argv becomes
    # `sleep 300` — the rc file marker, which is the *only* thing that makes this
    # process identifiable as looprs debris, would vanish from the process table.
    # With a second command there is nothing to exec into, so the argv keeps the
    # marker; the HUP trap keeps a hang-up on the way out from tidying the debris
    # up before the reaper gets to it, which is the point of planting it.
    nohup /bin/bash --rcfile "$f" -i -c "trap '' HUP; sleep 300; true" \
        >/dev/null 2>&1 &
    disown 2>/dev/null || true
done

echo "planted $count stale shell(s) with rc files owned by pid $owner in $tmp"
ps -eo pid,ppid,etime,command | grep "looprs-bash-integration-$owner-" | grep -v grep || true
