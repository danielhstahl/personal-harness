#!/usr/bin/env python3
"""dead_audit.py — who is still holding a `#[allow(dead_code)]`, and does the
reason written next to it still hold?

`cargo clippy -- --force-warn=dead_code` forces the lint to report even through
`#[allow(dead_code)]`, so the compiler itself answers "is this item actually
dead?" without editing the tree. The JSON carries a span for every item named in
the warning, including the ones the rendered text folds together.

Every allow in src/ is then classified:

  redundant  — rustc reports nothing inside the guarded item: the code is live
               and the attribute silences nothing (it should simply go away)
  dead       — rustc reports the item; the reason in the comment has to be true

The reason is prose, so the second half is a human's job; this prints it next to
the verdict so the question is answerable without re-reading the whole crate.

  ./scripts/dead_audit.py                       # full report
  ./scripts/dead_audit.py --gate                # quiet: fail on a redundant allow
  ./scripts/dead_audit.py --json out.json       # machine-readable

Exit status is 1 when any allow is redundant, 0 otherwise. Whether a *dead* item's
reason is still true is a question about prose, not about the call graph, so the
gate does not try to answer it — it prints the reason next to the verdict so a
human can, which is the thing that went stale in looprs-2nd.
"""
import json, os, subprocess, sys

# Anchored to the crate, not to the git root: this file lives in <crate>/scripts.
CRATE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(CRATE)


def reason_on(lines, i):
    """The reason a human wrote next to the attribute: the trailing comment, or
    the `//` line directly under it — which is where rustfmt puts a trailing
    comment once it stops fitting the width — or the `//` line directly above
    it. Empty means there is none — which the module doc in `src/session/mod.rs`
    calls "a warning we deleted rather than answered".

    Reading only the attribute line, as this used to, reported a reason-less
    allow for every attribute whose comment `cargo fmt` had pushed onto its own
    line — which is most of them, and made this function's own docstring a
    claim about a tool that did not do what it said.
    """
    tail = lines[i].split("dead_code)", 1)[-1].strip()
    if tail.startswith("]"):
        tail = tail[1:].strip()
    if tail.startswith("//"):
        return tail[2:].strip()
    if i + 1 < len(lines) and lines[i + 1].strip().startswith("//"):
        return lines[i + 1].strip()[2:].strip()
    if i > 0 and lines[i - 1].strip().startswith("//"):
        return lines[i - 1].strip()[2:].strip()
    return ""


def src_files():
    out = []
    for dp, _, fs in os.walk("src"):
        for f in fs:
            if f.endswith(".rs"):
                out.append(os.path.join(dp, f))
    return sorted(out)


def run_clippy():
    """file -> set of line_start that rustc calls dead.

    `--all-targets` matters: a `#[cfg(test)]` module (`src/testing.rs`,
    `src/measure.rs`) is absent from the binary build, so the binary's pass
    reports nothing for it and every allow inside would read as "live".
    With every target compiled, an item is called dead only if **no** target
    uses it, which is the question the attribute is answering.
    """
    proc = subprocess.run(
        [
            "cargo",
            "clippy",
            "--all-targets",
            "--message-format=json",
            "--",
            "--force-warn=dead_code",
        ],
        capture_output=True,
        text=True,
    )
    dead = {}
    for line in proc.stdout.split("\n"):
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        msg = obj.get("message") or {}
        code = msg.get("code")
        code = code.get("code") if isinstance(code, dict) else code
        if code != "dead_code":
            continue
        for sp in msg.get("spans", []):
            f = sp.get("file_name", "")
            # Primary spans only: the secondary ones point at the enclosing item
            # and at the lint's "remove this field" suggestions, neither of which
            # answers "is *this* item dead?".
            if f.startswith("src/") and sp.get("is_primary"):
                dead.setdefault(f, set()).add(sp["line_start"])
    return dead


def item_start(lines, i):
    j = i + 1
    while j < len(lines):
        s = lines[j].strip()
        if s.startswith("//") or s.startswith("#[allow"):
            j += 1
            continue
        return j
    return None


def extent(lines, start):
    """Last line of the item that begins at `start` (brace-matched; a one-line
    item or a bare field returns its own line)."""
    d, k, seen = 0, start, False
    while k < len(lines):
        d += lines[k].count("{") - lines[k].count("}")
        if "{" in lines[k]:
            seen = True
        if seen and d <= 0:
            return k
        k += 1
    return start


def main():
    as_json = None
    if "--json" in sys.argv:
        as_json = sys.argv[sys.argv.index("--json") + 1]
    dead = run_clippy()
    rows = []
    for p in src_files():
        lines = open(p).read().split("\n")
        for i, l in enumerate(lines):
            if not l.lstrip().startswith("#[allow(dead_code)]"):
                continue
            st = item_start(lines, i)
            if st is None:
                continue
            end = extent(lines, st)
            dead_here = sorted(x for x in dead.get(p, set()) if st <= x <= end)
            rows.append({
                "file": p, "attr": i + 1, "item_line": st + 1,
                "item": lines[st].strip()[:80],
                "dead_lines": dead_here,
                "state": "dead" if dead_here else "redundant",
                "why": reason_on(lines, i),
            })
    if as_json:
        json.dump(rows, open(as_json, "w"), indent=1)
    red = [r for r in rows if r["state"] == "redundant"]
    need = [r for r in rows if r["state"] == "dead"]
    print(f"{len(rows)} `#[allow(dead_code)]` in src/ -> "
          f"{len(need)} guarding dead code, {len(red)} redundant")
    if red:
        print("\nREDUNDANT — the item is live; the attribute silences nothing, so it goes")
        for r in red:
            print(f"  {r['file']}:{r['attr']}  {r['item']}")
            if r["why"]:
                print(f"      reason written next to it: {r['why'][:150]}")
    if "--gate" not in sys.argv:
        print("\nDEAD — the attribute is load-bearing; is the reason above still true?")
        for r in need:
            print(f"  {r['file']}:{r['attr']}  {r['item'][:70]}")
            print(f"      reason written next to it: {r['why'][:150]}")
    return 1 if red else 0


sys.exit(main())
