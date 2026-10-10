#!/usr/bin/env python3
"""What the built docs site does to a phone (the mobile squish).

The report was: the site looks right on a desktop and is *squished* on a phone,
with too much white space on the right. Both halves of that sentence are a layout
bug rather than taste, and both were measurable the moment the site could be
measured — which is all this driver is: load every built page at four widths and
assert what the frame has to keep doing.

It exists because the two bugs it found were invisible to every check this repo
already had. `scripts/docs_check.py` proves the prose is true; nothing proved the
rendered page was *shaped* right, and neither failure was a prose failure:

  1. **A grid area named but not defined.** The narrow media query retiled the
     page to one column and named its areas `top`/`main`, while `.sidebar` still
     asked for `side`. CSS does not reject that: it invents a track for the ghost
     area and lays the header in one column beside it. At 390px the body's tracks
     measured `348px 0px 42px` — the header stopped 42px short of the right edge
     (the "white space"), the whole page tree was crushed into that 42px, and the
     article was squeezed to 348px (the "squish").
  2. **A markdown parser with no extensions turned on.** `Parser::new` means *no*
     extensions, so every pipe table in the corpus — 1,024 rows across the ADRs,
     the keymap, the wire protocol and the configuration reference — rendered as a
     paragraph of literal `| … |` text. On a desktop that reads as a mess; on a
     phone the longest rows made the *document* wider than the screen
     (`guide/keymap.html` measured 581px of scroll on a 390px viewport).

Neither one fails a build, a linter, or a link checker. One is now refused at
build time (`render_page_markdown` in the generator refuses a page whose pipe
syntax survived), one is caught statically by `check_layout_css` in
`scripts/docs_check.py`, and this driver is the end-to-end proof over the pages a
reader actually opens — which is the only place a grid area can be wrong.

Playwright and a Chromium are needed, and neither is part of `./scripts/check.sh`:
the gate has to stay runnable offline with nothing installed (ADR-0008's rule).
This is a spike. Run it by hand after touching the stylesheet or the page
templates, and commit the capture under `spikes/results/` if a page cites it.

    ./scripts/docs.sh build
    python3 spikes/docs_layout.py                          # every page, four widths
    python3 spikes/docs_layout.py --widths 390             # one width
    python3 spikes/docs_layout.py --site /tmp/site-before  # measure a control build

On a machine where Chromium cannot start (no system libraries), the static half of
these same rules is `check_layout_css`, which needs nothing but python3.

Exits 0 when every check passes, 1 when one fails, 2 when the browser is missing.
"""

import argparse
import sys
import time
from pathlib import Path

here = Path(__file__).resolve().parent
root = here.parent

# The widths are not decorations. 360 and 390 are the phone widths a reader has
# (390 is where the frame changes from a sidebar to a folded tree); 768 is the
# tablet width *inside* the narrow layout, where a grid area named but not
# defined shows up just the same; 1440 is the desktop the site was already right
# at, kept as a regression guard so a fix for phones cannot break what worked.
DEFAULT_WIDTHS = (360, 390, 768, 1440)

#: The media query's breakpoint, duplicated from the stylesheet on purpose: this
#: driver asks what the *browser* resolved, so it has to say which side of the
#: breakpoint each measurement belongs on rather than trust the sheet.
NARROW_BREAKPOINT = 900

#: The two widths the interactive checks drive. They set the viewport themselves
#: rather than borrowing one from `--widths`, so `--widths 390` still answers the
#: desktop questions instead of skipping them and looking clean.
DESKTOP_W = 1440
PHONE_W = 390

CHECKS = []
_t0 = time.time()


def check(name):
    """Collect one named check; the summary line counts these."""

    def deco(fn):
        CHECKS.append((name, fn))
        return fn

    return deco


class Result:
    def __init__(self):
        self.ok = False
        self.detail = ""

    def done(self, ok, detail=""):
        self.ok, self.detail = bool(ok), detail
        return self


def say(msg: str) -> None:
    print(f"[{time.time() - _t0:6.2f}s] {msg}", flush=True)


def probe(width: int) -> str:
    """One evaluation per page: everything this driver needs about one frame.

    A template with `__WIDTH__` substituted rather than an f-string: the body is
    JavaScript full of braces, and every one of them has to be doubled inside an
    f-string — which is a class of syntax error that only shows up when the
    browser is already installed and the page already loaded.
    """
    return PROBE_JS.replace("__WIDTH__", str(width))


