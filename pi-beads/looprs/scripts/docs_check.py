#!/usr/bin/env python3
"""The documentation rot gate (looprs-00u.10 / ADR-0008).

Seven checks that keep the site true after the day it was written:

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
   rendering of `CHORD_TABLE` in `src/session/view/chord_table.rs`, compared row by row.
   `--fix-keymap` re-renders them.
4. **Measurement claims** (looprs-00u.23) — every spike check count restated in
   prose (`**149/149**`, `149 checks`) is backed by a committed capture under
   `spikes/results/`: either the sentence names a capture whose own
   `N/M checks passed` line agrees, or the number matches the newest committed
   capture of the spike it names. A count whose run was never captured needs an
   explicit `<!-- spike-count: unverified N/M spike — why -->` marker, which is
   itself rot-checked (a marker that no longer backs anything fails).
5. **Quoted test names** (looprs-00u.25) — a backticked `snake_case` name in a
   page that is long enough to read as a test-name citation (four words or
   more, the shape this repo's test names have) resolves to a real `fn <name>`
   in the tree. A page whose whole point is "the test list is a
   specification" is worthless when a quoted item cannot be grepped: the
   reader assumes their grep is wrong before they assume the docs are.
   Illustrative names that are not functions get an explicit entry in
   `ILLUSTRATIVE_TEST_NAMES` with the reason they are illustrative, and an
   entry that no longer covers a citation fails.
6. **The spike index** (looprs-00u.26) — every `*.py` and `*.sh` under `spikes/`
   has a row in `spikes/README.md`, and every row of that index that names a
   path under `spikes/` points at something that exists. `spikes/README.md` is
   cited by `docs/guide/operator.md` and `docs/guide/contributing.md` as *the*
   index of what each spike measures; three drivers sat unindexed there while two
   pages linked to them by name, so the index said a cited file did not exist.
   An unindexed spike and an orphan page are the same failure arriving later,
   which is why this check is the orphan check with the noun changed — including
   the direction that only bites later: an index that keeps advertising a driver
   somebody deleted is the same lie, told forward.
7. **Inclusive sizes** (looprs-00u.27) — a table row that says it **includes**
   another row cannot quote a *smaller* line count than the row it includes, and a
   "N-line difference" stated beside two totals has to equal the difference. The
   table whose whole job is to be the measurement — ADR-0008's corpus table —
   shipped as "7,071 lines under `docs/`" and "7,056 lines including `spikes/`":
   a superset 15 lines **smaller** than its own part, which no amount of
   re-reading can make true and nothing in the repo could contradict. Both figures
   were stale and had been typed at different times, and the table that existed to
   settle the question instead printed the impossibility. `--list-corpus` prints
   today's counts with the command that produced each one, so the next re-count is
   a re-run rather than a re-type.

Run from anywhere; runs in well under a second; no network, no cargo, no build.

    ./scripts/docs_check.py                 # the gate
    ./scripts/docs_check.py --fix-keymap    # rewrite the generated tables
    ./scripts/docs_check.py --list-knobs    # what the code reads, with file:line
    ./scripts/docs_check.py --list-captures # every committed capture and its total
    ./scripts/docs_check.py --list-spikes   # every driver, indexed or not, with its captures
    ./scripts/docs_check.py --list-corpus   # today's corpus line counts, by the book
"""

from __future__ import annotations

import argparse
import difflib
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
SPIKE_INDEX = SPIKES / "README.md"

# Pages outside docs/ that are part of the corpus the site carries, and whose links
# are therefore gated. Kept short on purpose: spikes/results/*.log files are
# evidence, not prose, and gating them would be gating captures.
EXTRA_PAGES = [ROOT / "spikes" / "README.md", ROOT / "README.md"]

KNOB_RE = re.compile(r"LOOPRS_[A-Z0-9_]+")
LINK_RE = re.compile(r"\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")

# A table row whose label declares it is *more than* the row above it. Worded on
# purpose: this is the shape that made ADR-0008's corpus table print "7,071 lines"
# and then "7,056 lines including spikes/", and `including` is how a human writes a
# superset. "total"/"all of" are here for the same sentence said the other way.
INCLUSIVE_LABEL_RE = re.compile(
    r"\binclud(?:e|es|ing)\b|\bplus\b|\ball of\b|\bwith .* includ|\btotal\b", re.I
)
# A size the reader takes away: "8,347 lines", "**7,071 lines**", "4,558 line".
LINE_COUNT_RE = re.compile(r"(\d[\d,]*)\s*(?:\*\*)?\s*(?:lines?|lines\b)", re.I)
# A stated gap between two totals, which has to equal the actual difference.
DELTA_LINE_RE = re.compile(
    r"(\d[\d,]*)[- ]line(?:s)?\s+(?:difference|gap|larger|bigger|more|higher)", re.I
)


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
    """The `CHORD_TABLE` rows out of `src/session/view/chord_table.rs`.

    The file is pre-processed with Rust's own line-continuation rule (a backslash at
    end of line inside a string literal swallows the newline and the leading
    whitespace of the next line) so a multi-line `note:` parses as one string.
    """
    path = SRC / "session" / "view" / "chord_table.rs"
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


