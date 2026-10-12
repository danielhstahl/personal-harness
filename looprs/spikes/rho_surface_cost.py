#!/usr/bin/env python3
"""workspace-eqn.3 / .4 — what linking `rho` as looprs' engine would cost and what it would replace.

The proposal in ADR-0010 is that looprs stops driving `pi --mode rpc` as a child
and links the `rho` crate (a Rust re-implementation of this same harness shape,
built on `rig`) as a library. Two claims ride on that: less overhead, and typed
tool calls. Neither is the question this script answers. This script prices the
dependency the way `spikes/dep_cost.sh` prices a dependency and `dep_audit.py`
prices a manifest line — as a set of numbers, on this machine, from a cold build:

  1. **PRICE** — a scratch consumer crate (`rho = "0.12.0"`, one `main.rs` that
     actually calls the library) built `--release` in a target dir of its own,
     against looprs' own cold `--release` build measured the same way on the same
     machine. Crates compiled, crates in the shipped graph, wall time, artifact
     size, target-dir size. Nothing incremental: an incremental build answers
     "what changed since last time", which is not the question.
  2. **SUPPLY** — what the crate's own public record says about who maintains it:
     crates.io publish history and download count, GitHub stars/forks/subscribers,
     whether GitHub can even see a licence, commit and contributor counts, and the
     share of commits the single human author carries. Read live off the two APIs;
     printed as `not measured` when there is no network, never guessed.
  3. **SURFACE** — what looprs would have to re-establish: the crate's own source
     stats (files, LOC, tests, `unsafe`, exact version pins, optional features),
     the licence question settled against the *published artifact* rather than
     against a README, and the event-vocabulary overlap between what looprs'
     `WIRE_INVENTORY` depends on today and what rho's `RpcEvent`/`UiEvent`
     actually name. The overlap number is the migration cost in rows of
     `src/wire.rs`.
  4. **EMBED** — the claim tested rather than argued: build the scratch program so
     it drives **one tool-using turn in process** against a local
     OpenAI-compatible endpoint and prints what the host process sees: engine
     build time, turn time, RSS, tool list, and the event kinds it was handed.
     Run twice: once with rho's permission gate on (the shape an unattended loop
     must configure around) and once off. No model endpoint → the leg prints
     `not measured`.

    python3 spikes/rho_surface_cost.py | tee spikes/results/rho-surface-cost.log

Env:
  LOOPRS_SPIKE_RHO_VERSION  crate version to price (default: 0.12.0)
  LOOPRS_RHO_TARGET         reuse this target dir instead of a cold one (the
                            cold release build is ~4 min on 8 cores; say so in
                            the capture if you reuse one)
  KEEP_TARGET=1             do not delete the scratch crate on exit
  LOOPRS_SPIKE_BASE_URL     OpenAI-compatible endpoint for the EMBED leg
                            (default: http://llm.home:8080/v1; unset host with
                            LOOPRS_SPIKE_NO_MODEL=1 to skip the leg)
  LOOPRS_SPIKE_MODEL        model id for the EMBED leg
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
RHO_VERSION = os.environ.get("LOOPRS_SPIKE_RHO_VERSION", "0.12.0")
BASE_URL = os.environ.get("LOOPRS_SPIKE_BASE_URL", "http://llm.home:8080/v1")
MODEL = os.environ.get("LOOPRS_SPIKE_MODEL", "halogen-qwen3.8-flash-next")


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


# ───────────────────────────── the embed program (leg 4) ───────────────────────
# Kept in the spike rather than in looprs' own manifest for the obvious reason:
# pricing an alternative does not mean shipping it (ADR-0009 rule 2 is exactly
# this rule, arrived at from the other direction).
EMBED_MAIN = r'''
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rho::auth::AuthStore;
use rho::config::{Config, ProviderConfig};
use rho::engine::runner::{TurnRequest, TurnOutput};
use rho::presentation::{RecordingSink, StructuredPresenter};

fn rss_kib() -> u64 {
    for line in std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

#[tokio::main]
async fn main() {
    let base = Instant::now();
    let mut config = match Config::load(None) {
        Ok(c) => c,
        Err(e) => { println!("config.load\terr\t{e}"); std::process::exit(2); }
    };
    let base_url =
        std::env::var("RHO_SPIKE_BASE_URL").unwrap_or_else(|_| "http://llm.home:8080/v1".into());
    let model = std::env::var("RHO_SPIKE_MODEL")
        .unwrap_or_else(|_| "halogen-qwen3.8-flash-next".into());
    config.provider = "spike".into();
    config.model = model.clone();
    config.allow_private_network = true;
    config.providers.insert(
        "spike".into(),
        ProviderConfig {
            base_url,
            key_env: Some("RHO_SPIKE_KEY".into()),
            default_model: Some(model.clone()),
        },
    );
    // A host that runs unattended has to say what happens when a tool wants
    // approval and nobody is there. rho's knob for that is this one boolean.
    if std::env::var("RHO_SPIKE_NO_PERMISSION").is_ok() {
        config.permission.enabled = false;
    }
    let auth = match AuthStore::load(&config.auth_file) {
        Ok(a) => a,
        Err(_) => {
            std::fs::write(&config.auth_file, "{}").ok();
            match AuthStore::load(&config.auth_file) {
                Ok(a) => a,
                Err(e) => { println!("auth.load\terr\t{e}"); std::process::exit(2); }
            }
        }
    };
    let t_config = base.elapsed();

    let engine = match rho::platform::agent_engine(config.clone(), auth, None).await {
        Ok(e) => e,
        Err(e) => { println!("engine.build\terr\t{e}"); std::process::exit(3); }
    };
    let t_engine = base.elapsed();
    let tools = engine.tool_names();

    let sink = RecordingSink::new();
    let presenter = Arc::new(StructuredPresenter::recording(sink.clone()));
    let prompt = "Run the bash command `echo LOOPRS_SPIKE_OK` and then reply with its output.";
    let out = engine.run_turn(TurnRequest::new(prompt), presenter.clone()).await;
    let t_turn = base.elapsed();

    match out {
        Ok(TurnOutput { final_text, tool_calls_count, requests, status, .. }) => {
            println!("turn.status\t{status:?}");
            println!("turn.tool_calls\t{tool_calls_count}");
            println!("turn.model_requests\t{requests}");
            println!("turn.reply\t{}", final_text.trim().chars().take(70).collect::<String>());
        }
        Err(e) => println!("turn.status\terr\t{e}"),
    }
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for ev in sink.events() {
        let tag = serde_json::to_value(&ev).ok()
            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .unwrap_or_else(|| "?".into());
        *kinds.entry(tag).or_default() += 1;
    }
    println!("events.total\t{}", sink.events().len());
    for (k, v) in &kinds {
        println!("event.{k}\t{v}");
    }
    println!("tools.registered\t{}\t{}", tools.len(), tools.join(","));
    println!("ms.config\t{:.1}", t_config.as_secs_f64() * 1000.0);
    println!("ms.engine\t{:.1}", t_engine.as_secs_f64() * 1000.0);
    println!("ms.turn\t{:.1}", t_turn.as_secs_f64() * 1000.0);
    println!("rss_after_turn_kib\t{}", rss_kib());
}
'''

EMBED_MANIFEST = f"""[package]
name = "rho-embed-spike"
version = "0.1.0"
edition = "2021"

