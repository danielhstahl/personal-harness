#!/usr/bin/env python3
"""ADR-0010 §4 trigger 3 as four testable checks, scored against both libraries that answer to "rho" — matthewyjiang/rho (the crate a consumer links is `rho-sdk`) and casonadams/rho (`rho`, `rho-engine`, `rho-harness-core`).

What this is
------------
ADR-0010 §4 trigger 3 says `rho` may be re-priced once it "becomes adoptable as a
dependency", and lists four conditions. As filed, three of the four were fill-in
phrasings rather than tests: *share of commits* named no denominator, *human
contributor* named no bar, *history* named no clock, and *a documented event
vocabulary carrying a settle signal* named no checkable form. A trigger whose
terms are prose gets re-litigated by whoever wants it to fire, so this driver
writes the four as measurements and runs them.

It scores **both** candidates, because the ADR filed its numbers against
casonadams/rho and the decision was requested against matthewyjiang/rho: a
definition that is not run against both cannot tell you whether the identity
correction changed the answer.

Nothing here builds, links or ships anything (ADR-0009 rule 2: pricing an
alternative does not require shipping it). Every leg reads a public record —
GitHub, crates.io, the published `.crate` artifact, the candidate's published
docs — or a capture already committed under `spikes/results/`.

The four definitions (the memo is the normative text; this is the code form)
--------------------------------------------------------------------------
1. **LICENCE** — the *received* licence, not the declared one. A crate passes
   when its published artifact (the `.crate` a consumer actually gets) carries a
   file named `/licen[cs]|copying/i`. Scored on **every crate a library consumer
   links**; a sibling crate's LICENSE does not travel inside another crate's
   artifact, and a `license = "MIT"` manifest string is a declaration. The
   declared side is printed beside the received side so the gap is visible.
2. **BUS FACTOR** — share of commits is measured against **human-authored commits
   only**: contributors whose GitHub `type` is not `User` (github-actions,
   dependabot, devin-ai-integration, pullfrog) are counted, printed and excluded
   from the denominator, because automation must be able to neither earn nor
   dilute a maintainer's share. Both denominators are printed on every run, so a
   re-run cannot change the convention quietly. A *human contributor* to the ≥2
   requirement is a `type == User` account carrying **>5%** of that human
   denominator — the bar is the whole test, so a drive-by commit is counted in the
   denominator and cannot satisfy the ≥2. Below 20 attributed human commits the
   percentage is printed and the check reports `not measured`: a ratio over a
   handful of commits is not a maintenance signal.
3. **HISTORY** — three clocks, one bar. Ages are taken to the moment of evaluation
   for (a) the repo's GitHub `created_at`, (b) its first GitHub release, (c) the
   first crates.io publish of the crate a consumer links. The bar is **12 months
   (365 d) on every one of the three**, i.e. the *shortest* clock binds: the
   strictest reading is chosen on purpose, because a definition that lets a
   friendlier clock rescue the trigger is the definition this driver exists to
   replace (matthewyjiang/rho's repo predates rho-sdk's first publish by 24 days,
   so the clocks differ by weeks and the answer must not). Extra clocks — first
   ≥1.0.0 publish, family-wide first publish — are printed as context, and a
   **liveness guard** refuses a dead repo a history it is no longer making: last
   push or last publish within 90 days.
4. **SETTLE VOCABULARY** — five sub-tests, all required. The terminal event set is
   a *declared claim* under test (below, in `TERMINAL`), not a vibe: S1 checks each
   name exists in the candidate's host-facing vocabulary **in the published
   artifact** — a name the artifact does not contain fails the run rather than
   ageing quietly into a wrong sentence, which is `rho_inventory_rescore.py`'s
   convention. S2 checks the name is rendered in the published API docs (docs.rs).
   S3 checks a **prose documentation page** names it and states when it fires —
   rustdoc proves an identifier shipped, prose proves a contract was written. S4
   checks the terminal signal is actually *delivered to the host on the stream* the
   loop reads, read out of a committed embed capture's own event census. S5 checks
   the delivery is **guaranteed on every path the run can end** — the two ways to
   fail it are the candidate's own docs disclaiming it, or a committed capture
   showing an arm where a run stopped and no terminal event arrived. S4 and S5 are
   what make the fourth condition mean "looprs can key a loop advance on it" and
   not "the spelling exists".

Run
---
    python3 spikes/dependency_maintenance_metrics.py                 # both candidates
    python3 spikes/dependency_maintenance_metrics.py --id matthewyjiang
    python3 spikes/dependency_maintenance_metrics.py --list

Seconds per candidate: no build, no model call, no endpoint. Every leg that cannot
run prints `not measured` with the reason and can never print PASS; if no leg can
run the driver exits 2 with the reason, so "no network" is never read as a verdict.

Knobs: `LOOPRS_SPIKE_RHO_ID` selects one candidate; `LOOPRS_DOCS_MAX` caps the
documentation pages fetched per candidate (default 30); `LOOPRS_KEEP_TMP` keeps
the scratch dir holding the downloaded artifacts; `GITHUB_TOKEN`, if present, is
sent to api.github.com so the anonymous 60/hour quota is not the limiter.
"""