PROBE_JS = r"""() => {
  const de = document.scrollingElement;
  const tracks = (getComputedStyle(document.body).gridTemplateColumns || '')
      .split(' ').map((s) => Math.round(parseFloat(s))).filter((n) => !Number.isNaN(n));
  const rect = (sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: Math.round(r.x), right: Math.round(r.right), top: Math.round(r.top),
             bottom: Math.round(r.bottom), w: Math.round(r.width), h: Math.round(r.height) };
  };

  // Markdown that survived as syntax: a direct text node whose line still looks
  // like a table row. Read from the DOM, not the HTML source, because the DOM is
  // what the reader reads - and because pre/code, where ASCII figures and shell
  // greps live and are allowed to look like anything, drop out by construction.
  const stray = [];
  for (const el of document.querySelectorAll('article *, main *')) {
    if (el.closest('pre, code')) continue;
    for (const n of el.childNodes) {
      if (n.nodeType !== Node.TEXT_NODE) continue;
      for (const line of n.textContent.split('\n')) {
        const t = line.trim();
        if (t.startsWith('|') && (t.match(/\|/g) || []).length >= 3)
          stray.push(el.tagName + ' ' + t.slice(0, 44));
      }
    }
  }

  const tables = [...document.querySelectorAll('article table')];
  const widest = tables.reduce((a, t) => (t.scrollWidth > (a ? a.scrollWidth : 0) ? t : a), null);
  const heading = document.querySelector('article h1[id], article h2[id]');

  return {
    win: __WIDTH__,
    // clientWidth, not the nominal viewport width: a page that scrolls vertically
    // loses the scrollbar's 8-15px from its frame, and a header that spans the
    // whole frame is then "short of the right edge" by exactly the scrollbar.
    client: de.clientWidth,
    doc: de.scrollWidth,
    tracks,
    topbar: rect('.topbar'), sidebar: rect('.sidebar'), main: rect('main'),
    article: rect('article'),
    // A carried repository file has no place in the tree, so it is one column on
    // purpose; the two-column assertions are about pages that *have* a tree.
    hasTree: !!document.querySelector('.sidebar') &&
             document.querySelector('.sidebar').getBoundingClientRect().width > 0,
    tables: tables.length,
    tableRows: tables.reduce((n, t) => n + t.querySelectorAll('tr').length, 0),
    widestTableScroll: widest ? widest.scrollWidth : 0,
    widestTableRight: widest ? Math.round(widest.getBoundingClientRect().right) : 0,
    strayPipes: stray.slice(0, 3),
    escapedText: /\\u\{[0-9a-fA-F]+\}/.test(document.body.innerText),
    anchorMargin: heading ? parseFloat(getComputedStyle(heading).scrollMarginTop) : -1,
  };
}"""


