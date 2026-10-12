#!/usr/bin/env python3
"""workspace-eqn.3 / .4, parameterized by workspace-8jk.1 — what linking **a** rho
as looprs' engine would cost and what it would replace.

The proposal in ADR-0010 is that looprs stops driving `pi --mode rpc` as a child
and links a `rho` crate as a library. Two claims ride on that: less overhead,
and typed tool calls. Neither is the question this script answers. This script
prices the dependency the way `spikes/dep_cost.sh` prices a dependency and
`dep_audit.py` prices a manifest line — as a set of numbers, on this machine,
from a cold build:

  1. **PRICE** — a scratch consumer crate (`main.rs` = the target's embed
     program) built `--release` in a target dir of its own, against looprs' own
     cold `--release` build measured the same way on the same machine. Crates
     compiled, crates in the shipped graph, wall time, artifact size, target-dir
     size. Nothing incremental: an incremental build answers "what changed since
     last time", which is not the question.
  2. **SUPPLY** — what the candidate's own public record says about who maintains
     it, per crate on crates.io and once for the repo on GitHub: publish history
     and downloads, stars/forks/subscribers, what GitHub's licence detector
     sees, and the commit split with the human and all-time denominators printed
     separately (they are different numbers, and a bus-factor trigger has to
     name one). Read live off the two APIs; printed as `not measured` when there
     is no network, never guessed.
  3. **SURFACE** — what looprs would have to re-establish: each crate's own source
     stats (files, LOC, `unsafe`) read from the **published artifact**, the
     licence question settled against that artifact rather than against a
     repo-root LICENSE or a Cargo.toml field, exact `=` pins, optional features,
     which dependencies are dev-only, and the event-vocabulary overlap between
     what looprs' `WIRE_INVENTORY` keys behaviour on today and what the
     candidate's own event type names. The overlap number is the migration cost
     in rows of `src/wire.rs`.
  4. **EMBED** — the claim tested rather than argued: build the scratch program so
     it drives **one tool-using turn in process** and print what the host process
     sees: engine/session build time, first-token and turn time, RSS (level and
     the kernel's high-water mark), the tool list, and the event kinds the host
     was handed. For `matthewyjiang` the arms are the rows of ADR-0010's
     inventory list, each run rather than read out of a README: the two
     endpoint arms (unattended allow-process policy versus the SDK's own
     defaults), the scripted fake-provider seam, tool gating (policy vs approval
     handler vs pre-tool hook vs per-request `ToolVisibility`), the end-of-run
     signal under provider death / a dropped Run / a host that stops reading,
     steering and the `clear_queue` analogue, the blast wall (a tool's child on
     cancel, drop, SIGKILL and panic) and persistence. The program is part of the
     target, because two libraries that share a name do not share an API. No
     model endpoint → the endpoint arms print `not measured`; the scripted arms
     are labelled so they cannot be read as a substitute.

    LOOPRS_SPIKE_RHO_ID=matthewyjiang python3 spikes/rho_surface_cost.py \
        | tee spikes/results/matthewyjiang-surface-cost.log

Why the identity is a parameter
-------------------------------
The first version of this script hardcoded one library: the GitHub repo in the
SUPPLY leg, the crate set in the SURFACE leg, a single version scalar, and an
EMBED program written against that library's API. ADR-0010 turned out to be
about a *different* library — **matthewyjiang/rho**, whose embeddable crate is
`rho-sdk` — and the header line that asserted `(casonadams/rho)` is the reason a
mis-identification survived a re-runnable spike: re-running it reproduced the
wrong numbers under the right-looking title.

So identity is now a table entry (`TARGETS`), every leg reads everything it
measures out of that entry, and the header prints the owner/repo and every
crate@version the run actually measured before any number follows. Two
unrelated projects answer to the name `rho` here, and a third pair
(rho-core/rho-tools) belongs to pyrex41, so a bare "rho" is not an identity.

  * `matthewyjiang` — matthewyjiang/rho: rho-sdk (the embeddable runtime),
    rho-providers, rho-coding-agent (the CLI). **The library ADR-0010 was
    requested against; the default when no id is given.**
  * `casonadams` — casonadams/rho: rho, rho-engine, rho-harness-core. The
    library §3 was *filed* against. Retained so the numbers already in the ADR
    stay reproducible, not because it is a candidate.

Env / args:
  LOOPRS_SPIKE_RHO_ID       target id (default: matthewyjiang). `--list` prints
                            the table; `--id <id>` or a bare `<id>` argument work
                            too.
  LOOPRS_SPIKE_RHO_VERSION  version override: either one scalar applied to every
                            crate of the target, or a comma list of
                            `crate=version` pairs. A crate with no pin and no
                            override gets its `max_stable_version` resolved from
                            crates.io per crate — the matthewyjiang crates are on
                            unrelated versions (rho-sdk 5.x, rho-providers 2.x,
                            rho-coding-agent 2.x), so one shared scalar cannot
                            describe them.
  LOOPRS_RHO_TARGET         reuse this target dir instead of a cold one (the
                            cold release build is ~4 min on 8 cores; say so in
                            the capture if you reuse one)
  KEEP_TARGET=1             do not delete the scratch crate on exit
  LOOPRS_SPIKE_BASE_URL     OpenAI-compatible endpoint for the EMBED leg
                            (default: http://llm.home:8080/v1; unset host with
                            LOOPRS_SPIKE_NO_MODEL=1 to skip the leg)
  LOOPRS_SPIKE_MODEL        model id for the EMBED leg
  LOOPRS_SPIKE_ARM_TIMEOUT  wall-clock cap per EMBED arm, seconds (default 300).
                            An arm that hangs is a result, not a hang: the cap is
                            printed with the arm's rc and the raw tail.
"""