import datetime
import glob
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SPIKES = os.path.join(REPO, "spikes")
MEMO = "docs/adr/0010-trigger-3-dependency-maintenance-metrics.md"
MONTH_DAYS = 365 / 12.0          # the "12 months" of trigger 3, as days
BAR_DAYS = 365                   # ≥12 months
LIVENESS_DAYS = 90               # a dead repo does not get to spend its history
MIN_HUMAN_COMMITS = 20           # below this, a share is not a maintenance signal
LICE_RE = re.compile(r"(?i)(licen[cs]|copying)")

_spec = importlib.util.spec_from_file_location(
    "rho_surface_cost", os.path.join(SPIKES, "rho_surface_cost.py"))
RHO = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(RHO)        # constants and helpers only; main() is guarded
TARGETS = RHO.TARGETS

# The terminal event set each candidate CLAIMS, and where its host-facing vocabulary
# lives inside the published artifact. These are claims under test (S1–S3), not
# answers: rename a variant upstream and S1 says so.
TERMINAL = {
    "matthewyjiang": {
        "names": ["Completed", "Failed", "Cancelled"],
        "claim": "RunEvent::{Completed,Failed,Cancelled} settle a run; Run::outcome "
                 "is the authoritative result",
        "vocab": {"rho-sdk": ["src/event.rs"]},
        "api_enums": {"rho-sdk": ["RunEvent"]},
        "capture": os.path.join(REPO, "spikes/results/matthewyjiang-surface-cost.log"),
        "capture_kind": "rho-sdk",
        "layer": "the RunEvent stream the host consumes from Run::next_event()",
    },
    "casonadams": {
        "names": ["TurnEnd", "print_turn_completed"],
        "claim": "RpcEvent::TurnEnd names a turn end in the RPC protocol, and "
                 "Presenter::print_turn_completed is a hook for it — both are "
                 "surface, not delivery: the embed arm saw completion arrive as "
                 "run_turn's return value",
        "vocab": {"rho-harness-core": ["src/rpc/protocol.rs",
                                       "src/presentation/presenter.rs",
                                       "src/presentation/types.rs"]},
        "api_enums": {"rho-harness-core": ["RpcEvent", "UiEvent", "Presenter"],
                      "rho": []},
        "capture": os.path.join(REPO, "spikes/results/rho-surface-cost.log"),
        "capture_kind": "rho-rpc",
        "layer": "RpcEvent (the RPC protocol enum) and the Presenter trait the "
                 "embed host implements — the two vocabularies an in-process host sees",
    },
}

# What in a candidate's published documentation counts as a settle claim.
DOC_SENTENCE = re.compile(r"(?i)\bterminal|emits?\b|complet|settles?\b|end of (the )?run|turn.complete")
DISCLAIMER = re.compile(r"(?i)does not guarantee|not guaranteed|may end the stream without|"
                        r"can close the channel without|without one|no terminal event")
AUTHORITY = re.compile(r"(?i)authoritative")


# ────────────────────────────── plumbing ──────────────────────────────────────
def sh(cmd, timeout=60):
    p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
    return p.returncode, (p.stdout + p.stderr).strip()


def get(url, kind="json", timeout=30):
    """A GET that answers `None` instead of guessing when there is no network.

    GitHub's anonymous core quota is 60 requests an hour and one full two-candidate
    run spends a fifth of it per leg, so `GITHUB_TOKEN` — when the environment has
    one — is sent as a bearer token and the quota stops being the thing that decides
    whether this runs. An unauthenticated run is still a real run; a rate-limited
    answer is reported as `not measured`, never as a verdict.
    """
    tok = os.environ.get("GITHUB_TOKEN") or ""
    cmd = ["curl", "-sSL", "-m", str(timeout), "-H", "User-Agent: looprs-spike/1.0"]
    if tok and "api.github.com" in url:
        cmd += ["-H", f"Authorization: Bearer {tok}"]
    cmd.append(url)
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout + 10)
    except Exception:                                   # noqa: BLE001 - no network is a leg, not a crash
        return None
    if p.returncode != 0 or not p.stdout.strip():
        return None
    body = p.stdout
    if kind == "json":
        try:
            return json.loads(body)
        except json.JSONDecodeError:
            return None
    return body


_VERSIONS = {}


def resolve_versions(target):
    """crate -> (version, how it was chosen), resolved once per candidate."""
    key = target["repo"]
    if key not in _VERSIONS:
        _VERSIONS[key] = RHO.crate_versions(target)[0]
    return _VERSIONS[key]


def verdict_word(v):
    return "PASS" if v else "FAIL"