# ─────────────── check 5: the wire protocol inventory ───────────────
#
# `docs/guide/wire-protocol.md` is generated out of `WIRE_INVENTORY` in
# `src/wire.rs` exactly the way the keymap tables are generated out of
# `CHORD_TABLE` (looprs-00u.7). Doing it to the wire as well was
# looprs-00u.19, which found the hole this closes: `role: String` with
# `"user" | "assistant" | "toolResult" …` behind it in a comment. A page that
# has to be written from pi's own docs is a second source of truth, and the
# ellipsis is where it drifts. A page rendered from the enum cannot: adding a
# value without its row makes the gate fail rather than making the page quietly
# stay silent about the new value.
#
# The Rust side of the same contract is `app::tests::wire_protocol`, which
# checks each row against the type that parses it and, for the message roles,
# against what `App::apply_pi` really does.

WIRE_RS = SRC / "wire.rs"

WIRE_MARKER_GROUPS = [
    "role",
    "event",
    "assistant-event",
    "compaction-reason",
    "stop-reason",
]


def _rust_string(literal: str) -> str:
    """The contents of a Rust string literal, minus the quotes, with the escapes
    this file actually uses resolved."""
    m = re.match(r'"((?:[^"\\]|\\.)*)"', literal.strip())
    if not m:
        return ""
    text = m.group(1)
    for esc, ch in ((r"\"", '"'), (r"\\", "\\"), (r"\n", "\n")):
        text = text.replace(esc, ch)
    return re.sub(
        r"\\u\{([0-9a-fA-F]+)\}", lambda m: chr(int(m.group(1), 16)), text
    )


def _rust_option(literal: str) -> str | None:
    lit = literal.strip()
    if lit == "None":
        return None
    m = re.match(r'Some\("((?:[^"\\]|\\.)*)"\)', lit)
    return _rust_string(f'"{m.group(1)}"') if m else None


def _rust_field(block: str, name: str) -> str:
    """The literal value of `name:` inside one `WireRow { … }` block.

    A scanner rather than a regex because the fields are not one-per-line in any
    meaningful sense and the values are not all plain strings: `Some("…")`,
    `Outcome::Silent("…")` and a bare `None` all have to come out whole, and a
    regex that stops at the first comma stops inside `Some("a, b")` while one
    that stops at the first newline stops inside a folded string. This walks the
    value at bracket depth, respecting string escapes, and stops at the comma or
    closing brace that actually ends it.
    """
    m = re.search(rf"\b{name}:\s*", block)
    if not m:
        return ""
    rest = block[m.end():]
    depth = 0
    in_str = False
    esc = False
    for i, ch in enumerate(rest):
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
            continue
        if ch == '"':
            in_str = True
        elif ch in "([{":
            depth += 1
        elif ch in ")]}":
            if depth == 0:
                return rest[:i].strip()
            depth -= 1
        elif ch == "," and depth == 0:
            return rest[:i].strip()
    return rest.strip()


def wire_inventory() -> list[dict]:
    """The `WIRE_INVENTORY` rows out of `src/wire.rs`.

    Same shape as `parse_chord_table`: a `const` array of literal structs, read
    with the line-continuation pre-pass so a backslash-folded string joins. The
    rows are written one field-group per line under `#[rustfmt::skip]`, which is
    what keeps this a field grep rather than a Rust parser.
    """
    raw = WIRE_RS.read_text(encoding="utf-8")
    raw = re.sub(r"\\\n[ \t]*", "", raw)
    start = raw.index("pub const WIRE_INVENTORY")
    end = raw.index("\n];", start)
    body = raw[start:end]
    rows = []
    for block in re.findall(r"WireRow\s*\{(.*?)\n    \},", body, re.S):
        def field(name: str) -> str:
            return _rust_field(block, name)

        outcome_lit = field("outcome")
        if "SurfacesAsNote" in outcome_lit:
            kind, text = "SurfacesAsNote", ""
        else:
            m = re.match(r'Outcome::(Renders|Silent)\("((?:[^"\\]|\\.)*)"\)', outcome_lit)
            kind, text = (m.group(1), _rust_string(f'"{m.group(2)}"')) if m else ("?", outcome_lit)

        rows.append(
            {
                "group": _rust_string(field("group")),
                "wire": _rust_string(field("wire")),
                "variant": _rust_string(field("variant")),
                "outcome": kind,
                "outcome_text": text,
                "reader": _rust_option(field("reader")),
                "waiting_on": _rust_option(field("waiting_on")),
                "note": _rust_string(field("note")),
            }
        )
    return rows


def outcome_label(kind: str, text: str) -> str:
    """The page's wording for one outcome.

    `wire.rs`'s `Outcome::label` is the definition; `the_three_outcomes_read_
    differently` in `app::tests::wire_protocol` pins these three strings, so
    the page and the type cannot describe the same outcome two different ways
    without a test noticing.
    """
    if kind == "Renders":
        return f"renders: {text}"
    if kind == "Silent":
        return f"not painted — {text}"
    if kind == "SurfacesAsNote":
        return "surfaces as a transcript note naming the value"
    return f"{kind}({text})"