import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SPIKES = os.path.join(REPO, "spikes")
BASE_URL = os.environ.get("LOOPRS_SPIKE_BASE_URL", "http://llm.home:8080/v1")
MODEL = os.environ.get("LOOPRS_SPIKE_MODEL", "halogen-qwen3.8-flash-next")

# The default when the run does not name one. It is the library ADR-0010 was
# *requested* against; the one §3 was filed against is still selectable, and a
# capture that does not name its subject is the artefact this table exists to
# stop being produced.
DEFAULT_TARGET = "matthewyjiang"

# ───────────────────────── the identity table (what, exactly) ─────────────────
# One entry per library that answers to the name "rho". Every leg below reads
# everything it measures — repo, crates, pins, manifests, event-vocabulary files,
# embed program, capture path — out of this entry. Nothing downstream is allowed
# to name a library.
#
#   repo               GitHub owner/repo (the SUPPLY leg's subject)
#   crates             crate -> pin, or None to resolve max_stable_version from
#                      crates.io per crate
#   library_crates     what a *library consumer* links: the PRICE leg builds a
#                      consumer of exactly these
#   consumer_note      printed so a graph size is never quoted without the
#                      distinction between the library and the binary crate
#   manifest_crates    artifacts read for `=` pins / [features] / dev-deps
#   protocol_files     where the candidate's own event vocabulary lives inside
#                      the published artifact (SURFACE's overlap leg)
#   embed_main         the program the EMBED leg compiles, per target
#   embed_extra_deps   anything the embed program needs beyond the library,
#                      tokio and serde_json
#   arms               EMBED arms to run: (label, extra env, needs an endpoint).
#                      The question each arm asks is a property of the candidate,
#                      not of the script — rho's is the permission gate, rho-sdk's
#                      is the endpoint versus its scripted fake-provider seam.
#   capture            where a re-run of this identity belongs
TARGETS = {
    "matthewyjiang": {
        "repo": "matthewyjiang/rho",
        "crates": {"rho-sdk": None, "rho-providers": None, "rho-coding-agent": None},
        # `rho-coding-agent` is the CLI bin crate and `rho-providers` is its
        # provider table (config, credential store, models.dev catalog). Neither
        # belongs in a shipped-graph number for a library consumer.
        "library_crates": ["rho-sdk"],
        "consumer_note": "a library consumer links rho-sdk; rho-coding-agent is "
                         "the CLI bin crate and rho-providers is its provider "
                         "table — neither is in the shipped-graph number",
        "manifest_crates": ["rho-sdk", "rho-providers", "rho-coding-agent"],
        "protocol_files": {"rho-sdk": ["src/event.rs", "src/tool.rs"]},
        "embed_main": os.path.join(SPIKES, "embed_programs", "matthewyjiang-rho-sdk.main.rs"),
        "embed_extra_deps": [
            'reqwest = { version = "0.12", default-features = false, '
            'features = ["json", "rustls-tls"] }',
        ],
        # The embed program holds runs open on purpose (cancellation, steering, the
        # backpressure wedge), which needs tokio's timer and a process handle for
        # the kill_on_drop and SIGKILL probes. The default feature set below is
        # what a program that only completes one turn needs.
        "embed_tokio_features": ["rt-multi-thread", "macros", "time", "process", "sync"],
        "arms": [
            ("endpoint, unattended: allow-process policy, nobody to approve",
             {"RHO_SPIKE_ARM": "endpoint-headless"}, True),
            ("endpoint, SDK default gating: DenyAllPolicy + DenyApprovals",
             {"RHO_SPIKE_ARM": "endpoint-default"}, True),
            ("scripted provider — the fake-provider seam, no endpoint involved",
             {"RHO_SPIKE_ARM": "scripted"}, False),
            ("tool gating: policy, approval handler, pre-tool hook, per-request visibility",
             {"RHO_SPIKE_ARM": "gate"}, False),
            ("end-of-run signal: completion, provider death, dropped Run, unread stream",
             {"RHO_SPIKE_ARM": "settle"}, False),
            ("steering, clear_queue analogue, and the host-input round trip",
             {"RHO_SPIKE_ARM": "steering"}, False),
            ("cancellation and the blast wall: tool children on cancel, drop, SIGKILL, panic",
             {"RHO_SPIKE_ARM": "blast"}, False),
            ("persistence: what is written, in what format, and the opt-out",
             {"RHO_SPIKE_ARM": "persist"}, False),
        ],
        "capture": "spikes/results/matthewyjiang-surface-cost.log",
        "identity_note": "the library ADR-0010 was requested against",
    },
    "casonadams": {
        "repo": "casonadams/rho",
        "crates": {"rho": "0.12.0", "rho-engine": "0.12.0", "rho-harness-core": "0.12.0"},
        "library_crates": ["rho"],
        "consumer_note": "a library consumer links `rho`; rho-engine and "
                         "rho-harness-core are workspace members of that repo",
        "manifest_crates": ["rho"],
        "protocol_files": {"rho-harness-core": ["src/rpc/protocol.rs",
                                                "src/presentation/types.rs"]},
        "embed_main": os.path.join(SPIKES, "embed_programs", "casonadams-rho.main.rs"),
        "embed_extra_deps": [],
        "arms": [
            ("permission gate ON (rho's default)", {}, True),
            ("permission gate OFF (config.permission.enabled=false)",
             {"RHO_SPIKE_NO_PERMISSION": "1"}, True),
        ],
        "capture": "spikes/results/casonadams-surface-cost.log",
        "prior_capture": "spikes/results/rho-surface-cost.log",
        "identity_note": "the library ADR-0010 §3 was FILED against, which is not "
                         "the library it was asked about; retained so those "
                         "numbers stay reproducible",
    },
}