def day(text):
    """`2026-07-15T18:11:51.444972Z` or `2026-07-15` as a date, or None."""
    if not text:
        return None
    try:
        return datetime.date.fromisoformat(str(text)[:10])
    except ValueError:
        return None


def artifact_dir(crate, version, scratch):
    """The published artifact's directory: cargo's cache if it has it, else a fetch.

    `None` means neither worked, which is reported as `not measured` and never as a
    licence file being absent — the difference between a FAIL and no answer.
    """
    hits = sorted(glob.glob(os.path.expanduser(f"~/.cargo/registry/src/*/{crate}-{version}")))
    if hits:
        return hits[-1], "cargo cache"
    d = os.path.join(scratch, f"{crate}-{version}")
    rc, _ = sh(f"curl -sS -m 60 -o {scratch}/{crate}.crate "
               f"https://static.crates.io/crates/{crate}/{crate}-{version}.crate "
               f"&& mkdir -p {d} && tar xzf {scratch}/{crate}.crate -C {d} --strip-components=1")
    if rc != 0 or not os.path.isdir(d):
        return None, None
    return d, "fresh static.crates.io fetch"


ENUM_RE = re.compile(r"^\s*pub enum\s+(\w+)[^ {]*\s*\{", re.M)
TRAIT_RE = re.compile(r"^\s*pub trait\s+(\w+)", re.M)


def vocab_of(path):
    """`{enum Name: [variants], trait Name: [methods]}` scraped from one source file."""
    try:
        text = open(path, encoding="utf-8", errors="replace").read()
    except OSError:
        return {}
    out = {}
    for m in ENUM_RE.finditer(text):
        body, depth, i = [], 1, m.end()
        while i < len(text) and depth:
            c = text[i]
            depth += (c == "{") - (c == "}")
            if depth:
                body.append(c)
            i += 1
        out[m.group(1)] = re.findall(r"^\s{4}(?:\/\/&?\[.*\]\s*)*([A-Z]\w*)\s*(?:\{|\(|,|;)",
                                     "".join(body), re.M)
    for m in TRAIT_RE.finditer(text):
        rest = text[m.end():]
        methods, depth, in_body = [], 0, False
        for line in rest.splitlines():
            s = line.strip()
            depth += line.count("{") - line.count("}")
            in_body = in_body or depth > 0
            fm = re.match(r"(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+(\w+)", s)
            if in_body and depth > 0 and fm:
                methods.append(fm.group(1))
            if depth <= 0 and in_body:
                break
        out[m.group(1)] = methods
    return out


def doc_pages(repo, cap=None):
    """README + every `docs/**.md` of the published repo, through one tree call."""
    cap = cap or int(os.environ.get("LOOPRS_DOCS_MAX", "30"))
    repo_json = get(f"https://api.github.com/repos/{repo}") or {}
    ref = repo_json.get("default_branch") or "HEAD"
    tree = get(f"https://api.github.com/repos/{repo}/git/trees/{ref}?recursive=1")
    if not tree or "tree" not in tree:
        return None, None
    paths = [t["path"] for t in tree["tree"] if t["path"].endswith(".md")]
    picked = [p for p in paths if p == "README.md"]
    docs = [p for p in paths if p.startswith("docs/")
            and not re.search(r"(?i)changelog|release-notes|/plans?/", p)]
    docs.sort(key=lambda p: (0 if re.search(r"(?i)event|sdk|session|concept|rpc|stream", p) else 1, len(p)))
    return picked + docs[:cap - len(picked)], ref


def grep_pages(repo, ref, pages, names):
    """Fetch the published documentation and pull out every settle-shaped sentence."""
    out = {"pages": 0, "sentences": [], "disclaimers": [], "authority": [], "unreachable": []}
    for path in pages:
        text = get(f"https://raw.githubusercontent.com/{repo}/{ref}/{path}", kind="text")
        if text is None:
            out["unreachable"].append(path)
            continue
        out["pages"] += 1
        for line in text.splitlines():
            if len(line) > 4000:
                continue
            named = [n for n in names if re.search(rf"`[^`]*\b{n}\b[^`]*`|::{n}\b|\b{n}\b", line)]
            if named and DOC_SENTENCE.search(line):
                out["sentences"].append((path, line.strip(), named))
            if DISCLAIMER.search(line) and re.search(r"(?i)terminal|event|stream|settle", line):
                out["disclaimers"].append((path, line.strip()))
            if AUTHORITY.search(line) and re.search(r"(?i)outcome|terminal|authoritative", line):
                out["authority"].append((path, line.strip()))
    return out


