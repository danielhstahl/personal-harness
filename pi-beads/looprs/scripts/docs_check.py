#!/usr/bin/env python3
"""The documentation rot gate (looprs-00u.10 / ADR-0008).

Three checks that keep the site true after the day it was written:

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

Run from anywhere; runs in well under a second; no network, no cargo, no build.

    ./scripts/docs_check.py                 # the gate
    ./scripts/docs_check.py --fix-keymap    # rewrite the generated tables
    ./scripts/docs_check.py --list-knobs    # what the code reads, with file:line
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
SRC = ROOT / "src"

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
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()

    if args.list_knobs:
        for knob, where in sorted(code_knobs().items()):
            print(f"{knob:36s} {', '.join(where[:3])}")
        return 0

    pages = corpus_pages()
    violations: list[str] = []
    violations += check_links(pages)
    violations += check_orphans(pages)
    violations += check_knobs()
    violations += check_keymap(fix=args.fix_keymap)

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
            f"{len(code_knobs())} knob(s), {len(parse_chord_table())} chord row(s))"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
