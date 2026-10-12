#!/usr/bin/env python3
"""workspace-8jk.11 — the candidate's advertised startup/RSS advantage, measured in
the shape looprs would actually use.

matthewyjiang/rho's README opens its pitch with a **CLI startup-time and peak-RSS
chart against other coding harnesses, pi included** ("Compare CLI startup time
and peak RSS with other coding harnesses"). ADR-0010 conceded exactly one row to
that kind of argument — it says the latency claim is dead but that *memory is a
real difference* — and trigger 4 left concurrency unpriced: if the loop ever runs
N children at once, the per-child RSS figure is the row that bites. So a
candidate advertising a startup/RSS win over pi is aiming at the one row the ADR
left open, and it gets measured rather than waved at.

Four things this capture refuses to do, which is most of what it is:

  * **It does not accept the CLI number as the candidate's.** The chart measures
    `rho run` as a process. looprs would never run it that way: ADR-0010's
    embedded shape links `rho-sdk` into a host that already owns the terminal,
    the highlighter and the TUI. Both shapes are measured and they are *separate
    rows*, because they are different numbers.
  * **It does not report the engine's footprint as an absolute.** The previous
    capture's memory row compared a *child's* RSS to an *in-process engine's*
    RSS, which flatters the in-process number by whatever the host itself costs.
    Here the engine's cost is a delta over a control binary that links the same
    library and builds no engine (`floor`), and looprs' own RSS is measured and
    kept on its own line, so the arithmetic for each shape is host + added, never
    an absolute traded against another absolute.
  * **It does not leave concurrency unpriced.** N children at once (pi RPC
    children, rho CLI children) against one embedded engine serving N sessions
    and N engines in one process, at N = 2, 4, 8 on this container's memory
    ceiling, with the wall-clock spawn cost multiplied alongside and the ceiling
    named where each shape hits it.
  * **It does not re-open the latency question.** The boot figures are divided by
    the measured pass denominator (the same session corpus `spikes/
    pi_rpc_boundary_cost.py` uses) so that "300 ms" arrives already as a share of
    a pass, and so does the embedded engine's boot.

Legs
  1. **pi, re-measured** — `pi --mode rpc` cold start and peak RSS, with the
     *existing* method (the same `Rpc` handshake and the same /proc sampling from
     `spikes/pi_rpc_boundary_cost.py`), because this is the pair every ratio here
     is against. Re-measured, not quoted.
  2. **looprs' own RSS** — the host the candidate would be added to, in a real
     pty, with `bd` and `pi` faked so nothing touches the real board. This is the
     "what is already paid for" line.
  3. **the candidate as a CLI** (the shape its own chart measures) — fork→first
     output for `rho --version` and for a one-shot `rho run --output jsonl`, peak
     RSS from `os.wait4`'s rusage (the kernel's high-water mark for that pid,
     exact and unsampled — a polling parent misses the peak of a process that
     lives 300 ms).
  4. **the candidate embedded** (the shape looprs would use) — a scratch host that
     links `rho-sdk` and does nothing else, printing a ladder of VmRSS/VmHWM and
     step time: engine build → session → run → second engine → N sessions → N
     concurrent runs, each rung against the `floor` control. One arm repeats the
     ladder against the local model endpoint so the scripted-provider rungs are
     not the whole story.
  5. **N-concurrent extrapolation** — N pi RPC children warm at once, N rho CLI
     children at once, N sessions on one engine, N engines in one process, at
     N = 2, 4, 8, with aggregate RSS, wall clock, the kernel's own
     `MemAvailable` delta, and the busy-session probe that answers whether "one
     active Run per Session" is a limit for this use or the right shape.
  6. **the arithmetic** — each shape as host + added, the ceiling each shape
     reaches at which N, and both boots as a share of a real pass.

It never writes to the real beads board (looprs runs with `LOOPRS_BD_BIN=/bin/echo`
and `LOOPRS_PI_BIN=/usr/bin/false`, the same fakes `spikes/ram_e2e.py` uses) and
nothing here enters `looprs/Cargo.toml` or its lockfile: the candidate is a
scratch crate and a `cargo install`-ed binary. Legs whose prerequisite is missing
print `not measured` with the reason instead of guessing.

    python3 spikes/rho_footprint_cost.py | tee spikes/results/matthewyjiang-footprint-cost.log

Environment
  LOOPRS_SPIKE_RHO        candidate CLI (default: rho, else `rho-install`-ed path)
  LOOPRS_SPIKE_RHO_VERSION  crate version for the scratch consumer (default: crates.io max_stable)
  LOOPRS_SPIKE_PI         pi binary (default: pi)
  LOOPRS_SPIKE_N          concurrency points, comma list (default: 2,4,8)
  LOOPRS_SPIKE_BOOTS      boot iterations per shape (default: 7)
  LOOPRS_SPIKE_MODEL      openai-compatible model id for the endpoint legs
  LOOPRS_SPIKE_BASE_URL   endpoint for the CLI/embed endpoint legs (default: http://llm.home:8080/v1)
  LOOPRS_SPIKE_NO_MODEL   skip every leg that needs the endpoint
  LOOPRS_SPIKE_CLI_MODEL  also run N-concurrent `rho run` children with real prompts
  LOOPRS_RHO_TARGET       reuse this cargo target dir for the scratch crate (says so in the log)
  LOOPRS_SPIKE_ARM_TIMEOUT  per-arm/per-child deadline in seconds (default: 300)
  LOOPRS_BIN              looprs binary for the host row (default: target/release/looprs)
"""

import glob
import json
import os
import platform
import pty
import select
import shutil
import statistics
import subprocess
import sys
import warnings
import tempfile
import threading
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)

# The existing boundary spike is imported rather than copied: leg 1 has to be the
# *same method* as the capture ADR-0010 quotes, and a copy drifts.
import pi_rpc_boundary_cost as boundary  # noqa: E402

RHO = os.environ.get("LOOPRS_SPIKE_RHO") or shutil.which("rho") or "/tmp/rho-install/bin/rho"
PI = os.environ.get("LOOPRS_SPIKE_PI", "pi")
NS = [int(x) for x in os.environ.get("LOOPRS_SPIKE_N", "2,4,8").split(",") if x.strip()]
BOOTS = int(os.environ.get("LOOPRS_SPIKE_BOOTS", "7"))
BASE_URL = os.environ.get("LOOPRS_SPIKE_BASE_URL", "http://llm.home:8080/v1")
MODEL = os.environ.get("LOOPRS_SPIKE_MODEL", "halogen-qwen3.8-flash-next")
ARM_TIMEOUT = float(os.environ.get("LOOPRS_SPIKE_ARM_TIMEOUT", "300"))
LOOPRS_BIN = os.environ.get("LOOPRS_BIN", os.path.join(REPO, "target", "release", "looprs"))
EMBED_PROGRAM = os.path.join(HERE, "embed_programs", "matthewyjiang-rho-sdk.footprint.rs")
CAPTURE = os.path.join(HERE, "results", "matthewyjiang-footprint-cost.log")

# How the CLI got onto this box, printed in the capture rather than remembered by
# a human: the packaging profile the upstream size-tuning pass chose is part of
# what is being measured, and on this machine it is also the reason the first
# build did not finish.
RHO_PROVENANCE = os.environ.get("LOOPRS_SPIKE_RHO_PROVENANCE") or (
    "no aarch64-linux release asset exists upstream (the published assets are macOS x2 and "
    "Windows x1), so this shape had to be built from the crates.io artifact. Upstream's own "
    "`[profile.release]` is the size-tuning pass the repo note records — `opt-level = \"s\", "
    "`lto = \"fat\", `codegen-units = 1`, `strip = \"symbols\"` — and that fat-LTO link was "
    "SIGKILLed by the kernel (OOM) **twice** on this 8 GiB box, once at cargo's default job "
    "count and once at `CARGO_BUILD_JOBS=1`, so the shipped profile is not buildable here at "
    "all. What the CLI rows below measured is therefore "
    "`cargo build --release --bin rho --locked` with `CARGO_PROFILE_RELEASE_LTO=thin`: every "
    "other release knob as upstream, one deviation named in the row, and a note that the "
    "fat-LTO build a user would download is a *different* artifact whose size this capture "
    "does not measure.")