[dependencies]
rho = "{RHO_VERSION}"
tokio = {{ version = "1", features = ["rt-multi-thread", "macros"] }}
serde_json = "1"

[profile.release]
debug = false
strip = true
"""


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


def leg_price(scratch):
    print("\n== 1. PRICE — a cold release build of a consumer of the crate ==")
    crate = os.path.join(scratch, "rho-embed")
    os.makedirs(os.path.join(crate, "src"), exist_ok=True)
    open(os.path.join(crate, "Cargo.toml"), "w").write(EMBED_MANIFEST)
    open(os.path.join(crate, "src", "main.rs"), "w").write(EMBED_MAIN)
    target = os.environ.get("LOOPRS_RHO_TARGET") or os.path.join(scratch, "rho-target")
    reuse = os.path.isdir(os.path.join(target, "release"))
    print(f"scratch crate: {crate}")
    print(f"target dir:    {target}{'  (REUSED — timings are incremental, not cold)' if reuse else '  (cold)'}")
    if not reuse:
        wall, rc, out = timed_sh(f"cargo build --release --target-dir {target}", cwd=crate)
        crates = out.count("Compiling")
        print(f"rho leg:       rc={rc}  {wall:.0f}s wall  {crates} crates compiled")
        if rc != 0:
            print(out[-2500:])
            return None
    else:
        sh(f"cargo build --release --target-dir {target}", cwd=crate)
        print("rho leg:       reused target dir; compile timing skipped rather than reported as cold")
    pkgs = cargo_tree(crate)
    binpath = os.path.join(target, "release", "rho-embed-spike")
    size = os.path.getsize(binpath) if os.path.exists(binpath) else 0
    print(f"shipped graph: {len(pkgs)} packages (`cargo tree -e normal`)")
    print(f"artifact:      {size} B ({kb(size)}) — the spike binary with rho linked in")

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
        print(f"\ndelta:         +{len(pkgs) - len(lpkgs)} packages on the shipped graph, "
              f"{(size / lsize if lsize else 0):.2f}x the artifact")
    extra = sorted(p for p in pkgs if p not in lpkgs)
    print(f"packages rho would add that looprs does not ship: {len(extra)}")
    return {"target": target, "crate": crate}


# ─────────────────────────────── leg 2: supply ────────────────────────────────
def leg_supply():
    print("\n== 2. SUPPLY — what the public record says about the maintainer ==")
    c = fetch(f"https://crates.io/api/v1/crates/rho")
    if c is None:
        print("not measured: crates.io unreachable")
    else:
        k = c["crate"]
        print(f"crates.io:     rho {k['max_stable_version']}, "
              f"{k['num_versions']} releases, first published {k['created_at'][:10]}, "
              f"updated {k['updated_at'][:10]}")
        print(f"               downloads {k['downloads']} total / {k['recent_downloads']} last 90 days")
        print(f"               published licence field: {k.get('links') and 'see Cargo.toml (leg 3)'}")
    g = fetch("https://api.github.com/repos/casonadams/rho")
    if g is None:
        print("not measured: GitHub unreachable")
        return
    print(f"github:        {g['full_name']}: {g['stargazers_count']} stars, {g['forks_count']} forks, "
          f"{g['subscribers_count']} subscribers, {g['open_issues_count']} open issues")
    print(f"               created {g['created_at'][:10]}, last push {g['pushed_at'][:10]}")
    print(f"               licence as GitHub's licence detector sees it: "
          f"{(g.get('license') or {}).get('spdx_id', 'NONE DETECTED')}")
    con = fetch("https://api.github.com/repos/casonadams/rho/contributors?per_page=100") or []
    named = [(x["login"], x["contributions"]) for x in con if x.get("type") == "User"]
    bots = [(x["login"], x["contributions"]) for x in con if x.get("type") != "User"]
    total = sum(x["contributions"] for x in con) or 1
    top, topn = named[0] if named else ("?", 0)
    print(f"contributors:  {len(named)} human, {len(bots)} bot; {total} commits attributed")
    print(f"               {top} carries {topn} of {total} = {100 * topn / total:.1f}% "
          f"(bus factor of {sum(1 for _, n in named if n > total * 0.05)})")
    print(f"               the rest: {[(n, c) for n, c in named[1:6]]}")


# ─────────────────────────────── leg 3: surface ───────────────────────────────
def crate_src(name, version):
    """The published artifact's directory, from cargo's own cache when it has it,
    otherwise unpacked from static.crates.io into the scratch dir."""
    hits = sorted(glob.glob(os.path.expanduser(f"~/.cargo/registry/src/*/{name}-{version}")))
    if hits:
        return hits[-1]
    return None


def leg_surface(scratch):
    print("\n== 3. SURFACE — what the published crates.io artifact actually is ==")
    dirs = {}
    for name in ("rho", "rho-engine", "rho-harness-core"):
        d = crate_src(name, RHO_VERSION)
        if d is None:
            d = os.path.join(scratch, f"{name}-{RHO_VERSION}")
            rc, out = sh(f"curl -sS -m 60 -o {scratch}/{name}.crate https://static.crates.io/crates/{name}/{name}-{RHO_VERSION}.crate"
                         f" && mkdir -p {d} && tar xzf {scratch}/{name}.crate -C {d} --strip-components=1")
            if rc != 0:
                print(f"{name}: not measured (cargo cache empty and static.crates.io unreachable)")
                continue
        dirs[name] = d
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
        lic = [os.path.basename(p) for p in glob.glob(os.path.join(d, "*")) 
               if re.search(r"(?i)(licen[cs]|copying)", os.path.basename(p))]
        print(f"{name:18} {len(rs):4} .rs files  {loc:7} LOC  {unsafe:3} `unsafe`  "
              f"licence file in published artifact: {lic or 'NONE'}")
    print(f"{'total':18} {total_rs:4} .rs files  {total_loc:7} LOC  {total_unsafe:3} `unsafe`")

    # Exact pins and the optional surface, read out of the published manifest.
    rho_dir = dirs.get("rho")
    if rho_dir:
        man = open(os.path.join(rho_dir, "Cargo.toml"), errors="replace").read()
        pins = re.findall(r'^version = "=(.+)"$', man, re.M)
        # Only the [features] block: the normalised manifest also has indented
        # `features = [...]` lines under every dependency, and a first draft read
        # those as feature names and printed fourteen copies of the word.
        fb = re.search(r"^\[features\]\n(.*?)(?=^\[|\Z)", man, re.M | re.S)
        feats = re.findall(r"^(\w+) = ", fb.group(1), re.M) if fb else []
        devs = re.findall(r"\[dev-dependencies\.(\S+?)\]", man)
        print(f"\nexact-pinned dependencies (an `=` requirement each): {pins or 'none'}")
        print(f"features: {feats}")
        print(f"dev-dependencies (built for rho's own tests, not by a library consumer): {devs}")
        print("  That last line matters: `rig` and `iroh` are named in the ticket as what")
        print("  rho drags in, and in this version they are dev-dependencies. What a")
        print("  consumer actually links is leg 1's `cargo tree`, which is the number to quote.")

    # The migration cost, in rows of src/wire.rs: what looprs keys on today vs
    # what rho's protocol can even be asked to name.
    print("\nevent vocabulary — looprs' WIRE_INVENTORY vs rho's own protocol:")
    wire = open(os.path.join(REPO, "src", "wire.rs"), errors="replace").read()
    looprs_tags = sorted(set(re.findall(r'wire:\s*"([a-z_]+)"', wire)))
    rho_tags = set()
    for name in ("rho-harness-core", "rho"):
        d = dirs.get(name)
        if not d:
            continue
        for f in ("src/rpc/protocol.rs", "src/presentation/types.rs"):
            p = os.path.join(d, f)
            if not os.path.exists(p):
                continue
            body = open(p, errors="replace").read()
            for variant in re.findall(r"^\s{4}([A-Z][A-Za-z0-9]+)[\s{,]", body, re.M):
                rho_tags.add(re.sub(r"(?<!^)(?=[A-Z])", "_", variant).lower())
    if rho_tags:
        shared = sorted(set(looprs_tags) & rho_tags)
        print(f"  looprs keys behaviour on {len(looprs_tags)} wire spellings (src/wire.rs)")
        print(f"  rho names {len(rho_tags)} event/variant spellings across RpcEvent + UiEvent")
        print(f"  intersection: {len(shared)} — {shared}")
        print(f"  i.e. {len(looprs_tags) - len(shared)} of looprs' {len(looprs_tags)} rows change meaning,")
        print("  not spelling, which is the expensive kind of change: the tests in")
        print("  `app::tests::wire_protocol` exist to make each row say what the harness")
        print("  does with the value, and every one of those rows is re-derived by hand.")
    else:
        print("  not measured (rho sources unavailable)")


# ──────────────────────────────── leg 4: embed ────────────────────────────────
def leg_embed(price):
    print("\n== 4. EMBED — drive one tool-using turn with rho linked into the process ==")
    if os.environ.get("LOOPRS_SPIKE_NO_MODEL"):
        print("not measured (LOOPRS_SPIKE_NO_MODEL set)")
        return
    code = sh(f"curl -s -m 10 -o /dev/null -w '%{{http_code}}' {BASE_URL}/models")[1].strip()
    if code != "200":
        print(f"not measured: model endpoint {BASE_URL} answered {code!r}")
        return
    if not price:
        print("not measured (the price leg did not produce a build)")
        return
    home = tempfile.mkdtemp(prefix="rho-home-")
    env = dict(os.environ, RHO_HOME=home, RHO_SPIKE_KEY="spike",
               RHO_SPIKE_BASE_URL=BASE_URL, RHO_SPIKE_MODEL=MODEL)
    binary = os.path.join(price["target"], "release", "rho-embed-spike")
    for label, extra in (("permission gate ON (rho's default)", {}),
                         ("permission gate OFF (config.permission.enabled=false)",
                          {"RHO_SPIKE_NO_PERMISSION": "1"})): 
        print(f"\n-- {label}")
        rc, out = sh(binary, env={**env, **extra})
        for line in out.splitlines():
            if line.startswith(("turn.", "event", "events.", "tools.", "ms.", "rss_", "config.", "auth.", "engine.")):
                print(f"   {line}")
        if rc != 0:
            print(f"   rc={rc}: {out.strip()[-300:]}")
    shutil.rmtree(home, ignore_errors=True)
    print("\nWhat this leg is for: it is the *decision-relevant* half. Everything the")
    print("loop needs from a child over stdio has to be re-expressed as these calls")
    print("(`platform::agent_engine`, `engine::run_turn`, a `Presenter`), and the")
    print("events the host is handed are rho's, not pi's — see leg 3's intersection.")


def main():
    import platform
    print(f"workspace-eqn rho surface/price measurement — host {platform.system()} "
          f"{platform.release()}, {os.cpu_count()} cpus")
    print(f"crate under assessment: rho {RHO_VERSION} (casonadams/rho)")
    print("Read before quoting a number: cold builds and one machine. The ADR cites")
    print("this capture; a number typed into a second page is a number that drifts.")
    scratch = tempfile.mkdtemp(prefix="eqn-rho-")
    try:
        price = leg_price(scratch)
        leg_supply()
        leg_surface(scratch)
        leg_embed(price)
    finally:
        if os.environ.get("KEEP_TARGET"):
            print(f"\n(scratch kept at {scratch})")
        else:
            shutil.rmtree(scratch, ignore_errors=True)
    print("\nDone.")


if __name__ == "__main__":
    main()
