#!/usr/bin/env python3
"""sweep_check.py — audit an improvement sweep (looprs-00u.12).

The protocol lives in `docs/guide/improvement-sweep.md`. This is the part of it
that can be answered with prints instead of hope. Three checks:

  1. **Finding shape** — every bead under the epic carrying an improvement label
     has the four required elements (`What is wrong`, `What is better`,
     `How we would know`, and a `Found while documenting` provenance line), cites
     at least one repo file, and every cited `path:NN` still resolves inside that
     file's length.

  2. **Read coverage** — the sweep ticket's notes carry a read log with one line
     per page the epic wrote, each ending in `filed: <ids>`, `read, nothing found`
     or `rejected: <reason>`. A page with no line is indistinguishable from a page
     nobody read, which is the failure this check exists for.

  3. **Branch hygiene** — no commit in `--since..HEAD` that touched the epic's
     pages also touched `src/`, `Cargo.toml` or `Cargo.lock`. A docs commit that
     carries production changes is a docs commit that cannot be reviewed.

Run it before closing a sweep ticket. It is *not* a step of ./scripts/check.sh:
it queries beads, and the beads database is not in CI.

    ./scripts/sweep_check.py                        # the docs epic, this repo's shape
    ./scripts/sweep_check.py --epic looprs-00u --sweep-ticket looprs-00u.12
    ./scripts/sweep_check.py --since <rev>          # rev = parent of the epic's first docs commit
    ./scripts/sweep_check.py --no-git               # skip check 3 (no git history)

Exit 0 = clean. Exit 1 = at least one failure, each printed with its bead id/page.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The labels that mean "this bead is a sweep finding". `improvement` is the set to
# grep; `found-in-sweep` is the provenance marker the sweep also writes.
FINDING_LABELS = {"improvement", "found-in-sweep"}

REQUIRED_SECTIONS = [
    ("provenance", r"found while documenting"),
    ("what is wrong", r"what is wrong"),
    ("what is better", r"what is better"),
    ("how we would know", r"how (we )?would know|how do we know"),
]

# A repo-internal citation: src/foo.rs, docs/guide/bar.md, Cargo.toml, ...
CITE_RE = re.compile(
    r"(?<![\w./-])((?:src|tests|docs|examples|spikes|scripts)/[A-Za-z0-9_./-]+\.(?:rs|py|sh|md|toml))"
    r"(?::(\d+)(?:\s*-\s*(\d+))?)?"
)

# The read-log line on the sweep ticket:  page | files read | outcome
READ_RE = re.compile(r"^([A-Za-z0-9_./-]+\.md)\s*\|\s*(.*?)\s*\|\s*(.+?)\s*$")
OUTCOME_RE = re.compile(
    r"^(filed:\s*[\w.,\s-]*looprs-[\w.]+|read,?\s*nothing found|rejected:\s*.+)$",
    re.I,
)
PAGES_DECL_RE = re.compile(r"^SWEEP EPIC PAGES\s*:\s*(.+)$", re.I)

PRODUCTION_PATHS = ("src/", "Cargo.toml", "Cargo.lock")


def bd_json(*args: str):
    proc = subprocess.run(
        ["bd", *args, "--json"], cwd=ROOT, capture_output=True, text=True
    )
    if proc.returncode != 0:
        raise SystemExit(f"bd {' '.join(args)} failed: {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def notes_of(issue_id: str) -> str:
    data = bd_json("show", issue_id)
    issue = data[0] if isinstance(data, list) else data
    return issue.get("notes") or ""


def descendants(epic: str) -> list[dict]:
    """Every bead with `parent == epic`, plus their descendants (depth-limited)."""
    out: list[dict] = []
    queue = [epic]
    seen: set[str] = set()
    while queue:
        parent = queue.pop(0)
        for child in bd_json("children", parent):
            if child["id"] in seen:
                continue
            seen.add(child["id"])
            out.append(child)
            queue.append(child["id"])
    return out


# ───────────────────────────── check 1: shape ─────────────────────────────


def check_findings(epic: str, sweep_ticket: str) -> list[str]:
    failures: list[str] = []
    findings = [
        b
        for b in descendants(epic)
        if FINDING_LABELS & {l.lower() for l in (b.get("labels") or [])}
        and b["id"] != sweep_ticket  # the protocol bead is not itself a finding
    ]
    if not findings:
        return [f"{epic}: no bead carries an improvement label — nothing was filed"]

    for bead in findings:
        text = f"{bead.get('description') or ''}\n{notes_of(bead['id'])}"
        low = text.lower()
        missing = [name for name, pat in REQUIRED_SECTIONS if not re.search(pat, low)]
        # The "how we would know" section legitimately names artifacts that do not
        # exist yet ("add spikes/zzz_new_spike.py, run the gate, it fails"), so
        # file existence is required only for the part of the text that describes
        # what is there now. Line-range liveness is still checked wherever the file
        # does exist.
        cut = re.search(r"how (we )?would know", low)
        now_part = text[: cut.start()] if cut else text
        later_part = text[cut.start() :] if cut else ""
        cites_now = CITE_RE.findall(now_part)
        cites_later = CITE_RE.findall(later_part)
        if not (cites_now or cites_later):
            missing.append("a repo file citation (src/…, docs/…, spikes/…)")
        for path, lo, hi in cites_now:
            p = ROOT / path
            if not p.exists():
                missing.append(f"cited file does not exist: {path}")
                continue
            if lo:
                lines = len(p.read_text(errors="replace").splitlines())
                if int(hi or lo) > lines:
                    missing.append(
                        f"cited line past EOF: {path}:{lo} (file has {lines} lines)"
                    )
        for path, lo, hi in cites_later:
            p = ROOT / path
            if p.exists() and lo:
                lines = len(p.read_text(errors="replace").splitlines())
                if int(hi or lo) > lines:
                    missing.append(
                        f"cited line past EOF (in 'how we would know'): {path}:{lo} "
                        f"(file has {lines} lines)"
                    )
        if missing:
            failures.append(
                f"{bead['id']} — missing: {', '.join(sorted(set(missing)))}"
            )
    print(
        f"  {len(findings)} finding bead(s) under {epic}; "
        f"{len([f for f in failures if 'missing' in f])} incomplete"
    )
    return failures


# ─────────────────────────── check 2: read coverage ───────────────────────────


def check_read_log(sweep_ticket: str) -> tuple[list[str], list[str]]:
    notes = notes_of(sweep_ticket)
    pages: list[str] = []
    for line in notes.splitlines():
        m = PAGES_DECL_RE.match(line.strip())
        if m:
            pages = [p.strip() for p in re.split(r"[,\s]+", m.group(1)) if p.strip()]
            break
    if not pages:
        return [
            f"{sweep_ticket}: no 'SWEEP EPIC PAGES:' line — the audit does not know "
            "which pages the epic wrote"
        ], []

    logged: dict[str, str] = {}
    bad_lines: list[str] = []
    for line in notes.splitlines():
        m = READ_RE.match(line.strip())
        if not m:
            continue
        page, _files, outcome = m.groups()
        if not OUTCOME_RE.match(outcome.strip()):
            bad_lines.append(f"{sweep_ticket}: unreadable outcome for {page}: {outcome!r}")
        logged[page] = outcome.strip()

    failures = list(bad_lines)
    for page in pages:
        if page not in logged:
            failures.append(f"{sweep_ticket}: no read logged for {page}")
    for page in logged:
        if page not in pages:
            failures.append(
                f"{sweep_ticket}: read log names {page}, which is not in "
                "SWEEP EPIC PAGES (fix the declaration or the line)"
            )
    print(
        f"  {len(pages)} page(s) declared; {len(logged)} read line(s) found; "
        f"{len(failures)} problem(s)"
    )
    return failures, pages


# ────────────────────────── check 3: branch hygiene ──────────────────────────


def check_branch(pages: list[str], since: str) -> list[str]:
    if not pages:
        return ["no pages declared, so nothing to filter the commit scan by"]
    rev = since or "HEAD~1"
    proc = subprocess.run(
        ["git", "log", "--format=%H", f"{rev}..HEAD", "--", *pages],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        return [f"git log {rev}..HEAD failed: {proc.stderr.strip()}"]

    failures: list[str] = []
    for sha in proc.stdout.split():
        names = subprocess.run(
            ["git", "show", "--name-only", "--format=", sha],
            cwd=ROOT,
            capture_output=True,
            text=True,
        ).stdout.split()
        # Normalise to this repo's subtree so the check reads the same from the
        # repo root or from a project directory.
        prefix = ""
        rel = Path(".").resolve()
        top = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], cwd=ROOT, capture_output=True, text=True
        ).stdout.strip()
        if rel != Path(top):
            prefix = str(rel.relative_to(top)) + "/"
        dirty = [
            n
            for n in names
            if n[len(prefix):].startswith(PRODUCTION_PATHS)
        ]
        if dirty:
            subject = subprocess.run(
                ["git", "show", "-s", "--format=%s", sha],
                cwd=ROOT,
                capture_output=True,
                text=True,
            ).stdout.strip()
            failures.append(
                f"{sha[:8]} \"{subject}\" touched production code alongside docs: "
                + ", ".join(sorted(d[len(prefix):] for d in dirty)[:4])
            )
    print(f"  {len(proc.stdout.split())} commit(s) touching the epic's pages inspected")
    return failures


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--epic", default="looprs-00u")
    ap.add_argument("--sweep-ticket", default="looprs-00u.12")
    ap.add_argument("--no-git", action="store_true", help="skip the branch-hygiene check")
    ap.add_argument(
        "--since",
        default="HEAD~1",
        help="the rev the epic's docs work started at (pass the parent of its first "
        "docs commit). The default checks only the last commit, which is the shape "
        "you want when closing the sweep ticket from the sweep commit itself.",
    )
    args = ap.parse_args()

    failed = False
    print(f"sweep audit — epic {args.epic}, sweep ticket {args.sweep_ticket}")

    print("[1] finding shape (four required elements + live citations)")
    f = check_findings(args.epic, args.sweep_ticket)
    failed |= bool(f)
    for line in f:
        print(f"    FAIL {line}")

    print("[2] read coverage (every page the epic wrote has a logged read)")
    f2, pages = check_read_log(args.sweep_ticket)
    failed |= bool(f2)
    for line in f2:
        print(f"    FAIL {line}")

    print("[3] branch hygiene (no production code in the epic's docs commits)")
    if args.no_git:
        print("    skipped (--no-git)")
    else:
        f3 = check_branch(pages, args.since)
        failed |= bool(f3)
        for line in f3:
            print(f"    FAIL {line}")

    print()
    print("sweep audit: " + ("FAILED" if failed else "clean"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
