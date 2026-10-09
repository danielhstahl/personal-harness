#!/usr/bin/env python3
"""The documentation rot gate (looprs-00u.10 / ADR-0008).

Four checks that keep the site true after the day it was written:

1. **Links and orphans** — every relative link in every Markdown page resolves,
   every `#anchor` exists in the page it names, and every `.md` under `docs/` is
   reachable from `docs/SUMMARY.md`. An unindexed page is invisible, which is the
   same failure as a broken link, arriving later.
2. **Knob coverage** — every `LOOPRS_*` the code reads is in the configuration
   reference, every knob the reference documents exists in the repo, and no two
   pages state a *different* default for the same knob. Defaults stated twice and
   drifting apart is the failure this check exists for; the "one home per fact"
   rule is only worth anything if the second home is checked against the first.
3. **Keymap coverage** — the per-mode tables in `docs/guide/keymap.md` are a
   rendering of `CHORD_TABLE` in `src/session/view.rs`, compared row by row.
   `--fix-keymap` re-renders them.
4. **Measurement claims** (looprs-00u.23) — every spike check count restated in
   prose (`**149/149**`, `149 checks`) is backed by a committed capture under
   `spikes/results/`: either the sentence names a capture whose own
   `N/M checks passed` line agrees, or the number matches the newest committed
   capture of the spike it names. A count whose run was never captured needs an
   explicit `<!-- spike-count: unverified N/M spike — why -->` marker, which is
   itself rot-checked (a marker that no longer backs anything fails).

Run from anywhere; runs in well under a second; no network, no cargo, no build.

    ./scripts/docs_check.py                 # the gate
    ./scripts/docs_check.py --fix-keymap    # rewrite the generated tables
    ./scripts/docs_check.py --list-knobs    # what the code reads, with file:line
    ./scripts/docs_check.py --list-captures # every committed capture and its total
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
SRC = ROOT / "src"
SPIKES = ROOT / "spikes"
RESULTS = SPIKES / "results"

# Pages outside docs/ that are part of the corpus the site carries, and whose links
# are therefore gated. Kept short on purpose: spikes/results/*.log files are
# evidence, not prose, and gating them would be gating captures.
EXTRA_PAGES = [ROOT / "spikes" / "README.md", ROOT / "README.md"]

KNOB_RE = re.compile(r"LOOPRS_[A-Z0-9_]+")
LINK_RE = re.compile(r"\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")


# ─────────────────────────────── shared ───────────────────────────────


def slugify(text: str) -> str:
    """GitHub's heading-slug rule, reimplemented to match the site generator.

    Lowercase; whitespace to `-`; drop everything that is not alphanumeric or `-`;
    do **not** collapse repeated hyphens. The last part is why
    `## 1. The status → column mapping` is `1-the-status--column-mapping`: the
    arrow contributes nothing and the two spaces around it leave two hyphens.
    `adr/0007` is linked by that anchor from three files, so a slug that does not
    match the generator's is a broken link this checker cannot see.
    """
    text = re.sub(r"<[^>]+>", "", text)
    text = (
        text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", '"')
    )
    out = []
    for ch in text:
        if ch.isspace():
            out.append("-")
        elif ch in "-_" or ch.isalnum():
            out.append(ch.lower())
    return "".join(out) or "section"


def md_headings_with_anchors(text: str):
    """(line_no, slug) for every heading, plus every explicit <a id=...> / {#slug}."""
    anchors = []
    in_fence = False
    for i, line in enumerate(text.splitlines(), start=1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        m = re.match(r"^(#{1,6})\s+(.*?)\s*#*\s*$", line)
        if m:
            anchors.append((i, slugify(m.group(2))))
        for explicit in re.findall(r"\{#([A-Za-z0-9_\-]+)\}", line):
            anchors.append((i, explicit))
        for explicit in re.findall(r'<a\s+id="([^"]+)"', line):
            anchors.append((i, explicit))
    return anchors


def md_tables(text: str):
    """Yield (header_cells, row_line_no, row_cells) for each Markdown table row.

    A deliberately small parser: pipe tables with a `---` separator row. That is
    every table this repo writes, and a small parser that is obviously correct beats
    a general one that has to be trusted.
    """
    lines = text.splitlines()
    i = 0
    header: list[str] = []
    while i < len(lines):
        line = lines[i]
        if line.strip().startswith("|") and i + 1 < len(lines) and re.match(
            r"^\s*\|[\s:|-]+\|\s*$", lines[i + 1]
        ):
            header = split_row(line)
            i += 2
            while i < len(lines) and lines[i].strip().startswith("|"):
                yield header, i + 1, split_row(lines[i])
                i += 1
        else:
            i += 1


def split_row(line: str) -> list[str]:
    s = line.strip()
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|"):
        s = s[:-1]
    return [c.strip() for c in s.split("|")]


def strip_md(s: str) -> str:
    """Remove the Markdown decoration so two phrasings of one default compare equal."""
    s = s.replace("**", "").replace("__", "").replace("`", "").replace("*", "")
    return re.sub(r"\s+", " ", s).strip().rstrip(".").lower()


# ──────────────────────────── check 1: links ────────────────────────────


def corpus_pages():
    pages = sorted(DOCS.rglob("*.md"))
    pages += [p for p in EXTRA_PAGES if p.exists()]
    return pages


def check_links(pages) -> list[str]:
    bad: list[str] = []
    cache: dict[Path, list[tuple[int, str]]] = {}
    for page in pages:
        text = page.read_text(encoding="utf-8")
        in_fence = False
        for lineno, line in enumerate(text.splitlines(), start=1):
            if line.lstrip().startswith("```"):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            for _, target in LINK_RE.findall(line):
                if target.startswith(("http://", "https://", "mailto:", "tel:")):
                    continue
                file_part, _, anchor = target.partition("#")
                if file_part == "":
                    # same-page anchor
                    if page not in cache:
                        cache[page] = md_headings_with_anchors(text)
                    slugs = {s for _, s in cache[page]}
                    if anchor and anchor not in slugs:
                        bad.append(
                            f"{rel(page)}:{lineno}: anchor #{anchor} does not exist in this page"
                        )
                    continue
                resolved = (page.parent / file_part).resolve()
                if not resolved.exists():
                    bad.append(f"{rel(page)}:{lineno}: broken link → {file_part} ({rel(resolved)})")
                    continue
                if anchor and resolved.is_file() and resolved.suffix == ".md":
                    if resolved not in cache:
                        cache[resolved] = md_headings_with_anchors(
                            resolved.read_text(encoding="utf-8")
                        )
                    slugs = {s for _, s in cache[resolved]}
                    if anchor not in slugs:
                        bad.append(
                            f"{rel(page)}:{lineno}: anchor #{anchor} not found in {rel(resolved)}"
                        )
    return bad


def check_orphans(pages) -> list[str]:
    """Every .md under docs/ must be reachable from SUMMARY.md by local links."""
    summary = DOCS / "SUMMARY.md"
    if not summary.exists():
        return [f"{rel(DOCS)}: SUMMARY.md is missing — the site has no tree"]
    reachable: set[Path] = set()
    queue = [summary]
    while queue:
        page = queue.pop()
        if page in reachable or not page.is_file():
            continue
        reachable.add(page)
        text = page.read_text(encoding="utf-8")
        in_fence = False
        for line in text.splitlines():
            if line.lstrip().startswith("```"):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            for _, target in LINK_RE.findall(line):
                if target.startswith(("http://", "https://", "mailto:")):
                    continue
                file_part = target.partition("#")[0]
                if not file_part:
                    continue
                resolved = (page.parent / file_part).resolve()
                if resolved.is_file() and resolved.suffix == ".md":
                    queue.append(resolved)
    orphans = [p for p in pages if p.is_file() and p not in reachable]
    return [
        f"{rel(p)}: not reachable from SUMMARY.md — an unindexed page is invisible"
        for p in sorted(orphans)
    ]


# ───────────────────────── check 2: knob coverage ─────────────────────────


def code_knobs():
    """Every LOOPRS_* that appears anywhere in src/, with the reads called out."""
    found: dict[str, list[str]] = {}
    for path in sorted(SRC.rglob("*.rs")):
        text = path.read_text(encoding="utf-8")
        for lineno, line in enumerate(text.splitlines(), start=1):
            for knob in KNOB_RE.findall(line):
                found.setdefault(knob, []).append(f"{rel(path)}:{lineno}")
    return found


def repo_knobs():
    """Knobs anywhere in the repo — used to decide 'documented but nobody reads it'."""
    out: set[str] = set()
    for base in (SRC, ROOT / "examples", ROOT / "spikes", ROOT / "scripts", ROOT / "tests"):
        if not base.exists():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix not in {".rs", ".py", ".sh", ".md", ".toml"}:
                continue
            try:
                out.update(KNOB_RE.findall(path.read_text(encoding="utf-8", errors="ignore")))
            except OSError:
                continue
    return out


def check_knobs() -> list[str]:
    bad: list[str] = []
    config_page = DOCS / "guide" / "configuration.md"
    if not config_page.exists():
        return [f"{rel(config_page)}: missing — the configuration reference is required"]
    text = config_page.read_text(encoding="utf-8")
    documented = {m for m in re.findall(r"`?(LOOPRS_[A-Z0-9_]+)`?", text)}
    code = code_knobs()
    everywhere = repo_knobs()

    for knob, where in sorted(code.items()):
        if knob not in documented:
            bad.append(
                f"{rel(config_page)}: {knob} is read in {', '.join(where[:4])} "
                f"but is not in the configuration reference"
            )

    for knob in sorted(documented):
        if knob not in everywhere:
            bad.append(
                f"{rel(config_page)}: {knob} is documented but nothing in the repo reads it "
                f"— a documented knob nobody reads is a lie about a feature"
            )

    bad += check_default_agreement()
    return bad


def check_default_agreement() -> list[str]:
    """No two pages may state a different default for the same knob.

    The rule this enforces is the repo's own: "one list of the LOOPRS_* knobs per
    feature, in the page that owns that feature". Several pages legitimately carry
    a knob table (the feature page and the reference index). That is only safe if
    the *default* in each is the same string once the Markdown decoration is
    stripped, which is exactly the kind of thing that decays silently otherwise.
    """
    stated: dict[str, list[tuple[str, str]]] = {}
    pages = [DOCS / "guide" / "configuration.md", DOCS / "kanban.md"]
    pages += sorted((DOCS / "adr").glob("*.md"))
    for page in pages:
        if not page.is_file():
            continue
        for header, lineno, cells in md_tables(page.read_text(encoding="utf-8")):
            joined = " ".join(header).lower()
            if "default" not in joined:
                continue
            # The knob is the one in the FIRST cell. Taking the first cell that
            # happens to mention a knob mis-assigns defaults: the `RUST_LOG` row
            # mentions `LOOPRS_PANIC` in its description, and the checker filed a
            # "two defaults for LOOPRS_PANIC" against a page that stated one.
            if not cells:
                continue
            m = KNOB_RE.search(cells[0])
            if not m:
                continue
            knob = m.group(0)
            try:
                idx = [i for i, h in enumerate(header) if "default" in h.lower()][0]
            except IndexError:
                continue
            if idx >= len(cells):
                continue
            default = strip_md(cells[idx])
            if not default or default.startswith(("-", "—")):
                continue
            stated.setdefault(knob, []).append((f"{rel(page)}:{lineno}", default))

    bad = []
    for knob, places in sorted(stated.items()):
        distinct = {d for _, d in places}
        if len(distinct) > 1:
            detail = " vs ".join(f"{p} says `{d}`" for p, d in places)
            bad.append(f"configuration: {knob} has two defaults — {detail}")
    return bad


# ────────────────────────── check 3: keymap ──────────────────────────


def parse_chord_table() -> list[dict[str, str]]:
    """The `CHORD_TABLE` rows out of `src/session/view.rs`.

    The file is pre-processed with Rust's own line-continuation rule (a backslash at
    end of line inside a string literal swallows the newline and the leading
    whitespace of the next line) so a multi-line `note:` parses as one string.
    """
    path = SRC / "session" / "view.rs"
    raw = path.read_text(encoding="utf-8")
    raw = re.sub(r"\\\n[ \t]*", "", raw)
    start = raw.index("pub const CHORD_TABLE")
    end = raw.index("\n];", start)
    body = raw[start:end]
    rows = []
    for block in re.findall(r"ChordRow\s*\{(.*?)\}\s*,", body, re.S):
        def field(name: str) -> str:
            m = re.search(rf"{name}\s*:\s*(.*?),\n", block, re.S)
            return m.group(1).strip() if m else ""

        def ident(expr: str) -> str:
            return expr.split("::")[-1].strip()

        keys_m = re.search(r'"([^"]*)"', field("keys"))
        note_m = re.search(r'"((?:[^"\\]|\\.)*)"', field("note"), re.S)
        rows.append(
            {
                "mode": ident(field("mode")),
                "key": ident(field("key")),
                "keys": keys_m.group(1) if keys_m else "",
                "state": ident(field("state")),
                "owner": ident(field("owner")),
                "does": ident(field("does")),
                "note": note_m.group(1) if note_m else "",
            }
        )
    return rows


MARKDOWN_ESCAPES = {r"\n": "\n", r"\"": '"', r"\\": "\\"}


def unescape_rust(s: str) -> str:
    for k, v in MARKDOWN_ESCAPES.items():
        s = s.replace(k, v)
    return s


def render_keymap_table(mode_filter: str, rows) -> str:
    """The Markdown table for one mode, in CHORD_TABLE order.

    `mode_filter` is the `TerminalType` variant name; the section markers in the
    page use the lowercase spelling (`keymap:beads`) because that is how the page
    reads.
    """
    sel = [r for r in rows if r["mode"] == mode_filter]
    out = [
        "| key | state | who owns it | effect | why |",
        "| --- | --- | --- | --- | --- |",
    ]
    for r in sel:
        note = unescape_rust(r["note"]) or "—"
        out.append(
            "| `{keys}` | {state} | {owner} | `{does}` | {note} |".format(
                keys=r["keys"],
                state=r["state"],
                owner=r["owner"].lower(),
                does=r["does"],
                note=note,
            )
        )
    return "\n".join(out)


# slug → the `TerminalType` variant name in CHORD_TABLE. Note the first one: the
# variant is `Beeds` (one `d`) — that is what `TerminalType::label()` puts on the
# status row — while the prose in these docs spells the mode "Beads", which is how
# the existing docs spelled it first (47 vs 7 in the pre-site corpus). The tables
# are generated from the variant, so the mapping has to know the difference; the
# keymap page says so out loud because a reader who does not know it will spend
# ten minutes grepping for a mode that is on screen the whole time.
KEYMAP_MARKERS = {
    "beads": "Beeds",
    "pi": "Pi",
    "bash": "Bash",
}


def check_keymap(fix: bool) -> list[str]:
    page = DOCS / "guide" / "keymap.md"
    if not page.exists():
        return [f"{rel(page)}: missing"]
    text = page.read_text(encoding="utf-8")
    rows = parse_chord_table()
    bad = []
    changed = text
    for slug, mode in KEYMAP_MARKERS.items():
        begin = f"<!-- BEGIN GENERATED:keymap:{slug} -->"
        end = f"<!-- END GENERATED:keymap:{slug} -->"
        if begin not in changed or end not in changed:
            bad.append(f"{rel(page)}: missing generated markers for {slug}")
            continue
        pre, rest = changed.split(begin, 1)
        old, post = rest.split(end, 1)
        expected = "\n" + render_keymap_table(mode, rows) + "\n"
        if old.strip() != expected.strip():
            if fix:
                changed = pre + begin + expected + end + post
            else:
                # Name the difference rather than dump a diff: the reader wants to
                # know which rows moved, and `--fix-keymap` is the answer either way.
                old_rows = {ln for ln in old.strip().splitlines() if ln.startswith("| `")}
                new_rows = {ln for ln in expected.strip().splitlines() if ln.startswith("| `")}
                missing = sorted(new_rows - old_rows)
                stale = sorted(old_rows - new_rows)
                bits = []
                if missing:
                    bits.append(f"{len(missing)} row(s) missing from the page")
                if stale:
                    bits.append(f"{len(stale)} row(s) in the page with no CHORD_TABLE row")
                detail = "\n".join(
                    [f"    expected: {ln}" for ln in missing[:5]]
                    + [f"    found:    {ln}" for ln in stale[:5]]
                )
                bad.append(
                    f"{rel(page)}: the generated {slug} table disagrees with CHORD_TABLE "
                    f"({'; '.join(bits) or 'formatting'}). Run: "
                    f"./scripts/docs_check.py --fix-keymap"
                    + ("\n" + detail if detail else "")
                )
    if fix and changed != text:
        page.write_text(changed, encoding="utf-8")
    return bad


# ─────────────── check 4: measurement claims (spike check counts) ───────────────
#
# The failure this closes (looprs-00u.23): a spike's check count is a fact about
# one run, and three pages restated it as a property of the tree. The tree moved to
# 171 checks, the pages stayed at 149, and no capture of 171 was committed, so
# the number could not be checked against anything. Knob defaults get a
# default-agreement check; measurement results had none.
#
# The rule the check enforces:
#   * a count in prose is a claim about a run; the run's home is a capture in
#     spikes/results/, so name it — and the capture you name has to say the same
#     number;
#   * an un-named ("current") count has to match the newest committed capture of
#     that spike, which is what "the current tree prints N" means;
#   * a count whose run was never captured is allowed only if the page says so,
#     with a reason, in a marker this check can see.

CAPTURE_SUMMARY_RE = re.compile(r"(\d+)\s*/\s*(\d+)\s+checks passed")
BARE_RATIO_LINE_RE = re.compile(r"^\s*(?:\[[^\]]*\]\s*)?(\d+)\s*/\s*(\d+)\s*$")
N_CHECKS_RE = re.compile(r"\b(\d{1,5})\s+checks\b")
CAPTURE_FILE_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.+\-]*\.log\b")
# Cheap page-level gate before any of this runs: a page that never mentions a
# spike, a capture or a check count is not in this check's business.
CLAIM_TRIGGER_RE = re.compile(r"checks\b|spike|e2e|results/", re.I)
# Captures that are comparison runs, not the current state of the tree.
CAPTURE_CONTROL_RE = re.compile(r"control|before|\bat-", re.I)
UNVERIFIED_MARKER_RE = re.compile(
    r"<!--\s*spike-count:\s*unverified\s+(\d{1,5}(?:\s*/\s*\d{1,5})?)\s+"
    r"([a-z0-9_]+)(?:\s*[\u2014\u2013:-])?\s*(.*?)\s*-->",
    re.S,
)


def _git_newest_times(dirs) -> dict:
    """{ROOT-relative path: newest unix commit time that touched it}, or {} with no git.

    One `git log` over the whole of `spikes/` rather than one per file: the check
    has to stay a sub-second step of the gate.
    """
    try:
        top = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=str(ROOT), capture_output=True, text=True, check=True,
        ).stdout.strip()
    except Exception:
        return {}
    prefix = ""
    try:
        prefix = str(ROOT.relative_to(top)) + "/"
    except ValueError:
        pass
    # `dirs` are relative to this crate and `git log` answers with paths relative
    # to the *repo* root (this crate is a subdirectory of one), so the pathspec is
    # cwd-relative and the answer is prefix-stripped. Getting this backwards makes
    # the log empty and every capture equally (un)current.
    out = {}
    try:
        proc = subprocess.run(
            ["git", "log", "--no-color", "--pretty=format:@%ct", "--name-only",
             "--", *dirs],
            cwd=str(ROOT), capture_output=True, text=True, check=True,
        )
    except Exception:
        return {}
    stamp = 0
    for line in proc.stdout.splitlines():
        if line.startswith("@"):
            stamp = int(line[1:])
        elif line.strip():
            path = line.strip()
            if prefix and not path.startswith(prefix):
                continue
            out.setdefault(path[len(prefix):], stamp)
    return out