def render_wire_table(group: str, rows) -> str:
    sel = [r for r in rows if r["group"] == group]
    out = [
        "| on the wire | the variant that catches it | what looprs does with it | who reads it | note |",
        "| --- | --- | --- | --- | --- |",
    ]
    for r in sel:
        who = r["reader"] or f"*nothing today — waiting on: {r['waiting_on']}*"
        out.append(
            f"| `{r['wire']}` | `{r['variant']}` | {outcome_label(r['outcome'], r['outcome_text'])}"
            f" | {who} | {r['note']} |"
        )
    return "\n".join(out)


def _reason_next_to(lines: list[str], i: int) -> str:
    """The reason a human wrote next to a `#[allow(dead_code)]` at line `i`.

    Three shapes, in the order they should be read:

    1. the trailing comment on the attribute line itself;
    2. the `//` line directly **under** it, which is where rustfmt puts a
       trailing comment that no longer fits in the width — the reason is
       unchanged in meaning, only in column;
    3. the `//` line directly **above** it, the shape several records in
       `wire.rs` were first written in.

    Empty means there is no reason anywhere next to the attribute, which is the
    thing both this checker and `scripts/dead_audit.py` treat as unanswered.
    """
    stripped = lines[i].strip()
    tail = stripped.split("dead_code)", 1)[-1].strip()
    if tail.startswith("]"):
        tail = tail[1:].strip()
    if tail.startswith("//"):
        return tail[2:].strip()
    if i + 1 < len(lines) and lines[i + 1].strip().startswith("//"):
        return lines[i + 1].strip()[2:].strip()
    if i > 0 and lines[i - 1].strip().startswith("//"):
        return lines[i - 1].strip()[2:].strip()
    return ""


def wire_allow_reasons() -> list[dict]:
    """Every `#[allow(dead_code)]` in `wire.rs` with the reason written beside it.

    `scripts/dead_audit.py` asks the compiler whether the guarded item is still
    dead. This asks the other half, mechanically: is there a reason at all. The
    rule is individually justified, never blanket (looprs-6ol), and "individually
    justified" is only real if a gate can tell a reason from no reason — the same
    reason `WIRE_INVENTORY` makes a `reader: None` carry a `waiting_on`.
    """
    lines = WIRE_RS.read_text(encoding="utf-8").split("\n")
    out = []
    for i, line in enumerate(lines):
        if not line.strip().startswith("#[allow(dead_code)]"):
            continue
        reason = _reason_next_to(lines, i)
        item_at = i
        j = i + 1
        while j < len(lines):
            nxt = lines[j].strip()
            if nxt.startswith("//") or nxt.startswith("#["):
                j += 1
                continue
            item_at = j
            break
        kind, name = _item_of(lines[item_at]) if item_at != i else ("other", "(nothing)")
        # A type that is itself the guarded item has no enclosing type to name it:
        # `Outcome` is `Outcome`, not `CompactionResult::Outcome`.
        owner = "" if kind == "type" else _owner_above(lines, item_at)
        out.append(
            {"line": i + 1, "reason": reason, "owner": owner, "item": name, "kind": kind}
        )
    return out


#: `impl|enum|struct|trait|fn|const name [for Target]`, with the visibility and
#: `default` prefixes a declaration can carry.
DECL_RE = re.compile(
    r"^(?:pub\s+)?(?:default\s+)?(impl|enum|struct|trait|fn|const)\s+([A-Za-z0-9_<>&]+)"
    r"(?:\s+for\s+([A-Za-z0-9_<>&]+))?"
)
#: An enum variant: a bare `CamelCase` name, with or without its field list.
VARIANT_RE = re.compile(r"^([A-Z][A-Za-z0-9_]*)$")
#: A struct or variant field: a `snake_case` name with a type after the colon.
FIELD_RE = re.compile(r"^(?:pub\s+)?([a-z_][a-z0-9_]*)\s*:")
#: Only these kinds can own another item.
OWNER_RE = re.compile(
    r"^(?:pub\s+)?(impl|enum|struct|trait)\s+([A-Za-z0-9_<>&]+)"
    r"(?:\s+for\s+([A-Za-z0-9_<>&]+))?"
)


def _item_of(line: str) -> tuple[str, str]:
    """`(kind, printed name)` for the line an allow guards — a variant, a field,
    a method, a const, a type — so the table says *what* is unread rather than
    reprinting a line of source with its braces and trailing comma still on it."""
    code = _strip_code_noise(line).strip().rstrip(",;").strip()
    code = code.rstrip("{").strip()
    m = DECL_RE.match(code)
    if m:
        kind, name, for_whom = m.group(1), m.group(2), m.group(3)
        if kind == "impl":
            return "impl", f"impl {name}" + (f" for {for_whom}" if for_whom else "")
        if kind == "fn":
            return "fn", f"{name}()"
        if kind == "const":
            return "const", name
        return "type", name
    m = VARIANT_RE.match(code)
    if m:
        return "variant", m.group(1)
    m = FIELD_RE.match(code)
    if m:
        return "field", m.group(1)
    return "other", code


