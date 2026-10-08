#!/usr/bin/env python3
"""looprs-5o4.1 — what one kanban poll costs, measured.

The ADR (docs/adr/0007-kanban-board.md) picks the read the board polls with and a
default interval to poll it at. Both of those are cost questions, and the ticket
refuses a guessed number, so this script measures:

  1. wall time, CPU time and peak RSS of each candidate `bd` read on **this**
     board at its current size;
  2. the same on a synthetic **~10x** board (a scratch `.beads` built in a temp
     dir from a duplicated export — the real board is never touched);
  3. the two alternatives that were rejected, priced side by side rather than
     argued with: three per-status reads (the shape that can disagree with itself)
     and `bd ready` (the read that cannot see half the statuses);
  4. `--skip-labels`, whose whole selling point is that it is cheaper, and whose
     whole risk is that it changes the payload *shape* — printed here so the shape
     claim in the ADR is a measurement and not a recollection.

Nothing in here needs the app built. It needs `bd` on PATH and this repo's board
for the "current size" legs; the 10x legs build their own scratch board under
`$TMPDIR` and delete it.

    python3 spikes/board_poll_cost.py | tee spikes/results/board-poll-cost.log

Env:
  LOOPRS_SPIKE_BD   bd binary to measure (default: bd)
  LOOPRS_SPIKE_N    iterations per leg (default: 7)
  LOOPRS_SPIKE_SCALE  multiplier for the scratch board (default: 10)
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from collections import Counter

BD = os.environ.get("LOOPRS_SPIKE_BD", "bd")
N = int(os.environ.get("LOOPRS_SPIKE_N", "7"))
SCALE = int(os.environ.get("LOOPRS_SPIKE_SCALE", "10"))
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Every leg runs with the envelope the bd service sets (src/services/bd.rs:
# .env("BD_JSON_ENVELOPE", "1")), so what is measured here is the payload the
# service actually parses and not a different one.
ENVELOPE = {**os.environ, "BD_JSON_ENVELOPE": "1"}


def run(cwd, args, env=ENVELOPE):
    """`args` are bd's argv, without the program name."""
    t = time.perf_counter()
    r = subprocess.run([BD, *args], cwd=cwd, capture_output=True, env=env)
    return time.perf_counter() - t, r


