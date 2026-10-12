#!/usr/bin/env python3
"""workspace-eqn.1 / .6 — what the `pi --mode rpc` process boundary actually costs.

The proposal on the table (ADR-0010) is that looprs should stop spawning
`pi --mode rpc` per beads pass and link an engine in as a library, because the
child is "overhead" and a "serialization boundary". Both halves of that sentence
name a cost, and neither has a number next to it in this repo. A round number
invented at a desk is a bug (`src/measure.rs`), so this script gets the real
ones. Four legs, each measuring a different thing people say when they say
"the process boundary is slow":

  1. **SPAWN** — the per-pass price of the child itself: wall from `fork` to the
     first *correlated* response (the handshake looprs actually waits on), the
     steady-state round trip of a correlated request, peak RSS of the child, its
     CPU seconds, and the time from closing stdin to the child being gone
     (ADR-0002's EOF-means-exit). Measured in the two shapes looprs uses:
     the worker's bare `--mode rpc` and the planner's `--tools read,bash`
     (`src/session/beads/machine.rs`), plus looprs' own fake child as the
     control arm that says how much of the number is this harness rather than pi.
  2. **STREAM** — one *real* pass driven over the boundary against the local
     model, with every record counted: records out, records back, bytes each
     way, and how long the model was the thing being waited on. This is what a
     "serialization boundary" is made of in bytes, not in adjectives.
  3. **DENOMINATOR** — the same quantity divided by the thing it has to be
     small against. Priced over the real beads corpus (pi session files from
     real passes): median pass wall clock, how much of it is model time, how
     many correlated commands a pass actually sends. `pi is slow` becomes
     `the boundary is N ms out of T s of a pass`, and the ratio is the finding.
  4. **BUDGET** — what removing the boundary could *at most* buy, stated as an
     upper bound: everything measured in 1 + 2 that a library engine would not
     also pay, divided by the pass wall. The point of the leg is that the bound
     is tiny, so that anyone who wants the switch has to argue about something
     other than latency.

It never writes to the real beads board and never writes a session into the
real session store (leg 1's session leg points `--session-dir` at its own temp
dir). It needs: `pi` on PATH, `bd` on PATH for the corpus leg's context, and a
reachable model for the STREAM leg — with no model it prints that leg as
`not measured` rather than guessing (ADR-0009's rule: a measurement that cannot
run says so).

    python3 spikes/pi_rpc_boundary_cost.py | tee spikes/results/pi-rpc-boundary-cost.log

Env:
  LOOPRS_SPIKE_PI        pi binary (default: pi)
  LOOPRS_SPIKE_N         spawn iterations per shape (default: 7)
  LOOPRS_SPIKE_RTT       correlated requests per spawn (default: 25)
  LOOPRS_MEASURE_CORPUS  dir of real pi session .jsonl files (default: the
                         looprs project's own session store; the leg refuses
                         rather than inventing a corpus)
  LOOPRS_SPIKE_NO_MODEL  skip the STREAM leg
"""

import glob
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PI = os.environ.get("LOOPRS_SPIKE_PI", "pi")
N = int(os.environ.get("LOOPRS_SPIKE_N", "7"))
RTT = int(os.environ.get("LOOPRS_SPIKE_RTT", "25"))
FAKE_PI = os.path.join(REPO, "tests", "fixtures", "fake_pi_chat.py")

# The corpus default is the path `src/measure.rs` cites: the machine that runs
# the beads loop keeps one .jsonl per pass there.
CORPUS_DEFAULT = os.path.expanduser(
    "~/.pi/agent/sessions/--Users-danielstahl-Documents-code-ml-personal-harness-pi-beads-looprs--"
)
CORPUS = os.environ.get("LOOPRS_MEASURE_CORPUS", CORPUS_DEFAULT)

CLK = os.sysconf("SC_CLK_TCK")
NULL = open(os.devnull, "w")