def _owner_above(lines: list[str], i: int) -> str:
    """The `impl` / `enum` / `struct` / `trait` the item at line `i` sits in.

    A bare `content_index` or `variant()` says nothing about which type holds it,
    and this table is read as the record of who owns which unread value, so the
    enclosing type is part of the name.
    """
    for k in range(i - 1, -1, -1):
        code = _strip_code_noise(lines[k]).strip()
        if not code:
            continue
        # Only the leftmost column can hold a declaration that owns a whole
        # block; anything indented further in is a member of that block (an enum
        # variant, a nested brace) and not a candidate owner. So: skip inward,
        # and read the first line flush left. A free `fn` or `const` at column 0
        # is its own thing and has no owner — which beats the nearest
        # plausible-looking type three declarations back.
        if _indent(lines[k]) > 0:
            continue
        m = OWNER_RE.match(code)
        if m:
            name, for_whom = m.group(2), m.group(3)
            return f"{name} for {for_whom}" if for_whom else name
        return ""
    return ""


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip())


def _strip_code_noise(line: str) -> str:
    """Drop comments and string literals, so the keyword matching above reads the
    structure of the file rather than its prose."""
    out = []
    i = 0
    while i < len(line):
        if line.startswith("//", i):
            break
        if line[i] == '"':
            i += 1
            while i < len(line) and line[i] != '"':
                i += 2 if line[i] == "\\" else 1
            i += 1
            continue
        out.append(line[i])
        i += 1
    return "".join(out)


def render_wire_unread_table() -> str:
    out = [
        "| the record kept unread | why it stays |",
        "| --- | --- |",
    ]
    for r in wire_allow_reasons():
        name = f"{r['owner']}::{r['item']}" if r["owner"] else r["item"]
        out.append(f"| `{name}` | {r['reason']} |")
    return "\n".join(out)


def check_wire(fix: bool) -> list[str]:
    page = DOCS / "guide" / "wire-protocol.md"
    if not page.exists():
        return [f"{rel(page)}: missing — the wire protocol page is generated, not hand-written"]

    bad = []
    rows = wire_inventory()

    # The declaration order the page prints in must be the order the enum is
    # declared in, and every declared group must be printed.
    declared_groups = re.findall(
        r'WIRE_GROUPS: &\[&str\] = &\[(.*?)\n\];',
        WIRE_RS.read_text(encoding="utf-8"),
        re.S,
    )
    if declared_groups:
        in_code = re.findall(r'"([^"]+)"', declared_groups[0])
        if in_code != WIRE_MARKER_GROUPS:
            bad.append(
                "src/wire.rs: WIRE_GROUPS is "
                f"{in_code} but this checker prints {WIRE_MARKER_GROUPS} — "
                "a group added to the enum needs its section here and in the page"
            )

    # An allow with no reason next to it is the blanket allow by another name.
    for r in wire_allow_reasons():
        if not r["reason"]:
            bad.append(
                f"src/wire.rs:{r['line']}: `#[allow(dead_code)]` on `{r['item']}` "
                "has no reason written next to it (looprs-6ol: individually "
                "justified, never blanket)"
            )

    text = page.read_text(encoding="utf-8")
    changed = text
    sections = [(f"wire:{g}", render_wire_table(g, rows)) for g in WIRE_MARKER_GROUPS]
    sections.append(("wire:unread", render_wire_unread_table()))
    for slug, expected in sections:
        begin = f"<!-- BEGIN GENERATED:{slug} -->"
        end = f"<!-- END GENERATED:{slug} -->"
        if begin not in changed or end not in changed:
            bad.append(f"{rel(page)}: missing generated markers for {slug}")
            continue
        pre, rest = changed.split(begin, 1)
        old, post = rest.split(end, 1)
        if old.strip() != expected.strip():
            if fix:
                changed = pre + begin + "\n" + expected + "\n" + end + post
            else:
                old_rows = {ln for ln in old.strip().splitlines() if ln.startswith("| ")}
                new_rows = {ln for ln in expected.strip().splitlines() if ln.startswith("| ")}
                missing = sorted(new_rows - old_rows)
                stale = sorted(old_rows - new_rows)
                bits = []
                if missing:
                    bits.append(f"{len(missing)} row(s) missing from the page")
                if stale:
                    bits.append(f"{len(stale)} row(s) in the page with no inventory row")
                detail = "\n".join(
                    [f"    expected: {ln}" for ln in missing[:3]]
                    + [f"    found:    {ln}" for ln in stale[:3]]
                )
                bad.append(
                    f"{rel(page)}: the generated {slug} table disagrees with "
                    f"WIRE_INVENTORY ({'; '.join(bits) or 'formatting'}). Run: "
                    f"./scripts/docs_check.py --fix-wire"
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


# ─────────────────── check 5: quoted test names ───────────────────

#: How many words make a backticked `snake_case` name read as a *test-name
#: citation*. This repo writes test names as sentences —
#: `a_ticket_that_is_never_closed_stops_the_loop_instead_of_spinning` — and a
#: lower-case snake identifier of four or more words is nothing else Rust has.
#: Shorter backticks are fields, knobs and helpers (`last_good_read`,
#: `retained_ceiling_bytes`, `blocking_send`) and are none of this check's
#: business. The threshold is a number rather than a feel because it is the
#: whole heuristic: at four words the tree's own pages cite no invented names,
#: and dropping to three would flag a dozen legitimate ones.
TEST_NAME_WORDS = 4

#: A backticked name: optionally path-qualified, all lower-case after the last
#: `::`, at least one underscore. Upper-case segments are types, and this
#: check does not read them — resolving a moved type is a different question
#: than resolving a renamed test, and guessing wrong costs a reader their trust
#: in the gate.
TEST_NAME_RE = re.compile(r"`((?:[A-Za-z_][A-Za-z0-9_]*::)*[a-z][a-z0-9_]*_[a-z0-9_]*)`")

#: Citations that read as test names and are **not** functions — a name written
#: to illustrate the naming rule, or the name of something the page is about
#: the *absence* of. Each entry is the reason the gate is wrong about it, in
#: the voice `wire.rs` uses for an `#[allow(dead_code)]` that needs a reason
#: beside it. The allowlist is rot-checked in the other direction too: an
#: entry whose citation has disappeared fails, so the list cannot quietly turn
#: into a dump.
ILLUSTRATIVE_TEST_NAMES = {
    "follow_the_pane_down": (
        "docs/adr/0004-fullscreen-tui.md names the helpers the inline-viewport "
        "rewrite deleted; the page's point is that they are gone (it counts "
        "them at 0 occurrences), so the citation is of a decision, not of a "
        "function"
    ),
}

#: Where a quoted test name is allowed to live: everything in the tree that can
#: hold a test, plus the spike drivers, which the docs cite by function name.
DEF_SITES = ("src", "tests", "examples", "spikes", "scripts")
DEF_SUFFIXES = {".rs", ".py"}
FN_DEF_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)", re.M
)
PY_DEF_RE = re.compile(r"^\s*def\s+([a-z_][a-z0-9_]*)", re.M)