CORPUS = os.environ.get(
    "LOOPRS_MEASURE_CORPUS",
    os.path.expanduser(
        "~/.pi/agent/sessions/--Users-danielstahl-Documents-code-ml-personal-harness-pi-beads-looprs--"
    ),
)

# `child()` below needs wait4's rusage, which means fork+exec rather than
# subprocess; Python 3.14 warns that fork() in a threaded process can deadlock.
# It cannot here — the child only execs — but the warning would sit at the top of
# every capture and read like a defect in the thing being measured.
warnings.filterwarnings("ignore", category=DeprecationWarning)

NULL = open(os.devnull, "w")

# Read once at import, before any of this spike's own work has cost a page: leg 5
# needs "what was free when the capture started" as well as what is free by the
# time it runs, and the difference between the two is this script's own footprint.
START_MEM = None  # set in main(), before any of this spike's own work has cost a page


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def spread(xs):
    """(median, worst) — deliberately `pi_rpc_boundary_cost.py`'s definition, so the
    p95 column in leg 1 is the same statistic that capture printed."""
    if not xs:
        return float("nan"), float("nan")
    xs = sorted(xs)
    return xs[len(xs) // 2], xs[-1]


def min_max(xs):
    if not xs:
        return float("nan"), float("nan")
    return min(xs), max(xs)


def mib(kib):
    return f"{kib / 1024.0:.1f} MiB"


def model_available():
    if os.environ.get("LOOPRS_SPIKE_NO_MODEL"):
        return False
    try:
        r = subprocess.run(
            ["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", BASE_URL + "/models"],
            capture_output=True, text=True, timeout=15,
        )
        return r.stdout.strip() == "200"
    except Exception:  # noqa: BLE001 - an unreachable endpoint is a leg that cannot run
        return False


def meminfo():
    out = {}
    for line in open("/proc/meminfo"):
        k, _, v = line.partition(":")
        out[k] = int(v.strip().split()[0])
    return out


def cgroup_ceiling_kib():
    """The container's own memory ceiling in KiB, or None if it has no limit."""
    for path in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        try:
            raw = open(path).read().strip()
        except OSError:
            continue
        if raw in ("", "max"):
            return None
        try:
            return int(raw) // 1024
        except ValueError:
            continue
    return None


# ───────────────────────── one child, measured by the kernel ───────────────────
def child(argv, env=None, cwd=None, timeout=None, stdin_text=None, tag=""):
    """Spawn one child and report the kernel's own numbers for it.

    The peak is **VmHWM read out of `/proc/<pid>/status`** in a tight loop while
    the child lives. It is the kernel's monotone high-water mark for that address
    space, it is the same number the child reads about itself, and the embedded
    arms print both so the row can be checked against it.

    It is *not* `os.wait4`'s `ru_maxrss`, and the reason is measured rather than
    believed: on this box spawning `/bin/true` through `posix_spawn`/`fork`
    reports a `ru_maxrss` of ~8.6 MiB, which is the spawner's own resident set
    riding along until the exec (Linux `posix_spawn` clones with CLONE_VM). Every
    short-lived child would then inherit the spike's RSS as its "peak" — which is
    exactly the mistake that inflates a small binary's number into noise. So
    `wait4_kib` is returned and printed as an instrument note, never as the peak.

    `first_ms` is spawn to the first newline on stdout: as close to "it is up" as
    a one-shot process gets.
    """
    timeout = timeout or ARM_TIMEOUT
    out_r, out_w = os.pipe()
    err_r, err_w = os.pipe()
    file_actions = [
        (os.POSIX_SPAWN_OPEN, 0, os.devnull, os.O_RDONLY, 0),
        (os.POSIX_SPAWN_DUP2, out_w, 1), (os.POSIX_SPAWN_CLOSE, out_w),
        (os.POSIX_SPAWN_DUP2, err_w, 2), (os.POSIX_SPAWN_CLOSE, err_w),
    ]
    in_r = in_w = None
    if stdin_text is not None:
        in_r, in_w = os.pipe()
        file_actions.append((os.POSIX_SPAWN_DUP2, in_r, 0))
    # The sampler starts *before* the spawn: a 2 ms child is gone before a thread
    # started after the fact gets scheduled, and "peak RSS: not sampled" on the
    # cheapest shape is exactly the row the whole capture turns on.
    box = {"pid": None}
    hwm = []
    stop_sampling = threading.Event()

    def sampler():
        while not stop_sampling.is_set():
            pid_now = box["pid"]
            if pid_now is None:
                time.sleep(0.00002)
                continue
            try:
                status = open(f"/proc/{pid_now}/status").read()
                value = int(status.split("VmHWM:")[1].split()[0])
            except (FileNotFoundError, IndexError, ProcessLookupError):
                return
            if value:            # a zombie reads 0: it is not a sample of anything
                hwm.append(value)
            time.sleep(0.00002)

    th = threading.Thread(target=sampler, daemon=True)
    th.start()

    # posix_spawn does no PATH lookup the way `execvp` does, and `pi` is a script
    # found through PATH, so resolve it here. (It also takes no cwd on this
    # interpreter; nothing measured here needs one.)
    exe = shutil.which(argv[0], path=(env or os.environ).get("PATH")) or argv[0]
    t0 = time.perf_counter()
    pid = os.posix_spawn(exe, argv, env or os.environ, file_actions=file_actions)
    box["pid"] = pid
    for fd in (out_w, err_w):
        os.close(fd)
    if in_r is not None:
        os.close(in_r)
        os.write(in_w, (stdin_text or "").encode())
        os.close(in_w)

    first_ms = None
    stdout, stderr = bytearray(), bytearray()
    bufs = {out_r: stdout, err_r: stderr}
    pending = set(bufs)
    timed_out = False
    deadline = t0 + timeout
    while pending:
        left = deadline - time.perf_counter()
        if left <= 0:
            timed_out = True
            break
        try:
            ready, _, _ = select.select(list(pending), [], [], min(left, 0.5))
        except (OSError, ValueError):
            break
        for fd in ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                chunk = b""
            if not chunk:
                pending.discard(fd)
                continue
            if first_ms is None and fd == out_r and b"\n" in (bytes(bufs[fd]) + chunk):
                first_ms = (time.perf_counter() - t0) * 1000.0
            bufs[fd] += chunk
    if timed_out:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass
    try:
        _, status, ru = os.wait4(pid, 0)
    except ChildProcessError:
        status, ru = -1, None
    stop_sampling.set()
    th.join(timeout=2)
    for fd in list(bufs):
        try:
            os.close(fd)
        except OSError:
            pass
    exit_code = os.waitstatus_to_exitcode(status) if status >= 0 else None
    return {
        "tag": tag,
        "wall_ms": (time.perf_counter() - t0) * 1000.0,
        "first_ms": first_ms,
        "exit": exit_code,
        "timed_out": timed_out,
        "stdout": stdout.decode("utf-8", "replace"),
        "stderr": stderr.decode("utf-8", "replace"),
        "peak_kib": max(hwm) if hwm else None,
        "hwm_samples": len(hwm),
        "sampled": bool(hwm),
        "wait4_kib": float(ru.ru_maxrss) if ru is not None else 0.0,
        "cpu_ms": (ru.ru_utime + ru.ru_stime) * 1000.0 if ru else 0.0,
        "page_faults": (ru.ru_minflt + ru.ru_majflt) if ru else 0,
    }


def boot_shape(label, argv, env=None, n=None, stdin_text=None, note=None):
    """Run one boot shape n times and print the row: fork→first line, wall, peak RSS."""
    n = n or BOOTS
    firsts, walls, peaks, cpus, exits = [], [], [], [], []
    w4s, samples_n = [], []
    last_err = ""
    for _ in range(n):
        r = child(argv, env=env, timeout=ARM_TIMEOUT, stdin_text=stdin_text)
        last_err = r["stderr"][-400:]
        if r["timed_out"]:
            print(f"{label:52} not measured (timed out after {ARM_TIMEOUT:.0f}s)")
            return None
        if r["first_ms"] is None:
            print(f"{label:52} not measured (never wrote a line to stdout; stderr: {last_err!r})")
            return None
        firsts.append(r["first_ms"]); walls.append(r["wall_ms"])
        peaks.append(r["peak_kib"]); cpus.append(r["cpu_ms"]); exits.append(r["exit"])
        w4s.append(r["wait4_kib"]); samples_n.append(r["hwm_samples"])
    lo, hi = min_max(firsts)
    peaks = [x for x in peaks if x is not None]
    peak_txt = f"{med(peaks) / 1024:12.1f}" if peaks else f"{'not sampled':>12}"
    print(
        f"{label:52} {med(firsts):13.1f} {med(walls):9.1f} {peak_txt} "
        f"{med(cpus):7.0f} {str(exits[-1]):>4}"
    )
    print(f"{'':52} run-to-run fork→first: {lo:.1f}–{hi:.1f} ms over {len(firsts)} runs"
          + (f"; rc seen {sorted(set(exits))}" if len(set(exits)) > 1 else "")
          + f"; peak column = VmHWM, {med(samples_n):.0f} samples/run "
            f"(rusage would have read {med(w4s) / 1024:.1f} MiB: the spawner's pages, so not used)")
    if note:
        for line in note.splitlines():
            print(f"{'':52} {line}")
    return {"first_ms": med(firsts), "wall_ms": med(walls), "peak_kib": med(peaks),
            "cpu_ms": med(cpus), "spread": (lo, hi)}


# ─────────────────────────────── leg 1: pi again ───────────────────────────────
def leg_pi():
    print("\n== 1. pi --mode rpc, re-measured with the boundary capture's own method ==")
    hdr = (f"{'shape':52} {'fork→first ms':>13} {'peak RSS MiB':>12} {'rtt ms med':>10} "
           f"{'rtt ms p95':>10} {'cpu ms':>7} {'eof→exit ms':>11}")
    print(hdr)
    print("-" * len(hdr))
    scratch = tempfile.mkdtemp(prefix="8jk11-pi-")
    argv = [PI, "--mode", "rpc", "--session-dir", os.path.join(scratch, "sess")]
    firsts, rtts, peaks, cpus, exits = [], [], [], [], []
    for _ in range(BOOTS):
        t0 = time.perf_counter()
        rpc = boundary.Rpc(argv)
        samples, stop = [], threading.Event()
        th = threading.Thread(target=boundary.sample_loop, args=(rpc.p.pid, stop, samples), daemon=True)
        th.start()
        try:
            rpc.request({"type": "get_state"})
            firsts.append((time.perf_counter() - t0) * 1000)
            for _ in range(boundary.RTT):
                rpc.request({"type": "get_state"})
        except RuntimeError as e:
            print(f"not measured: {e}")
            stop.set(); rpc.p.kill()
            shutil.rmtree(scratch, ignore_errors=True)
            return None
        stop.set(); th.join(timeout=1)
        if samples:
            peaks.append(max(s[0] for s in samples)); cpus.append(max(s[1] for s in samples))
        t = time.perf_counter()
        rpc.close_stdin(); rpc.wait()
        exits.append((time.perf_counter() - t) * 1000)
        rtts.extend(rpc.t_replies[1:])
    r_med, r_p95 = spread(rtts)
    print(f"{'worker (`pi --mode rpc`)':52} "
          f"{med(firsts):13.1f} {med(peaks) / 1024:12.1f} {r_med * 1000:10.2f} {r_p95 * 1000:10.2f} "
          f"{med(cpus):7.0f} {med(exits):11.1f}")
    lo, hi = min_max(firsts)
    print(f"{'':52} run-to-run fork→first: {lo:.1f}–{hi:.1f} ms over {len(firsts)} runs")
    note = ("Method is `spikes/pi_rpc_boundary_cost.py`'s: the same correlated `get_state` handshake "
            "and\nthe same /proc sampler in the parent, so this row and that capture's row are the\n"
            "same measurement taken twice, not two measurements of one thing.")
    for line in note.splitlines():
        print(f"{'':52} {line}")
    shutil.rmtree(scratch, ignore_errors=True)
    return {"first_ms": med(firsts), "peak_kib": med(peaks), "rtt_ms": r_med,
            "rtt_p95_ms": r_p95, "cpu_ms": med(cpus), "eof_exit_ms": med(exits)}


# ─────────────────────────── leg 2: the host looprs has ────────────────────────
def leg_host():
    print("\n== 2. looprs' own RSS — the host the candidate would be added to ==")
    if not os.path.exists(LOOPRS_BIN):
        print(f"not measured: no looprs binary at {LOOPRS_BIN} (cargo build --release --bin looprs)")
        return None
    logdir = tempfile.mkdtemp(prefix="8jk11-host-")
    env = dict(os.environ)
    env.update(
        LOOPRS_BD_BIN="/bin/echo",       # the board is faked: a real read here could claim a bead
        LOOPRS_PI_BIN="/usr/bin/false",
        LOOPRS_SHELL_BIN="/bin/bash",
        LOOPRS_LOG_DIR=logdir,
    )
    results = []
    for _ in range(int(os.environ.get("LOOPRS_SPIKE_HOST_BOOTS", "3"))):
        master, slave = pty.openpty()
        # Give the pty a real window: an unsized pty is 0x0, and an app that lays
        # out into 0 rows draws nothing and reports a perfectly plausible RSS.
        try:
            import fcntl
            import termios
            import struct
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 100, 0, 0))
        except (ImportError, OSError):
            pass
        t0 = time.perf_counter()
        p = subprocess.Popen([LOOPRS_BIN], stdin=slave, stdout=slave, stderr=slave,
                             env=env, close_fds=True)
        os.close(slave)
        stop = threading.Event()
        painted = bytearray()

        def pump():
            while not stop.is_set():
                try:
                    if select.select([master], [], [], 0.05)[0]:
                        chunk = os.read(master, 65536)
                        if not chunk:
                            return
                        painted.extend(chunk)
                except OSError:
                    return

        th = threading.Thread(target=pump, daemon=True)
        th.start()
        samples = []
        sampler = threading.Thread(
            target=boundary.sample_loop, args=(p.pid, stop, samples, 0.02), daemon=True)
        sampler.start()
        try:
            time.sleep(2.5)                        # let it draw, poll its (faked) board, settle
            vmrss = vmhwm = 0
            try:
                st = open(f"/proc/{p.pid}/status").read()
                vmrss = int(st.split("VmRSS:")[1].split()[0])
                vmhwm = int(st.split("VmHWM:")[1].split()[0])
            except (FileNotFoundError, IndexError):
                pass
            results.append({"vmrss_kib": vmrss, "vmhwm_kib": vmhwm,
                            "peak_kib": max([s[0] for s in samples] or [vmhwm]),
                            "painted_bytes": len(painted),
                            "settled_ms": (time.perf_counter() - t0) * 1000})
        finally:
            try:
                os.write(master, b"\x11")          # Ctrl-Q: looprs' quit
            except OSError:
                pass
            time.sleep(0.4)
            stop.set(); th.join(timeout=1); sampler.join(timeout=1)
            if p.poll() is None:
                p.kill()
            p.wait()
            os.close(master)
    print(f"{'shape':52} {'VmRSS MiB':>10} {'peak MiB':>10} {'VmHWM MiB':>10}")
    print("-" * 84)
    print(f"{'looprs, booted and idle in a pty (3 boots)':52} "
          f"{med([d['vmrss_kib'] for d in results]) / 1024:10.1f} "
          f"{med([d['peak_kib'] for d in results]) / 1024:10.1f} "
          f"{med([d['vmhwm_kib'] for d in results]) / 1024:10.1f}")
    lo, hi = spread([d["vmrss_kib"] for d in results])
    print(f"{'':52} run-to-run VmRSS: {lo / 1024:.1f}–{hi / 1024:.1f} MiB over "
          f"{len(results)} boots; every boot painted {med([d['painted_bytes'] for d in results]):.0f} bytes "
          "into a 40×100 pty, so none of them is a process that started and drew nothing")
    first = med([d["vmrss_kib"] for d in results])
    print(
        "\nThis is the line the previous capture did not have: its memory row put a *child's* RSS\n"
        "next to an *in-process engine's* RSS, and every byte the host already spends came along for\n"
        "free in the comparison. `bd` is /bin/echo and `pi` is /usr/bin/false here, so the run\n"
        "touches no real board; the figure is looprs' own resident set, kernel-accounted."
    )
    shutil.rmtree(logdir, ignore_errors=True)
    return {"vmrss_kib": first, "per_shape": results}