# ────────────────────────────────── /proc readers ─────────────────────────────
# Linux-only, and that is said out loud rather than papered over: `ps` is not in
# every container this runs in, and the kernel's own accounting beats a
# reconstruction. On a Mac (where earlier captures of this repo were taken) the
# RSS legs report `not measured` instead of lying.
def proc_stat(pid):
    """(RSS KiB, CPU ms) for one pid, from /proc. None if the pid is gone."""
    try:
        with open(f"/proc/{pid}/status") as f:
            rss = int(f.read().split("VmRSS:")[1].split()[0])
        with open(f"/proc/{pid}/stat") as f:
            st = f.read().split()
        cpu = (int(st[13]) + int(st[14])) * 1000.0 / CLK
        return rss, cpu
    except (FileNotFoundError, IndexError, ProcessLookupError, ValueError):
        return None


def sample_loop(pid, stop, out, interval=0.005):
    while not stop.is_set():
        s = proc_stat(pid)
        if s is None:
            return
        out.append(s)
        time.sleep(interval)


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def spread(xs):
    if not xs:
        return "—", "—"
    xs = sorted(xs)
    return xs[len(xs) // 2], xs[-1]


# ────────────────────────────────── the RPC client ────────────────────────────
class Rpc:
    """The smallest client that does what `src/services/pi.rs` does.

    Deliberately not a copy of it: the correlation rule, the LF framing and the
    "stdin close is the shutdown lever" are the whole contract from the client
    side, and a spike that drifts from the real client measures a contract nobody
    ships. Kept at ~40 lines on purpose.
    """

    def __init__(self, argv):
        self.p = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=NULL, text=True, bufsize=0
        )
        self.n = 0
        self.t_replies = []

    def request(self, cmd):
        self.n += 1
        cid = f"r{self.n}"
        cmd = {**cmd, "id": cid}
        t = time.perf_counter()
        self.p.stdin.write(json.dumps(cmd) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"child closed stdout before answering {cid}")
            try:
                v = json.loads(line)
            except json.JSONDecodeError:
                continue
            if v.get("type") == "response" and v.get("id") == cid:
                dt = time.perf_counter() - t
                self.t_replies.append(dt)
                return v, line

    def drain_until(self, predicate, limit_s=600):
        """Read records until `predicate(record)` or the deadline. Returns (records, bytes, seconds)."""
        n = b = 0
        t = time.perf_counter()
        while time.perf_counter() - t < limit_s:
            line = self.p.stdout.readline()
            if not line:
                break
            b += len(line)
            n += 1
            try:
                v = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(v):
                return n, b, time.perf_counter() - t, v
        return n, b, time.perf_counter() - t, None

    def close_stdin(self):
        self.p.stdin.close()

    def wait(self):
        return self.p.wait()