def select_target(argv):
    """LOOPRS_SPIKE_RHO_ID / --id <id> / a bare <id>, or the default."""
    chosen = None
    args = [a for a in argv[1:]]
    if "--list" in args:
        print("targets (LOOPRS_SPIKE_RHO_ID):")
        for tid, t in sorted(TARGETS.items()):
            pin = ", ".join(f"{c}@{v or 'crates.io max_stable'}"
                            for c, v in t["crates"].items())
            print(f"  {tid:14} {t['repo']:22} {pin}")
            print(f"  {'':14} {t['identity_note']}")
        sys.exit(0)
    if "--id" in args:
        i = args.index("--id")
        if i + 1 < len(args):
            chosen = args[i + 1]
    if chosen is None:
        positional = [a for a in args if not a.startswith("-")]
        if positional:
            chosen = positional[0]
    if chosen is None:
        chosen = os.environ.get("LOOPRS_SPIKE_RHO_ID") or DEFAULT_TARGET
    if chosen not in TARGETS:
        print(f"unknown target id {chosen!r}; known: {', '.join(sorted(TARGETS))} "
              f"(or LOOPRS_SPIKE_RHO_ID / --list)")
        sys.exit(2)
    return chosen, TARGETS[chosen]


def crate_versions(target):
    """Resolve crate -> (version, how it was chosen), per crate.

    Precedence: a `crate=version` pair in LOOPRS_SPIKE_RHO_VERSION, then a bare
    scalar in the same variable, then the table's pin, then crates.io's
    max_stable_version. A shared scalar is honoured but named in the header, so a
    run that assumes one version across crates that do not share one says so out
    loud instead of measuring a mixtape.
    """
    raw = (os.environ.get("LOOPRS_SPIKE_RHO_VERSION") or "").strip()
    pairs, bare = {}, None
    for part in [p.strip() for p in raw.split(",") if p.strip()]:
        if "=" in part:
            k, v = part.split("=", 1)
            pairs[k.strip()] = v.strip()
        else:
            bare = part
    out, forced = {}, bool(bare)
    for name, pin in target["crates"].items():
        if name in pairs:
            out[name] = (pairs[name], "LOOPRS_SPIKE_RHO_VERSION override")
        elif bare:
            out[name] = (bare, "LOOPRS_SPIKE_RHO_VERSION scalar applied to every crate")
        elif pin:
            out[name] = (pin, "pinned in the target table")
        else:
            c = fetch(f"https://crates.io/api/v1/crates/{name}")
            if c and c.get("crate", {}).get("max_stable_version"):
                out[name] = (c["crate"]["max_stable_version"],
                             "crates.io max_stable_version (no pin given)")
            else:
                out[name] = (None, "not measured: no pin and crates.io unreachable")
    return out, forced