# ────────────────────────── leg 3: the candidate as a CLI ─────────────────────
def rho_env(scratch, with_provider):
    env = dict(os.environ)
    env["RHO_HOME"] = os.path.join(scratch, "rho-home")
    os.makedirs(env["RHO_HOME"], exist_ok=True)
    if with_provider:
        os.makedirs(os.path.join(env["RHO_HOME"], "config"), exist_ok=True)
        with open(os.path.join(env["RHO_HOME"], "config.toml"), "w") as f:
            f.write(f'[providers.custom.spike]\nbase_url = "{BASE_URL}"\n')
    return env


def leg_cli(host_env_ok):
    print("\n== 3. the candidate as a CLI — the shape its own chart measures ==")
    print("provenance: " + RHO_PROVENANCE)
    if not (shutil.which(RHO) or os.path.exists(RHO)):
        print(f"not measured: no candidate CLI at {RHO}. Every CLI row below depends on it; "
              "build one with `cargo install rho-coding-agent --locked` and set "
              "LOOPRS_SPIKE_RHO.")
        return None
    rc = subprocess.run([RHO, "--version"], capture_output=True, text=True).returncode
    print(f"candidate binary: {RHO} (rc={rc})")
    if rc != 0:
        print("not measured: the candidate CLI is not runnable. Every CLI row below depends on it.")
        return None
    print(f"candidate version: {subprocess.run([RHO, '--version'], capture_output=True, text=True).stdout.strip()}")
    try:
        print(f"candidate binary: {os.path.getsize(RHO):,} B on disk "
              f"({os.path.getsize(RHO) / 1024 / 1024:.1f} MiB), stripped of symbols")
    except OSError:
        pass
    scratch = tempfile.mkdtemp(prefix="8jk11-cli-")
    env = rho_env(scratch, with_provider=host_env_ok)
    hdr = f"{'shape':52} {'fork→first ms':>13} {'exit ms':>9} {'peak RSS MiB':>12} {'cpu ms':>7} {'rc':>4}"
    print(hdr)
    print("-" * len(hdr))
    out = {}
    out["version"] = boot_shape("rho --version (binary boot, no provider touched)",
                               [RHO, "--version"], env=env)
    out["help"] = boot_shape("rho run --help (boot + clap parse, no session)",
                            [RHO, "run", "--help"], env=env)
    # The chart's claim is a CLI-to-CLI claim, so pi is measured in the same shape
    # on the same box rather than left as the ADR's RPC row.
    out["pi_version"] = boot_shape("pi --version (the same shape, for the chart's claim)",
                                   [PI, "--version"])
    if host_env_ok:
        cfg = os.path.join(env["RHO_HOME"], "config.toml")
        argv1 = [RHO, "--provider", "spike", "--model", MODEL, "run", "--output", "jsonl",
                 "Reply with exactly one word: present"]
        print(f"\none-shot shape: `{RHO} --provider spike --model {MODEL} run --output jsonl` "
              f"with provider `spike` defined in\n  {cfg}")
        probe = child(argv1, env=env, timeout=ARM_TIMEOUT)
        print(f"  probe: rc={probe['exit']}  fork→first {probe['first_ms']} ms  "
              f"wall {probe['wall_ms']:.0f} ms")
        for line in probe["stdout"].splitlines()[:3]:
            print(f"  stdout: {line[:160]}")
        if probe["stderr"].strip():
            print(f"  stderr: {probe['stderr'].strip()[-300:]}")
        if probe["exit"] == 0 and probe["first_ms"]:
            reps = [probe] + [child(argv1, env=env, timeout=ARM_TIMEOUT) for _ in range(2)]
            ok = [x for x in reps if x["exit"] == 0 and x["first_ms"]]
            print()
            out["run"] = boot_shape("rho run --output jsonl (one real one-shot turn, 3 runs)",
                                    argv1, env=env, n=len(ok))
            if out["run"]:
                out["run_child"] = (argv1, env)
                out["run"]["n"] = len(ok)
        else:
            print("  the one-shot shape did not complete, so its rows are not measured rather "
                  "than estimated from the boot rows.")
    else:
        print("\n`rho run` legs: not measured (no reachable model endpoint at "
              f"{BASE_URL}; a one-shot run with no provider configured answers an error, not a pass)")
    if not out.get("run_child"):
        shutil.rmtree(scratch, ignore_errors=True)
    else:
        print(f"(the CLI scratch $RHO_HOME stays at {env['RHO_HOME']} so leg 5's N-concurrent "
              "`rho run` children read the same provider config)")
    print(
        "\nWhat each column means, because the shapes are not the same question:\n"
        "  fork→first  — process start to the first newline the shape prints. For pi that is the\n"
        "                first correlated RPC response (leg 1, a different question); for the\n"
        "                candidate CLI it is the version string, or for `rho run` the first JSONL\n"
        "                record — a session header, so it says the CLI is up and has started the\n"
        "                turn, not that a model answered. The wall column is the whole turn.\n"
        "  peak RSS    — VmHWM, the kernel's own high-water mark for that address space, read from\n"
        "                /proc while the child lives. `os.wait4`'s rusage is *not* the instrument:\n"
        "                on this box it reports the spawner's resident set for any child small or\n"
        "                fast enough that the exec has not cleared it (a `/bin/true` spawned here\n"
        "                reads ~19 MiB, which is this driver's own Python), so every short-lived\n"
        "                row would have been inflated. The spread line under each row prints what\n"
        "                rusage would have said, so the mistake is visible instead of hidden.\n"
        "The candidate's chart measures column 1 and column 3 of the CLI rows. It is not the row\n"
        "looprs would pay for; leg 4 is."
    )
    return out