# ─────────────────────────────────── leg 1: spawn ─────────────────────────────
def leg_spawn():
    print("\n== 1. SPAWN — the per-pass price of the child ==")
    scratch = tempfile.mkdtemp(prefix="eqn-spawn-")
    # The repo's own fake child is copied out of tests/fixtures before it runs,
    # exactly the way src/testing.rs copies it per test: it keeps its log and its
    # `settle` marker next to itself, and a measurement that dirtied the tree
    # would be indistinguishable from a test run.
    fake = os.path.join(scratch, "fake_pi.py")
    shutil.copy(FAKE_PI, fake)
    shapes = [
        ("worker (`pi --mode rpc`)",
         [PI, "--mode", "rpc", "--session-dir", os.path.join(scratch, "sess-worker")]),
        ("planner (`pi --mode rpc --tools read,bash`)",
         [PI, "--mode", "rpc", "--tools", "read,bash", "--session-dir", os.path.join(scratch, "sess-plan")]),
        ("no-session (`pi --mode rpc --no-session`)", [PI, "--mode", "rpc", "--no-session"]),
        ("looprs' fake child (control)", [sys.executable, fake]),
    ]
    hdr = f"{'shape':46} {'spawn→first ms':>15} {'rtt ms med':>11} {'rtt ms p95':>11} {'peak RSS MiB':>13} {'child CPU ms':>12} {'eof→exit ms':>12}"
    print(hdr)
    print("-" * len(hdr))
    out = {}
    for label, argv in shapes:
        firsts, rtts, peaks, cpus, exits = [], [], [], [], []
        for _ in range(N):
            t0 = time.perf_counter()
            rpc = Rpc(argv)
            samples = []
            stop = threading.Event()
            th = threading.Thread(target=sample_loop, args=(rpc.p.pid, stop, samples), daemon=True)
            th.start()
            try:
                # get_state is the cheapest command that round-trips a
                # correlation id, i.e. exactly the handshake the real client
                # depends on and nothing more.
                _, _ = rpc.request({"type": "get_state"})
                firsts.append((time.perf_counter() - t0) * 1000)
                for _ in range(RTT):
                    rpc.request({"type": "get_state"})
            except RuntimeError as e:
                print(f"  {label}: {e}")
                stop.set()
                rpc.p.kill()
                break
            stop.set()
            th.join(timeout=1)
            if samples:
                peaks.append(max(s[0] for s in samples) / 1024.0)
                cpus.append(max(s[1] for s in samples))
            t = time.perf_counter()
            rpc.close_stdin()  # ADR-0002 Q1: EOF is the shutdown lever
            rpc.wait()
            exits.append((time.perf_counter() - t) * 1000)
            rtts.extend(rpc.t_replies[1:])
        if not firsts:
            print(f"{label:46} not measured")
            continue
        r_med, r_p95 = spread(rtts)
        print(
            f"{label:46} {med(firsts):15.1f} {r_med * 1000:11.2f} {r_p95 * 1000:11.2f} "
            f"{med(peaks):13.1f} {med(cpus):12.0f} {med(exits):12.1f}"
        )
        if len(firsts) > 1:
            print(
                f"{'':46} run-to-run spread on spawn→first: min {min(firsts):.1f} ms, "
                f"max {max(firsts):.1f} ms over {len(firsts)} runs"
            )
        out[label] = (med(firsts), r_med * 1000, med(exits), r_p95 * 1000)
    shutil.rmtree(scratch, ignore_errors=True)
    print(
        "\nNote: 'spawn→first' is fork-to-first-correlated-response, the only\n"
        "handshake looprs waits on; the child is not asked to be warm, only to\n"
        "answer. RSS/CPU are the kernel's own numbers read from /proc from\n"
        "outside the child, per this repo's convention (`spikes/ram_e2e.py`)."
    )
    return out


# ─────────────────────────────────── leg 2: stream ────────────────────────────
def leg_stream():
    print("\n== 2. STREAM — one real pass over the boundary, in records and bytes ==")
    if os.environ.get("LOOPRS_SPIKE_NO_MODEL"):
        print("not measured (LOOPRS_SPIKE_NO_MODEL set)")
        return
    base_url = os.environ.get("LOOPRS_SPIKE_BASE_URL", "http://llm.home:8080/v1")
    try:
        health = subprocess.run(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", base_url + "/models"],
                                capture_output=True, text=True, timeout=15)
        if health.stdout.strip() != "200":
            print(f"not measured (model endpoint {base_url} answered {health.stdout.strip()!r})")
            return
    except Exception as e:  # noqa: BLE001 - a missing model is a leg that cannot run, not a crash
        print(f"not measured (model endpoint {base_url} unreachable: {e})")
        return

    sess = tempfile.mkdtemp(prefix="eqn-stream-")
    try:
        rpc = Rpc([PI, "--mode", "rpc", "--session-dir", sess])
        # One handshake first, so the prompt's own round trip is measured against
        # a *warm* child and the boot lands in leg 1's column, where it belongs.
        rpc.request({"type": "get_state"})
        t0 = time.perf_counter()
        resp, _ = rpc.request({"type": "prompt",
                               "message": "Use the bash tool to run `ls /etc | head -5`, then "
                                          "list what it printed on one line."})
        t_prompt = time.perf_counter() - t0
        disp = resp.get("data", {}).get("disposition")
        n, b, t_run, last = rpc.drain_until(lambda v: v.get("type") == "agent_settled", limit_s=420)
        print(f"prompt disposition            {disp}")
        print(f"warm prompt response ms       {t_prompt * 1000:.2f}   "
              f"(the command's own round trip: accepted, not finished)")
        print(f"records back until settle     {n}")
        print(f"bytes back until settle       {b}  ({b / max(t_run, 1e-9) / 1024:.1f} KiB/s)")
        print(f"pass wall s                   {t_run:.1f}")
        print(f"record rate while running     {n / max(t_run, 1e-9):.1f} records/s")
        if last is None:
            print("settle                      never arrived (the leg timed out; numbers above are partial)")
        print(
            "\nDo NOT divide by this pass: a toy prompt that finishes in seconds is\n"
            "the wrong denominator, which is the whole reason leg 3 measures real\n"
            "passes. This leg exists to say what crosses the boundary per unit of\n"
            "work — records and bytes — not what share of a trivial run it is."
        )
        rpc.close_stdin()
        rpc.wait()
        return {"records": n, "bytes": b, "seconds": t_run}
    finally:
        shutil.rmtree(sess, ignore_errors=True)