#: Fold behaviour, measured with a real click rather than by reading the
#: stylesheet: a fold nobody can open with a thumb is not a fold.
FOLD_JS = r"""() => {
  const shown = () => {
    const ul = document.querySelector('ul.nav');
    return !!ul && getComputedStyle(ul).display !== 'none';
  };
  const label = document.querySelector('.navtoggle-label');
  return {
    hasFold: !!document.querySelector('#nav-toggle'),
    folded: !shown(),
    label: label ? label.textContent.replace(/\s+/g, ' ').trim() : '',
    links: document.querySelectorAll('ul.nav a').length,
  };
}"""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--site", default=None, help="built site dir (default: docs/_site)")
    ap.add_argument("--widths", default=",".join(str(w) for w in DEFAULT_WIDTHS))
    args = ap.parse_args()

    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print(
            "docs_layout: needs playwright and a Chromium, and deliberately does not "
            "install them (a spike, not a gate — scripts/check.sh must run with nothing "
            "installed).\n"
            "    pip install playwright && python3 -m playwright install --only-shell chromium\n"
            "The static half of these rules — check_layout_css in scripts/docs_check.py "
            "— needs nothing.",
            file=sys.stderr,
        )
        return 2

    site = Path(args.site) if args.site else root / "docs" / "_site"
    if not site.is_dir():
        sys.exit(f"docs_layout: no built site at {site}. Run ./scripts/docs.sh build first.")
    pages = sorted(p for p in site.rglob("*.html") if "__" not in p.name)
    if not pages:
        sys.exit(f"docs_layout: {site} holds no .html")
    widths = [int(w) for w in args.widths.split(",")]
    # No fallbacks: a check that quietly measured a width nobody asked for is a check
    # that reports PASS for a frame it never looked at. When one side of the
    # breakpoint is not in the list, the checks that need it say so.
    narrow = [w for w in widths if w <= NARROW_BREAKPOINT]
    wide = [w for w in widths if w > NARROW_BREAKPOINT]

    front = next((p for p in pages if p.parent == site), pages[0])
    keymap = site / "guide" / "keymap.html"
    config = site / "guide" / "configuration.html"
    source_page = next((p for p in pages if "_repo" in p.parts), None)

    say(f"site {site}")
    say(f"{len(pages)} page(s) × {len(widths)} width(s) = {len(pages) * len(widths)} frame(s)")

    frames = {}
    passed = 0
    with sync_playwright() as p:
        try:
            browser = p.chromium.launch()
        except Exception as e:  # noqa: BLE001 — missing shared libs is the usual case
            print(f"docs_layout: chromium would not start: {e}", file=sys.stderr)
            return 2

        pg = browser.new_page(viewport={"width": widths[0], "height": 800})
        for w in widths:
            pg.set_viewport_size({"width": w, "height": 800})
            for f in pages:
                pg.goto(f.as_uri())
                pg.wait_for_timeout(20)
                frames[(w, f)] = pg.evaluate(probe(w))
            say(f"measured {len(pages)} page(s) at {w}px")

        @check("no page scrolls wider than the viewport, at any width")
        def c_no_hscroll(r):
            bad = [(w, f, fr) for (w, f), fr in frames.items() if fr["doc"] > fr["client"] + 1]
            return r.done(
                not bad,
                f"{len(bad)} page(s) overflow"
                + (": " + ", ".join(f"{w}px {f.relative_to(site).as_posix()}={fr['doc']}px"
                                    for w, f, fr in bad[:3]) if bad else ""),
            )

        @check("the header spans the whole frame at every width")
        def c_header_span(r):
            bad = [(w, fr) for (w, _f), fr in frames.items()
                   if fr["topbar"] and fr["topbar"]["right"] < fr["client"] - 1]
            return r.done(
                not bad,
                f"{len(bad)} page(s) with a header short of the frame"
                + (f" (e.g. {bad[0][0]}px: frame {bad[0][1]['client']}px, header ends "
                   f"{bad[0][1]['topbar']['right']}px)" if bad else ""),
            )

        @check(f"the narrow layout resolves exactly one column (≤{NARROW_BREAKPOINT}px)")
        def c_narrow_tracks(r):
            bad = [fr for (w, _f), fr in frames.items()
                   if w <= NARROW_BREAKPOINT and len(fr["tracks"]) != 1]
            return r.done(
                not bad,
                f"{len(bad)} page(s) with a ghost track"
                + (f" (e.g. {' '.join(str(n) + 'px' for n in bad[0]['tracks'])})" if bad else ""),
            )

        @check(f"every page with a tree is two columns side by side (>{NARROW_BREAKPOINT}px)")
        def c_wide_tracks(r):
            bad = [fr for (w, _f), fr in frames.items()
                   if w > NARROW_BREAKPOINT and fr["hasTree"]
                   and (len(fr["tracks"]) != 2 or fr["sidebar"]["x"] >= fr["main"]["x"])]
            return r.done(not bad, f"{len(bad)} page(s) with a tree not side-by-side")

        @check("a page without a tree (a carried file) is one column, not an empty gutter")
        def c_no_gutter(r):
            bad = [fr for (w, _f), fr in frames.items()
                   if not fr["hasTree"] and (len(fr["tracks"]) != 1
                                             or fr["main"]["w"] != fr["client"])]
            return r.done(not bad, f"{len(bad)} carried page(s) with a ghost track or an empty gutter")

        @check("the article keeps a real measure: ≥80% of the viewport when narrow")
        def c_measure(r):
            bad = [(w, fr["article"]["w"]) for (w, _f), fr in frames.items()
                   if w <= NARROW_BREAKPOINT and fr["article"]
                   and fr["article"]["w"] < 0.8 * fr["client"]]
            return r.done(
                not bad,
                f"{len(bad)} page(s) narrower than 80% of the viewport"
                + (f" (e.g. {bad[0][0]}px → article {bad[0][1]}px)" if bad else ""),
            )

        @check("no page renders markdown pipe syntax as prose")
        def c_stray_pipes(r):
            bad = [(f, fr) for (_w, f), fr in frames.items() if fr["strayPipes"]]
            return r.done(
                not bad,
                f"{len(bad)} page(s) with unrendered table rows"
                + (f" (e.g. {bad[0][0].relative_to(site).as_posix()}: {bad[0][1]['strayPipes'][0]})"
                   if bad else ""),
            )

        @check("tables render as tables (the keymap page alone has three)")
        def c_tables(r):
            fr = next((frames[k] for k in frames if k[1] == keymap), None)
            if fr is None:
                return r.done(False, f"no {keymap} in the built site")
            return r.done(fr["tables"] >= 3 and fr["tableRows"] >= 40,
                          f"keymap: {fr['tables']} table(s), {fr['tableRows']} row(s)")

        @check("no Rust escape sequence reaches the reader")
        def c_escapes(r):
            bad = sorted({f.name for (_w, f), fr in frames.items() if fr["escapedText"]})
            return r.done(
                not bad,
                f"{len(bad)} page(s) printing a literal \\u{{…}}"
                + (f" (e.g. {', '.join(bad[:3])})" if bad else ""),
            )

        @check("a wide table scrolls inside the page instead of widening it")
        def c_table_scroll(r):
            w = 390 if 390 in widths else (narrow[0] if narrow else widths[0])
            fr = frames.get((w, config))
            if fr is None:
                return r.done(False, f"no {config} in the built site")
            inside = fr["widestTableRight"] <= fr["client"] + 1 and fr["doc"] <= fr["client"] + 1
            return r.done(inside,
                          f"widest table scrolls to {fr['widestTableScroll']}px, right edge "
                          f"{fr['widestTableRight']}px of {w}px, document {fr['doc']}px")

        @check("a carried source file gets no empty sidebar gutter")
        def c_source_gutter(r):
            w = 390 if 390 in widths else (narrow[0] if narrow else widths[0])
            if source_page is None:
                return r.done(False, "no carried repository file in the site")
            fr = frames[(w, source_page)]
            return r.done(fr["sidebar"]["w"] == 0 and fr["main"]["w"] == fr["client"],
                          f"{source_page.name}: sidebar {fr['sidebar']['w']}px wide, main "
                          f"{fr['main']['w']}px of {fr['win']}px")

        # The last three need interaction, so they drive one page each.
        @check("the sidebar sticks exactly under the header, not a guessed offset")
        def c_sticky(r):
            pg.set_viewport_size({"width": DESKTOP_W, "height": 800})
            pg.goto(front.as_uri())
            pg.wait_for_timeout(80)
            gap = pg.evaluate("""() => {
              scrollTo(0, 600);
              const t = document.querySelector('.topbar').getBoundingClientRect();
              const s = document.querySelector('.sidebar').getBoundingClientRect();
              return {headerBottom: Math.round(t.bottom), sidebarTop: Math.round(s.top),
                      headerH: Math.round(t.height)};
            }""")
            drift = gap["sidebarTop"] - gap["headerBottom"]
            return r.done(abs(drift) <= 1,
                          f"header {gap['headerH']}px tall, sidebar sticks {drift}px below it")

        @check("the tree is folded on a phone and a tap opens it")
        def c_fold(r):
            pg.set_viewport_size({"width": 390, "height": 800})
            pg.goto(front.as_uri())
            pg.wait_for_timeout(80)
            before = pg.evaluate(FOLD_JS)
            if not before["hasFold"]:
                return r.done(False, "no fold control in the page at all")
            pg.locator(".navtoggle-label").click()
            pg.wait_for_timeout(80)
            after = pg.evaluate(FOLD_JS)
            return r.done(before["folded"] and not after["folded"] and after["links"] > 10,
                          f"folded before tap={before['folded']}, open after tap="
                          f"{not after['folded']}, {after['links']} link(s)")

        @check("the fold names the page you are on (a folded tree is no map)")
        def c_fold_label(r):
            pg.set_viewport_size({"width": 390, "height": 800})
            pg.goto((site / "guide" / "keymap.html").as_uri())
            pg.wait_for_timeout(80)
            fold = pg.evaluate(FOLD_JS)
            return r.done("Keymap" in fold["label"], f"label reads {fold['label']!r}")

        @check("an anchor jump lands below the sticky header, not under it")
        def c_anchor(r):
            pg.set_viewport_size({"width": DESKTOP_W, "height": 800})
            pg.goto(front.as_uri())
            pg.wait_for_timeout(60)
            m = pg.evaluate("""() => {
              const h = document.querySelector('article h1[id], article h2[id]');
              return {margin: h ? parseFloat(getComputedStyle(h).scrollMarginTop) : -1,
                      header: Math.round(document.querySelector('.topbar').getBoundingClientRect().height)};
            }""")
            return r.done(m["margin"] >= m["header"],
                          f"scroll-margin-top {m['margin']}px vs a {m['header']}px header")

        for name, fn in CHECKS:
            res = fn(Result())
            passed += int(res.ok)
            say(f"{'PASS' if res.ok else 'FAIL'}  {name}" + (f" — {res.detail}" if res.detail else ""))
        browser.close()

    say(f"{passed}/{len(CHECKS)} checks passed")
    return 0 if passed == len(CHECKS) else 1


if __name__ == "__main__":
    sys.exit(main())