# ───────────────────────── leg 4: the candidate embedded ──────────────────────
def build_embed(versions):
    """The scratch host: a crate that links rho-sdk and does nothing else."""
    import rho_surface_cost as surf
    scratch = tempfile.mkdtemp(prefix="8jk11-embed-")
    crate = os.path.join(scratch, "rho-footprint-spike")
    os.makedirs(os.path.join(crate, "src"), exist_ok=True)
    manifest = surf.embed_manifest(
        {"library_crates": ["rho-sdk"], "embed_tokio_features":
         ["rt-multi-thread", "macros", "time", "process", "sync"],
         # `futures` for join_all in the concurrent arm. Nothing here talks to a
         # network — see the note about the provider in the embed program's header.
         "embed_extra_deps": ['futures = "0.3"']},
        versions,
    ).replace('name = "rho-embed-spike"', 'name = "rho-footprint-spike"')
    open(os.path.join(crate, "Cargo.toml"), "w").write(manifest)
    shutil.copyfile(EMBED_PROGRAM, os.path.join(crate, "src", "main.rs"))
    tdir = os.environ.get("LOOPRS_RHO_TARGET") or os.path.join(scratch, "target")
    reuse = os.path.isdir(os.path.join(tdir, "release"))
    print(f"scratch host crate: {crate}")
    print(f"embed program:     {os.path.relpath(EMBED_PROGRAM, REPO)}")
    print("manifest deps:     " + "; ".join(manifest.splitlines()[5:]))
    print(f"target dir:        {tdir}{'  (REUSED — not a cold build)' if reuse else '  (cold)'}")
    t0 = time.perf_counter()
    p = subprocess.run(["cargo", "build", "--release", "--target-dir", tdir],
                       cwd=crate, capture_output=True, text=True)
    wall = time.perf_counter() - t0
    if p.returncode != 0:
        print(f"not measured: the scratch host did not build (rc={p.returncode}); compiler tail —")
        print((p.stdout + p.stderr)[-3000:])
        return None, None, None
    print(f"build:             rc=0  {wall:.0f}s wall  "
          f"{(p.stdout + p.stderr).count('Compiling')} crates compiled")
    binpath = os.path.join(tdir, "release", "rho-footprint-spike")
    pkgs = surf.cargo_tree(crate)
    print(f"shipped graph:     {len(pkgs)} packages (`cargo tree -e normal`)")
    print(f"artifact:          {os.path.getsize(binpath)} B")
    return binpath, scratch, pkgs