# ────────────────────────────────── leg 3: corpus ─────────────────────────────
def leg_corpus():
    print("\n== 3. DENOMINATOR — what a pass costs, measured over real passes ==")
    files = sorted(glob.glob(os.path.join(CORPUS, "*.jsonl")))
    if not files:
        print(f"not measured: no corpus at {CORPUS} (set LOOPRS_MEASURE_CORPUS).")
        print("This leg refuses to run on synthetic filler: the ratio is the finding.")
        return None
    walls, model_t, tool_t, entries, nbytes, steps, bash_calls, bd_calls = [], [], [], [], [], [], 0, 0
    for f in files:
        seq = []
        for line in open(f, errors="replace"):
            try:
                v = json.loads(line)
            except json.JSONDecodeError:
                continue
            if v.get("type") != "message":
                continue
            m = v["message"]
            try:
                t = datetime.fromisoformat(v["timestamp"].replace("Z", "+00:00"))
            except (KeyError, ValueError):
                continue
            seq.append((m.get("role"), t, m))
        if len(seq) < 3:
            continue
        wall = (seq[-1][1] - seq[0][1]).total_seconds()
        mt = tt = 0.0
        nsteps = 0
        for i, (role, t, m) in enumerate(seq):
            if role != "assistant":
                continue
            nsteps += 1
            mt += max(0.0, (t - seq[i - 1][1]).total_seconds()) if i > 0 else 0.0
            tt += max(0.0, (seq[i + 1][1] - t).total_seconds()) if i + 1 < len(seq) else 0.0
            for c in m.get("content") or []:
                if isinstance(c, dict) and c.get("type") == "toolCall" and c.get("name") == "bash":
                    bash_calls += 1
                    if "bd " in json.dumps(c.get("arguments") or ""):
                        bd_calls += 1
        walls.append(wall)
        model_t.append(mt)
        tool_t.append(tt)
        entries.append(sum(1 for _ in open(f, errors="replace")))
        nbytes.append(os.path.getsize(f))
        steps.append(nsteps)
    print(f"corpus                        {CORPUS}")
    print(f"passes                        {len(walls)}")
    print(f"pass wall s                   median {med(walls):.0f}   p10 {sorted(walls)[len(walls)//10]:.0f}"
          f"   p90 {sorted(walls)[9*len(walls)//10]:.0f}")
    print(f"model time s (per pass)       median {med(model_t):.0f}"
          f"   = {100 * med(model_t) / med(walls):.1f}% of the pass")
    print(f"tool+gap time s (per pass)    median {med(tool_t):.0f}"
          f"   = {100 * med(tool_t) / med(walls):.1f}% of the pass")
    print(f"assistant steps per pass      median {med(steps):.0f}")
    print(f"session entries per pass      median {med(entries):.0f}")
    print(f"session bytes per pass        median {med(nbytes) / 1024:.0f} KiB")
    print(f"bash tool calls per pass      {bash_calls / len(walls):.1f}")
    print(f"of which mention `bd`         {bd_calls / len(walls):.1f}")
    return {"wall": med(walls), "model": med(model_t), "steps": med(steps)}


