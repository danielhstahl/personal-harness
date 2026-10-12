#!/usr/bin/env python3
"""workspace-eqn.5 / .7, shipped by workspace-01z — typed `bd` tools: what they cost.

ADR-0010's proposal sells two things at once. One of them — typed tool calls
instead of the worker shelling `bd` out of its bash tool — does not need an
engine swap at all: pi registers schema-validated tools from an extension
(`extensions/bd_tools.ts` — the shipped one, which is what this measures), and the
worker's `--tools` allowlist gates them like the built-ins. This spike is that arm,
measured, so the engine question can be argued without the tooling question riding
on it, and so the number the shipped prompt cost is quoted from is a capture rather
than an opinion.

Leg 2 prices the tool section for the tool lists looprs actually spawns with: pi's
four built-ins (what the worker had before the typed surface), the shipped default
`LOOPRS_BEADS_WORKER_TOOLS`, the full five verbs, and a shell-less list. Note the
plain-name requirement: pi 1.0.0's `+name` delta form declares *no* tools at all, so
every tool has to be named, and the byte counts below are for whole allowlists.

Three legs:

  1. **CALL** — what one `bd` interaction costs, in the three shapes it can take:
     `bash -c 'bd …'` (what the worker does today), `bd …` straight, and the
     typed tool's own `spawnSync` from inside the Node process. The point of
     measuring all three is that `bd` is a Go binary over a SQLite file — nobody
     can link it, in this engine or any other — so the only layer a typed tool
     can remove is the shell, and the log says what that layer is worth.
  2. **PROMPT** — the price the typed tools *do* add: tool schemas are prompt.
     Taken out of the first system message of a real run (`sections.tools`), not
     estimated from the TypeScript.
  3. **PASS** — the A/B on the real model: the same question asked twice per arm
     (`LOOPRS_SPIKE_REPEATS`, default 2), once with the worker's `bash`+`bd` path
     and once with the typed tool, scoring wall clock, tokens in, model requests,
     and which tool the model actually reached for. Real passes, no fixture: the
     claim "typed tools change what the model does" has to be falsified by a model,
     not by an opinion. And what it prints is what happened — if the arm that was
     *told* in prose to run `bd ready` answers without running it, the log says so
     instead of describing the run it asked for.

No model endpoint → leg 3 prints `not measured` and legs 1–2 still stand.

    python3 spikes/bd_tools_cost.py | tee spikes/results/bd-tools-cost.log

Env:
  LOOPRS_SPIKE_BD         bd binary (default: bd)
  LOOPRS_SPIKE_N          iterations per leg-1 row (default: 15)
  LOOPRS_SPIKE_REPEATS    passes per arm in leg 3 (default: 2)
  LOOPRS_SPIKE_NO_MODEL   skip leg 3
  LOOPRS_SPIKE_BASE_URL   model endpoint probed before leg 3
"""

import glob
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXT = os.path.join(REPO, "extensions", "bd_tools.ts")
BD = os.environ.get("LOOPRS_SPIKE_BD", "bd")
N = int(os.environ.get("LOOPRS_SPIKE_N", "15"))
BASE_URL = os.environ.get("LOOPRS_SPIKE_BASE_URL", "http://llm.home:8080/v1")
#: The verbs `extensions/bd_tools.ts` registers, and the shipped default list —
#: mirrored from `src/services/bd_tools.rs`, whose own test reads them out of the
#: extension so this file cannot quietly price a different set.
VERBS = ["ready", "show", "claim", "close", "remember"]
DEFAULT_WORKER_TOOLS = "read,bash,edit,write,bd_show,bd_close,bd_remember"


def probe_model():
    """HTTP code from the model endpoint, or '' when curl cannot say."""
    return subprocess.run(["curl", "-s", "-m", "10", "-o", "/dev/null", "-w", "%{http_code}",
                           BASE_URL + "/models"], capture_output=True, text=True).stdout.strip()


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def run_timed(cmd, n=N):
    """min / median / max wall ms over `n` runs of a shell command."""
    xs = []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run(cmd, shell=True, capture_output=True)
        xs.append((time.perf_counter() - t) * 1000)
    return min(xs), med(xs), max(xs)