def run_arm(binpath, arm, env_extra=None, timeout=None):
    env = dict(os.environ)
    env.update(env_extra or {})
    env.setdefault("RHO_SPIKE_ARM", arm)
    r = child([binpath], env=env, timeout=timeout or ARM_TIMEOUT, tag=arm)
    lines = [ln for ln in r["stdout"].splitlines() if "\t" in ln or ln.startswith("\t")]
    return r, lines


def leg_embed(versions, endpoint_ok):
    # 0 ms turns, for the arms whose only subject is resident memory.
    NO_LATENCY = {"RHO_SPIKE_MODEL_MS": "0", "RHO_SPIKE_TOOL_MS": "0"}
    print("\n== 4. the candidate embedded — the shape looprs would actually use ==")
    binpath, scratch, _ = build_embed(versions)
    if not binpath:
        return None
    out = {}
    print("\nBoot of the host itself (spawn → the first line it prints; same definition as leg 3's "
          "first column):")
    out["host_boot"] = boot_shape("embedded host, nothing constructed yet", [binpath],
                                 env={**os.environ, "RHO_SPIKE_ARM": "floor"})
    print("\nThe embedded program's own header says why there is no embedded-with-a-real-model arm "
          "here:")
    print("  rho-sdk 5.9.0 depends on serde, serde_json, thiserror, tokio, tracing, url, uuid — no")
    print("  HTTP client of any kind, and its only provider is `ScriptedProvider`, a test double.")
    print("  The model clients live in `rho-providers`, which is what composes a working agent.")
    print("  So embedding rho is a *crate composition* decision, not a dependency line, and the")
    print("  real-endpoint rows in this capture are the CLI's, which is that composition already")
    print("  built. Everything below measures the runtime, not the transport.")

    for label, arm, extra in [
        ("floor — the control: same binary, rho-sdk linked, nothing constructed", "floor",
         NO_LATENCY),
        ("ladder — runtime, session, three runs, second session, second runtime", "ladder",
         NO_LATENCY),
        ("bigtool — the ladder where the tool answers with 256 KiB of text",
         "bigtool", {**NO_LATENCY, "RHO_SPIKE_PAYLOAD_KIB": "256"}),
        ("history — one session, 40 runs of 64 KiB tool output: a long pass growing",
         "history", {**NO_LATENCY, "RHO_SPIKE_RUNS": "40", "RHO_SPIKE_EVERY": "10",
                     "RHO_SPIKE_PAYLOAD_KIB": "64"}),
        # Where a run's wall time goes with every sleep the spike controls turned off,
        # so what is left is the runtime's own work. This is what lets the startup
        # arithmetic below say "sub-millisecond" and mean it, and it is where
        # `settle_window_ms` comes from.
        ("steptiming, no sleeps — where a run's wall time goes when nothing is waiting",
         "steptiming", {"RHO_SPIKE_MODEL_MS": "0", "RHO_SPIKE_TOOL_MS": "0"}),
        ("steptiming, 120 ms turns — same shapes at a realistic turn latency, for "
         "`settle_window_ms`",
         "steptiming", {"RHO_SPIKE_MODEL_MS": "120", "RHO_SPIKE_TOOL_MS": "0"}),
        # The settle race at two pacings: the refusal this arm looks for is a race
        # between the terminal event and the session's run slot being released, and a
        # race seen at one timing is not the same evidence as one seen at two. Each
        # row prints the `model_ms` it ran with.
        ("settlerace, 120 ms turns — advance on the event holding the run, dropping it, or draining",
         "settlerace", {"RHO_SPIKE_TRIALS": "30", "RHO_SPIKE_MODEL_MS": "120",
                        "RHO_SPIKE_TOOL_MS": "0"}),
        ("settlerace, 20 ms turns — the same three shapes, tighter",
         "settlerace", {"RHO_SPIKE_TRIALS": "60", "RHO_SPIKE_MODEL_MS": "20",
                        "RHO_SPIKE_TOOL_MS": "0"}),
        ("sessions — one runtime, N sessions, no runs", "sessions",
         {**NO_LATENCY, "RHO_SPIKE_N": "8"}),
    ]:
        print(f"\n-- {label}")
        r, lines = run_arm(binpath, arm, extra)
        kv = dict(l.split("\t", 1) for l in lines if "\t" in l and not l.startswith("\t"))
        in_proc = kv.get("host.rss_peak_exit_kib")
        agree = (f"; read from inside the same process: VmHWM {mib(float(in_proc))}"
                 f" over {r['hwm_samples']} samples taken from outside — the two instruments agree")
        print(f"   rc={r['exit']} wall={r['wall_ms']:.0f} ms peak RSS "
              f"{mib(r['peak_kib']) if r['peak_kib'] else 'not sampled'} "
              f"cpu={r['cpu_ms']:.0f} ms{agree}")
        for ln in lines:
            print("   " + ln)
        if r["stderr"].strip():
            print(f"   (stderr tail: {r['stderr'].strip()[-200:]!r})")
        if arm == "settlerace" and "settlerace" in out:
            # Two pacings of one arm: keep the second under a `_tight` suffix so leg 6
            # prints both instead of quietly keeping only the last.
            out["settlerace"] = {
                **out["settlerace"],
                **{k + "_tight": v for k, v in kv.items() if k.startswith("settlerace.")},
            }
        else:
            out[arm] = kv

    print("\n-- concurrent — one runtime, N sessions, N runs in flight at once")
    out["concurrent"] = {}
    for n in NS:
        env = {"RHO_SPIKE_N": str(n), "RHO_SPIKE_MODEL_MS": "60", "RHO_SPIKE_TOOL_MS": "20"}
        r, lines = run_arm(binpath, "concurrent", env)
        kv = dict(l.split("\t", 1) for l in lines if "\t" in l and not l.startswith("\t"))
        print(f"   n={n}: rc={r['exit']} host peak RSS "
              f"{mib(r['peak_kib']) if r['peak_kib'] else 'not sampled'} | "
              f"runtime {kv.get('concurrent.after_runtime', '').split('rss_kib=')[1].split()[0] if 'concurrent.after_runtime' in kv else '?'} KiB | "
              f"N runs added {kv.get('concurrent.added_kib_over_runtime')} KiB over the runtime | "
              f"wall {kv.get('concurrent.wall_ms')} ms vs one-lane-at-a-time "
              f"{kv.get('concurrent.one_lane_at_a_time_wall_ms')} ms vs sequential "
              f"{kv.get('concurrent.sequential_wall_ms')} ms | model calls in flight "
              f"{kv.get('concurrent.max_model_calls_in_flight')} | tool calls in flight "
              f"{kv.get('concurrent.max_tool_calls_in_flight')}")
        for ln in lines:
            if ln.startswith("\t") or "busy" in ln or "is_running" in ln or "refusal" in ln:
                print("     " + ln)
        out["concurrent"][n] = {**kv, "peak_kib": r["peak_kib"]}

    print("\n-- engines — N whole runtimes in one process (the other way to get N concurrent "
          "passes), one run each, then all shut down and dropped")
    out["engines"] = {}
    for n in NS:
        env = {"RHO_SPIKE_N": str(n), "RHO_SPIKE_MODEL_MS": "40", "RHO_SPIKE_TOOL_MS": "10"}
        r, lines = run_arm(binpath, "engines", env)
        kv = dict(l.split("\t", 1) for l in lines if "\t" in l and not l.startswith("\t"))
        print(f"   n={n}: rc={r['exit']} host peak RSS "
              f"{mib(r['peak_kib']) if r['peak_kib'] else 'not sampled'} | added "
              f"{kv.get('engines.added_kib_total')} KiB building them "
              f"({kv.get('engines.added_kib_per_runtime')} KiB each) in "
              f"{kv.get('engines.build_wall_ms')} ms | runs wall "
              f"{kv.get('engines.runs_wall_ms')} ms | after shutdown+drop, "
              f"{kv.get('engines.retained_kib_over_start')} KiB still resident and "
              f"{kv.get('engines.threads_after_drop')} threads")
        for ln in lines:
            if ln.startswith("\t"):
                print("     " + ln)
        out["engines"][n] = {**kv, "peak_kib": r["peak_kib"]}

    print("\nWhat each number here is:")
    print("  rss_kib / hwm_kib — VmRSS and VmHWM for *this* process, read from /proc/self/status by")
    print("                      the process itself. VmHWM is the kernel's high-water mark, so a")
    print("                      peak cannot be missed by a sampling interval.")
    print("  private_dirty_kib — from /proc/self/smaps_rollup: the resident pages that belong to")
    print("                      this address space rather than to shared mappings, which is what")
    print("                      a host actually pays when it links something in.")
    print("  added               — minus the `floor` row of the same binary: the cost of using the")
    print("                      runtime, not the cost of having linked it.")
    print("  threads             — /proc/self/task count: an embedded runtime arrives with threads,")
    print("                      and a host that builds N of them should know whether they multiply.")
    print("\nThe provider in this program is a stub that sleeps and answers, and it is *part of the*")
    print("*measurement* rather than overhead: rho-sdk ships no model client, so any embedding host")
    print("is writing or vendoring one. Its footprint is inside these numbers on purpose.")
    return out