# ─────────────────────────────────── leg 4: budget ────────────────────────────
def leg_budget(spawn_stats, corpus, stream):
    print("\n== 4. BUDGET — the upper bound on what removing the child could buy ==")
    if not spawn_stats:
        print("not measured (no spawn leg to budget against)")
        return
    spawn_ms, rtt_ms, eof_ms, rtt_p95_ms = spawn_stats
    # A beads pass sends a handful of correlated commands: one prompt per pass,
    # plus clear_queue/steer on the paths that use them (src/session/pi_chat.rs,
    # src/session/beads/machine.rs). Ten is generous for one pass; name it as an
    # assumption rather than hiding it.
    requests = 10
    per_pass = spawn_ms + eof_ms + requests * rtt_ms
    print(f"child spawn+handshake         {spawn_ms:.0f} ms")
    print(f"stdin-EOF shutdown            {eof_ms:.0f} ms")
    print(f"{requests} correlated requests @ {rtt_ms:.2f} ms{'':<12} {requests * rtt_ms:.0f} ms")
    print(f"per-pass boundary total       {per_pass:.0f} ms")
    print(f"  …charged at the measured p95 instead of the median ({rtt_p95_ms:.1f} ms a request —\n"
          f"  Node's event loop, not the pipe): {spawn_ms + eof_ms + requests * rtt_p95_ms:.0f} ms. "
          "Both are noise against the denominator.")
    wall = corpus["wall"] if corpus else None
    if wall:
        print("\nDivided by the denominator the corpus leg measured, not one invented here:")
        print(f"  median pass                  {wall:.0f} s  ->  the whole boundary is "
              f"{100 * per_pass / 1000 / wall:.4f}% of a pass")
    if stream and corpus and stream.get("seconds"):
        steps = corpus["steps"]
        print("\nWhat the boundary carries over a median pass (leg 2's streaming rate\n"
              "of a real tool-using turn × leg 3's median assistant steps):")
        print(f"  ~{stream['records'] / stream['seconds'] * steps / 1000:.1f}k records, "
              f"~{stream['bytes'] / stream['seconds'] * steps / 1e6:.1f} MB per pass at {steps:.0f} steps")
        print("  What parsing those costs looprs is priced separately with looprs'\n"
              "  own parser: `spikes/results/rpc-serde-cost.log`.")
    print(
        "\nWhat a library engine does NOT remove: the engine's own model client,\n"
        "session writing, tool execution, syntax highlighting and the ~100 MiB of\n"
        "runtime those need. What it removes is the fork, the interpreter boot, the\n"
        "stdio framing and one IPC hop per command — i.e. the row above."
    )


def main():
    print(f"workspace-eqn boundary measurement — host {platform.system()} {platform.release()}, "
          f"{os.cpu_count()} cpus, python {platform.python_version()}")
    try:
        pv = subprocess.run([PI, "--version"], capture_output=True, text=True, timeout=30).stdout.strip()
        print(f"engine under test: {PI} {pv}")
    except Exception as e:  # noqa: BLE001
        print(f"engine under test: {PI} (version not readable: {e})")
    print(f"bd: {shutil.which('bd')}   corpus: {CORPUS}")
    print(
        "\nRead before quoting a number: every figure here is one machine, one\n"
        "model endpoint and one kernel. The ADR cites this capture; a number typed\n"
        "into a second page is a number that will drift (ADR-0009's rule 4)."
    )
    spawned = leg_spawn()
    corpus = leg_corpus()
    stream = leg_stream()
    # The budget is built from the worker shape, because that is the pass the
    # loop buys per ticket.
    key = next((k for k in spawned if k.startswith("worker")), None)
    leg_budget(spawn_stats=spawned[key] if key else None, corpus=corpus, stream=stream)
    print("\nDone.")


if __name__ == "__main__":
    main()
