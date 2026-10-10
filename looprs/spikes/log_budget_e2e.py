#!/usr/bin/env python3
"""Where the log goes, how loud it is and how big it gets — on the shipped binary
(looprs-00u.13).

`src/services/logging.rs` resolves the three logging knobs and enforces the size
cap, and its unit tests prove the ladder and the rotator against a temp dir. What
a unit test cannot see is the thing the ticket was actually about: **the binary a
person runs**, over real wall-clock time, with real polling behind it. This
script measures four claims on that binary.

  1. **The destination is the knob's, and the app says so.** Every run prints
     one line naming the resolved path, the rung that produced it, and the
     ceiling. The check reads that line back and asserts the file it names is the
     file that exists — the report and the bytes cannot disagree.
  2. **The whole ladder works in a real process**: `$LOOPRS_LOG_DIR` over
     `$XDG_STATE_HOME` over `$HOME/.local/state` over the system temp dir. Four
     runs, four expected paths, and nothing appearing at a rung a higher one
     shadowed.
  3. **The default run is bounded, and it is `info` — and the `debug` volume the
     old default produced is measured against it.** Same fixture, same length
     (60 s by default), two levels, two numbers. That pair is the before/after the
     ticket asks for, printed rather than remembered.
  4. **The cap holds under volume, and the operator's greps still hit.** A
     deliberately chatty run — `RUST_LOG=debug`, a `bd` that fails every poll, a
     fast poll interval, a 16 KiB cap — is pushed through several rotations. The
     file set stays inside `active + keep`, no file runs more than one line over
     the cap, the active name `looprs.log` is greppable at the end, and the
     `docs/guide/operator.md` grep index is run over both results so the page's
     level column can be checked against reality rather than against memory.

The last one is also the honest answer to "does `info` by default break the
diagnostics?": the index prints hits at the default level and at `debug` for
every pattern on the page, and names the ones that only exist at `debug`.

    cargo build
    python3 spikes/log_budget_e2e.py | tee spikes/results/log-budget-e2e.log

    # A short pass while iterating (not the recorded numbers):
    python3 spikes/log_budget_e2e.py --quick

Costs no model call and touches no network: `pi` is `spikes/fake_pi_slow.py` and
`bd` is the bash fake `spikes/status_e2e.py` writes, with its fail marker set for
the chatty run. Every run lives in its own `mkdtemp()` including `HOME`, so it
cannot leave anything in the real `~/.local/state`.
"""

import argparse
import glob
import os
import re
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from status_e2e import Driver, Fakes, say  # noqa: E402

BIN = os.path.abspath(os.environ.get("LOOPRS_BIN", "target/debug/looprs"))
ROWS, COLS = 32, 100

RESULTS = []

# Every knob this script takes off the inherited environment, so a `RUST_LOG` or a
# `LOOPRS_LOG_DIR` left over in the caller's shell cannot decide what "default
# settings" means here.
KNOBS = [
    "LOOPRS_LOG_DIR",
    "LOOPRS_LOG_MAX_BYTES",
    "LOOPRS_LOG_KEEP",
    "RUST_LOG",
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "LOOPRS_KANBAN_POLL_MS",
    "LOOPRS_KANBAN",
    "LOOPRS_TRANSCRIPT",
]
BASE = {k: v for k, v in os.environ.items() if k not in KNOBS}

# The grep index from `docs/guide/operator.md`, pattern for pattern. If a pattern
# here matches nothing in either run, that row of the page is a lie, and this is
# the script that notices.
OPERATOR_GREPS = [
    ("what is configured", r"kanban board|clipboard:|transcript dump:|notifications"),
    ("the board is wrong / stale", r"board poll"),
    ("the change detector broke", r"change detector"),
    ("the wheel ate my scroll", r"wheel gesture closed"),
    ("a knob was ignored", r"is not a number|clamped to|falling back"),
    ("something refused to copy", r"(?i)clipboard|copied|Nothing copied"),
    ("the terminal did not come back", r"restore|ledger|screen debt"),
    ("where the log itself went", r"logging: |log budget"),
]

STARTUP_RE = re.compile(
    r"logging: (?P<path>\S+) \| via (?P<via>.+?) \| level (?P<level>\S+) \((?P<how>[^)]*)\) "
    r"\| budget (?P<cap>.+?) per file x (?P<files>\d+) file\(s\) = <= (?P<ceiling>.+?) total"
)