def spike_registry() -> dict:
    """{alias: spike name} for every spike script in `spikes/`.

    Prose names a spike as `spikes/shutdown_e2e.py`, as `shutdown_e2e`, and as
    `shutdown-e2e` inside a capture name; all three have to find the same spike.

    A "spike" here is a program under `spikes/` that a page can measure with. Not a
    fixture: `fake_pi_slow.py` is a stand-in the drivers run, not a measurement,
    and registering it made the gate read the status spike's `20/20` as a claim
    about a fake with no captures of its own.
    """
    reg = {}
    if not SPIKES.is_dir():
        return reg
    for path in sorted(SPIKES.iterdir()):
        if not path.is_file() or path.suffix not in {".py", ".sh"}:
            continue
        if path.name.startswith(("fake_", "fixture", "_")) or "fixtures" in path.parts:
            continue
        name = path.name
        stem = path.stem
        for alias in {name, stem, stem.replace("_", "-")}:
            reg.setdefault(alias, name)
    return reg


def capture_records() -> dict:
    """{capture name: {"path", "passed", "total", "control", "when", "spike"}}.

    The total is read out of the capture's own summary line — `N/M checks passed`,
    or the bare `N/M` a few spikes finish with — which is why this check needs no
    sidecar manifest: the log is the record, and the prose points at it.
    """
    if not RESULTS.is_dir():
        return {}
    times = _git_newest_times([str((Path("spikes") / "results").as_posix()), "spikes"])
    spikes = spike_registry()
    recs = {}
    for path in sorted(RESULTS.iterdir()):
        if not path.is_file() or path.suffix not in {".log", ".txt", ".out"}:
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        passed = total = None
        summary = CAPTURE_SUMMARY_RE.findall(text)
        if summary:
            passed, total = (int(x) for x in summary[-1])
        else:
            for line in reversed(text.splitlines()):
                m = BARE_RATIO_LINE_RE.match(line)
                if m:
                    passed, total = int(m.group(1)), int(m.group(2))
                    break
        stem = path.stem
        spike = None
        for alias, sname in spikes.items():
            if stem == alias or stem.startswith(alias + "-") or stem.startswith(alias + "."):
                spike = sname
                break
        recs[path.name] = {
            "path": path,
            "passed": passed,
            "total": total,
            "control": bool(CAPTURE_CONTROL_RE.search(path.name)),
            "when": times.get(f"spikes/results/{path.name}", 0),
            "spike": spike,
        }
    return recs