def defined_names() -> dict[str, list[str]]:
    """Every function name the tree defines, with the places it is defined.

    Rust `fn` and Python `def` only — a test is a function, and this check is
    about the names of tests. Cached on the function because every page of the
    site asks.
    """
    cached = getattr(defined_names, "_cache", None)
    if cached is not None:
        return cached
    out: dict[str, list[str]] = {}
    for base_name in DEF_SITES:
        base = ROOT / base_name
        if not base.exists():
            continue
        for path in sorted(base.rglob("*")):
            if not path.is_file() or path.suffix not in DEF_SUFFIXES:
                continue
            if "__pycache__" in path.parts:
                continue
            text = path.read_text(encoding="utf-8", errors="ignore")
            rx = FN_DEF_RE if path.suffix == ".rs" else PY_DEF_RE
            for lineno, line in enumerate(text.splitlines(), start=1):
                for m in rx.finditer(line):
                    out.setdefault(m.group(1), []).append(f"{rel(path)}:{lineno}")
    defined_names._cache = out  # type: ignore[attr-defined]
    return out


def check_test_names(pages) -> tuple[list[str], int]:
    """Every test-shaped name quoted in a page exists as a function in the tree.

    Reports the page, the line and the unresolved name, plus the nearest name
    that *does* exist when there is one: the commonest way this fails is a
    test that got renamed and a page that was never told, so the nearest match
    is usually the answer, and pointing at it turns a ten-minute grep into a
    ten-second one.

    Returns `(violations, citations_checked)`. The count is printed on a clean
    run so "the gate checks the quoted test names" stays a number rather than
    becoming a claim.
    """
    defs = defined_names()
    bad: list[str] = []
    checked = 0
    cited_illustrative: set[str] = set()

    for page in pages:
        text = page.read_text(encoding="utf-8")
        in_fence = False
        for lineno, line in enumerate(text.splitlines(), start=1):
            if line.lstrip().startswith("```"):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            for quoted in TEST_NAME_RE.findall(line):
                leaf = quoted.rsplit("::", 1)[-1]
                if leaf.count("_") + 1 < TEST_NAME_WORDS:
                    continue
                checked += 1
                if leaf in defs:
                    continue
                if leaf in ILLUSTRATIVE_TEST_NAMES:
                    cited_illustrative.add(leaf)
                    continue
                near = difflib.get_close_matches(leaf, defs, n=1, cutoff=0.62)
                hint = ""
                if near:
                    hit = near[0]
                    hint = (
                        f" — the closest name that does exist is `{hit}` "
                        f"({', '.join(defs[hit][:2])})"
                    )
                bad.append(
                    f"{rel(page)}:{lineno}: `{quoted}` reads like a test name "
                    f"(at least {TEST_NAME_WORDS} words) but no `fn {leaf}` exists "
                    f"in {'/'.join(DEF_SITES)}{hint}"
                )

    for name, why in sorted(ILLUSTRATIVE_TEST_NAMES.items()):
        if name not in cited_illustrative:
            bad.append(
                f"scripts/docs_check.py: ILLUSTRATIVE_TEST_NAMES excuses `{name}`, "
                f"which no page quotes any more — drop the entry (it was excused "
                f"as: {why})"
            )
    return bad, checked