def timed(args_cwd_args, n=N):
    """min / median / max wall, plus the last run's payload size."""
    cwd, args = args_cwd_args
    walls, sizes = [], []
    for _ in range(n):
        wall, r = run(cwd, args)
        walls.append(wall)
        sizes.append(len(r.stdout))
    walls.sort()
    return walls[0], walls[len(walls) // 2], walls[-1], sizes[-1]


def cpu_and_rss(cwd, args):
    """Peak RSS + user/sys from /usr/bin/time -l (BSD flavour, macOS)."""
    out = subprocess.run(
        ["/usr/bin/time", "-l", BD, *args],
        cwd=cwd,
        capture_output=True,
        env=ENVELOPE,
        text=True,
    ).stderr
    real = user = sys_t = rss = None
    for line in out.splitlines():
        tok = line.split()
        if len(tok) >= 2 and tok[1] == "maximum" and "resident set size" in line:
            rss = int(tok[0])
        elif len(tok) >= 6 and tok[1] == "real" and tok[3] == "user" and tok[5] == "sys":
            real, user, sys_t = float(tok[0]), float(tok[2]), float(tok[4])
    return real, user, sys_t, rss


def shape(cwd, args):
    """Top-level JSON shape of a read, as the parser sees it."""
    _, r = run(cwd, args)
    try:
        d = json.loads(r.stdout)
    except Exception as e:  # noqa: BLE001 - the shape IS the finding
        return f"UNPARSEABLE ({e}) rc={r.returncode}"
    if isinstance(d, list):
        return f"bare array, {len(d)} rows"
    keys = list(d.keys())
    inner = d.get("data")
    if isinstance(inner, dict):
        return f"object {keys}, data is an OBJECT with {list(inner.keys())}"
    if isinstance(inner, list):
        return f"object {keys}, data is an ARRAY of {len(inner)}"
    return f"object {keys}"


def board_size(cwd):
    _, r = run(cwd, ["list", "--all", "--limit", "0", "--json"])
    d = json.loads(r.stdout)
    rows = d if isinstance(d, list) else d["data"]
    return len(rows), Counter(b["status"] for b in rows)


def build_scratch_board(scale):
    """A throwaway board ~`scale` x this repo, in a temp dir. Never touches the real one."""
    tmp = tempfile.mkdtemp(prefix="looprs-bigboard-")
    subprocess.run([BD, "init", "--prefix", "big"], cwd=tmp, capture_output=True)
    src = subprocess.run(
        [BD, "export"], cwd=REPO, capture_output=True, text=True, check=True
    ).stdout
    rows = [json.loads(l) for l in src.splitlines() if l.strip()]
    out, i = [], 0
    for mult in range(scale):
        for r in rows:
            r = dict(r)
            r["id"] = f"big-{i}"
            # Drop the relational bits: they carry primary keys that collide when
            # duplicated, and the board read does not depend on them for cost.
            for k in ("dependencies", "dependents", "comments"):
                r.pop(k, None)
            r["title"] = (r.get("title") or "t") + f" [x{mult}]"
            out.append(r)
            i += 1
    jsonl = os.path.join(tmp, "big.jsonl")
    with open(jsonl, "w") as f:
        for r in out:
            f.write(json.dumps(r) + "\n")
    subprocess.run([BD, "import", jsonl], cwd=tmp, capture_output=True, check=True)
    return tmp, len(out)


def leg(label, cwd, args, n=N):
    mn, md, mx, size = timed((cwd, args), n)
    real, user, sys_t, rss = cpu_and_rss(cwd, args)
    print(
        f"  {label:<46} med {md*1000:6.0f} ms  "
        f"(min {mn*1000:5.0f} / max {mx*1000:6.0f})   "
        f"cpu {user + sys_t:4.2f} s real {real:4.2f} s   "
        f"peak RSS {(rss or 0) / 1e6:5.0f} MB   "
        f"{size / 1024:6.0f} KiB"
    )
    return md


def main():
    print(f"bd: {BD}   iterations/leg: {N}   scale: {SCALE}x")
    n0, c0 = board_size(REPO)
    print(f"\ncurrent board ({REPO}): {n0} issues  {dict(c0)}")

    READ = ["--readonly", "list", "--all", "--limit", "0", "--json"]
    READ_SKIP = ["--readonly", "list", "--all", "--limit", "0", "--skip-labels", "--json"]
    THREE = [["list", "--limit", "0", "--status", s, "--json"] for s in
             ("open", "in_progress", "closed")]

    print("\n== current size ==")
    one = leg("bd --readonly list --all --limit 0 --json  (CHOSEN)", REPO, READ)
    leg("bd list --all --limit 0 --skip-labels --json", REPO, READ_SKIP)
    leg("bd ready --json                       (loop read)", REPO, ["ready", "--json"])
    three = 0.0
    for a in THREE:
        three += leg(f"bd {' '.join(a[:4])} …  (per-status read)", REPO, a, n=3)
    print(f"  {'three per-status reads, summed':<46} med {three*1000:6.0f} ms  "
          f"= {three / one:4.1f}x the single read")

    print("\n== payload shape (what BdList in src/services/bd.rs must parse) ==")
    print(f"  bd --readonly list --all --limit 0 --json  -> {shape(REPO, READ)}")
    print(f"  bd --readonly list --all --limit 0 --skip-labels --json -> "
          f"{shape(REPO, READ_SKIP)}")
    print(f"  bd ready --json                            -> {shape(REPO, ['ready', '--json'])}")
    print("  (the skip-labels `data` is an OBJECT {issues, meta}; BdList's Envelope wants a "
          "Vec<Bead>, so that shape is BdError::Malformed to this codebase)")

    print(f"\n== {SCALE}x board (scratch, built in a temp dir) ==")
    big, nb = build_scratch_board(SCALE)
    try:
        n1, c1 = board_size(big)
        print(f"  scratch board: {n1} issues  {dict(c1)}")
        b_one = leg("bd --readonly list --all --limit 0 --json", big, READ)
        leg("bd --readonly list --all --limit 0 --skip-labels --json", big, READ_SKIP)
        # The safety property the board's own read is built on, proved where a
        # mistake cannot reach the real board.
        probe_rows = json.loads(subprocess.run(
            [BD, "list", "--limit", "1", "--json"], cwd=big,
            capture_output=True, text=True, env=ENVELOPE).stdout)
        probe = (probe_rows["data"] if isinstance(probe_rows, dict) else probe_rows)[0]["id"]
        w = subprocess.run([BD, "--readonly", "update", probe, "--status", "closed"],
                          cwd=big, capture_output=True, text=True, env=ENVELOPE)
        shown = json.loads(subprocess.run(
            [BD, "show", probe, "--json"], cwd=big,
            capture_output=True, text=True, env=ENVELOPE).stdout)
        shown = shown["data"] if isinstance(shown, dict) else shown
        still = shown[0]["status"]
        print(f"  bd --readonly update {probe} --status closed -> rc={w.returncode} "
              f"err={w.stderr.strip()[:70]!r}; status afterwards: {still}")
        print(f"  10x rows cost {b_one / one:4.1f}x the current-size read "
              f"({n0} -> {nb} rows)")
    finally:
        shutil.rmtree(big, ignore_errors=True)

    print("\nDone.")


if __name__ == "__main__":
    main()