def current_capture(recs, spike, when_known: bool = True) -> dict | None:
    """The capture that says what the tree prints today: the newest non-control
    capture of that spike, preferring one that passed outright.

    With no git history to order the captures by (`when_known` false — a tree
    without git, a tarball, a fixture), "newest" is not a fact and picking one by
    filename would fail pages for something the machine cannot know. Callers get
    None and skip that half of the rule.
    """
    if not when_known:
        return None
    mine = [r for r in recs.values() if r["spike"] == spike and not r["control"]]
    if not mine:
        return None
    passing = [r for r in mine if r["total"] is not None and r["passed"] == r["total"]]
    pool = passing or mine
    return sorted(pool, key=lambda r: (r["when"], r["path"].name))[-1]


def _inside_fence(text: str, offset: int) -> bool:
    """Is this offset inside a ``` fence? A marker shown as documentation is not a
    marker on the page; counting it would fail the page for quoting the gate."""
    return text.count("```", 0, offset) % 2 == 1


def markdown_units(text: str):
    """Units of prose this gate reads as one statement.

    Yields `(unit_text, headers, lineno_of)`:
      * a Markdown table becomes one unit per row — a row is one statement, and
        the block of rows is not (`spikes/README.md` is one row per spike, and
        reading it as one paragraph would attribute every capture on the page to
        every number on it);
      * a block of prose becomes one unit — the paragraph, because the sentence
        carrying the number is routinely a line away from the sentence naming the
        spike it measured.

    `lineno_of(offset)` maps a match inside the unit back to the line to report.
    Everything inside a ``` fence is skipped: a transcript or a command is not a
    claim about the tree.
    """
    lines = text.splitlines(keepends=True)
    blocks: list[list[tuple[int, str]]] = []
    cur: list[tuple[int, str]] = []
    in_fence = False
    for i, line in enumerate(lines, start=1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            if cur:
                blocks.append(cur)
                cur = []
            continue
        if in_fence:
            continue
        if not line.strip():
            if cur:
                blocks.append(cur)
                cur = []
            continue
        cur.append((i, line))
    if cur:
        blocks.append(cur)

    out = []
    for block in blocks:
        table_rows = [(n, l) for n, l in block if l.lstrip().startswith("|")]
        if table_rows:
            headers = None
            for lineno, row in table_rows:
                if re.match(r"^\s*\|[\s:|-]+\|\s*$", row):
                    continue
                if headers is None:
                    headers = split_row(row)
                    continue
                out.append((row, headers, lambda offset, n=lineno: n))
            continue
        unit = "".join(l for _, l in block)
        starts = []
        pos = 0
        for lineno, l in block:
            starts.append((pos, lineno))
            pos += len(l)

        def lineno_of(offset: int, starts=starts) -> int:
            found = starts[0][1] if starts else 1
            for start, lineno in starts:
                if start <= offset:
                    found = lineno
                else:
                    break
            return found

        out.append((unit, None, lineno_of))
    return out


# What makes a number read as a *check count* rather than some other ratio. Each
# pattern is an adjacency: "149/149" is only a claim about a test suite when the
# words beside it say so. `[ -t 0/1/2 ]`, `16/256/truecolour` and "9 of 10 runs"
# are ratios too, and are not this.
PAIR_BEFORE_CHECKS_RE = re.compile(
    r"(?<![\d./])(\d{1,5})\s*/\s*(\d{1,5})(?![\d./])\*{0,2}\s+checks\b"
)
PAIR_BOLD_RE = re.compile(r"\*\*(\d{1,5})\s*/\s*(\d{1,5})\*\*")
CODE_THEN_PAIR_RE = re.compile(r"`([A-Za-z0-9_.+\-]+)`\s+(\d{1,5})\s*/\s*(\d{1,5})(?![\d./])")
N_CHECKS_RE = re.compile(r"\b(\d{1,5})\s+checks\b")
BARE_PAIR_RE = re.compile(r"(?<![\d./])(\d{1,5})\s*/\s*(\d{1,5})(?![\d./])")
RESULT_COLUMN_RE = re.compile(r"check|result|score|pass", re.I)


def claims_in(unit_text: str, headers=None) -> list[tuple[int, str, str]]:
    """`(offset, count, kind)` for every check-count claim in one unit.

    `kind` is `pair` (an `N/M`, both numbers claimed) or `total` (an `N checks`,
    the denominator). Deduplicated on the claim itself: one paragraph asserting
    the same total twice is one claim.
    """
    found: list[tuple[int, str, str]] = []
    for m in PAIR_BEFORE_CHECKS_RE.finditer(unit_text):
        found.append((m.start(), f"{m.group(1)}/{m.group(2)}", "pair"))
    for m in PAIR_BOLD_RE.finditer(unit_text):
        found.append((m.start(), f"{m.group(1)}/{m.group(2)}", "pair"))
    for m in CODE_THEN_PAIR_RE.finditer(unit_text):
        token = m.group(1)
        if token.endswith(".log") or "e2e" in token or "spike" in token:
            found.append((m.start(2), f"{m.group(2)}/{m.group(3)}", "pair"))
    if headers and any(RESULT_COLUMN_RE.search(h) for h in headers):
        # A number under a column called "Result"/"checks" is read as a result.
        for m in BARE_PAIR_RE.finditer(unit_text):
            found.append((m.start(), f"{m.group(1)}/{m.group(2)}", "pair"))
    for m in N_CHECKS_RE.finditer(unit_text):
        found.append((m.start(), m.group(1), "total"))
    seen, out = set(), []
    for item in sorted(found):
        key = (item[1], item[2])
        if key not in seen:
            seen.add(key)
            out.append(item)
    return out


def alias_hits(text: str, spikes: dict) -> list[tuple[int, int, str]]:
    """(start, end, spike) for every way this text can name a spike."""
    hits = []
    for alias, name in spikes.items():
        for m in re.finditer(re.escape(alias), text):
            hits.append((m.start(), m.end(), name))
    return hits


def subject_at(offset: int, hits) -> str | None:
    """The spike a number belongs to: the nearest name to its left.

    `viewport_e2e` 16/16, `status_e2e` 20/20 has two subjects in one line, and
    taking the first name on the line would attribute both totals to viewport.
    """
    left = [h for h in hits if h[1] <= offset]
    if left:
        return max(left, key=lambda h: h[1])[2]
    right = [h for h in hits if h[0] > offset]
    if right:
        return min(right, key=lambda h: h[0])[2]
    return None


def check_measurement_claims(pages) -> tuple[list[str], int]:
    """Every spike check count in prose has to be backed by a committed capture.

    The failure this closes (looprs-00u.23): three pages said "149 checks" while
    the tree printed 171, and no capture of the 171 run existed, so the number
    could not be checked against anything. Knob defaults have had an
    agreement check since the gate was written; measurement results had none.

    Reading order for one claim:

      1. it matches the **newest committed capture** of the spike it names —
         that is what "the suite prints N" means today;
      2. else it matches a capture **named in the same statement** — a
         historical number is fine while you can see which run it came from;
      3. else it carries an explicit
         `<!-- spike-count: unverified N/M <spike> — why -->` marker, which is
         itself rot-checked: a marker that no longer backs a claim fails.

    Anything else is a violation.
    """
    recs = capture_records()
    spikes = spike_registry()
    when_known = any(r["when"] for r in recs.values())
    bad: list[str] = []
    backed = 0

    for page in pages:
        try:
            text = page.read_text(encoding="utf-8")
        except OSError:
            continue
        if not CLAIM_TRIGGER_RE.search(text):
            continue
        markers = [
            {
                "count": m.group(1).replace(" ", ""),
                "spike": m.group(2),
                "line": text[: m.start()].count("\n") + 1,
                "used": False,
            }
            for m in UNVERIFIED_MARKER_RE.finditer(text)
            if not _inside_fence(text, m.start())
        ]
        for unit, _headers, lineno_of in markdown_units(text):
            hits = alias_hits(unit, spikes)
            named_line = [n for n in CAPTURE_FILE_RE.findall(unit) if n in recs]
            named_with_total = [n for n in named_line if recs[n]["total"] is not None]
            for offset, count, kind in claims_in(unit, _headers):
                lineno = lineno_of(offset)
                subject = subject_at(offset, hits)

                def marker_ok(mk, count=count, subject=subject, unit=unit):
                    if mk["used"] or mk["count"] != count:
                        return False
                    if subject is not None and mk["spike"] in {
                        subject,
                        subject.replace(".py", ""),
                        subject.replace(".sh", ""),
                    }:
                        return True
                    # A marker for a spike named somewhere in the same statement,
                    # for the case where the number itself has no owner in the
                    # sentence and the page is declaring *that* unverified.
                    return any(a in unit for a in (mk["spike"], mk["spike"] + ".py"))

                marker = next((mk for mk in markers if marker_ok(mk)), None)
                if marker:
                    marker["used"] = True
                    backed += 1
                    continue

                if subject is None and not named_with_total:
                    # A count with no spike and no capture in its statement is not
                    # something a reader can check, which is the ticket's actual
                    # complaint: "It now runs 8 scenarios and 149 checks" is
                    # unfalsifiable without an owner. Say whose number it is, or
                    # mark it unverified with the reason.
                    bad.append(
                        f"{rel(page)}:{lineno}: check count {count} with no owner in "
                        f"the statement — name the spike (`spikes/<name>.py`) or the "
                        f"capture it came from, or mark it "
                        f"<!-- spike-count: unverified {count} <spike> — why -->"
                    )
                    continue
                current = current_capture(recs, subject, when_known) if subject else None
                if current is not None and _capture_matches(current, count, kind):
                    backed += 1
                    continue

                if any(_capture_matches(recs[n], count, kind) for n in named_with_total):
                    backed += 1
                    continue

                where = subject or "a spike"
                short = where.replace(".py", "").replace(".sh", "")
                spike_captures = (
                    [r for r in recs.values() if r["spike"] == subject]
                    if subject
                    else []
                )
                if current is None and spike_captures:
                    # Captures exist but nothing orders them (no git history to
                    # call one of them current) and the sentence named none that
                    # matches, so there is no honest way to call the number
                    # stale. The named-capture half above still ran.
                    continue
                if current is None:
                    bad.append(
                        f"{rel(page)}:{lineno}: claims {count} for {where} and no "
                        f"capture of it is committed under spikes/results/ — a count "
                        f"nobody captured is a count nobody can re-check. Run the "
                        f"spike and commit the log, or admit it: "
                        f"<!-- spike-count: unverified {count} {short} — why -->"
                    )
                else:
                    said = ", ".join(
                        f"{n} is {_fmt(recs[n])}"
                        for n in named_with_total[:3]
                    )
                    bad.append(
                        f"{rel(page)}:{lineno}: claims {count} for {where}; the newest "
                        f"committed capture is {current['path'].name} at "
                        f"{_fmt(current)}"
                        + (f" ({said})" if said else "")
                        + ". Update the number *and* commit a fresh capture "
                        f"(`./scripts/capture.sh {subject or '<spike>'} "
                        f"<name>.log`), or name the older capture the "
                        f"sentence means, or admit it: "
                        f"<!-- spike-count: unverified {count} {short} — why -->"
                    )

        for mk in markers:
            if not mk["used"]:
                bad.append(
                    f"{rel(page)}:{mk['line']}: marker declares {mk['count']} for "
                    f"{mk['spike']} unverified, but no claim of that number is on the "
                    f"page — delete the marker or fix the count"
                )
    return bad, backed


def _fmt(rec) -> str:
    if rec["total"] is None:
        return "no `N/M checks passed` line"
    return f"{rec['passed']}/{rec['total']}"


def _capture_matches(rec, count, kind) -> bool:
    if rec["total"] is None:
        return False
    if kind == "total":
        return rec["total"] == int(count)
    passed, _, total = str(count).partition("/")
    if total == "":
        return rec["total"] == int(passed)
    return rec["passed"] == int(passed) and rec["total"] == int(total)


# ──────────────────────────────── main ────────────────────────────────


def rel(p: Path) -> str:
    try:
        return str(p.relative_to(ROOT))
    except ValueError:
        return str(p)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--fix-keymap", action="store_true", help="regenerate the keymap tables")
    ap.add_argument("--list-knobs", action="store_true", help="print the knobs the code reads")
    ap.add_argument(
        "--list-captures", action="store_true",
        help="print every committed spike capture, its total, and which spike it belongs to",
    )
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()

    if args.list_knobs:
        for knob, where in sorted(code_knobs().items()):
            print(f"{knob:36s} {', '.join(where[:3])}")
        return 0

    if args.list_captures:
        recs = capture_records()
        for spike in sorted({r["spike"] or "(unmapped)" for r in recs.values()}):
            mine = sorted(
                (n for n, r in recs.items() if (r["spike"] or "(unmapped)") == spike)
            )
            print(f"{spike}")
            for name in mine:
                r = recs[name]
                tag = "control run" if r["control"] else "capture"
                star = "  <- current" if current_capture(recs, r["spike"]) is r else ""
                print(f"  {name:38s} {_fmt(r):>10s}  [{tag}]{star}")
        return 0

    pages = corpus_pages()
    violations: list[str] = []
    violations += check_links(pages)
    violations += check_orphans(pages)
    violations += check_knobs()
    violations += check_keymap(fix=args.fix_keymap)
    measurement_violations, claims_checked = check_measurement_claims(pages)
    violations += measurement_violations

    if args.fix_keymap and not violations:
        print("docs_check: regenerated the keymap tables; no other violations")
        return 0

    if violations:
        print(f"docs_check: {len(violations)} violation(s)")
        for v in violations:
            print(f"  {v}")
        print()
        print("  These are docs-rot failures: a link that lands nowhere, a knob the")
        print("  reference does not list, a page nothing links to, or a table that has")
        print("  drifted from the code it claims to describe.")
        return 1

    if not args.quiet:
        print(
            f"docs_check: clean ({len(pages)} page(s), "
            f"{len(code_knobs())} knob(s), {len(parse_chord_table())} chord row(s), "
            f"{claims_checked} measurement claim(s) backed by spikes/results/)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