def sh(cmd, **kw):
    """Run, capture, return (rc, stdout+stderr). Never raises on a nonzero rc."""
    p = subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True, text=True, **kw)
    return p.returncode, (p.stdout or "") + (p.stderr or "")


def timed_sh(cmd, cwd=None):
    t = time.perf_counter()
    rc, out = sh(cmd, cwd=cwd)
    return time.perf_counter() - t, rc, out


def kb(n):
    return f"{n / 1024:.0f} KiB" if n < 1024 * 1024 else f"{n / 1024 / 1024:.1f} MiB"


def fetch(url, timeout=25):
    """A GET that says `None` instead of guessing when the network is not there."""
    try:
        p = subprocess.run(
            ["curl", "-sS", "-m", str(timeout), "-H", "User-Agent: looprs-spike/1.0", url],
            capture_output=True, text=True, timeout=timeout + 5,
        )
        if p.returncode == 0 and p.stdout.strip():
            return json.loads(p.stdout)
    except Exception:  # noqa: BLE001 - no network is a leg that cannot run, not a crash
        pass
    return None


# ───────────────────── the embed program (leg 4, per identity) ────────────────
# The program lives in spikes/embed_programs/<target>.main.rs and is named by the
# target table, not inlined here: it is Rust that has to compile against the
# candidate, and a reviewer should be able to read and `cargo check` it without
# reading it out of a Python string. It is kept out of looprs' own manifest for
# the obvious reason: pricing an alternative does not mean shipping it
# (ADR-0009 rule 2, arrived at from the other direction).
def embed_manifest(target, versions):
    """Cargo.toml for the scratch consumer: the crates a *library consumer* links,
    each at the version this run resolved for it, plus what the embed program
    needs beyond them."""
    deps = []
    for name in target["library_crates"]:
        deps.append(f'{name} = "{versions[name][0]}"')
    features = ", ".join(
        f'"{f}"' for f in target.get("embed_tokio_features")
        or ["rt-multi-thread", "macros"]
    )
    deps += [
        f'tokio = {{ version = "1", features = [{features}] }}',
        'serde_json = "1"',
    ]
    deps += list(target.get("embed_extra_deps") or [])
    return (
        "[package]\n"
        'name = "rho-embed-spike"\n'
        'version = "0.1.0"\n'
        'edition = "2021"\n\n'
        "[dependencies]\n" + "\n".join(deps) + "\n\n"
        "[profile.release]\ndebug = false\nstrip = true\n"
    )


# ─────────────────────────────── leg 1: price ─────────────────────────────────
def cargo_tree(cwd):
    """The package names in the *normal* (shipped) graph, tree glyphs and all.

    Two first drafts got this wrong, and both errors produced a *plausible*
    number rather than an error, which is why they are recorded here: splitting
    on a space counted four "packages" for a 472-crate graph (the box-drawing
    glyph is the first token on an indented line), and passing `--target-dir` —
    a flag `cargo tree` does not accept — failed the call and the spike printed a
    cheerful zero. The name is matched by pattern, and the command runs from the
    crate directory, which is the only place its graph is defined.
    """
    _, out = sh("cargo tree -e normal", cwd=cwd)
    return {m.group(1) for m in re.finditer(r"([A-Za-z0-9][A-Za-z0-9_.-]*) v[0-9]", out)}