# ─────────────────── check 6: the spike index ───────────────────

#: What counts as a "driver" under `spikes/`: a program you run to measure
#: something. A fixture the drivers *use* (`fake_pi_slow.py`) counts too — it
#: lives under `spikes/`, a reader wondering "what is this?" deserves an answer
#: for it as much as for a measurement, and the index already carries a row for
#: the one that exists. `--list-spikes` prints the whole set so the gate's view
#: can be checked by hand.
SPIKE_DRIVER_SUFFIXES = {".py", ".sh"}


def spike_drivers() -> list[Path]:
    """Every driver under `spikes/`, at any depth, minus the evidence and the cache.

    Recursive like the orphan check it mirrors: a driver filed into
    `spikes/subdir/` is exactly as unfindable as one at the top level, and a
    non-recursive check would just teach people to add a directory. `results/` is
    the committed output, not a driver, and `__pycache__` is the drivers' own
    bytecode (`spikes/*.py` import each other).
    """
    if not SPIKES.is_dir():
        return []
    out = []
    for path in SPIKES.rglob("*"):
        if not path.is_file() or path.suffix not in SPIKE_DRIVER_SUFFIXES:
            continue
        if "results" in path.relative_to(SPIKES).parts:
            continue
        if "__pycache__" in path.parts or path.name.startswith((".", "_")):
            continue
        out.append(path)
    return sorted(out)


def spike_index_entries() -> list[tuple[str, int]]:
    """`(named_path, line_no)` for the first column of every row of the index.

    A **row** counts and a prose mention does not. The failure this check is
    about is a reader who cannot find out *what measures X*; a driver name
    inside a shell snippet in "Running it" does not answer that, and neither
    does one in a paragraph. The table is the index, so the table is what gets
    checked — which is also what keeps the check cheap and exact: no fuzzy
    matching, no "did the author mention it somewhere" heuristic.

    The named thing is the first backticked token in the cell, or the bare cell
    text if there is no code span; a link in the cell (`[`x`](y)`) is unwrapped
    first so a linked row still names the file it points at.
    """
    if not SPIKE_INDEX.is_file():
        return []
    text = SPIKE_INDEX.read_text(encoding="utf-8")
    out: list[tuple[str, int]] = []
    for _header, lineno, cells in md_tables(text):
        if not cells:
            continue
        cell = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", cells[0])
        codes = re.findall(r"`([^`]+)`", cell)
        named = codes[0] if codes else cell
        named = named.strip().strip("`*").strip()
        if named:
            out.append((named, lineno))
    return out


def check_size_rows(pages) -> tuple[list[str], int]:
    """A row that includes another row is never smaller than it.

    The failure this closes is arithmetic, not prose: ADR-0008's corpus table said
    the site serves 7,071 lines of Markdown under `docs/` and 7,056 lines
    *including* `spikes/`. Adding a directory cannot subtract fifteen lines. Each
    number had been typed from a different moment in the day, and the table — the
    one place a reader goes to be told a size — printed an impossibility that was
    checkable by eye and checked by nothing.

    The rule is deliberately narrow and needs no filesystem, so it stays true as
    the corpus grows and never needs a baseline:

    * a row counts as **inclusive** when its label says so (`including`, `plus`,
      `with X included`, `total`), and its *headline* size is the first line count
      in its own size cell — the number the reader takes away;
    * the **parent** is the nearest preceding row of the same table with a line
      count in the same column, which is what "the row it includes" means in every
      table this repo writes;
    * the inclusive headline must be `>=` the parent's, and any difference quoted
      in the inclusive row ("a 377-line difference") must equal `inclusive -
      parent` exactly — a delta typed next to two totals is a third number that can
      disagree with both.

    Returns `(violations, pairs_checked)`.
    """
    bad: list[str] = []
    checked = 0
    for page in pages:
        text = page.read_text(encoding="utf-8")
        # column index -> (line_no, headline size, label) for the nearest row above
        # **in the same table**. `md_tables` hands back the same header object for
        # every row of one table, so a change of header is the table boundary; a
        # page-scoped parent would let a total in one table get compared against a
        # row in the next one, which is a false positive waiting for a second table.
        above: dict[int, tuple[int, int, str]] = {}
        table_key = None
        for header, lineno, cells in md_tables(text):
            if not cells:
                continue
            if tuple(header) != table_key:
                table_key = tuple(header)
                above = {}
            label = strip_md(cells[0])
            inclusive = bool(INCLUSIVE_LABEL_RE.search(label))
            for idx, cell in enumerate(cells):
                plain = strip_md(cell)
                deltas = [int(m.group(1).replace(",", "")) for m in DELTA_LINE_RE.finditer(plain)]
                counts = [
                    int(m.group(1).replace(",", ""))
                    for m in LINE_COUNT_RE.finditer(DELTA_LINE_RE.sub(" ", plain))
                ]
                if not counts:
                    continue
                headline = counts[0]
                parent = above.get(idx)
                if inclusive and parent is not None:
                    checked += 1
                    p_line, p_size, p_label = parent
                    if headline < p_size:
                        bad.append(
                            f"{rel(page)}:{lineno}: {cells[0]!r} includes the row "
                            f"at line {p_line} ({p_label!r}) but quotes a smaller "
                            f"size — {headline:,} lines vs {p_size:,}. A superset "
                            f"cannot be smaller than its own part; the two "
                            f"numbers were measured at different times. "
                            f"Re-measure with `./scripts/docs_check.py "
                            f"--list-corpus` instead of re-typing one of them"
                        )
                    for d in deltas:
                        diff = headline - p_size
                        if d != diff:
                            bad.append(
                                f"{rel(page)}:{lineno}: {cells[0]!r} quotes a "
                                f"{d:,}-line difference from the row at line "
                                f"{p_line}, but {headline:,} - {p_size:,} = "
                                f"{diff:,}. A difference stated beside two totals "
                                f"is a third number that has to reconcile with "
                                f"both"
                            )
                above[idx] = (lineno, headline, label)
    return bad, checked