def api_variants(crate, enums):
    """Which declared names the *rendered* API docs carry: {name: url} plus the pages read."""
    lib = crate.replace("-", "_")
    base = f"https://docs.rs/{crate}/latest/{lib}/"
    all_html = get(base + "all.html", kind="text")
    if all_html is None:
        return None, None
    found, pages = {}, []
    hrefs = set(re.findall(r'href="([^"]+\.html)"', all_html))
    for enum in enums:
        page = next((h for h in hrefs if h.endswith(f"enum.{enum}.html")
                     or h.endswith(f"trait.{enum}.html")), None)
        if not page:
            continue
        url = base + page.lstrip("./")
        html = get(url, kind="text")
        if html is None:
            continue
        pages.append(url)
        for name in set(re.findall(r'id="(?:variant|method)\.([A-Za-z0-9_]+)"', html)):
            found.setdefault(name, url)
    return found, pages


def capture_evidence(path, kind):
    """Read the embed capture's own event census — S4 and S5 evidence, already run."""
    if not os.path.isfile(path):
        return None
    text = open(path, encoding="utf-8", errors="replace").read()
    ev = {"path": os.path.relpath(path, REPO), "census": {}, "settles": [], "gaps": [],
          "totals": {}, "arms": 0}
    for line in text.splitlines():
        s = line.strip()
        if "\t" not in s:
            continue
        k, v = s.split("\t", 1)
        k, v = k.strip(), v.strip()
        if kind == "rho-rpc":
            if k.startswith("event."):
                ev["census"][k[6:]] = ev["census"].get(k[6:], 0) + int(v or 0)
            elif k == "events.total":
                ev["arms"] += 1
                ev["totals"]["total"] = ev["totals"].get("total", 0) + int(v)
            elif k == "turn.status":
                ev["settles"].append(("run_turn return value", v))
        else:
            if k.endswith("events_by_kind"):
                ev["arms"] += 1
                for part in v.split(","):
                    if "=" in part:
                        name, n = part.rsplit("=", 1)
                        ev["census"][name] = ev["census"].get(name, 0) + int(n)
            elif k.endswith("events_total"):
                ev["totals"]["total"] = ev["totals"].get("total", 0) + int(v)
            elif k.endswith((".settle", ".settle_event")):
                ev["settles"].append((k, v))
            elif "terminal_events_observed" in k:
                ev["gaps"].append((k, v))
            elif "NONE" in v and re.search(r"(?i)settle|stream closed", k + v):
                ev["gaps"].append((k, v))
            elif "session_state_while_stalled" in k:
                ev["gaps"].append((k, v))
    return ev


# ─────────────────────────────── the four legs ────────────────────────────────
def leg_licence(t, target, now, scratch):
    print("\n== 1. LICENCE — the licence a consumer RECEIVES, not the one a manifest declares ==")
    libs, family = target["library_crates"], list(target["crates"])
    versions = resolve_versions(target)
    read, lib_missing, unread = [], [], []
    for crate in family:
        version = (versions.get(crate) or (None, None))[0]
        if not version:
            print(f"{crate}: not measured (crates.io would not name a version)")
            if crate in libs:
                unread.append(crate)
            continue
        d, origin = artifact_dir(crate, version, scratch)
        if d is None:
            print(f"{crate} {version}: not measured (no cargo cache entry, and "
                  f"static.crates.io would not hand over the .crate)")
            if crate in libs:
                unread.append(crate)
            continue
        files = sorted({os.path.basename(p) for p in
                        glob.glob(os.path.join(d, "**", "*"), recursive=True)
                        if os.path.isfile(p) and LICE_RE.search(os.path.basename(p))})
        claim = None
        for man in ("Cargo.toml.orig", "Cargo.toml"):
            p = os.path.join(d, man)
            if os.path.isfile(p):
                m = re.search(r'^license\s*=\s*"([^"]+)"', open(p, encoding="utf-8", errors="replace").read(), re.M)
                if m:
                    claim = m.group(1)
                    break
        read.append(crate)
        print(f"{crate} {version}:  {origin}")
        print(f"    licence file inside the published artifact: {files or 'NONE'}")
        print(f"    manifest declares: license = \"{claim}\"  (a declaration, not a grant)")
        print(f"    read from: {origin if origin != 'cargo cache' else 'the local cargo cache copy of ' + crate + '-' + version}"
              + (f" (published as https://static.crates.io/crates/{crate}/{crate}-{version}.crate)"
                 if origin == 'cargo cache' else ''))
        if crate in libs and not files:
            lib_missing.append(crate)
    if unread or not read:
        print(f"verdict: licence = not measured "
              f"(could not read the artifact of {', '.join(unread) or 'any crate'})")
        return None
    verdict = "PASS" if not lib_missing else "FAIL"
    g = get(f"https://api.github.com/repos/{target['repo']}")
    g = g if isinstance(g, dict) else {}
    root = get(f"https://api.github.com/repos/{target['repo']}/contents/")
    root_has = isinstance(root, list) and any(
        LICE_RE.search(x.get("name") or "") for x in root if isinstance(x, dict))
    print(f"    (context) repo-root LICENSE: {'present' if root_has else 'absent'}"
          f"   licence GitHub's detector reads: "
          f"{(g.get('license') or {}).get('spdx_id', 'NONE DETECTED')}")
    print(f"    rule: scored on the crate(s) a consumer links — {', '.join(libs)} — not on a "
          f"sibling crate's file, which does not travel inside another artifact")
    if lib_missing:
        print(f"    crates a consumer links that ship no licence text: {', '.join(lib_missing)}")
    print(f"verdict: licence = {verdict}")
    return verdict