def leg_price(target, versions, scratch):
    print("\n== 1. PRICE — a cold release build of a consumer of the library ==")
    missing = [c for c in target["library_crates"] if not versions[c][0]]
    if missing:
        print(f"not measured: no version resolved for {', '.join(missing)} "
              f"(no pin in the target table and crates.io unreachable)")
        return None
    crate = os.path.join(scratch, "rho-embed")
    os.makedirs(os.path.join(crate, "src"), exist_ok=True)
    open(os.path.join(crate, "Cargo.toml"), "w").write(embed_manifest(target, versions))
    shutil.copyfile(target["embed_main"], os.path.join(crate, "src", "main.rs"))
    tdir = os.environ.get("LOOPRS_RHO_TARGET") or os.path.join(scratch, "rho-target")
    reuse = os.path.isdir(os.path.join(tdir, "release"))
    print(f"scratch crate: {crate}")
    print(f"embed program: {os.path.relpath(target['embed_main'], REPO)} — written against "
          f"{target['repo']}'s API, not portable to another crate that shares the name")
    print(f"consumer deps: {', '.join(f'{c} = \"{versions[c][0]}\"' for c in target['library_crates'])}")
    print(f"               ({target['consumer_note']})")
    print(f"target dir:    {tdir}{'  (REUSED — timings are incremental, not cold)' if reuse else '  (cold)'}")
    if not reuse:
        wall, rc, out = timed_sh(f"cargo build --release --target-dir {tdir}", cwd=crate)
        crates = out.count("Compiling")
        print(f"rho leg:       rc={rc}  {wall:.0f}s wall  {crates} crates compiled")
        if rc != 0:
            print("not measured: the scratch consumer of "
                  f"{target['repo']} did not build; last of the compiler output —")
            print(out[-2500:])
            return None
    else:
        rc2, out2 = sh(f"cargo build --release --target-dir {tdir}", cwd=crate)
        print("rho leg:       reused target dir; compile timing skipped rather than reported as cold")
        if rc2 != 0:
            # A reused target dir already holds a binary built from some *older*
            # copy of the embed program. Running that and printing its numbers
            # under this run's header is the failure mode this guards.
            print("not measured: the reused target dir will not build the current embed")
            print("program, and the binary already in it was built from a different one —")
            print("so no EMBED arm runs against a stale artifact. Compiler output —")
            print(out2[-1500:])
            return None
    pkgs = cargo_tree(crate)
    binpath = os.path.join(tdir, "release", "rho-embed-spike")
    size = os.path.getsize(binpath) if os.path.exists(binpath) else 0
    print(f"shipped graph: {len(pkgs)} packages (`cargo tree -e normal`)")
    print(f"artifact:      {size} B ({kb(size)}) — the spike binary with the library linked in")

    # looprs on the same machine, same method, so the ratio between the two rows
    # is a fact about the two dependency graphs and not about two machines.
    loop_target = os.path.join(scratch, "looprs-target")
    wall, rc, out = timed_sh(f"cargo build --release --target-dir {loop_target}", cwd=REPO)
    crates = out.count("Compiling")
    lb = os.path.join(loop_target, "release", "looprs")
    lsize = os.path.getsize(lb) if os.path.exists(lb) else 0
    print(f"\nlooprs today:  rc={rc}  {wall:.0f}s wall  {crates} crates compiled  "
          f"artifact {lsize} B ({kb(lsize)})")
    lpkgs = cargo_tree(REPO)
    print(f"               {len(lpkgs)} packages shipped today")
    if pkgs and lpkgs:
        print(f"\ndelta:         {len(pkgs) - len(lpkgs):+d} packages on the shipped graph, "
              f"{(size / lsize if lsize else 0):.2f}x the artifact")
    extra = sorted(p for p in pkgs if p not in lpkgs)
    print(f"packages the candidate would add that looprs does not ship: {len(extra)}")
    return {"target": tdir, "crate": crate}