def corpus_sizes() -> list[tuple[str, str, int]]:
    """Today's corpus counts: `(what, the command that says so, lines)`.

    These are printed rather than asserted: the corpus grows on every page, so a
    count is a dated measurement, not a property of the tree. What is asserted is
    that the pair *reconciles* — see `check_size_rows`.
    """
    def counted(cmd: str) -> int:
        # Run the command that is printed beside the number, so the number in the
        # page is the number the command prints rather than a parallel
        # implementation of the same idea.
        out = subprocess.run(
            cmd, shell=True, cwd=ROOT, capture_output=True, text=True, check=False
        ).stdout.strip()
        m = re.search(r"(\d[\d,]*)\s+total\Z", out)
        if not m:
            raise SystemExit(f"corpus_sizes: `{cmd}` printed no total: {out!r}")
        return int(m.group(1).replace(",", ""))

    docs_cmd = "find docs -name '*.md' -not -path 'docs/_site/*' | xargs wc -l | tail -1"
    both_cmd = "find docs spikes -name '*.md' -not -path 'docs/_site/*' | xargs wc -l | tail -1"
    spikes_cmd = "find spikes -name '*.md' | xargs wc -l | tail -1"
    docs_lines, both_lines, spikes_lines = (counted(c) for c in (docs_cmd, both_cmd, spikes_cmd))
    return [
        ("Markdown under docs/", docs_cmd, docs_lines),
        ("…including spikes/", both_cmd, both_lines),
        ("spikes/ on its own", spikes_cmd, spikes_lines),
    ]


def check_spike_index() -> tuple[list[str], int]:
    """Every driver under `spikes/` is indexed, and the index names no ghost.

    Both halves are reported because they fail at different times and get missed
    for different reasons. The missing row is missed on the day the driver lands
    — or, as it happened here, three drivers and forty tickets later, because the
    file grew from one ticket's harness into the repo-wide index while the table
    stayed where the first ticket left it. The ghost row is found by whoever
    follows a docs page into `spikes/README.md` looking for the measurement that
    answers their question and is sent to a file that is not there.

    The message points at the row that has to be written rather than at the
    rule: the rule is obvious once the file is open and useless as an
    abstraction.

    Returns `(violations, drivers_checked)`.
    """
    drivers = spike_drivers()
    if not SPIKE_INDEX.is_file():
        return (
            [
                f"{rel(SPIKE_INDEX)}: missing — the harness has no index, and "
                f"{len(drivers)} driver(s) under spikes/ are undocumented"
            ],
            len(drivers),
        )

    entries = spike_index_entries()
    indexed: set[str] = set()
    bad: list[str] = []

    for named, lineno in entries:
        path = named.rstrip("/")
        if path.startswith("spikes/"):
            target = ROOT / path
            if not target.exists():
                bad.append(
                    f"{rel(SPIKE_INDEX)}:{lineno}: the index names `{path}`, which does "
                    f"not exist — a reader following a docs page here is sent to a file "
                    f"that isn't there"
                )
            if target.suffix in SPIKE_DRIVER_SUFFIXES and target.is_file():
                indexed.add(target.name)
            continue
        # A bare file name in the first column still indexes the driver it names.
        for drv in drivers:
            if path in {drv.name, drv.stem}:
                indexed.add(drv.name)

    for drv in drivers:
        if drv.name in indexed:
            continue
        bad.append(
            f"{rel(drv)}: no row in {rel(SPIKE_INDEX)} — the declared index of the "
            f"measurement harness is incomplete. Add a row: the ticket it answers, "
            f"what it proves, and the committed log under "
            f"{rel(RESULTS)}/ (or a note saying why there is no capture). An "
            f"unindexed spike is invisible to the reader who arrives looking for "
            f"'what measures this' — the same failure as an orphan page, "
            f"arriving later"
        )

    # The other half of the harness's index — the Rust examples the spikes run —
    # is deliberately not checked here. `examples/*.rs` are compiled by cargo on
    # every gate run, so a deleted one stops the build on its own; a Python driver
    # under `spikes/` is nothing's compile unit, which is why the index is the only
    # thing that can notice it.
    return bad, len(drivers)


# ──────────────────────────────── main ────────────────────────────────