def leg_bus_factor(t, target, now):
    print("\n== 2. BUS FACTOR — share of commits, and whose share counts ==")
    con, page = [], 1
    while True:
        rows = get(f"https://api.github.com/repos/{target['repo']}"
                   f"/contributors?per_page=100&page={page}")
        # A rate-limited or errored answer is a dict ({"message": …}), not a list:
        # that is this leg not running, and it must not be read as "no contributors".
        if not isinstance(rows, list):
            con = []
            break
        con += rows
        if len(rows) < 100:
            break
        page += 1
        if page > 10:
            break
    if not con:
        print("not measured: GitHub's contributor list is empty, rate-limited or "
              "unreachable — an absent answer is never a PASS")
        print("verdict: bus_factor = not measured")
        return None
    print(f"read from: https://api.github.com/repos/{target['repo']}/contributors"
          f"?per_page=100 ({page} page(s), GitHub's own commit attribution, default branch)")
    humans = [(x["login"], x["contributions"]) for x in con if x.get("type") == "User"]
    bots = [(x["login"], x["contributions"]) for x in con if x.get("type") != "User"]
    total = sum(x["contributions"] for x in con) or 1
    human_total = sum(n for _, n in humans)
    print(f"attributed commits: {total} all-time, of which {human_total} human "
          f"({total - human_total} bot: {', '.join(f'{a} {b}' for a, b in bots) or 'none'})")
    over_human = [(l, n) for l, n in humans if human_total and n > 0.05 * human_total]
    over_all = [(l, n) for l, n in humans if n > 0.05 * total]
    for login, n in humans[:6]:
        note = "counts toward the ≥2" if (login, n) in over_human else \
               "drive-by: counted in the denominator, cannot satisfy the ≥2"
        print(f"    {login:26} {n:5}   {100 * n / total:5.1f}% of all   "
              f"{(100 * n / human_total if human_total else 0):5.1f}% of human   {note}")
    if len(humans) > 6:
        print(f"    … {len(humans) - 6} further humans below the printed cut")
    print("bar: ≥2 humans each carrying >5% of the HUMAN denominator")
    print(f"    humans above 5% of human commits: {len(over_human)} "
          f"({', '.join(f'{l} {n}' for l, n in over_human) or 'nobody'})")
    print(f"    same test against the all-commits denominator: {len(over_all)} "
          f"(printed so a change of convention is visible, not quiet)")
    if human_total < MIN_HUMAN_COMMITS:
        print(f"    denominator floor: only {human_total} attributed human commits "
              f"(floor {MIN_HUMAN_COMMITS}); a share over that is not a maintenance signal")
        print("verdict: bus_factor = not measured")
        return None
    gap = ""
    if len(over_human) < 2 and len(humans) > 1:
        s0 = humans[1][1]
        need = int((0.05 * human_total - s0) / 0.95) + 1
        gap = (f" — the second human ({humans[1][0]}, {s0}) needs {max(need, 1)} more "
               f"attributed commits to clear the bar alone")
    elif len(over_human) < 2:
        gap = " — there is no second human at all"
    print(f"verdict: bus_factor = {'PASS' if len(over_human) >= 2 else 'FAIL'}"
          f" (bus factor {max(len(over_human), 1) if humans else 0}{gap})")
    return "PASS" if len(over_human) >= 2 else "FAIL"