# ─────────────────────────────── leg 2: supply ────────────────────────────────
def leg_supply(target, versions):
    print("\n== 2. SUPPLY — what the public record says about the maintainer ==")
    print(f"subject:       {target['repo']}")
    for name in target["crates"]:
        c = fetch(f"https://crates.io/api/v1/crates/{name}")
        if not c or "crate" not in c:
            print(f"crates.io:     {name}: not measured (crates.io unreachable)")
            continue
        k = c["crate"]
        print(f"crates.io:     {name} {k['max_stable_version']}, {k['num_versions']} releases, "
              f"first published {k['created_at'][:10]}, updated {k['updated_at'][:10]}")
        print(f"               downloads {k['downloads']} total / {k['recent_downloads']} last 90 days")
    g = fetch(f"https://api.github.com/repos/{target['repo']}")
    if g is None:
        print("not measured: GitHub unreachable")
        return
    print(f"github:        {g['full_name']}: {g['stargazers_count']} stars, {g['forks_count']} forks, "
          f"{g['subscribers_count']} subscribers, {g['open_issues_count']} open issues")
    print(f"               created {g['created_at'][:10]}, last push {g['pushed_at'][:10]}")
    print(f"               licence as GitHub's licence detector sees it: "
          f"{(g.get('license') or {}).get('spdx_id', 'NONE DETECTED')}")
    con = []
    for page in range(1, 6):
        rows = fetch(f"https://api.github.com/repos/{target['repo']}"
                     f"/contributors?per_page=100&page={page}") or []
        con += rows
        if len(rows) < 100:
            break
    if not con:
        print("contributors:  not measured (GitHub contributor list empty or unreachable)")
        return
    # Bots are counted and reported, never merged into the human denominator:
    # "share of commits the top human carries" is a different number against each
    # denominator (~73% vs ~98% on matthewyjiang/rho, ~95% vs ~100% here), and
    # ADR-0010 §4's trigger is written as one of the two without saying which.
    humans = [(x["login"], x["contributions"]) for x in con if x.get("type") == "User"]
    bots = [(x["login"], x["contributions"]) for x in con if x.get("type") != "User"]
    total = sum(x["contributions"] for x in con) or 1
    human_total = sum(n for _, n in humans)
    top, topn = humans[0] if humans else ("?", 0)
    over5 = [n for n, k in humans if human_total and k > 0.05 * human_total]
    print(f"contributors:  {len(humans)} human + {len(bots)} bot; "
          f"{total} commits attributed in total, {human_total} of them human")
    print(f"               top human {top} carries {topn} of {total} all-time = "
          f"{100 * topn / total:.1f}%  |  {topn} of {human_total} human-only = "
          f"{(100 * topn / human_total if human_total else 0):.1f}%")
    print(f"               humans above 5% of the HUMAN total: {len(over5)} "
          f"({', '.join(over5) or 'nobody'}) — the bus factor under the convention below")
    print(f"               other humans: {humans[1:6]}")
    print(f"               bots (excluded from the human denominator): {bots}")
    print("               convention: the share a bus-factor trigger reads is the HUMAN one")
    print("               (bot commits excluded). Both denominators are printed on every run so")
    print("               a re-run cannot move the trigger by changing the convention quietly.")


# ─────────────────────────────── leg 3: surface ───────────────────────────────
def crate_src(name, version):
    """The published artifact's directory, from cargo's own cache when it has it.

    `None` means the cache does not have it, which is not the same as the artifact
    not existing — the caller then fetches it from static.crates.io and says
    which of the two it did.
    """
    hits = sorted(glob.glob(os.path.expanduser(f"~/.cargo/registry/src/*/{name}-{version}")))
    if hits:
        return hits[-1]
    return None