def rel(p: Path) -> str:
    try:
        return str(p.relative_to(ROOT))
    except ValueError:
        return str(p)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--fix-keymap", action="store_true", help="regenerate the keymap tables")
    ap.add_argument(
        "--fix-wire", action="store_true",
        help="regenerate the wire protocol tables in docs/guide/wire-protocol.md",
    )
    ap.add_argument(
        "--list-wire", action="store_true",
        help="print the wire inventory and every unread wire record with its stated reason",
    )
    ap.add_argument("--list-knobs", action="store_true", help="print the knobs the code reads")
    ap.add_argument(
        "--list-captures", action="store_true",
        help="print every committed spike capture, its total, and which spike it belongs to",
    )
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument(
        "--list-test-names", action="store_true",
        help="print every test-shaped name the docs quote, where it resolves, and what is excused",
    )
    ap.add_argument(
        "--list-spikes",
        action="store_true",
        help="print every driver under spikes/, whether the index carries it, and its captures",
    )
    ap.add_argument(
        "--list-corpus",
        action="store_true",
        help="print today's corpus line counts with the command that produces each one",
    )
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

    if args.list_wire:
        for group in WIRE_MARKER_GROUPS:
            print(f"{group}")
            for r in [x for x in wire_inventory() if x["group"] == group]:
                who = r["reader"] or f"nothing today, waiting on: {r['waiting_on']}"
                print(f"  {r['wire']:20s} {r['variant']:18s} {who}")
        print("unread records in wire.rs (allow + reason)")
        for r in wire_allow_reasons():
            print(f"  {r['line']:>5d}  {r['item']:34s} {r['reason']}")
        return 0

    if args.list_test_names:
        defs = defined_names()
        rows = set()
        for page in corpus_pages():
            text = page.read_text(encoding="utf-8")
            in_fence = False
            for lineno, line in enumerate(text.splitlines(), start=1):
                if line.lstrip().startswith("```"):
                    in_fence = not in_fence
                    continue
                if in_fence:
                    continue
                for quoted in TEST_NAME_RE.findall(line):
                    leaf = quoted.rsplit("::", 1)[-1]
                    if leaf.count("_") + 1 < TEST_NAME_WORDS:
                        continue
                    if leaf in defs:
                        status = ", ".join(defs[leaf])[:64]
                    elif leaf in ILLUSTRATIVE_TEST_NAMES:
                        status = "EXCUSED (illustrative)"
                    else:
                        status = "UNRESOLVED"
                    rows.add((leaf, f"{rel(page)}:{lineno}", status))
        for leaf, where, status in sorted(rows):
            print(f"{leaf:62s} {where:34s} {status}")
        return 0

    if args.list_spikes:
        recs = capture_records()
        bad, _ = check_spike_index()
        unindexed = {line.split(":", 1)[0] for line in bad if "no row in" in line}
        for drv in spike_drivers():
            mine = sorted(n for n, r in recs.items() if r["spike"] == drv.name)
            state = "NOT INDEXED" if rel(drv) in unindexed else "indexed"
            cur = current_capture(recs, drv.name)
            caps = ", ".join(
                f"{n}{' <- current' if cur is not None and cur['path'].name == n else ''}"
                for n in mine
            )
            print(f"{rel(drv):32s} {state:12s} {caps or 'no capture committed'}")
        return 0

    if args.list_corpus:
        for what, cmd, lines in corpus_sizes():
            print(f"{what:26s} {lines:>7,d} lines  {cmd}")
        return 0

    pages = corpus_pages()
    violations: list[str] = []
    violations += check_links(pages)
    violations += check_orphans(pages)
    violations += check_knobs()
    violations += check_keymap(fix=args.fix_keymap)
    violations += check_wire(fix=args.fix_wire)
    measurement_violations, claims_checked = check_measurement_claims(pages)
    violations += measurement_violations
    name_violations, names_checked = check_test_names(pages)
    violations += name_violations
    index_violations, drivers_checked = check_spike_index()
    violations += index_violations
    size_violations, size_pairs_checked = check_size_rows(pages)
    violations += size_violations

    if (args.fix_keymap or args.fix_wire) and not violations:
        print("docs_check: regenerated the generated tables; no other violations")
        return 0

    if violations:
        print(f"docs_check: {len(violations)} violation(s)")
        for v in violations:
            print(f"  {v}")
        print()
        print("  These are docs-rot failures: a link that lands nowhere, a knob the")
        print("  reference does not list, a page nothing links to, a table that has")
        print("  drifted from the code it claims to describe, or a spike under")
        print("  spikes/ that the harness's own index does not carry.")
        return 1

    if not args.quiet:
        print(
            f"docs_check: clean ({len(pages)} page(s), "
            f"{len(code_knobs())} knob(s), {len(parse_chord_table())} chord row(s), "
            f"{len(wire_inventory())} wire value(s), {claims_checked} measurement "
            f"claim(s) backed by spikes/results/, {names_checked} quoted test "
            f"name(s) resolved to a function in the tree, {drivers_checked} "
            f"driver(s) under spikes/ all indexed in spikes/README.md, "
            f"{size_pairs_checked} inclusive size row(s) reconciled against their "
            f"parent row)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