def leg_history(t, target, now, scratch):
    print("\n== 3. HISTORY — which clock, and does it clear twelve months ==")
    g = get(f"https://api.github.com/repos/{target['repo']}")
    if not isinstance(g, dict) or not g.get("created_at"):
        print("not measured: the GitHub repo API is rate-limited or unreachable")
        print("verdict: history = not measured")
        return None
    clocks, labels = [], []
    created = day(g.get("created_at"))
    if created:
        clocks.append(created)
        labels.append(("repo created (GitHub)", created, created.isoformat()))
    rel = get(f"https://api.github.com/repos/{target['repo']}/releases?per_page=100")
    allrel = rel if isinstance(rel, list) else []
    p = 2
    while allrel and len(rel) == 100 and p <= 6:
        rel = get(f"https://api.github.com/repos/{target['repo']}/releases?per_page=100&page={p}")
        if not isinstance(rel, list):
            break
        allrel += rel
        p += 1
    if allrel:
        first = min(allrel, key=lambda x: x.get("published_at") or "9999")
        d = day(first.get("published_at"))
        if d:
            clocks.append(d)
            labels.append(("first GitHub release", d,
                           f"{first.get('tag_name')} {d.isoformat()}"))
    lib = target["library_crates"][0]
    io = get(f"https://crates.io/api/v1/crates/{lib}")
    lib_first = None
    if isinstance(io, dict) and isinstance(io.get("versions"), list) and io["versions"]:
        vs = sorted(io["versions"], key=lambda v: v.get("created_at") or "9999")
        lib_first = day(vs[0].get("created_at"))
        if lib_first:
            clocks.append(lib_first)
            labels.append((f"first crates.io publish of {lib}", lib_first,
                           f"{vs[0].get('num')} {lib_first.isoformat()}"))
    if not clocks:
        print("not measured: no clock could be read")
        print("verdict: history = not measured")
        return None
    binding = max(clocks)                                  # the youngest clock binds
    print(f"read from: GitHub repo + releases APIs, crates.io versions API; "
          f"ages measured to {now.isoformat()} (12 months = {BAR_DAYS} d)")
    for name, d, note in labels:
        age = (now - d).days
        print(f"    clock {name:38} {note:34} {age:4} d = {age / MONTH_DAYS:4.1f} months"
              f"   {'BINDS (shortest)' if d == binding else 'clears' if age >= BAR_DAYS else 'short'}")
    # context clocks, printed so a future reader can see what the choice cost
    fam_first, fam_note = None, ""
    for crate in target["crates"]:
        io2 = get(f"https://crates.io/api/v1/crates/{crate}")
        if not (isinstance(io2, dict) and isinstance(io2.get("versions"), list) and io2["versions"]):
            continue
        vs = sorted(io2["versions"], key=lambda v: v.get("created_at") or "9999")
        d = day(vs[0].get("created_at"))
        if d and (fam_first is None or d < fam_first):
            fam_first, fam_note = d, f"{crate} {vs[0].get('num')} {d.isoformat()}"
    stable = ""
    if isinstance(io, dict) and isinstance(io.get("versions"), list) and io["versions"]:
        st = [v for v in sorted(io["versions"], key=lambda v: v.get("created_at") or "9999")
              if not re.search(r"(?i)alpha|beta|rc|pre", v.get("num", ""))
              and re.match(r"^[1-9]\.", v.get("num", ""))]
        stable = f"{st[0].get('num')} {day(st[0].get('created_at')).isoformat()}" if st else "never (no ≥1.0.0 publish)"
    push = day(g.get("pushed_at"))
    live = push and (now - push).days <= LIVENESS_DAYS
    print(f"    context, printed not scored: family-wide first publish "
          f"{fam_note or '?'}; first ≥1.0.0 publish of {lib}: {stable}; "
          f"last push {push.isoformat() if push else '?'}")
    print(f"    liveness guard (a dead repo may not spend history it stopped making): "
          f"last push {(now - push).days if push else '?'} d ago, bar ≤{LIVENESS_DAYS} d → "
          f"{'PASS' if live else 'FAIL'}")
    fail = [n for n, d, _ in labels if (now - d).days < BAR_DAYS]
    print(f"    rule: EVERY clock ≥ {BAR_DAYS} d, so the youngest one binds — "
          f"{len(fail)} of {len(labels)} fall short: {', '.join(fail) or 'none'}")
    print(f"    earliest date this leg can pass: {(binding + datetime.timedelta(days=BAR_DAYS)).isoformat()} "
          f"(the binding clock's own date + 12 months); re-run after it rather than re-arguing before it")
    verdict = "PASS" if (not fail and live) else "FAIL"
    print(f"verdict: history = {verdict}")
    return verdict