def leg_surface(target, versions, scratch):
    print("\n== 3. SURFACE — what the published crates.io artifact actually is ==")
    print(f"subject:       {target['repo']}")
    dirs = {}
    for name in target["crates"]:
        version = versions[name][0]
        if not version:
            print(f"{name}: not measured (no version resolved)")
            continue
        d = crate_src(name, version)
        origin = "cargo cache"
        if d is None:
            d = os.path.join(scratch, f"{name}-{version}")
            rc, _ = sh(f"curl -sS -m 60 -o {scratch}/{name}.crate "
                       f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
                       f" && mkdir -p {d} && tar xzf {scratch}/{name}.crate -C {d} --strip-components=1")
            if rc != 0:
                print(f"{name}: not measured (not in the cargo cache, and static.crates.io "
                      f"would not hand over {name}-{version}.crate)")
                continue
            origin = "fresh static.crates.io fetch"
        dirs[name] = d
        print(f"{name} {version}: {origin}")
    if not dirs:
        return
    total_rs = total_loc = total_unsafe = 0
    for name, d in dirs.items():
        rs = glob.glob(os.path.join(d, "**", "*.rs"), recursive=True)
        loc = sum(sum(1 for _ in open(f, errors="replace")) for f in rs)
        unsafe = sum(open(f, errors="replace").read().count("unsafe ") for f in rs)
        total_rs += len(rs)
        total_loc += loc
        total_unsafe += unsafe
        # The licence a consumer links against is the one that ships IN the
        # artifact. The repo root is not what a consumer gets, and neither is the
        # Cargo.toml `license` field: the last assessment trusted the manifest,
        # which is the mirror-image of the mistake this leg exists to avoid, so
        # both are printed and disagreement between them is called out.
        lic = sorted({os.path.basename(p) for p in glob.glob(os.path.join(d, "*"))
                      if re.search(r"(?i)(licen[cs]|copying)", os.path.basename(p))})
        man = open(os.path.join(d, "Cargo.toml"), errors="replace").read()
        claim = (re.search(r'^license = "([^"]+)"', man, re.M)
                 or re.search(r'^license-file = "([^"]+)"', man, re.M))
        claim = claim.group(1) if claim else "NONE"
        flag = ""
        if not lic:
            flag = "  <-- artifact carries NO licence file; the manifest claim is not something a consumer receives"
        print(f"{name:18} {len(rs):4} .rs files  {loc:7} LOC  {unsafe:3} `unsafe`  "
              f"licence file in published artifact: {lic or 'NONE'}"
              f"  (Cargo.toml says: {claim}){flag}")
    print(f"{'total':18} {total_rs:4} .rs files  {total_loc:7} LOC  {total_unsafe:3} `unsafe`")

    # Exact pins and the optional surface, read out of the published manifest.
    for name in target["manifest_crates"]:
        d = dirs.get(name)
        if not d:
            continue
        man = open(os.path.join(d, "Cargo.toml"), errors="replace").read()
        pins = re.findall(r'^version = "=(.+)"$', man, re.M)
        # Only the [features] block: the normalised manifest also has indented
        # `features = [...]` lines under every dependency, and a first draft read
        # those as feature names and printed fourteen copies of the word.
        fb = re.search(r"^\[features\]\n(.*?)(?=^\[|\Z)", man, re.M | re.S)
        feats = re.findall(r"^(\w+) = ", fb.group(1), re.M) if fb else []
        devs = re.findall(r"\[dev-dependencies\.(\S+?)\]", man)
        print(f"\n{name} — exact-pinned dependencies (an `=` requirement each): {pins or 'none'}")
        print(f"  features: {feats}")
        print(f"  dev-dependencies (built for the crate's own tests, not by a library consumer): {devs}")
        print("  That last line matters: a heavy dependency named in a ticket as what a")
        print("  library 'drags in' is often dev-only. What a consumer actually links is")
        print("  leg 1's `cargo tree -e normal`, which is the number to quote.")

    # The migration cost, in rows of src/wire.rs: what looprs keys on today vs
    # what the candidate's own protocol can even be asked to name.
    print(f"\nevent vocabulary — looprs' WIRE_INVENTORY vs {target['repo']}'s own protocol:")
    wire = open(os.path.join(REPO, "src", "wire.rs"), errors="replace").read()
    looprs_tags = sorted(set(re.findall(r'wire:\s*"([a-z_]+)"', wire)))
    rho_tags, read = set(), []
    for name, files in target["protocol_files"].items():
        d = dirs.get(name)
        if not d:
            continue
        for f in files:
            p = os.path.join(d, f)
            if not os.path.exists(p):
                continue
            read.append(f"{name}:{f}")
            body = open(p, errors="replace").read()
            for variant in re.findall(r"^\s{4}([A-Z][A-Za-z0-9]+)[\s{,]", body, re.M):
                rho_tags.add(re.sub(r"(?<!^)(?=[A-Z])", "_", variant).lower())
    if rho_tags:
        shared = sorted(set(looprs_tags) & rho_tags)
        print(f"  read from: {read}")
        print(f"  looprs keys behaviour on {len(looprs_tags)} wire spellings (src/wire.rs)")
        print(f"  the candidate names {len(rho_tags)} event/variant spellings in those files")
        print(f"  intersection: {len(shared)} — {shared}")
        print(f"  i.e. {len(looprs_tags) - len(shared)} of looprs' {len(looprs_tags)} rows change meaning,")
        print("  not spelling, which is the expensive kind of change: the tests in")
        print("  `app::tests::wire_protocol` exist to make each row say what the harness")
        print("  does with the value, and every one of those rows is re-derived by hand.")
    else:
        print("  not measured (candidate sources unavailable)")


# ──────────────────────────────── leg 4: embed ────────────────────────────────
def run_capped(cmd, env, cap):
    """Run one arm with a wall-clock cap. An arm that hangs is a finding about the
    engine (an in-process run waiting on your own handler has nothing to kill it),
    so the cap is reported, not swallowed."""
    started = time.perf_counter()
    try:
        p = subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True,
                           text=True, env=env, timeout=cap)
        return p.returncode, (p.stdout or "") + (p.stderr or ""), time.perf_counter() - started
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or b"") + (e.stderr or b"")
        if isinstance(out, bytes):
            out = out.decode("utf-8", "replace")
        return 124, out + f"\nkilled at the {cap:.0f}s arm cap\n", time.perf_counter() - started