def leg_call():
    print("== 1. CALL — one bd interaction, three ways ==")
    rows = [
        ("fork a shell (`bash -c 'true'`)", "bash -c 'true'"),
        ("fork+exec bd (`bd ready`)", f"{BD} ready"),
        ("through a shell (`bash -c 'bd ready'`)", f"bash -c '{BD} ready'"),
        ("`bd show` on a real id", f"{BD} show workspace-eqn"),
    ]
    print(f"{'shape':46} {'min ms':>8} {'median ms':>10} {'max ms':>8}")
    print("-" * 74)
    for label, cmd in rows:
        lo, m, hi = run_timed(cmd)
        print(f"{label:46} {lo:8.1f} {m:10.1f} {hi:8.1f}")

    # The typed tool's own cost: the same spawnSync the extension runs, no model,
    # no pi. Node is the process the tool lives in, so that is the process whose
    # fork price counts.
    node = r"""
const { spawnSync } = require("node:child_process");
const bd = process.env.LOOPRS_SPIKE_BD || "bd";
const t = [];
for (let i = 0; i < %d; i++) { const s = Date.now(); spawnSync(bd, ["ready"], { shell: false }); t.push(Date.now() - s); }
t.sort((a,b)=>a-b);
console.log(JSON.stringify({min: t[0], med: t[t.length>>1], max: t[t.length-1]}));
""" % N
    env = dict(os.environ, LOOPRS_SPIKE_BD=BD)
    p = subprocess.run(["node", "-e", node], capture_output=True, text=True, env=env)
    print(f"{'typed tool call (`spawnSync bd ready`)':46}", end="")
    if p.returncode == 0:
        d = json.loads(p.stdout)
        print(f" {d['min']:8.1f} {d['med']:10.1f} {d['max']:8.1f}")
    else:
        print(" not measured (node not runnable here)")
    print(
        "\nReading: the row that matters is the *difference* between 'through a\n"
        "shell' and 'fork+exec bd'. That gap is the only thing a typed tool can\n"
        "remove from the call path, because `bd` itself is a separate process in\n"
        "every architecture on the table — including one where the agent engine\n"
        "is a Rust library: there is no bd library to link, only the CLI."
    )


def parse_pi_json(path):
    """Return (tool_calls, assistant_usage, tools_section, text) from a `pi --mode json` run."""
    calls, usage, tools, text = [], [], None, None
    for line in open(path, errors="replace"):
        try:
            v = json.loads(line)
        except json.JSONDecodeError:
            continue
        m = v.get("message") or {}
        if m.get("role") == "system" and m.get("sections"):
            tools = m["sections"].get("tools") or ""
        if v.get("type") == "message_end" and m.get("role") == "assistant":
            if m.get("usage"):
                usage.append(m["usage"])
            for c in m.get("content") or []:
                if c.get("type") == "toolCall":
                    calls.append(c.get("name"))
                if c.get("type") == "text":
                    text = c["text"].strip()
    return calls, usage, tools, text


def pi_pass(label, extra_tools, prompt, extra_argv):
    sess = tempfile.mkdtemp(prefix="eqn-bd-")
    argv = ["pi", "-p", "--mode", "json", "--session-dir", sess, *extra_argv,
            "--tools", extra_tools, "-n", f"eqn-{label}", prompt]
    t = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, text=True)
    wall = time.perf_counter() - t
    # `--mode json` streams the records on stdout; parse that, and keep the run's
    # own session dir until it has been parsed so nothing is guessed from files.
    tmp = os.path.join(sess, "stream.jsonl")
    open(tmp, "w").write(p.stdout)
    calls, usage, tools, text = parse_pi_json(tmp)
    shutil.rmtree(sess, ignore_errors=True)
    tin = sum(u.get("input", 0) + u.get("cacheRead", 0) for u in usage)
    tout = sum(u.get("output", 0) for u in usage)
    return {"wall": wall, "calls": calls, "requests": len(usage), "tokens_in": tin,
            "tokens_out": tout,
            "tools_section": tools or "", "text": text, "rc": p.returncode,
            "stderr": p.stderr[-200:] if p.returncode else ""}