UNIT_BYTES = {"B": 1, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3}


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    mark = "PASS" if ok else "FAIL"
    say(f"{mark}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def measured(name, value):
    say(f"      {name}: {value}")


def set_env(**overrides):
    """Replace the knob part of the environment for the next spawned app."""
    env = dict(BASE)
    for key, value in overrides.items():
        if value is not None:
            env[key] = str(value)
    os.environ.clear()
    os.environ.update(env)


def read_file(path):
    if not os.path.exists(path):
        return ""
    with open(path, "r", errors="replace") as f:
        return f.read()


def read_logs(log_dir):
    """({name: bytes}, whole-run text, active-file text) for one log directory."""
    files, blob = {}, ""
    if os.path.isdir(log_dir):
        for path in sorted(glob.glob(os.path.join(log_dir, "looprs.log*"))):
            with open(path, "rb") as f:
                files[os.path.basename(path)] = len(f.read())
            blob += read_file(path)
    return files, blob, read_file(os.path.join(log_dir, "looprs.log"))


def startup_line(text):
    for line in text.splitlines():
        m = STARTUP_RE.search(line)
        if m:
            return m
    return None


def parse_size(text):
    """`1.0 MiB` / `512 KiB` / `900 B` → int, to compare against `getsize`."""
    m = re.match(r"^([\d.]+)\s*(B|KiB|MiB|GiB)$", text.strip())
    if not m:
        return -1
    return int(float(m.group(1)) * UNIT_BYTES[m.group(2)])


LEVEL_RE = re.compile(r"\b(TRACE|DEBUG|INFO|WARN|ERROR)\b")


def levels_for(blob, rx):
    """The distinct level tokens of the lines this pattern matched.

    This is the `needs` column of the operator page's grep index, read off the log
    rather than asserted from memory: a pattern whose matches are all `DEBUG` is a
    pattern that returns nothing on a default-level run, and the page has to say
    so or the reader concludes the diagnostic is gone.
    """
    out = set()
    for line in blob.splitlines():
        if rx.search(line):
            m = LEVEL_RE.search(line)
            if m:
                out.add(m.group(1))
    return sorted(out) or ["(none)"]


def boot(tmp, extra_env=None):
    """One app in a pty, under a scratch `HOME`, on the fake `bd`/`pi`.

    `status_e2e.Driver` builds its own env from `os.environ` and sets the
    `bd`/`pi`/shell knobs itself, so the values under test are set here and picked
    up there.
    """
    fakes = Fakes(tmp)
    env = {"HOME": os.path.join(tmp, "home")}
    env.update(extra_env or {})
    set_env(**env)
    return fakes, Driver(fakes, ROWS, COLS)


def sample_growth(log_path, seconds, tick=5):
    """Sleep `seconds`, sampling the active log's size every `tick`."""
    samples = []
    t0 = time.time()
    while True:
        left = seconds - (time.time() - t0)
        if left <= 0:
            return samples
        time.sleep(min(tick, left))
        size = os.path.getsize(log_path) if os.path.exists(log_path) else -1
        samples.append((round(time.time() - t0, 1), size))


def drive_with_flicks(d, log_path, seconds):
    """The same sampling, with a synthetic wheel flick every couple of seconds.

    The flick is here for one reason: `wheel gesture closed` is a `debug` line the
    operator page greps for, and greping for a gesture line needs a gesture. Six
    SGR wheel-up reports is a gesture. *Whether the band scrolled correctly* is
    `mouse_scroll_e2e.py`'s claim; this script only cares that the line got
    written, so it can say which operator greps need `RUST_LOG=debug`.
    """
    samples = []
    t0 = time.time()
    while True:
        left = seconds - (time.time() - t0)
        if left <= 0:
            return samples
        for _ in range(6):
            d.send(b"\x1b[<64;30;10M")  # x=30, y=10: the band, not the input row
            time.sleep(0.03)
        time.sleep(min(2.0, left))
        size = os.path.getsize(log_path) if os.path.exists(log_path) else -1
        samples.append((round(time.time() - t0, 1), size))


def scenario_idle(seconds, rust_log=None):
    """One idle run at the default cap, and the numbers it left behind.

    `rust_log=None` is *nothing set at all*, which is the default-settings case;
    `rust_log="debug"` is the level the old code defaulted to, run through the
    same fixture so the two numbers are comparable rather than remembered.
    """
    label = "default settings (no RUST_LOG)" if rust_log is None else f"RUST_LOG={rust_log}"
    say(f"--- {label}: {seconds}s idle, default cap ---")
    tmp = tempfile.mkdtemp(prefix="looprs-log-idle-")
    logs = os.path.join(tmp, "logs")
    env = {"LOOPRS_LOG_DIR": logs}
    if rust_log:
        env["RUST_LOG"] = rust_log
    fakes, d = boot(tmp, env)
    try:
        for elapsed, size in sample_growth(os.path.join(logs, "looprs.log"), seconds):
            measured(f"looprs.log at {elapsed}s", size)
    finally:
        d.quit()
        time.sleep(0.5)

    files, blob, active = read_logs(logs)
    total = sum(files.values())
    m = startup_line(active)
    check(f"{label}: a log in the LOOPRS_LOG_DIR it was given", bool(files), str(files))
    check(
        f"{label}: one startup line naming path, rung, level and ceiling",
        m is not None,
        f"nothing matched; first 400 chars: {active[:400]!r}",
    )
    if m:
        measured("what the startup line said", m.group(0))
        check(f"{label}: names the rung we set", m.group("via") == "LOOPRS_LOG_DIR", m.group("via"))
        want = "info" if rust_log is None else rust_log
        check(f"{label}: the level it says is the level it ran at", m.group("level") == want,
              m.group("level"))
        check(f"{label}: points at RUST_LOG as the knob", "RUST_LOG" in m.group("how"), m.group("how"))
        check(
            f"{label}: the whole log dir ({total} B) is inside the ceiling it stated "
            f"({m.group('ceiling')})",
            0 < total <= parse_size(m.group("ceiling")),
            f"{total} vs {m.group('ceiling')}",
        )
    debug_files = sorted(
        n for n in glob.glob(os.path.join(logs, "looprs.log*")) if "DEBUG" in read_file(n)
    )
    if rust_log is None:
        check(
            "…and no DEBUG line anywhere: the default really is info, not debug",
            not debug_files,
            f"DEBUG in {debug_files}",
        )
    else:
        check(f"{label}: DEBUG lines present", bool(debug_files), str(debug_files))
    measured(f"{label}: files left behind", sorted(files))
    measured(f"{label}: bytes per file", files)
    measured(f"{label}: total bytes", total)
    if rust_log is None:
        # The whole default-level run, printed. This is what "the default is info"
        # costs, line for line, and it is the block the operator page quotes.
        say("  every line the default-level run wrote:")
        for line in active.splitlines():
            say(f"    {line}")
    check(
        f"{label}: an idle run stays under the 1 MiB default, so nothing rotated",
        list(files) == ["looprs.log"],
        str(files),
    )
    shutil.rmtree(tmp, ignore_errors=True)
    return blob, total


def scenario_ladder(seconds):
    say("--- the destination ladder: four runs, one rung shadowing the next ---")
    base = tempfile.mkdtemp(prefix="looprs-log-ladder-")
    temp_rung = os.path.join(tempfile.gettempdir(), "looprs.log")

    # Every case gets its *own* knob/XDG/HOME directories, all of them under one
    # scratch `base`. Sharing them would let the previous case's output read as
    # this case's stray: a `knob/looprs.log` that case 1 wrote is not case 3
    # failing the ladder, it is a dirty fixture.
    def layout(case_dir):
        return {
            "knob": os.path.join(case_dir, "knob"),
            "xdg": os.path.join(case_dir, "xdgstate"),
            "home": os.path.join(case_dir, "home"),
        }

    home_log = lambda p: os.path.join(p["home"], ".local", "state", "looprs", "looprs.log")
    xdg_log = lambda p: os.path.join(p["xdg"], "looprs", "looprs.log")

    cases = [
        (
            "LOOPRS_LOG_DIR wins over XDG_STATE_HOME and HOME",
            lambda p: {"LOOPRS_LOG_DIR": p["knob"], "XDG_STATE_HOME": p["xdg"], "HOME": p["home"]},
            lambda p: os.path.join(p["knob"], "looprs.log"),
            lambda p: [xdg_log(p), home_log(p)],
            "LOOPRS_LOG_DIR",
        ),
        (
            "XDG_STATE_HOME is next once the knob is unset",
            lambda p: {"XDG_STATE_HOME": p["xdg"], "HOME": p["home"]},
            xdg_log,
            lambda p: [home_log(p)],
            "XDG_STATE_HOME",
        ),
        (
            "$HOME/.local/state is next once XDG_STATE_HOME is unset",
            lambda p: {"HOME": p["home"]},
            home_log,
            lambda p: [os.path.join(p["knob"], "looprs.log")],
            "$HOME/.local/state",
        ),
        (
            "the system temp dir is the last rung",
            lambda p: {},
            lambda p: temp_rung,
            lambda p: [home_log(p)],
            "the system temp dir",
        ),
    ]

    for label, env_for, expected_for, forbidden_for, said_via in cases:
        case_dir = tempfile.mkdtemp(prefix="rung-", dir=base)
        paths = layout(case_dir)
        expected = expected_for(paths)
        forbidden = forbidden_for(paths)
        fakes = Fakes(case_dir)
        # The Driver owns `bd`/`pi`/shell; this decides only where HOME and the
        # log knobs point, so the ladder is the one thing varying between runs.
        set_env(**env_for(paths))
        os.environ["LOOPRS_BD_BIN"] = fakes.bin
        d = Driver(fakes, ROWS, COLS)
        try:
            time.sleep(seconds)
        finally:
            d.quit()
            time.sleep(0.4)

        text = read_file(expected)
        m = startup_line(text)
        check(f"{label}: a log at {expected}", os.path.exists(expected))
        check(
            f"{label}: and the app named that very path",
            m is not None
            and os.path.expanduser(m.group("path")) == os.path.abspath(expected),
            f"startup said {m.group('path') if m else None!r}",
        )
        if m:
            check(f"{label}: and named the rung ({said_via})", m.group("via") == said_via,
                  m.group("via"))
        for other in forbidden:
            check(
                f"{label}: and nothing at the shadowed rung {other}",
                not os.path.exists(other),
                "wrote where a higher rung should have won",
            )
        # The last rung is the machine's real temp dir, so clean up there rather
        # than leaving a `looprs.log` for the next person's `ls /tmp`.
        if os.path.abspath(expected) == os.path.abspath(temp_rung):
            for leftover in glob.glob(temp_rung + "*"):
                try:
                    os.remove(leftover)
                except OSError:
                    pass
    shutil.rmtree(base, ignore_errors=True)


def scenario_chatty(seconds):
    say(f"--- forced volume, {seconds}s: debug level, `bd` failing every poll, 16 KiB cap ---")
    tmp = tempfile.mkdtemp(prefix="looprs-log-rotate-")
    logs = os.path.join(tmp, "logs")
    cap, keep = 16 * 1024, 3
    fakes, d = boot(
        tmp,
        {
            "LOOPRS_LOG_DIR": logs,
            "LOOPRS_LOG_MAX_BYTES": "16K",
            "LOOPRS_LOG_KEEP": str(keep),
            "RUST_LOG": "debug",
            "LOOPRS_KANBAN_POLL_MS": "120",
        },
    )
    # The fail marker: every `bd` read exits 3, so the poller logs one line per
    # poll — the cheapest volume this app produces on command without a model.
    with open(fakes.fail, "w") as f:
        f.write("down\n")
    try:
        sizes = drive_with_flicks(d, os.path.join(logs, "looprs.log"), seconds)
        measured("active-file size over the run", [f"{t}s={n}" for t, n in sizes])
    finally:
        d.quit()
        time.sleep(0.5)

    files, blob, active = read_logs(logs)
    m = startup_line(active)
    measured("files left behind", sorted(files))
    measured("bytes per file", files)
    measured("total bytes", sum(files.values()))
    check("a chatty run rotated at all", len(files) > 1, str(files))
    allowed = {"looprs.log"} | {f"looprs.log.{i}" for i in range(1, keep + 1)}
    check("and produced nothing outside active + keep",
          set(files) <= allowed, f"strays: {set(files) - allowed}")
    check("the active name survived every rotation", "looprs.log" in files)
    # The cap holds to within one line: the check is before the write, so a file
    # can finish one line over and never two. One whole cap of slack covers any
    # single line this app writes.
    over = {n: b for n, b in files.items() if b > 2 * cap}
    check("no file is more than one line over the cap", not over, str(over))
    check(
        f"total stays <= (keep+1) x 2 x cap = {(keep + 1) * 2 * cap} B",
        sum(files.values()) <= (keep + 1) * 2 * cap,
        str(sum(files.values())),
    )
    if m:
        check("the startup line stated the cap that was set",
              parse_size(m.group("cap")) == cap and m.group("files") == str(keep + 1), m.group(0))
    check("the run really was debug", "DEBUG" in blob)
    check("the failing board poll is in it (the stale-board grep)", "board poll" in blob)
    check("the synthetic flick produced a gesture line at debug",
          "wheel gesture closed" in blob)
    shutil.rmtree(tmp, ignore_errors=True)
    return blob


def scenario_bad_knob(seconds):
    """Knobs set to nonsense: loud fallback, app alive, defaults still in force.

    Every knob in this app is required to explain itself when it is ignored, and
    the operator page has a row that claims "there are no quiet ones". This is the
    run that keeps that claim honest for the logging knobs specifically — the
    warnings must be `WARN`, i.e. visible at the default level, because a fallback
    you can only read at `debug` is a fallback you will not see.
    """
    say("--- a deliberately bad knob: loud fallback at the default level ---")
    tmp = tempfile.mkdtemp(prefix="looprs-log-badknob-")
    logs = os.path.join(tmp, "logs")
    fakes, d = boot(
        tmp,
        {
            "LOOPRS_LOG_DIR": logs,
            "LOOPRS_LOG_MAX_BYTES": "lots",
            "LOOPRS_LOG_KEEP": "many",
            "RUST_LOG": "=debug",  # `=` with no target: EnvFilter refuses this one
        },
    )
    try:
        time.sleep(seconds)
    finally:
        d.quit()
        time.sleep(0.4)

    files, blob, active = read_logs(logs)
    m = startup_line(active)
    warns = [line for line in active.splitlines() if "WARN" in line]
    say("  the warnings this run emitted:")
    for line in warns:
        say(f"    {line}")
    check("a bad RUST_LOG did not stop the run: it still logged its startup line",
          m is not None, active[:200])
    check("and it said so at WARN (visible without RUST_LOG=debug)",
          any("RUST_LOG" in line for line in warns), str(warns))
    check("and said the same about LOOPRS_LOG_MAX_BYTES",
          any("LOOPRS_LOG_MAX_BYTES" in line for line in warns), str(warns))
    check("and about LOOPRS_LOG_KEEP",
          any("LOOPRS_LOG_KEEP" in line for line in warns), str(warns))
    if m:
        check("and ran on the defaults it fell back to",
              m.group("level") == "info" and parse_size(m.group("cap")) == 1024 * 1024, m.group(0))
    measured("files left behind", sorted(files))
    shutil.rmtree(tmp, ignore_errors=True)
    return blob


def report_greps(blobs):
    """The operator page's grep index, run against every blob collected so far."""
    labels = [label for label, _ in blobs]
    say("--- the operator page's grep index, run against every run above ---")
    widths = max(len(l) for l in labels)
    say(f"  {'diagnostic':<42}" + "".join(f"{l:<20}" for l in labels))
    default_blob = dict(blobs)["default"]
    debug_only = []
    for label, pattern in OPERATOR_GREPS:
        rx = re.compile(pattern)
        hits = {l: len(rx.findall(b)) for l, b in blobs}
        say(f"  {label:<42}" + "".join(f"{f'{hits[l]} hit(s)':<20}" for l in labels))
        for l, b in blobs:
            measured(f"{label} [{l}] levels that produced it", levels_for(b, rx))
        if any(h for h in hits.values()) and not hits["default"]:
            debug_only.append(label)
    say(f"  patterns the default level does not reach: {debug_only}")
    check(
        "the startup-resolution greps still hit at the default level",
        re.search(r"kanban board|clipboard:|transcript dump:|notifications", default_blob) is not None,
    )
    unreached = [
        l for l, p in OPERATOR_GREPS if not any(re.compile(p).findall(b) for _, b in blobs)
    ]
    check(
        "every grep on the page is reached by some run here (none is unverifiable)",
        not unreached,
        f"unreached: {unreached}",
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=int, default=60,
                   help="length of each idle run (the default-settings one and the debug comparison)")
    ap.add_argument("--quick", action="store_true",
                   help="short versions of every run, for iterating (not the recorded numbers)")
    args = ap.parse_args()
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2

    idle = 8 if args.quick else args.seconds
    ladder = 2 if args.quick else 4
    chatty = 12 if args.quick else 30

    default_blob, default_total = scenario_idle(idle)
    debug_blob, debug_total = scenario_idle(idle, rust_log="debug")
    scenario_ladder(ladder)
    chatty_blob = scenario_chatty(chatty)
    bad_knob_blob = scenario_bad_knob(4)
    report_greps(
        [
            ("default", default_blob),
            ("debug", debug_blob),
            ("chatty", chatty_blob),
            ("bad knob", bad_knob_blob),
        ]
    )

    say("--- the number the ticket asks for ---")
    say(f"  {idle}s idle at the default level (info): {default_total} bytes")
    say(f"  {idle}s idle at the old default level (debug): {debug_total} bytes")
    if default_total:
        say(f"  ratio: the default run writes {debug_total / max(default_total, 1):.1f}x less "
            f"than the old default at the same length")
    say("  and neither can exceed the ceiling printed in its own startup line; the")
    say("  chatty run above proves the ceiling bites (rotated, bounded, active name intact)")

    ok = sum(1 for _, v in RESULTS if v)
    say(f"{ok}/{len(RESULTS)} checks passed")
    return 0 if ok == len(RESULTS) else 1


if __name__ == "__main__":
    sys.exit(main())
