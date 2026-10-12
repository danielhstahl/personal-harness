#!/usr/bin/env python3
"""dep_audit.py — does every declared dependency appear in the shipped code?

The counterpart to `dead_audit.py` for the other half of the tree: that one reads
`#[allow(dead_code)]` attributes in `src/`, this one reads `[dependencies]` in
`Cargo.toml`. Both exist because a blanket answer is a question nobody has to
answer, and `unused_dependencies = "allow"` sat in this manifest for exactly that
reason until looprs-00u.15.

Why a grep and not just the cargo lint: the lint is the first thing this audit ran
against, and it is not a complete witness here. Dropping the blanket and running
`cargo check` / `cargo check --all-targets` reports nothing — correct, as far as it
goes — but a *deliberately unused* `ryu = "1"` added to this same manifest also
reported nothing, while the identical mistake in a two-dependency reproduction was
caught. So this audit asks its own question of its own source set rather than
trusting one signal, and prints the reference counts so a reader can judge the
answer instead of taking it.

What it checks, per entry in `[dependencies]`:

  * **referenced in `src/`** — the shipped binary names it. Pass.
  * **not in `src/`, named in `examples/` or `tests/` only** — a spike's
    dependency riding in the shipped binary. FAILS unless the manifest carries a
    justification for it (see the marker below).
  * **named nowhere** — FAIL outright.

The justification marker, for the case where the grep is wrong — a dep used through
a macro, or one whose use is not a `path::` expression at all — is a line in the
comment block above the dependency that begins `dep-audit:`. That is the whole
escape hatch, and it is deliberately the same shape as a per-item
`#[allow(dead_code)]`: it names the thing and the reason in the place a reader is
already looking, which is the rule `src/session/mod.rs` states for dead code.

A third question, and the only one here with a *decision* behind it rather than a
convention: ADR-0010 decided that the engine stays a `pi --mode rpc` child and that
`rho` does not enter the tree — not as an optional dependency, not behind a feature
flag, because an unbuilt door rots. That is prose in an ADR, and prose in an ADR is
what the next person with a benchmark and a bad day re-litigates. So the ban is
checked here, in the gate that already reads this manifest, for the manifest *and*
the lockfile (a transitive pull is the same event as a direct one, and is the one
nobody edits by hand).

The ban is a set of *families*, because two unrelated projects answer to the name
`rho` and the first version of this check conflated them: it carried the crate set
of the library ADR-0010 §3 got *filed* against (`rho`, `rho-engine`,
`rho-harness-core` from casonadams/rho) while the decision was requested against
matthewyjiang/rho, which publishes `rho-sdk`, `rho-providers`, `rho-coding-agent`
(and `rho-agent-tools`, `rho-tui-pty`). A `rho-sdk = "5"` therefore passed a gate
whose only job was to fail it, and passed it *while printing the ADR clause* — the
worst shape a check can have, because it produces positive false comfort. See
`FAMILIES` for the set, the matching rule (project-shaped, name-exact, never
prefix-shaped) and the deliberate non-members — and for the escape route, which is
deliberately a decision amendment and not a flag.

    ./scripts/dep_audit.py            # the table
    ./scripts/dep_audit.py --gate     # fail on an unjustified offender or a banned crate
    ./scripts/dep_audit.py --features # also print the resolved feature surface
    ./scripts/dep_audit.py --self-test # every ban form, per family member, in scratch trees

Exit 0 with the gate when every dependency is accounted for and nothing banned is
present. The ban half is testable without touching this manifest — the audit resolves
its crate root from its own path, so a scratch dir holding a copy of this script and
a poisoned `Cargo.toml` fails for real:

    d=$(mktemp -d); mkdir -p $d/scripts; cp scripts/dep_audit.py $d/scripts/; \
      printf '[dependencies]\nrho-sdk = "5"\n' > $d/Cargo.toml; \
      python3 $d/scripts/dep_audit.py --gate; echo "exit $?"   # 1

`--self-test` is that recipe, run over every banned name in every dependency form
(direct, renamed, optional, feature-gated, dev/target/section tables, transitive via
`Cargo.lock`) plus the benign `rho`-prefixed names that must *not* fail — and
`scripts/check.sh` runs it, so the ban cannot regress into the state where it names
the wrong library again.

"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

CRATE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(CRATE)

MARKER = "dep-audit:"

# The crates ADR-0010 keeps out of the tree, grouped by the project that publishes
# them. Not a performance list and not a taste list: the ADR's blocker is the licence
# (no licence file in the published artifact, while the manifest declares a licence)
# and the bus factor, which do not expire when someone's benchmark moves. The route
# to changing this is ADR-0010 §4 — a named trigger beside a fresh capture, and an
# amendment to the ADR — not an escape hatch in a script. There is deliberately no
# allow-marker for these, the way there is none for a dependency that does not exist.
#
# Why families and not a flat list: the flat list was the bug. A name is not an
# identity — `rho` is a word three unrelated projects have chosen — so a deny-list
# of bare names silently means "the crate set of whichever repo the author had open".
# Each family here states its publisher and the clause that bans it, so a failure can
# say *which* decision it enforces instead of gesturing at an ADR.
FAMILIES = [
    {
        "id": "matthewyjiang/rho",
        # The library ADR-0010 was requested against, and the one its decision
        # actually decides about. Read off the repo's own workspace members
        # (`crates/rho-sdk`, `crates/rho-providers`, `crates/rho` — which publishes
        # as `rho-coding-agent` — `crates/rho-tools`, which publishes as
        # `rho-agent-tools`, and `crates/rho-tui-pty`) and each crate's
        # `repository` field on crates.io. Two notes:
        #   * `rho-agent-tools` is not in the ticket's list and is banned anyway:
        #     it is that repo's tool library and depends on `rho-sdk`, so omitting
        #     it would be the same miss, one crate smaller.
        #   * `rho-tui-pty` is `publish = false`, i.e. crates.io cannot even serve
        #     it. It stays, because a ban asks which project a crate belongs to, not
        #     whether the index has it — a vendored or path dep carries the name.
        "crates": [
            "rho-sdk",
            "rho-providers",
            "rho-coding-agent",
            "rho-agent-tools",
            "rho-tui-pty",
        ],
        "clause": "ADR-0010 decision 3",
        "note": "the library ADR-0010 was requested against",
    },
    {
        "id": "casonadams/rho",
        # The crate set §3 was FILED against and measured: 669 files, 120,799 LOC,
        # no licence file in any published artifact, 95.1% of commits by one person.
        # Still banned — the blocker is the licence and the bus factor, not the mixup.
        "crates": ["rho", "rho-engine", "rho-harness-core"],
        "clause": "ADR-0010 decision 3",
        "note": "the crate set ADR-0010 §3 was filed against and priced",
    },
]

# crate -> family. Built from FAMILIES so there is one list to keep correct.
BANNED: dict[str, dict] = {c: f for f in FAMILIES for c in f["crates"]}

# `rho`-prefixed crates that are **not** banned, and what they are. This is the
# explicit half of a decision, not an oversight: the ban is PROJECT-SHAPED — a named
# project's published crate names, matched exactly — and NOT NAME-SHAPED, so a `rho-*`
# crate from elsewhere is allowed. Prefix-matching `rho` would forbid third-party
# crates that have nothing to do with either decision, which is a gate that bites an
# innocent dependency and teaches people to distrust it. What the ban *does* do on a
# miss is print these names (`near-miss` below), so silence is never read as a verdict.
#
# The three named here are the ones a reader is most likely to mistake, including the
# two the ticket calls out by name; pyrex41/rho publishes a whole family of them
# (rho-agent, rho-auth, rho-core, rho-hashline, rho-lib, rho-provider, rho-server,
# rho-session, rho-tools) and none is in scope.
NEAR_MISS = {
    "rho-core": (
        "pyrex41/rho — unrelated project; ADR-0010 is about casonadams/rho "
        "and matthewyjiang/rho, and this is neither"
    ),
    "rho-tools": (
        "pyrex41/rho — unrelated project; and note that matthewyjiang/rho's "
        "crates/rho-tools directory publishes as rho-agent-tools, which IS banned"
    ),
    "rho-provider": (
        "pyrex41/rho — one letter from matthewyjiang/rho's rho-providers, which is "
        "banned; two publishers, two crates, one letter apart"
    ),
}


def reason(crate: str) -> str:
    """The failure reason for one banned crate: which family it matched, which clause.

    Deliberately not just the crate name. The old output said `rho forbidden
    [ADR-0010 §3]` for a manifest line that had nothing to do with §3, so a reader
    who saw it concluded the ban had been tested and held.
    """
    fam = BANNED[crate]
    return f"matched {fam['id']} family, {fam['clause']}"


def families_line() -> str:
    """The ban as one line: `rho, rho-engine (…) (casonadams/rho); rho-sdk … (matthewyjiang/rho)`."""
    return "; ".join(
        f"{', '.join(fam['crates'])} ({fam['id']})"
        for fam in sorted(FAMILIES, key=lambda fam: fam["id"])
    )


# Every pattern below is *delimited* — by `\s*=`, by `]`, by a closing quote, or by an
# explicit negative lookahead — because the ban is name-exact. `rho-sdk` must never
# match the pattern built for `rho`, and vice versa; alternation is longest-first for
# the same reason. This is where "project-shaped, not prefix-shaped" is enforced.
BANNED_ALT = "|".join(re.escape(n) for n in sorted(BANNED, key=len, reverse=True))
NEAR_MISS_ALT = "|".join(re.escape(n) for n in sorted(NEAR_MISS, key=len, reverse=True))
NOT_NAME_CHAR = r"(?![A-Za-z0-9_-])"

ENTRY_RE = re.compile(rf"^({BANNED_ALT}){NOT_NAME_CHAR}\s*=")
TABLE_RE = re.compile(rf"^\[[^\]]*dependenc(?:y|ies)\.({BANNED_ALT})\]")
RENAME_RE = re.compile(rf'package\s*=\s*"({BANNED_ALT})"')
FEATURE_REF_RE = re.compile(rf'"dep:({BANNED_ALT})"')
LOCK_NAME_RE = re.compile(r'^name = "([^"]+)"')

NEAR_ENTRY_RE = re.compile(rf"^({NEAR_MISS_ALT}){NOT_NAME_CHAR}\s*=")
NEAR_RENAME_RE = re.compile(rf'package\s*=\s*"({NEAR_MISS_ALT})"')


def manifest_deps(table: str) -> list[dict]:
    """Parse one `[table]` block of Cargo.toml by hand.

    Hand-rolled rather than `tomllib` because what's wanted here is the *comment
    lines above each entry*, which a parser throws away, and because a gate that
    needs a third-party toml library is a gate that doesn't run. Only the shapes
    this manifest actually uses are handled: `name = "req"`,
    `name = { ... }`, and `# comment` lines. Anything unparseable is skipped with
    a note rather than silently dropped.
    """
    with open("Cargo.toml", encoding="utf-8") as fh:
        lines = fh.read().splitlines()

    deps: list[dict] = []
    in_table = False
    pending: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("["):
            in_table = stripped == f"[{table}]"
            pending = []
            continue
        if not in_table:
            continue
        if not stripped:
            pending = []
            continue
        if stripped.startswith("#"):
            pending.append(stripped.lstrip("# ").strip())
            continue
        match = re.match(r"^([A-Za-z0-9_-]+)\s*=\s*(.*)$", stripped)
        if not match:
            pending = []
            continue
        name, rest = match.group(1), match.group(2)
        optional = bool(re.search(r"\boptional\s*=\s*true", rest))
        pkg = name
        rename = re.search(r'package\s*=\s*"([^"]+)"', rest)
        if rename:
            pkg = rename.group(1)
        deps.append(
            {
                "name": name,
                "package": pkg,
                "optional": optional,
                "comment": pending,
            }
        )
        pending = []
    return deps


def rs_files(*dirs: str) -> list[str]:
    out = []
    for d in dirs:
        if not os.path.isdir(d):
            continue
        for dp, _, fs in os.walk(d):
            if "target" in dp.split(os.sep):
                continue
            out += [os.path.join(dp, f) for f in fs if f.endswith(".rs")]
    return sorted(out)


def references(crate: str, files: list[str]) -> list[str]:
    """`file:line` for every place `files` names the crate by its Rust identifier."""
    ident = crate.replace("-", "_")
    pats = [
        re.compile(rf"(^|[^A-Za-z0-9_])use\s+(r#)?{ident}\b"),
        re.compile(rf"(^|[^A-Za-z0-9_]){ident}\s*::"),
        re.compile(rf"extern\s+crate\s+(r#)?{ident}\b"),
    ]
    hits = []
    for path in files:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for n, line in enumerate(fh, 1):
                if any(p.search(line) for p in pats):
                    hits.append(f"{path}:{n}")
    return hits


def justified(dep: dict) -> str | None:
    """The `dep-audit: <reason>` line above the manifest entry, if there is one."""
    for line in dep["comment"]:
        if MARKER in line:
            return line.split(MARKER, 1)[1].strip() or "(empty reason)"
    return None


def manifest_lines(path: str = "Cargo.toml") -> list[str]:
    with open(path, encoding="utf-8") as fh:
        return fh.read().splitlines()


def banned_in_manifest() -> list[dict]:
    """One hit per banned crate declared as a dependency, with the form that caught it.

    Every dependency-shaped table counts: `[dependencies]`, `[dev-dependencies]`,
    `[build-dependencies]`, `[workspace.dependencies]` and the `[target.*.dependencies]`
    forms, whether written as an entry (`rho-sdk = "5"`), an inline table with
    `package = "rho-sdk"`, or a `[dependencies.rho-sdk]` section. An *optional* entry
    is not different: ADR-0010 forbids the feature-flag door explicitly ("What this
    forbids" 2), and an optional dep is that door — which is why a `[features]` line
    carrying `"dep:rho-sdk"` is caught on its own too, so a half-typed attempt does
    not read as a clean tree.
    """
    hits: list[dict] = []
    for n, line in enumerate(manifest_lines(), 1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        for form, match in (
            ("direct", ENTRY_RE.match(stripped)),
            ("section", TABLE_RE.match(stripped)),
            ("renamed", RENAME_RE.search(stripped)),
            ("feature-gated", FEATURE_REF_RE.search(stripped)),
        ):
            if match:
                crate = match.group(1)
                hits.append(
                    {
                        "where": f"Cargo.toml:{n}",
                        "text": stripped,
                        "crate": crate,
                        "family": BANNED[crate]["id"],
                        "form": form,
                    }
                )
                break
    return hits


def banned_in_lock() -> list[dict]:
    """A banned crate in the *resolved* graph, one hit per lockfile entry.

    The manifest question is "did someone add it"; this one is "did it arrive", and
    the two are not the same event — a transitive pull lands in the lockfile with no
    manifest line to fail, which is exactly how a banned engine would actually get
    in.
    """
    if not os.path.exists("Cargo.lock"):
        return []
    hits: list[dict] = []
    for n, line in enumerate(manifest_lines("Cargo.lock"), 1):
        match = LOCK_NAME_RE.match(line.strip())
        if match and match.group(1) in BANNED:
            crate = match.group(1)
            hits.append(
                {
                    "where": f"Cargo.lock:{n}",
                    "text": crate,
                    "crate": crate,
                    "family": BANNED[crate]["id"],
                    "form": "transitive",
                }
            )
    return hits


def near_misses() -> list[dict]:
    """`rho`-prefixed crates from `NEAR_MISS` present in the tree, manifest or lock.

    The complement of a name-exact ban: a reader who adds `rho-core` and sees the
    gate stay green cannot tell "allowed" from "not looked at", so the audit prints
    which of the two it is. This prints; it never fails.
    """
    found: list[dict] = []
    seen: set[str] = set()
    for n, line in enumerate(manifest_lines(), 1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        match = NEAR_ENTRY_RE.match(stripped) or NEAR_RENAME_RE.search(stripped)
        if match and match.group(1) not in seen:
            seen.add(match.group(1))
            found.append({"crate": match.group(1), "where": f"Cargo.toml:{n}", "text": stripped})
    if os.path.exists("Cargo.lock"):
        for n, line in enumerate(manifest_lines("Cargo.lock"), 1):
            match = LOCK_NAME_RE.match(line.strip())
            if match and match.group(1) in NEAR_MISS and match.group(1) not in seen:
                seen.add(match.group(1))
                found.append({"crate": match.group(1), "where": f"Cargo.lock:{n}", "text": match.group(1)})
    return found


def feature_surface() -> None:
    print("resolved feature surface (shipped graph, `-e normal`):")
    for line in (
        subprocess.run(
            ["cargo", "tree", "-e", "normal", "-f", "{p} {f}", "--depth", "1"],
            capture_output=True,
            text=True,
        )
        .stdout.splitlines()
    ):
        if line.strip():
            print("  " + line.strip())


# ───────────────────────── the ban's own regression test ─────────────────────────
#
# Why a self-test lives inside the script it tests: the ban is the only thing in this
# file that enforces a *decision*, and the bug it is here to prevent is a ban that
# quietly names the wrong library while reporting "ADR-0010 holds". A check whose
# failure looks like a pass needs its own positive test — the docstring recipe was
# that test and it was never run, so the wrong crate set sat in the gate until a
# reader noticed. The scratch tree is the point: the audit resolves its crate root
# from its own path, so a copy of this script in a temp dir is a real audit of a real
# poisoned manifest, with nothing of this crate touched.

SELFTEST_PKG = '[package]\nname = "scratch"\nversion = "0.1.0"\nedition = "2021"\n\n'


def ban_forms(crate: str) -> list[tuple[str, dict[str, str]]]:
    """Every dependency shape that could smuggle `crate` in, as (form, files).

    Eight forms, and the list is the interesting part: `optional` and `feature-gated`
    are there because ADR-0010 "What this forbids" 2 forbids the feature door by
    name, `dev-dependencies` and the `[target.*]` form because a ban that only reads
    `[dependencies]` is a ban with holes in it, and `transitive` because the lockfile
    is where an unplanned pull actually lands.
    """
    ident = crate.replace("-", "_")
    use = f"pub use {ident};\n"
    return [
        (
            "direct",
            {
                "Cargo.toml": SELFTEST_PKG + f'[dependencies]\n{crate} = "1"\n',
                "src/lib.rs": use,
            },
        ),
        (
            "renamed",
            {
                "Cargo.toml": SELFTEST_PKG
                + f'[dependencies]\nengine = {{ package = "{crate}", version = "1" }}\n',
                "src/lib.rs": use,
            },
        ),
        (
            "optional",
            {
                "Cargo.toml": SELFTEST_PKG
                + f'[dependencies]\n{crate} = {{ version = "1", optional = true }}\n\n'
                + f'[features]\nengine = ["dep:{crate}"]\n',
                "src/lib.rs": use,
            },
        ),
        (
            "feature-gated",
            {
                "Cargo.toml": SELFTEST_PKG
                + f'[dependencies]\n\n[features]\nengine = ["dep:{crate}"]\n',
            },
        ),
        (
            "dev-dependency",
            {
                "Cargo.toml": SELFTEST_PKG + f'[dev-dependencies]\n{crate} = "1"\n',
                "src/lib.rs": use,
            },
        ),
        (
            "target-dependency",
            {
                "Cargo.toml": SELFTEST_PKG
                + f"[target.'cfg(unix)'.dependencies]\n{crate} = \"1\"\n",
                "src/lib.rs": use,
            },
        ),
        (
            "section-table",
            {
                "Cargo.toml": SELFTEST_PKG + f"[dependencies.{crate}]\nversion = \"1\"\n",
                "src/lib.rs": use,
            },
        ),
        (
            "transitive",
            {
                "Cargo.toml": SELFTEST_PKG + "[dependencies]\n",
                "Cargo.lock": SELFTEST_PKG
                + f'[[package]]\nname = "scratch"\nversion = "0.1.0"\n\n'
                + f'[[package]]\nname = "{crate}"\nversion = "1.0.0"\n',
            },
        ),
    ]


def benign_case() -> dict:
    """The false-positive half: `rho`-prefixed crates that must NOT fail the gate.

    `rho-core` and `rho-tools` are pyrex41/rho's, `rho-provider` is one letter from a
    banned name and belongs to that same third project, and `rho-notation` is nothing
    at all — it exists to prove a made-up `rho-*` is not caught either. This is the
    case that fails if anyone ever "hardens" the ban into a prefix match.
    """
    crates = ["rho-core", "rho-tools", "rho-provider", "rho-notation"]
    body = "".join(f"{c} = \"1\"\n" for c in crates)
    use = "".join(f"pub use {c.replace('-', '_')};\n" for c in crates)
    return {
        "name": "benign rho-prefixed crates (pyrex41/rho + an invented one)",
        "files": {"Cargo.toml": SELFTEST_PKG + f"[dependencies]\n{body}", "src/lib.rs": use},
        "expect": "pass",
        "contains": [
            "matching rule: project-shaped and name-exact",
            "near-miss  Cargo.toml:",
            "rho-core",
            "rho-tools",
            "rho-provider",
        ],
        "forbid": ["BANNED", "matched "],
    }


def self_test() -> int:
    """Run every banned name through every form, plus the benign controls.

    Each failing case is checked on the *reason string*, not just the exit code: a
    gate that fails for the wrong reason is the bug this whole function exists to
    catch, and `rho-sdk` failing with "matched casonadams/rho" would be that bug in
    a new costume.
    """
    script = os.path.join(CRATE, "scripts", "dep_audit.py")
    if not os.path.exists(script):  # pragma: no cover - the self-test is run from the tree
        print(f"self-test: cannot find itself at {script}")
        return 1

    cases: list[dict] = []
    for fam in sorted(FAMILIES, key=lambda f: f["id"]):
        for crate in fam["crates"]:
            for form, files in ban_forms(crate):
                cases.append(
                    {
                        "family": fam["id"],
                        "name": f"{crate} as a {form} dependency",
                        "files": files,
                        "expect": "fail",
                        "contains": [f"matched {fam['id']} family, {fam['clause']}", crate],
                    }
                )
    controls = [
        {
            "name": "clean scratch tree",
            "files": {"Cargo.toml": SELFTEST_PKG + "[dependencies]\n"},
            "expect": "pass",
            "contains": ["ADR-0010 decision 3 holds"],
            "forbid": ["BANNED"],
        },
        benign_case(),
    ]

    failures: list[str] = []
    groups: dict[str, list[int]] = {}
    for case in cases + controls:
        rc, out = run_scratch(script, case["files"])
        group = case.get("family", "controls")
        stats = groups.setdefault(group, [0, 0])
        stats[0] += 1
        problems: list[str] = []
        if (rc != 0) != (case["expect"] == "fail"):
            problems.append(f"expected {case['expect']}, gate exited {rc}")
        for needle in case.get("contains", ()):
            if needle not in out:
                problems.append(f"output does not carry {needle!r}")
        for needle in case.get("forbid", ()):
            if needle in out:
                problems.append(f"output must not carry {needle!r}")
        if problems:
            stats[1] += 1
            tail = "\n             ".join(out.splitlines()[-6:]) or "(no output)"
            failures.append(f"  FAIL {case['name']}: {'; '.join(problems)}\n             {tail}")

    forms = len(ban_forms("x"))
    total = sum(stats[0] for stats in groups.values())
    print(
        f"dep_audit self-test: {total} scratch trees "
        f"({len(BANNED)} banned crates x {forms} dependency forms + {len(controls)} controls)"
    )
    for fam in sorted(FAMILIES, key=lambda f: f["id"]):
        seen, wrong = groups.get(fam["id"], [0, 0])
        print(
            f"  {fam['id']:<20} {len(fam['crates'])} crates x {forms} forms"
            f"   {seen - wrong}/{seen} fail with the right family and clause"
        )
    seen, wrong = groups.get("controls", [0, 0])
    print(
        f"  {'controls':<20} {len(controls)} cases          "
        f"{seen - wrong}/{seen} behave as stated"
    )
    for line in failures:
        print(line)
    if failures:
        print(f"self-test: FAIL — {len(failures)} of {total} cases wrong")
        return 1
    print("self-test: clean — every banned name fails, naming the family and clause it matched")
    return 0


def run_scratch(script: str, files: dict[str, str]) -> tuple[int, str]:
    """Copy this script into a scratch crate, write `files`, and run its --gate."""
    with tempfile.TemporaryDirectory(prefix="dep-audit-selftest-") as d:
        os.makedirs(os.path.join(d, "scripts"))
        shutil.copy(script, os.path.join(d, "scripts", "dep_audit.py"))
        for rel, content in files.items():
            path = os.path.join(d, rel)
            os.makedirs(os.path.dirname(path) or d, exist_ok=True)
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(content)
        proc = subprocess.run(
            [sys.executable, os.path.join(d, "scripts", "dep_audit.py"), "--gate"],
            capture_output=True,
            text=True,
        )
        return proc.returncode, proc.stdout + proc.stderr


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--gate", action="store_true", help="fail on an unjustified offender")
    ap.add_argument("--features", action="store_true", help="print the resolved feature surface too")
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run the ban over scratch trees (every family member, every dependency form) and exit",
    )
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    deps = manifest_deps("dependencies")
    src = rs_files("src")
    dev = rs_files("tests", "examples")

    offenders: list[tuple[dict, str, str | None]] = []
    print(f"{'dependency':<18} {'src/':>6} {'tests+examples':>15}  where")
    for dep in deps:
        crate = dep["package"]
        in_src = references(crate, src)
        in_dev = references(crate, dev)
        note = ""
        if not in_src and not in_dev:
            status = "named NOWHERE"
            offenders.append((dep, status, justified(dep)))
        elif not in_src:
            status = "spike/test-only"
            offenders.append((dep, status, justified(dep)))
        else:
            status = "shipped"
        sample = (in_src or in_dev or ["-"])[0]
        print(f"{crate:<18} {len(in_src):>6} {len(in_dev):>15}  {status} ({sample})")

    banned = banned_in_manifest() + banned_in_lock()

    if args.features:
        print()
        feature_surface()

    print()
    bad: list[str] = []
    if not offenders:
        print("dep audit: every [dependencies] entry is named in src/ — nothing riding along")
    else:
        for dep, why, just in offenders:
            if just:
                print(f"  answered   {dep['name']}: {why} — manifest says: {just}")
            else:
                print(f"  UNJUSTIFIED {dep['name']}: {why}")
                bad.append(dep["name"])
            if dep["optional"]:
                print(f"             (optional entry; feature `{dep['name']}` gates it)")

    print()
    if banned:
        print(
            f"  BANNED     {len(banned)} reference(s) to a crate this tree may not depend on "
            f"— the ban is family-shaped, so each line names the family it matched:"
        )
        for hit in banned:
            print(f"             {hit['where']}: {hit['text']}   [{reason(hit['crate'])}]")
    else:
        print(
            "banned-dependency check: no member of "
            f"{len(FAMILIES)} ADR-0010 families ({families_line()}) "
            "in Cargo.toml or Cargo.lock — ADR-0010 decision 3 holds"
        )
    print(
        "             matching rule: project-shaped and name-exact, never prefix-shaped — a "
        "`rho-*` crate outside those families is allowed"
    )
    for miss in near_misses():
        print(
            f"  near-miss  {miss['where']}: {miss['text']} — not banned: {NEAR_MISS[miss['crate']]}"
        )

    if args.gate and banned:
        print("\ndep audit: FAIL — a crate ADR-0010 keeps out of the tree is present")
        for hit in banned:
            fam = BANNED[hit["crate"]]
            print(
                f"  {hit['where']}: {hit['crate']} — {reason(hit['crate'])} "
                f"({fam['note']}; {hit['form']} dependency)"
            )
        print("The decision is `docs/adr/0010-pi-rpc-child-vs-embedded-rho-engine.md`; the blocker")
        print("there is the licence and the bus factor, not the boundary cost, so it does not")
        print("expire when a benchmark moves. Re-opening it means a §4 trigger named beside a")
        print("fresh capture and an amendment to that ADR — the phase-2 epic that priced this")
        print("work (workspace-bt8) was closed unworked, so no half-built engine seam exists to")
        print("wire a backend into. \"What this forbids\" 2 rules out the half-measures: do not add")
        print("it optionally, do not add it behind a feature flag, and do not rely on it staying")
        print("out of Cargo.lock by luck.")
        return 1

    if args.gate and bad:
        print(f"\ndep audit: FAIL — {', '.join(bad)}")
        print("Fix it by removing the entry, or by answering it in the manifest with a")
        print(f"`{MARKER} <reason>` line above the entry, as `src/session/mod.rs` requires of a")
        print("dead-code allow. Do not restore a blanket.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