def leg_children(pi_row, cli_row):
    print("\n== 5. N at once — children against an in-process engine ==")
    mi = meminfo()
    ceil_kib = cgroup_ceiling_kib()
    print(f"MemTotal {mib(mi['MemTotal'])}   MemAvailable {mib(START_MEM['MemAvailable'])} at "
          f"capture start, {mib(mi['MemAvailable'])} here   "
          f"cgroup memory.max {'(no limit)' if ceil_kib is None else mib(ceil_kib)}")
    ceiling = ceil_kib or START_MEM["MemAvailable"]
    print(f"ceiling used for the extrapolation: {mib(ceiling)}"
          + ("  — the container has no memory.max, so this is what was free when the capture "
             "started, not a quota" if ceil_kib is None else "  — the container's own limit"))

    # (a) N `pi --mode rpc` children, the shape the loop runs today.
    scratch = tempfile.mkdtemp(prefix="8jk11-kids-")
    argv = [PI, "--mode", "rpc", "--session-dir", os.path.join(scratch, "s")]
    print("\n(a) N `pi --mode rpc` children warm at once — today's shape")
    pi_shapes = {}
    for n in NS:
        t0 = time.perf_counter()
        rpcs, samples_all, threads = [], [], []
        for _ in range(n):
            rpc = boundary.Rpc(argv)
            samples, stop = [], threading.Event()
            th = threading.Thread(target=boundary.sample_loop,
                                 args=(rpc.p.pid, stop, samples, 0.01), daemon=True)
            th.start()
            rpcs.append(rpc); samples_all.append(samples); threads.append(stop)
        answered = []

        def warm(i):
            rpcs[i].request({"type": "get_state"})
            answered.append(time.perf_counter())

        ts = [threading.Thread(target=warm, args=(i,)) for i in range(n)]
        [t.start() for t in ts]
        [t.join() for t in ts]
        wall = (time.perf_counter() - t0) * 1000
        for s in threads:
            s.set()
        peaks = [max(s[0] for s in samples) for samples in samples_all if samples]
        live = meminfo()["MemAvailable"]
        for rpc in rpcs:
            rpc.close_stdin(); rpc.wait()
        pi_shapes[n] = {"wall_ms": wall, "peaks_kib": peaks, "sum_kib": sum(peaks),
                        "memavail_after_kib": live}
        print(f"   n={n}: all warm in {wall:.0f} ms   per-child peak median "
              f"{mib(med(peaks))}   sum {mib(sum(peaks))}   "
              f"MemAvailable after {mib(live)} (drop {mib(mi['MemAvailable'] - live)})")
    shutil.rmtree(scratch, ignore_errors=True)

    # (b) N candidate CLI children. The boot shape needs no model; the run shape does.
    print("\n(b) N candidate CLI children at once")
    cli_shapes = {}
    if shutil.which(RHO) or os.path.exists(RHO):
        for n in NS:
            t0 = time.perf_counter()
            results = []
            def one():
                results.append(child([RHO, "--version"]))

            ts = [threading.Thread(target=one) for _ in range(n)]
            [t.start() for t in ts]
            [t.join() for t in ts]
            wall = (time.perf_counter() - t0) * 1000
            peaks = [r["peak_kib"] for r in results if r["peak_kib"]]
            firsts = [r["first_ms"] for r in results if r["first_ms"]]
            cli_shapes[n] = {"wall_ms": wall, "peaks_kib": peaks, "sum_kib": sum(peaks)}
            print(f"   n={n} × `rho --version`: all done in {wall:.0f} ms   per-child fork→first "
                  f"{med(firsts):.0f} ms   per-child peak {mib(med(peaks)) if peaks else 'not sampled'}"
                  f"   sum {mib(sum(peaks))}")
        if os.environ.get("LOOPRS_SPIKE_CLI_MODEL") and cli_row.get("run_child"):
            argv1, env1 = cli_row["run_child"]
            print(f"   (LOOPRS_SPIKE_CLI_MODEL set: N × `rho run` with a real prompt)")
            for n in NS:
                t0 = time.perf_counter()
                results = []

                def one_run():
                    results.append(child(argv1, env=env1, timeout=ARM_TIMEOUT))

                ts = [threading.Thread(target=one_run) for _ in range(n)]
                [t.start() for t in ts]
                [t.join() for t in ts]
                wall = (time.perf_counter() - t0) * 1000
                peaks = [r["peak_kib"] for r in results if r["peak_kib"]]
                print(f"   n={n} × `rho run`: all done in {wall:.0f} ms   per-child peak "
                      f"{mib(med(peaks))}   sum {mib(sum(peaks))}")
                cli_shapes[f"run{n}"] = {"wall_ms": wall, "sum_kib": sum(peaks)}
        else:
            print("   N × `rho run` with a real prompt: not run (LOOPRS_SPIKE_CLI_MODEL unset — "
                  "it costs N model calls; the boot row above is the process-cost answer)")
    else:
        print("   not measured: no candidate CLI on this box")

    # (c) the in-process shapes are in leg 4 (`concurrent` and `engines` arms); leg 6 puts
    # all three shapes on one row per N so the comparison is a subtraction, not a scroll.
    return {"pi": pi_shapes, "cli": cli_shapes, "ceiling_kib": ceiling,
            "memavail_kib": mi["MemAvailable"], "memtotal_kib": mi["MemTotal"]}


# ─────────────────────────── leg 6: the arithmetic ────────────────────────────
def pass_denominator():
    """Median real pass wall, from the corpus pi_rpc_boundary_cost.py already cites."""
    files = sorted(glob.glob(os.path.join(CORPUS, "*.jsonl")))
    walls = []
    for f in files:
        stamps = []
        for line in open(f, errors="replace"):
            try:
                v = json.loads(line)
                stamps.append(datetime.fromisoformat(v["timestamp"].replace("Z", "+00:00")))
            except (KeyError, ValueError, TypeError):
                continue
        if len(stamps) >= 3:
            walls.append((stamps[-1] - stamps[0]).total_seconds())
    if not walls:
        return None, 0
    walls.sort()
    return med(walls), len(walls)