#: The tool lists looprs actually spawns with, as priced objects. The first is what
#: the worker had before this surface existed; the second is
#: `DEFAULT_WORKER_TOOLS` in `src/services/bd_tools.rs`, i.e. what ships; the third
#: is the extension's whole vocabulary; the fourth is the shell-less gate the knob
#: makes possible. Keep in sync with the Rust — `cargo test` reads the shipped list
#: out of the extension, and this table is the byte price of the same names.
TOOL_LISTS = [
    ("built-ins only (pre-surface)", "read,bash,edit,write", []),
    ("shipped default (+show,close,remember)", DEFAULT_WORKER_TOOLS, ["-e", EXT]),
    ("all five verbs", "read,bash,edit,write," + ",".join("bd_" + v for v in VERBS), ["-e", EXT]),
    ("no shell (+show,claim,close)", "read,edit,write,bd_show,bd_claim,bd_close", ["-e", EXT]),
]

#: Assistant steps per pass, measured over this project's own session corpus in
#: ADR-0010. Schemas ride on every one of them, which is the only reason a per-call
#: measurement of the schema is the wrong number.
STEPS_PER_PASS = 114


def leg_sizes():
    """What the tool section costs, for the allowlists looprs really passes."""
    print("\n== 2. PROMPT — the tool section, per allowlist, read out of a real run ==")
    code = probe_model()
    if code != "200":
        print(f"not measured: model endpoint {BASE_URL} answered {code!r} "
              f"(or LOOPRS_SPIKE_NO_MODEL is set)")
        return
    rows = []
    for i, (label, tools, extra) in enumerate(TOOL_LISTS):
        # A stable session name per row: this leg is a measurement, and a run that
        # could not be reproduced byte for byte is a worse witness than a shorter one.
        r = pi_pass(f"size{i}", tools, "Reply with exactly: ok", extra)
        rows.append((label, tools, len(r["tools_section"])))
    base = rows[0][2]
    print(f"{'allowlist':40} {'bytes':>6} {'vs built-ins':>13} {'≈ tokens/req':>13} "
          f"{'≈ tokens/pass':>15}")
    print("-" * 92)
    for label, tools, n in rows:
        d = n - base
        print(f"{label:40} {n:6} {d:+13} {d // 4:+13} {(d // 4) * STEPS_PER_PASS:+15}")
    print(f"\nPer-pass is ×{STEPS_PER_PASS} model requests — the median pass of this corpus "
          f"makes that many\nassistant steps (ADR-0010), and a schema is prompt on every one. "
          f"That is the number\nto quote, not the per-call one, and it is what the extension's "
          f"header states.")
    print("Note: pi 1.0.0's `+name` delta form declares *no* tools at all, so every "
          "allowlist\nabove is spelled in full; a delta list would price as nothing and "
          "leave the worker tool-less.")