def leg_settle(t, target, now, scratch):
    spec = TERMINAL[t]
    print("\n== 4. SETTLE VOCABULARY — a signal a loop advance can be keyed on ==")
    print(f"the vocabulary in question: {spec['layer']}")
    print(f"claim under test:           {spec['claim']}")
    names = spec["names"]
    print(f"declared terminal set:      {', '.join(names)}")
    versions = resolve_versions(target)
    found, where, vocab_read, found_file = {}, {}, False, {}
    for crate, files in spec["vocab"].items():
        version = (versions.get(crate) or (None, None))[0]
        if not version:
            continue
        d, _ = artifact_dir(crate, version, scratch)
        if d is None:
            continue
        vocab_read = True
        for rel in files:
            for name, items in vocab_of(os.path.join(d, rel)).items():
                for n in names:
                    if n in items:
                        found.setdefault(n, f"{crate}:{rel} → {name}")
                        found_file.setdefault(n, os.path.join(d, rel))
                        where.setdefault(f"{crate}:{rel}", []).append(n)
    s1 = None if not vocab_read else len(found) == len(names)
    print("S1 named in the published artifact's host-facing vocabulary: "
          f"{verdict_word(s1) if s1 is not None else 'not measured'}")
    for n in names:
        note = ""
        if n in found_file:
            try:
                src = open(found_file[n], encoding="utf-8", errors="replace").read()
            except OSError:
                src = ""
            if re.search(rf"fn\s+{n}\b[^;]*\{{\s*\}}", src):
                note = "   (declared with an EMPTY default body: a host that does not override it is handed nothing at that moment)"
        print(f"    {n:22} {found.get(n, 'NOT FOUND in the declared vocabulary files')}{note}")
    # S2: rendered API docs
    rendered, pages_seen, s2 = {}, [], None
    for crate, enums in spec["api_enums"].items():
        got, pages = api_variants(crate, enums or [])
        if got is None:
            print(f"    (docs.rs {crate}: not reachable)")
            continue
        rendered.update(got)
        pages_seen += pages
    if pages_seen:
        s2 = all(n in rendered for n in names)
    print(f"S2 rendered in the published API docs (docs.rs): "
          f"{verdict_word(s2) if s2 is not None else 'not measured'}")
    for n in names:
        print(f"    {n:22} {rendered.get(n, 'not found on the rendered item pages')}")
    if pages_seen:
        print(f"    read from: {' '.join(pages_seen)}")
    # S3: prose documentation, and the disclaimers S5 reads
    pages, ref = doc_pages(target["repo"])
    hits = {"pages": 0, "sentences": [], "disclaimers": [], "authority": [], "unreachable": []}
    if pages is None:
        print("S3 prose-documented: not measured (no tree from GitHub)")
    else:
        hits = grep_pages(target["repo"], ref, pages, names)
    good = [h for h in hits["sentences"] if any(n in h[2] for n in names)]
    good.sort(key=lambda h: -len(set(h[2])))
    s3 = None if pages is None else bool(good)
    print(f"S3 prose-documented (a written page states when it fires): "
          f"{verdict_word(s3) if s3 is not None else 'not measured'}"
          f"  — {hits['pages']} of {len(pages) if pages else 0} published pages read "
          f"(README + docs/**.md"
          f"{', ' + str(len(hits['unreachable'])) + ' unreachable' if hits['unreachable'] else ''})")
    for path, line, named in good[:4]:
        print(f"    https://raw.githubusercontent.com/{target['repo']}/{ref}/{path}: {line[:230]}")
    if pages and not good:
        print("    no published page names the terminal set and states when it fires: "
              "an identifier in rustdoc is a spelling shipped, not a contract written")
    for path, line in hits["authority"][:2]:
        print(f"    authority claim — https://raw.githubusercontent.com/{target['repo']}/{ref}/{path}: {line[:200]}")
    # S4 / S5: what the committed embed capture actually observed
    ev = capture_evidence(spec["capture"], spec["capture_kind"])
    terminal_seen = {k: v for k, v in ev["census"].items() if k in names} if ev else {}
    total = ev["totals"].get("total") if ev else None
    s4 = None if ev is None else bool(terminal_seen)
    print(f"S4 delivered to the host on its stream: "
          f"{verdict_word(s4) if s4 is not None else 'not measured'}"
          + (f"  — read from {ev['path']}" if ev else "  — no committed capture to read"))
    if ev:
        acc = sum(ev["census"].values())
        arms = ev["arms"] or 1
        print(f"    census: {acc} of {total} host callbacks recorded by the embed arm(s) "
              f"({arms} arm(s) in that capture), kinds: "
              f"{', '.join(f'{k}={v}' for k, v in sorted(ev['census'].items()))}")
        print(f"    terminal kinds among them: {terminal_seen or 'NONE'}")
        for k, v in ev["settles"][:10]:
            print(f"    settle seen — {k}: {v}")
    gaps = ev["gaps"] if ev else []
    disclaim = hits["disclaimers"]
    # A guarantee about a signal that is never delivered is not a thing to measure:
    # S5 is only asked when S4 answered yes, and it stays `not measured` — never a
    # PASS — when no probe arm covered the exit paths.
    s5 = None if (ev is None or not s4) else (not gaps and not disclaim)
    print(f"S5 guaranteed on every path the run can end: "
          f"{verdict_word(s5) if s5 is not None else 'not measured'}"
          + ("" if s5 is not None else " — nothing to guarantee a delivery of yet"))
    for path, line in disclaim[:3]:
        print(f"    its own documentation says — https://raw.githubusercontent.com/{target['repo']}/{ref}/{path}: {line[:250]}")
    for k, v in gaps[:6]:
        print(f"    and the capture shows — {k}: {v}")
    if s5 and not gaps:
        print("    no disclaimer in its docs and no capture arm that ended a run without a "
              "terminal event — which is an absence of disproof, and is only as good as "
              "the arms that capture ran")
    subs = {
        "S1 named": s1,
        "S2 API-published": s2,
        "S3 prose-documented": s3,
        "S4 delivered-on-stream": s4,
        "S5 guaranteed": s5,
    }
    passed = sum(1 for v in subs.values() if v)
    print("    sub-tests: " + ", ".join(f"{k}={verdict_word(v) if v is not None else 'not measured'}"
                                      for k, v in subs.items()))
    ok = all(v for v in subs.values())
    print(f"verdict: settle_vocabulary = {'PASS' if ok else 'FAIL'} "
          f"({passed} of {len(subs)} sub-tests; the trigger asks for all five)")
    return "PASS" if ok else "FAIL"