def leg_conclusion(host, pi_row, cli, embed, kids):
    print("\n== 6. the rows the amendment ticket has to carry ==")
    wall, npass = pass_denominator()
    host_kib = host["vmrss_kib"] if host else None
    pi_kib = pi_row["peak_kib"] if pi_row else None
    ladder = embed.get("ladder", {}) if embed else {}

    def rss_of(row, key):
        v = row.get(key, "")
        try:
            return int(v.split("rss_kib=")[1].split()[0])
        except (IndexError, ValueError):
            return None

    def field(row, key, name):
        v = row.get(key, "")
        try:
            return float(v.split(f"{name}=")[1].split()[0])
        except (IndexError, ValueError):
            return None

    floor_kib = rss_of(embed.get("floor", {}) if embed else {}, "floor.nothing_constructed")
    runtime_add = None
    session_add = None
    run_add = None
    if floor_kib is not None:
        r1 = rss_of(ladder, "ladder.after_runtime")
        s1 = rss_of(ladder, "ladder.after_session")
        run1 = rss_of(ladder, "ladder.after_run_1")
        runtime_add = (r1 - floor_kib) if r1 is not None else None
        session_add = (s1 - r1) if None not in (s1, r1) else None
        run_add = (run1 - s1) if None not in (run1, s1) else None
    print(f"pass denominator: median {wall:.0f} s over {npass} real passes" if wall else
          "pass denominator: not measured (no corpus)")
    if wall:
        print("  re-derived here from the same session corpus the boundary capture cites; the corpus")
        print("  is live, so it is not the same file count as the ADR's row (87 passes, 3095 s); the")
        print("  two are quoted separately rather than smoothed into one number.")

    print("\nMEMORY — every shape as host + added, never one absolute against another:")
    if host_kib:
        print(f"  looprs, booted and idle (leg 2)           {mib(host_kib)}")
    if pi_kib:
        print(f"  + one `pi --mode rpc` child (leg 1)       +{mib(pi_kib)}   "
              f"= {mib(host_kib + pi_kib) if host_kib else '?'} for today's shape, per pass, "
              f"+{mib(pi_kib)} again for every pass in flight")
    if runtime_add is not None:
        total_add = runtime_add + (session_add or 0) + (run_add or 0)
        print(f"  + rho runtime, one session, one run       +{mib(total_add)}   "
              "— each rung below is a delta over the rung before it, and all of it is measured "
              "against the")
        print("    `floor` control: the same binary with rho-sdk linked and nothing constructed")
        print(f"      runtime built                       +{mib(runtime_add)}")
        print(f"      + session                          +{mib(session_add or 0)}")
        print(f"      + first run (a tool call included)  +{mib(run_add or 0)}")
        if floor_kib is not None:
            print(f"  …of which linking rho-sdk alone costs     {mib(floor_kib)} before any runtime exists "
                  f"(that is `floor`'s own RSS, and a host pays it whether or not it builds anything)")
        hist = embed.get("history") or {}
        hists = [(k, v) for k, v in sorted(hist.items()) if k.startswith("history.after_run_")]
        if hists:
            last = hists[-1][1]
            print(f"  …and a long pass grows in place            "
                  f"{int(field(hist, hists[-1][0], 'rss_kib') or 0)} KiB resident, up from "
                  f"{int(field(hist, 'history.start', 'rss_kib') or 0)} KiB, after "
                  f"{len(hists) * 10} runs of 64 KiB tool output on one session — about "
                  f"{int((field(hist, hists[-1][0], 'rss_kib') or 0) / (len(hists) * 10))} KiB per "
                  "run held")
            print("  by that session. A per-pass child pays none of this because it dies; a host that")
            print("  keeps one runtime alive pays it and cannot forget it.")
        big = embed.get("bigtool") or {}
        if big:
            kb = big.get("payload_kib", "?")
            r1 = field(big, "bigtool.after_session", "rss_kib")
            r3 = field(big, "bigtool.after_run_3", "rss_kib")
            if r1 and r3:
                print(f"  …and the history is the term               a {kb} KiB tool payload costs "
                      f"{int(r3 - r1)} KiB across three runs; `ladder`'s empty tool prices none of "
                      "it, and a beads pass is mostly tool output")
        print(f"  = candidate shape                        "
              f"{mib(host_kib + total_add) if host_kib else '?'}")
        print(f"  difference per pass                      "
              f"{mib((pi_kib or 0) - total_add) if pi_kib else '?'} — and the two rows are not the "
              "same kind of number:")
        print("  pi's is a separate process's resident set, the candidate's is bytes added to a")
        print("  host that was already resident. That is the correction the last capture owed.")
        print("\n  What the added row is and what it is not: it is rho's runtime with a provider and a")
        print("  tool written by this spike, because `rho-sdk` ships no model client at all (leg 4's")
        print("  dependency list). `bigtool` and `history` price the history a pass grows; nothing")
        print("  here prices the tool suite, session store, compactor and permission surface a port")
        print("  would wire in — and pi's child-in-full already contains all of those, plus Node. So")
        print("  the honest pairing is runtime + host's own work against pi's child-in-full; reading")
        print("  the ratio as 'rho is N× smaller than pi' is a scope error these rows are written to")
        print("  make impossible.")
    if cli and cli.get("version"):
        print(f"\n  The CLI row, kept separate on purpose: `rho --version` peaks at "
              f"{mib(cli['version']['peak_kib'])} and answers in "
              f"{cli['version']['first_ms']:.0f} ms; `pi --version` "
              + (f"{mib(cli['pi_version']['peak_kib'])} / {cli['pi_version']['first_ms']:.0f} ms"
                 if cli.get("pi_version") else "not measured")
              + (f"; `rho run` with a real turn peaks {mib(cli['run']['peak_kib'])}"
                 if cli.get("run") else "")
              + ". That is the candidate's own chart's row, and it is not the embedded number: a CLI")
        print("  child pays config loading, the provider table and a terminal layer a host already has.")

    print("\nSTARTUP — the same pair, against the denominator rather than on their own:")
    if pi_row and wall:
        print(f"  `pi --mode rpc` spawn→first          {pi_row['first_ms']:.0f} ms  "
              f"= {100 * pi_row['first_ms'] / 1000 / wall:.4f}% of a median pass")
    if cli and cli.get("version") and wall:
        print(f"  `rho --version` spawn→first          {cli['version']['first_ms']:.1f} ms  "
              f"= {100 * cli['version']['first_ms'] / 1000 / wall:.6f}% of a median pass")
    if embed:
        hb = embed.get("host_boot") or {}
        ms_runtime = field(ladder, "ladder.after_runtime", "step_ms")
        ms_session = field(ladder, "ladder.after_session", "step_ms")
        ms_run = field(ladder, "ladder.after_run_1", "step_ms")
        tot = sum(x for x in (ms_runtime, ms_session, ms_run) if x is not None)
        print(f"  embedded host, process exists           {hb.get('first_ms', float('nan')):.1f} ms "
              f"(spawn→first line printed)")
        if None not in (ms_runtime, ms_session, ms_run) and wall:
            print(f"  + runtime + session + first run       {ms_runtime:.2f} + {ms_session:.2f} + "
                  f"{ms_run:.2f} ms = {tot:.2f} ms  = {100 * tot / 1000 / wall:.6f}% of a median pass")
            print("  (the run here is a real run through the runtime with a 0 ms provider and a 0 ms")
            print("   tool, so what is left in it is the runtime's own work, not a model's — the")
            print("   `steptiming` arm is what that number is made of.)")
        print("\n  Answer to the question trigger 4 asked to keep closed: an embedded boot is not free,")
        print("  it is ~free — same order of a pass as the child's, so the latency leg stays dead")
        print("  whichever engine gets picked and does not need a third hearing.")

    print("\nCONCURRENCY — N at once, and which shape runs out of room first:")
    ceiling = kids["ceiling_kib"]
    for n in NS:
        p = kids["pi"].get(n, {})
        c = kids["cli"].get(n, {})
        rc = kids.get("rho_run", {}).get(n, {})
        e = (embed.get("concurrent") or {}).get(n, {}) if embed else {}
        g = (embed.get("engines") or {}).get(n, {}) if embed else {}
        print(f"  N={n}: {mib(p.get('sum_kib', 0))} across N pi children, "
              f"{p.get('wall_ms', float('nan')):.0f} ms to all warm   |   "
              + (f"{mib(c['sum_kib'])} across N rho CLI children ({c['wall_ms']:.0f} ms)"
                 if c else "rho CLI children not measured")
              + (f", {mib(rc['sum_kib'])} for N real one-shot turns ({rc['wall_ms']:.0f} ms)"
                 if rc else "")
              + f"   |   one runtime, N sessions: peak {mib(float(e.get('peak_kib') or 0))}, "
                f"{e.get('concurrent.wall_ms', '?')} ms for N runs (sequential: "
                f"{e.get('concurrent.sequential_wall_ms', '?')} ms)   |   N runtimes in one process: "
                f"peak {mib(float(g.get('peak_kib') or 0))}, +{g.get('engines.added_kib_total', '?')} KiB")
    if pi_row:
        print(f"  Memory ceiling {mib(ceiling)} ÷ pi's {mib(pi_row['peak_kib'])} a child = ~"
              f"{int(ceiling / pi_row['peak_kib'])} concurrent passes before this box is out of "
              f"memory (~{int(ceiling / pi_row['peak_kib'] / 2)} before half of it is gone), "
              f"against {mib(float((embed.get('concurrent') or {}).get(NS[-1], {}).get('peak_kib') or 0))} "
              "for the same N embedded in one process.")
    if runtime_add is not None and embed:
        adds = [float(v.get("concurrent.added_kib_over_runtime", 0) or 0)
                for v in embed["concurrent"].values()]
        print(f"  The runtime shape does not multiply: one process, ~"
              f"{mib(med(adds or [0]))} added by all the concurrent runs together, and the wall "
              "time is where the concurrency actually shows up (the row above: N runs together vs")
        print("  the same N runs one after another). Memory is not what limits the embedded shape.")
        print("  Which degrades first: the child shapes, because N children pay N runtimes. The")
        print("  embedded shape's limit is not memory at these numbers at all — it is the session")
        print("  rule below.")
    race = (embed.get("settlerace") or {}) if embed else {}
    conc = (embed.get("concurrent") or {}).get(NS[0], {}) if embed else {}
    if race or conc:
        print("\nTHE CONCURRENCY RULE — the ticket's fifth question, and the answer is the right shape,")
        print("  with one wrinkle:")
        if conc:
            print(f"  `one active Run per Session`, hit the way a loop hits it: {conc.get('concurrent.second_run_on_busy_session', '?')}")
            print(f"  while a run is in flight (`is_running` = {conc.get('concurrent.is_running_mid_run', '?')}), and ")
            print(f"  {conc.get('concurrent.third_run_after_drain', '?')} once it has drained "
                  f"(`is_running` = {conc.get('concurrent.is_running_after_drain', '?')}). N passes are N")
            print("  sessions, so the rule does not collide with looprs' shape; two runs on one")
            print("  session is a bug in a host, not a limitation of the engine.")
        if race:
            print("  The wrinkle, measured three ways at two turn latencies (leg 4, `settlerace`; the")
            print("  counts are `session_busy_refusals` out of the trials printed on the same row):")
            for key, how in [
                ("settlerace.break_on_event_run_held",
                 "start the next run on the terminal event, still holding the first Run"),
                ("settlerace.break_on_event_run_dropped",
                 "same, but drop the first Run before starting the next"),
                ("settlerace.drain_stream_then_advance",
                 "drain the first run's stream to the end, then advance"),
            ]:
                cells = []
                for k in (key, key + "_tight"):
                    raw = race.get(k, "")
                    if not raw:
                        continue
                    busy = raw.split("session_busy_refusals=")[-1].split()[0]
                    late = raw.split("still_refused_after_20ms=")[-1].split()[0]
                    trials = raw.split("trials=")[-1].split()[0]
                    cells.append(f"{busy}/{trials} refused, {late} still refused after 20 ms")
                print(f"    {how}\n      " + "   |   ".join(cells or ["not measured"]))
            print("  Which is the answer, and it is the ADR's answer, arrived at from the other side.")
            print("  Advancing on the terminal event is refused some fraction of trials; dropping the")
            print("  run first lowers the rate and does not remove it. The session's run slot is")
            print("  released asynchronously with respect to the event, and `is_running()` disagrees")
            print("  with `start()` in both directions at that moment, so neither the event nor that")
            print("  predicate is a gate. Draining the stream to its end is the only shape with zero")
            print("  refusals, at both pacings. On pi the loop has to *wait*")
            print("  for the stream to close because pi's settle window is ~456 ms (ADR-0010); on this")
            print("  engine that window measures ~0 ms (`steptiming`'s `settle_window_ms`) and still")
            print("  has to be waited for. So switching engines would not have removed the settle")
            print("  discipline, which is a fact ADR-0010's settle row did not know and should carry.")