def leg_embed(target, versions, price):
    print("\n== 4. EMBED — drive one tool-using turn with the library linked into the process ==")
    if not price:
        print("not measured (the price leg did not produce a build)")
        return
    binary = os.path.join(price["target"], "release", "rho-embed-spike")
    if not os.path.exists(binary):
        print("not measured (no spike binary in the target dir)")
        return
    code = sh(f"curl -s -m 10 -o /dev/null -w '%{{http_code}}' {BASE_URL}/models")[1].strip()
    endpoint_ok = code == "200" and not os.environ.get("LOOPRS_SPIKE_NO_MODEL")
    if os.environ.get("LOOPRS_SPIKE_NO_MODEL"):
        print("endpoint arms: skipped (LOOPRS_SPIKE_NO_MODEL set) — the scripted arms below")
        print("are labelled results, not a substitute for the endpoint arm.")
    elif code != "200":
        print(f"endpoint arms: not measured — model endpoint {BASE_URL} answered {code!r}. No")
        print("scripted answer is substituted for them; see the labelled arms below.")
    else:
        print(f"endpoint:        {BASE_URL} answered {code}, model {MODEL}")
    cap = float(os.environ.get("LOOPRS_SPIKE_ARM_TIMEOUT", 300))
    print(f"arm cap:         {cap:.0f}s each (LOOPRS_SPIKE_ARM_TIMEOUT)")
    home = tempfile.mkdtemp(prefix="rho-home-")
    library = target["library_crates"][0]
    env = dict(os.environ, RHO_HOME=home, RHO_SPIKE_KEY="spike",
               RHO_SPIKE_BASE_URL=BASE_URL, RHO_SPIKE_MODEL=MODEL,
               RHO_SPIKE_CRATE_VERSION=versions[library][0] or "")
    for label, extra, needs_endpoint in target["arms"]:
        print(f"\n-- {label}")
        if needs_endpoint and not endpoint_ok:
            print("   not measured (no model endpoint; see the reason above)")
            continue
        if not needs_endpoint:
            print("   labelled arm: no model endpoint involved. It exercises the runtime,")
            print("   the in-process tool round trip and the event stream, and says nothing")
            print("   about the model — reported separately so it can never be read as the")
            print("   endpoint arm passing.")
        rc, out, secs = run_capped(binary, {**env, **extra}, cap)
        printed = 0
        for line in out.splitlines():
            # The programs print `key<TAB>value`, with indented note lines; anything
            # else is the library's own stderr (a panic message, a tokio warning)
            # and belongs to the arm that provoked it, not to the capture.
            if "\t" in line:
                print(f"   {line}")
                printed += 1
        print(f"   (rc={rc}, {secs:.1f}s, {printed} lines)")
        if rc != 0 or printed == 0:
            print("   raw tail of that arm's output:")
            for line in out.strip().splitlines()[-25:]:
                print(f"   | {line[:200]}")
    shutil.rmtree(home, ignore_errors=True)
    print("\nWhat this leg is for: it is the *decision-relevant* half. Everything the")
    print("loop needs from a child over stdio has to be re-expressed as this library's")
    print("calls, and the events the host is handed are the library's, not pi's — see")
    print("leg 3's intersection. The program that proves it is per target, because the")
    print("two candidates' APIs are not the same API.")


def main():
    import platform
    tid, target = select_target(sys.argv)
    versions, forced = crate_versions(target)
    print(f"workspace-8jk rho surface/price measurement — host {platform.system()} "
          f"{platform.release()}, {os.cpu_count()} cpus")
    print(f"identity:      {tid} — {target['identity_note']}")
    print(f"subject:       {target['repo']} (github.com/{target['repo']})")
    print(f"crates measured by this run ({len(versions)}):")
    for name, (ver, how) in versions.items():
        print(f"               {name} {ver or 'NOT RESOLVED'}   [{how}]")
    print(f"consumer:      {target['consumer_note']}")
    if forced:
        print("               NOTE: LOOPRS_SPIKE_RHO_VERSION applied one scalar to every crate.")
        print("               If these crates are not released in lockstep this run measured a")
        print("               mixtape that no consumer could install.")
    capture = target["capture"]
    if target.get("prior_capture"):
        capture += (f"  (the capture ADR-0010 §3 cites as filed is retained at "
                    f"{target['prior_capture']})")
    print(f"capture:       {capture}")
    print("Read before quoting a number: cold builds and one machine. The ADR cites")
    print("this capture; a number typed into a second page is a number that drifts.")
    scratch = tempfile.mkdtemp(prefix="eqn-rho-")
    try:
        price = leg_price(target, versions, scratch)
        leg_supply(target, versions)
        leg_surface(target, versions, scratch)
        leg_embed(target, versions, price)
    finally:
        if os.environ.get("KEEP_TARGET"):
            print(f"\n(scratch kept at {scratch})")
        else:
            shutil.rmtree(scratch, ignore_errors=True)
    print("\nDone.")


if __name__ == "__main__":
    main()