def leg_prompt_and_pass():
    print("\n== 3. PASS — the same question, bash+bd vs the typed tool ==")
    code = probe_model()
    if code != "200" or os.environ.get("LOOPRS_SPIKE_NO_MODEL"):
        print(f"not measured: model endpoint {BASE_URL} answered {code!r} "
              f"(or LOOPRS_SPIKE_NO_MODEL is set)")
        return
    q_bash = ("How many tickets are ready on this beads board right now? Answer with "
              "just the number. Find out with the bash tool by running `bd ready`.")
    q_typed = ("How many tickets are ready on this beads board right now? Answer with "
               "just the number. Find out with the bd_ready tool.")
    repeats = int(os.environ.get("LOOPRS_SPIKE_REPEATS", "2"))
    # The typed arm is the shipped worker list plus `bd_ready`, which the shipped
    # default leaves out on purpose (the loop picks the work; a worker that can ask
    # what is ready is a worker that shops). The bash arm is the pre-surface list.
    typed_tools = "read,bash,edit,write,bd_ready"
    runs = []
    hdr = (f"{'arm':14} {'tools in prompt':16} {'tool calls':30} {'reqs':5} "
           f"{'tokens in':10} {'tokens out':11} {'wall s':7} answer")
    print(hdr)
    print("-" * len(hdr))
    for i in range(repeats):
        for name, tools, prompt, extra in (
            ("bash + bd", "read,bash,edit,write", q_bash, []),
            ("typed tools", typed_tools, q_typed, ["-e", EXT]),
        ):
            r = pi_pass(f"{name.split()[0]}{i}", tools, prompt, extra)
            r["arm"], r["iter"] = name, i
            runs.append(r)
            print(f"{name:14} {len(r['tools_section']):16} "
                  f"{(','.join(r['calls']) or 'NONE')[:29]:30} {r['requests']:5} "
                  f"{r['tokens_in']:10} {r['tokens_out']:11} {r['wall']:7.1f} {(r['text'] or '')[:24]!r}")
            if r["rc"]:
                print(f"{'':14} rc={r['rc']} {r['stderr']}")
    bash_runs = [r for r in runs if r["arm"].startswith("bash")]
    typed_runs = [r for r in runs if r["arm"].startswith("typed")]
    a, b = bash_runs[0], typed_runs[0]
    d = len(b["tools_section"]) - len(a["tools_section"])
    print(f"\ntool-section bytes: bash arm {len(a['tools_section'])}, typed arm "
          f"{len(b['tools_section'])} ({d:+} bytes, ≈{max(d, 0) // 4} extra prompt tokens "
          f"on every model request of the pass)")
    print(f"model requests: bash arm {sorted(set(r['requests'] for r in bash_runs))}, "
          f"typed arm {sorted(set(r['requests'] for r in typed_runs))} — the schema rides on "
          f"every one of them, which is the honest way to price it")
    # Say what happened, not what the arms were told to do. The control arm was
    # instructed to run `bd ready` through bash and, on the run this capture kept,
    # answered without calling anything at all — which is a finding about a
    # free-form instruction, and it must not be smoothed over into "both arms
    # consulted the board".
    bash_acted = any(r["calls"] for r in bash_runs)
    typed_acted = all(any(c.startswith("bd_") for c in r["calls"]) for r in typed_runs)
    print()
    if typed_acted and not bash_acted:
        print("What actually happened: the typed arm reached for a `bd_*` tool every time; the")
        print("bash arm was told, in prose, to run `bd ready` and answered without calling")
        print("any tool at least once. That is the real argument for the typed surface — an")
        print("entry in the tool list is a guarantee the model can act on, a sentence in a")
        print("prompt is a preference.")
    else:
        print(f"What actually happened: the bash arm reached for {sorted({c for r in bash_runs for c in r['calls']})}, "
              f"the typed arm for {sorted({c for r in typed_runs for c in r['calls']})}.")
    # The repeats are not a confidence interval and this run says so. One typed arm
    # above wandered into a long bash loop and billed over a million tokens on a
    # question the other three passes answered in two requests. That is the local
    # model, not the schema: leg 3 is one free-form question against one endpoint,
    # and it is printed as what happened rather than trimmed to what flatters the
    # arm it was written to favour. Read the per-arm spread before quoting any of it.
    spread = sorted(r["requests"] for r in runs)
    print(f"\nRepeats are {repeats} per arm against one local endpoint, and the request counts "
          f"across\nall of them were {spread}. Quote the spread, not the mean: what this leg is "
          f"evidence of\nis which tool the model reaches for, not how many tokens a pass costs.")
    print(
        "\nNeither arm is cheaper: the typed arm is *typed* — schema-validated params,\n"
        "no shell quoting, the refusal path returned as data, and `--tools` gating a\n"
        "bare bash command cannot have. That is what this arm was for: it isolates the\n"
        "tooling win from the engine question, and the engine buys neither of them."
    )


def main():
    print(f"workspace-eqn typed-bd-tools measurement — bd at {BD}, extension {os.path.relpath(EXT, REPO)}")
    print(f"pi: {subprocess.run(['pi', '--version'], capture_output=True, text=True).stdout.strip()}")
    print()
    leg_call()
    leg_sizes()
    leg_prompt_and_pass()
    print("\nDone.")


if __name__ == "__main__":
    main()