# ────────────────────────────────── driver ────────────────────────────────────
def tree_sha():
    return sh("git -C " + REPO + " rev-parse --short HEAD")[1] or "unknown"


def main(argv):
    ids = [a for a in argv[1:] if not a.startswith("-")]
    if "--list" in argv:
        print("candidates (one per library that answers to the name `rho`):")
        for t in TARGETS:
            print(f"  {t:14} {TARGETS[t]['repo']:22} library crates: "
                  f"{', '.join(TARGETS[t]['library_crates'])}")
        return 0
    chosen = ids[0] if ids else (os.environ.get("LOOPRS_SPIKE_RHO_ID") or None)
    order = [chosen] if chosen in TARGETS else sorted(TARGETS)
    now = datetime.date.today()
    print(f"# dependency-maintenance-metrics — captured {datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}, tree {tree_sha()}")
    print(f"# ran: python3 spikes/dependency_maintenance_metrics.py{' ' + ' '.join(ids) if ids else ''}")
    print(f"definitions: {MEMO} — that memo is the normative text; this driver is the")
    print("             measurement it points at. Re-score by re-running this and re-reading the memo,")
    print("             not by re-typing a number into a page (ADR-0010 forbids 7).")
    print(f"subjects:    {', '.join(TARGETS[t]['repo'] + ' (' + ', '.join(TARGETS[t]['library_crates']) + ')' for t in order)}")
    print("evaluated against: ADR-0010 §4 trigger 3 — all four definitions must hold.")
    print("method:      nothing is built, linked or shipped here (ADR-0009 rule 2). Every number is")
    print("             read from GitHub, crates.io, the published .crate artifact, the candidate's")
    print("             published docs, or a capture already committed under spikes/results/.")
    print(f"             conventions: 12 months = {BAR_DAYS} d; liveness bar ≤{LIVENESS_DAYS} d since the last")
    print(f"             push; bus-factor denominator floor {MIN_HUMAN_COMMITS} human commits; bots excluded")
    print("             from the human denominator by GitHub's own type field.")
    quota = get("https://api.github.com/rate_limit") or {}
    core = (quota.get("resources") or {}).get("core") if isinstance(quota, dict) else None
    print(f"github api:    {'authenticated (GITHUB_TOKEN)' if os.environ.get('GITHUB_TOKEN') else 'anonymous'}"
          + (f" — core quota remaining {core['remaining']}/{core['limit']} at run start"
             if isinstance(core, dict) else "")
          + "; a rate-limited leg prints `not measured`, never a verdict")

    scratch = tempfile.mkdtemp(prefix="dmm-")
    scores = {}
    try:
        for t in order:
            target = TARGETS[t]
            print("\n" + "#" * 78)
            print(f"# {target['repo']} — the library a consumer would link is "
                  f"{', '.join(target['library_crates'])}")
            print("#" * 78)
            scores[t] = {
                "licence": leg_licence(t, target, now, scratch),
                "bus_factor": leg_bus_factor(t, target, now),
                "history": leg_history(t, target, now, scratch),
                "settle_vocabulary": leg_settle(t, target, now, scratch),
            }
    finally:
        if os.environ.get("LOOPRS_KEEP_TMP"):
            print(f"\n(scratch kept at {scratch})")
        else:
            shutil.rmtree(scratch, ignore_errors=True)

    print("\n== SUMMARY — ADR-0010 §4 trigger 3, one row per definition ==")
    order_sorted = sorted(scores)
    print("definition            " + "".join(f"{TARGETS[t]['repo']:28}" for t in order_sorted))
    for d in ["licence", "bus_factor", "history", "settle_vocabulary"]:
        print(f"{d:20}" + "".join(f"{scores[t][d] or 'not measured':28}" for t in order_sorted))
    for t in order_sorted:
        vals = [scores[t][d] for d in ["licence", "bus_factor", "history", "settle_vocabulary"]]
        met = all(v == "PASS" for v in vals)
        print(f"trigger 3 for {TARGETS[t]['repo']:26} {'MET' if met else 'NOT MET'} "
              f"({sum(1 for v in vals if v == 'PASS')} of 4 definitions; the trigger is a conjunction)")
    if all(v is None for t in order for v in scores[t].values()):
        print("\nnothing could be measured — no verdict is implied by this run")
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
