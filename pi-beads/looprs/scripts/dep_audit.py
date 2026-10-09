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

    ./scripts/dep_audit.py            # the table
    ./scripts/dep_audit.py --gate     # fail on an unjustified offender
    ./scripts/dep_audit.py --features # also print the resolved feature surface

Exit 0 with the gate when every dependency is accounted for.
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys

CRATE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(CRATE)

MARKER = "dep-audit:"


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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--gate", action="store_true", help="fail on an unjustified offender")
    ap.add_argument("--features", action="store_true", help="print the resolved feature surface too")
    args = ap.parse_args()

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

    if args.features:
        print()
        feature_surface()

    print()
    if not offenders:
        print("dep audit: every [dependencies] entry is named in src/ — nothing riding along")
        return 0

    bad = []
    for dep, why, reason in offenders:
        if reason:
            print(f"  answered   {dep['name']}: {why} — manifest says: {reason}")
        else:
            print(f"  UNJUSTIFIED {dep['name']}: {why}")
            bad.append(dep["name"])
        if dep["optional"]:
            print(f"             (optional entry; feature `{dep['name']}` gates it)")

    if args.gate and bad:
        print(f"\ndep audit: FAIL — {', '.join(bad)}")
        print("Fix it by removing the entry, or by answering it in the manifest with a")
        print(f"`{MARKER} <reason>` line above the entry, as `src/session/mod.rs` requires of a")
        print("dead-code allow. Do not restore a blanket.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