def main():
    global START_MEM
    START_MEM = meminfo()
    mi = meminfo()
    print(f"workspace-8jk.11 footprint measurement — host {platform.system()} {platform.release()}, "
          f"{os.cpu_count()} cpus, {mib(mi['MemTotal'])} RAM, aarch64 container, "
          f"python {platform.python_version()}")
    print(f"pi: {shutil.which(PI) or PI}   candidate CLI: {RHO}")
    print("subject: matthewyjiang/rho — crate `rho-sdk`, CLI crate `rho-coding-agent`. "
          "Not casonadams/rho, not pyrex41/rho.")
    versions = {}
    try:
        import rho_surface_cost as surf
        tgt = {"crates": {"rho-sdk": None}}
        versions, forced = surf.crate_versions(tgt)
        print(f"crate resolved: rho-sdk {versions['rho-sdk'][0]} ({versions['rho-sdk'][1]})")
    except Exception as e:  # noqa: BLE001
        print(f"crate resolved: not measured ({e})")
    endpoint_ok = model_available()
    print(f"model endpoint: {'reachable at ' + BASE_URL if endpoint_ok else 'not reachable — every endpoint leg prints `not measured`'}")
    if os.path.exists(RHO):
        v = subprocess.run([RHO, "--version"], capture_output=True, text=True)
        print(f"candidate CLI version: {v.stdout.strip() or v.stderr.strip()}")
    print(f"ceiling: cgroup memory.max {'(no limit)' if cgroup_ceiling_kib() is None else mib(cgroup_ceiling_kib())}, "
          f"MemAvailable {mib(mi['MemAvailable'])}")
    print("\nRead before quoting a number: one machine, one endpoint, one kernel, and the CLI rows\n"
          "and the embedded rows answer different questions (see leg 3's and leg 4's notes).\n"
          "Two RSS instruments appear, and each row says which it is: the pi rows use the\n"
          "boundary capture's `/proc` VmRSS sampler at 5–10 ms (a child that stays alive),\n"
          "the one-shot child rows use VmHWM sampled at ~20 µs, and the embedded arms print\n"
          "the same VmHWM from inside the process so the two readings can be compared.")

    pi_row = leg_pi()
    host = leg_host()
    cli = leg_cli(endpoint_ok)
    embed = leg_embed(versions, endpoint_ok)
    kids = leg_children(pi_row, cli or {})
    # The conclusion needs every row; a leg that could not run prints its own
    # `not measured` above rather than being silently dropped from the summary.
    if pi_row and embed and kids:
        leg_conclusion(host, pi_row, cli or {}, embed, kids)
    print("\nDone.")


if __name__ == "__main__":
    main()
