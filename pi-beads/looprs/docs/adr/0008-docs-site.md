# ADR-0008: The documentation site — a generator nobody has to install, and nothing moved

- **ID:** looprs-00u.1
- **Status:** Accepted — 2026-10-09
- **Epic:** looprs-00u (Static docs site: what looprs does, how it differs, how to use it well)
- **Decides for:** looprs-00u.2 (the scaffold), .3–.9 (the content pages), .10 (the rot gate),
  .11 (publish + the top-level README) — and for every later contributor who wants to add a page
- **Establishes:** the ADR template the next ADR copies (§6), so the format stops drifting
- **Measured by:** the timings in §1 and §4 were taken on this working tree on 2026-10-09;
  the generator is
  [`docs/tools/gen/`](../../docs/tools/gen) and the driver is
  [`scripts/docs.sh`](../../scripts/docs.sh). Reproduce with `./scripts/docs.sh build`.

---

## Context

`looprs` has unusually good decision documentation — seven ADRs, a testing page, a
kanban page, a spike harness with committed evidence — and no on-ramp. There was no
top-level README, no "what is this", no tour. A newcomer landed in
`docs/README.md`, found a table of seven ADRs, and had to read a *rationale* before
being told what the program does.

The epic asks for a static book. Four questions had to be argued once rather than
decided implicitly by whoever scaffolded first, because each one is expensive to
reverse later: which generator, where the source lives, what gets committed, and how
it is published.

### The corpus being served (measured 2026-10-09)

| thing | size |
| --- | --- |
| Markdown under `docs/` (after this epic's pages) | **8,395 lines** as measured 2026-10-09 (this page's own first published figure was 7,071; 7,281 once the sweep's own pages had landed) |
| …including `spikes/` (which the site carries) | **8,772 lines** in that same measurement — the harness is the 377-line difference |
| How the first two rows are obtained | docs only: `find docs -name '*.md' -not -path 'docs/_site/*' \| xargs wc -l \| tail -1`. With the harness: `find docs spikes -name '*.md' -not -path 'docs/_site/*' \| xargs wc -l \| tail -1`. The harness alone: `find spikes -name '*.md' \| xargs wc -l \| tail -1`. All three at once: `./scripts/docs_check.py --list-corpus`. Re-run one of these rather than re-typing a number into this table |
| The pair that made this row a ticket | "7,071 / 7,056" — a superset 15 lines *smaller* than its own part. That is not a stale measurement, it is an impossible one: the two figures were typed at different times and neither reconciled with the other. The sweep corrected the pair and then closed the shape (`looprs-00u.27`): `docs_check`'s seventh check fails any "including X" size row that is smaller than the row it includes, so a transposed or mismatched pair cannot come back by way of a re-type |
| Markdown before this epic | 4,558 lines (`docs/`), 5,003 with `spikes/` |
| Distinct relative link targets outside `docs/` | `src/*.rs`, `spikes/*.py`, `spikes/results/*.log`, `examples/*.rs`, `tests/fixtures` |
| Existing cross-links that must keep working | `../../spikes/results/…`, `../src/session/mod.rs`, `adr/0007-kanban-board.md#1-the-status--column-mapping` |

The last row is the constraint that rules out the tidy option. Those links are
*outward* — from a page about a decision to the file and the captured log that prove
it. They are the most valuable thing in the corpus and the layout decision is the
one that either keeps them or breaks them.

---

## Q1 — Which generator?

Three candidates were priced: `mdbook`, a hand-rolled generator, and "no build at
all". The pricing criterion is not features. It is **what a contributor has to
install before they can preview a page they just edited**, and how long that takes on
this corpus.

| option | already on this machine? | install path | clean build | internal links | verdict |
| --- | --- | --- | --- | --- | --- |
| **`mdbook` (+ `mdbook-admonish`)** | **No** — `which mdbook` is empty | `brew install mdbook` or `cargo install mdbook`: needs network, pulls a fresh dependency tree, and must be re-run by every contributor and by the CI image | not measured — **not measured because the first criterion already decided it.** Installing a toolchain to discover a build time that a zero-install option does not have is the wrong experiment | survives only under mdbook's own layout rule (see Q2) | **rejected** |
| **hand-rolled over `pulldown-cmark` 0.13.4** | **Yes** — it is in `Cargo.lock` because [`src/utils/md.rs`](../../src/utils/md.rs) renders markdown for the transcript | nothing. It is already a dependency of the app | **4.7 s cold** from an empty target dir (`--release`, `--offline`, compiling `pulldown-cmark` itself from the local registry cache); **~0.7 s warm** for the corpus as it stood at the decision — 157 files, 35 pages + 122 carried (36 + 123 since the sweep added its page) / 3.9 MB | full control of path mapping and heading slugs (see below) | **chosen** |
| **"Markdown only on the git host"** | yes | nothing | n/a | *some*: host rendering keeps `docs/adr/*.md` links working but there is no book, no sidebar, no reading order, no per-page nav — and the outward links to `../../src/…` depend on the host's tree view | **rejected** |

Two details of the chosen option are load-bearing rather than incidental, and both
exist because the existing corpus needs them:

**Path mapping.** The generator mirrors the repo under `docs/_site/_repo/` **on
demand**: only files a page actually links to are carried in. That is why a link from
an ADR into `src/` — 2.3 MB of Rust — costs one file in the site instead of a copy
of the tree, and why the whole build is under a second. Relative links are rewritten
to be relative *within the site*, so it serves correctly from any prefix: a
subdirectory, GitHub Pages under `/looprs/`, or `file://` off disk. No base-URL
configuration exists to get wrong.

**Heading slugs match GitHub's rule.** `docs/adr/0007-kanban-board.md` is linked by
section — `#1-the-status--column-mapping` — from three places. GitHub's slug rule is
lowercase, spaces to `-`, drop everything else, and **do not collapse repeated
hyphens**, which is why the arrow in `## 1. The status → column mapping` leaves two
hyphens behind. The generator implements exactly that rule and `docs_check.py`
implements the same rule when it validates anchors. A generator that picked its own
slugs would silently turn every section link in the existing corpus into a link that
lands at the top of a page.

## Q2 — Where does the source live?

Two options:

| | option | cost |
| --- | --- | --- |
| **A** | `docs/` as the book root; every existing page keeps its exact path and gets listed in `docs/SUMMARY.md` | none |
| **B** | `docs/src/` as the mdbook default convention, with `docs/` content moved under it | **breaks every outward relative link in the corpus.** `docs/adr/0001-…md`'s `../src/session/mod.rs` becomes `../../src/…`; `../../spikes/results/…` becomes `../../../…`. Every one of those edits is a chance to break a link silently, and each ADR's provenance links are the ones you most want intact |

**Option A.** Nothing moved. `docs/adr/`, `docs/kanban.md`, `docs/testing.md` and
`spikes/README.md` are where they always were. The new pages are the only additions:
`docs/index.md` (the front door) and `docs/guide/*.md` (the tracks), which put them
at one directory level further from the repo root than `docs/adr/*` — so the guide
pages use `../../src/…` where the ADRs use `../src/…`. That asymmetry is written
down in the contributor guide rather than smoothed over, and the rot gate checks
every one of those links on every build, which is what makes the asymmetry safe.

**Rule this ADR sets: no page in this corpus moves without amending this file first.**
A move that turns out to be unavoidable is a reason to change the decision here, not a
reason to fix up links by hand in a commit nobody will review link by link.

## Q3 — What is committed and what is built?

**Committed: the Markdown, the generator, and the gate. Built: the HTML.**
`docs/_site/` is gitignored.

| | committed HTML | built HTML (chosen) |
| --- | --- | --- |
| readable without a toolchain | yes | **not quite** — `cargo` is needed to *build*, and the build is 0.7 s. The alternative was committing 3.9 MB / 157 files of generated output |
| diff noise on a prose edit | every prose edit becomes a two-file diff, and a rebase touching two pages produces a generated-file conflict that nobody can read | one file, one diff, reviewable as prose |
| can it lie? | **yes** — committed HTML can be stale relative to its own Markdown, and a reader cannot tell | no. The HTML is derived, and if the derivation is broken the build is broken |
| CI cost | none at read time | one job, ~5 s cold |

The tie-breaker is the third row. Committed HTML is an artefact that can disagree
with its source while looking authoritative, which is a worse failure than "run one
command". The build is fast enough (0.7 s warm, 4.7 s with an empty target directory)
that the command costs nothing, and the gate that proves the derivation works runs on
every `./scripts/check.sh`.

**What a reader without network sees:** the Markdown is readable as it always was —
every page is written to read correctly in a terminal and on the git host's Markdown
view, per the diagram policy in §5. The generated site is an improvement on the
reading experience, not a requirement for it.

## Q4 — How is it published?

**Decision: local-first, with a documented one-command publish path, and no
Pages deployment wired up in this ticket.**

| | decision | why |
| --- | --- | --- |
| Primary | `./scripts/docs.sh serve` → `http://localhost:3000` with live reload | The audience for these docs is a person with a checkout, and that person is 0.7 s from the site |
| Publish | `docs/_site/` is a static directory: publishing is "copy it somewhere static". A GitHub Pages job would be `docs.sh build` + `actions/upload-pages-artifact` on a `docs` push | **Not wired up here, deliberately.** The repo's CI today is `scripts/check.sh` (`.github/workflows/looprs-gate.yml`, path-filtered to `pi-beads/looprs/**`). Adding a publish job needs a Pages-enabled repo setting that cannot be verified from inside this ticket, and an unverified publish path is worse than a documented one — a docs edit that silently fails to publish is the "worse than not publishing" case the ticket named |
| Follow-up | filed as [looprs-00u.11](https://github.com/) and closed against this ADR: the README states the site is local-first, and the Pages job is a one-line change when the repo setting is confirmed | the honest version of "publish" available from inside this repo |

The path-filter question the ticket asked is answered: the existing filter is
`pi-beads/looprs/**`, which **does** cover `docs/**` and `spikes/**`, so docs-only
edits already run the gate. That is the part of "docs can change silently while the
published site goes stale" that this repo can actually control, and it is covered.

---

## 5. Diagram policy

**ASCII in Markdown. No renderer.** Three reasons, in order:

1. the existing corpus is ASCII-only and is read in terminals as well as browsers — a
   `│`-boxed band diagram in a fenced block reads correctly everywhere, and a
   Mermaid diagram reads as source code in 80% of the places these docs get read;
2. an ASCII diagram is diffable. A band layout change shows up as lines moving in a
   diff you can review; a rendered image does not;
3. it forces the diagram to stay small. A 1,200-line SVG is possible; a 20-line ASCII
   figure is a decision about what to include, which was the point of drawing it.

The one hard requirement: **ASCII figures go in fenced code blocks** (`` ```text ``),
so they are not re-flowed by the Markdown renderer and the rot gate's fence-awareness
skips link-scanning inside them.

## 6. The ADR template

Every ADR in this repo already looks roughly like this. It is written down here so
the eighth and the ninth are the same shape rather than approximately the same shape.

```markdown
# ADR-NNNN: <the decision, as a sentence, not a topic>

- **ID:** <bead id>
- **Status:** Accepted — YYYY-MM-DD        (or Proposed / Superseded / Amended)
- **Epic:** <epic id and its one-line goal>
- **Decides for:** <the tickets downstream of this one, by id>
- **Widens / Amends:** <any prior ADR's rule this extends, quoted>
- **Measured by:** <the spike script and the committed log that produced the
  numbers used below; if nothing was measured, say "not measured, because X">

## Context
<what is true, what is hard, and what the reader needs to weigh. Numbers here.>

## Decision
<numbered, each one actionable, each one citing the file it lands in>

## Consequences
<what this makes easy, what this makes hard, what it costs>

## What this forbids
<numbered prohibitions with the reason each exists — this is the part a later
editor is most likely to violate, so it is the part worth being explicit about>
```

Rules the template carries:

* **A claim with no measurement behind it is worse than no claim.** Either cite the
  spike/log or write "not measured, because X". The second is acceptable. Silence
  is not.
* **Amend, do not rewrite.** An amendment is a dated section that cites the ticket
  and states precisely which claim changed and which stand (see ADR-0007's amendment
  to §3's "one read per tick"). A rewritten ADR destroys the record of what was
  believed when the decision was made, which was the whole point of writing it.
* **Prohibitions are the deliverable.** ADR-0007's "ten things the band must not do"
  is the most-quoted part of that document. A decision that names no forbidden
  behaviour has not been stress-tested.

## 7. The tree

The full `SUMMARY.md` tree as accepted, with the ticket that owns each page:

| tree entry | file | owner ticket |
| --- | --- | --- |
| What looprs is | `docs/index.md` | looprs-00u.3 |
| How it differs | `docs/guide/differences.md` | looprs-00u.4 |
| **Use it effectively** | | |
| Your first session | `docs/guide/first-session.md` | looprs-00u.5a |
| Driving the Beads loop | `docs/guide/beads-loop.md` | looprs-00u.5b |
| Living with a transcript | `docs/guide/transcript.md` | looprs-00u.5c |
| **Reference** | | |
| Keymap and chords | `docs/guide/keymap.md` | looprs-00u.7 |
| Configuration reference | `docs/guide/configuration.md` | looprs-00u.6 |
| Files, logs and recovery | `docs/guide/operator.md` | looprs-00u.9 |
| The kanban band | `docs/kanban.md` | (existing, indexed) |
| How this documentation is organised | `docs/README.md` | looprs-00u.11 |
| The project README | `README.md` (repo front page, carried in via `SUMMARY.md`) | looprs-00u.11 |
| **Contributing** | | |
| Contributor guide | `docs/guide/contributing.md` | looprs-00u.8 |
| Testing looprs | `docs/testing.md` | (existing, indexed) |
| The measurement harness | `spikes/README.md` | (existing, indexed) |
| **Decision records** | `docs/adr/0001…0008` | (existing + this) |

Every entry maps to exactly one ticket in this epic or is an existing page indexed
without being rewritten. That mapping is not just a table in this file:
`docs_check.py`'s orphan check fails any `.md` under `docs/` that is not reachable
from `SUMMARY.md`, so a page cannot exist outside the tree. The same rule reaches
the harness now (`looprs-00u.26`): `spikes/README.md` is cited by
[Files, logs and recovery](../guide/operator.md) and
[Contributing](../guide/contributing.md) as *the* index of what each spike
measures, and it had drifted — three drivers existed with no row while two pages
linked to them by name. So a `*.py` or `*.sh` under `spikes/` with no row in that
index, and an index row pointing at a driver that has been deleted, both fail the
gate. An index that is declared authoritative and is allowed to drift is worse than
no index, because the reader trusts it first.

## Consequences

**What this makes cheap:**

* adding a page is `write docs/guide/whatever.md`, add it to `SUMMARY.md`, run the
  build. No plugin, no theme, no install;
* a page can link to a source file, a captured log and an ADR in one paragraph, and
  all three resolve in the built site. That is the property this whole design exists
  to preserve;
* the docs gate runs in the same `./scripts/check.sh` as clippy, so a dead link is
  as expensive as a lint warning — which is the only currency that actually works.

**What this makes expensive:**

* the generator is now *ours* — **1,136 lines of Rust** in
  [`docs/tools/gen/src/main.rs`](../../docs/tools/gen/src/main.rs), plus
  **519 lines** of gate in
  [`scripts/docs_check.py`](../../scripts/docs_check.py) at this ADR's writing —
  `looprs-00u.23`'s measurement-claim check has grown it since, and the number to
  quote is `wc -l` on the file rather than this sentence, because a line count kept
  in prose is the exact shape of number this ticket is about
  — which no upstream
  maintains. That is a real cost and it is paid knowingly: ~80% of the generator is
  the two jobs a generic generator gets wrong for this corpus (outward path mapping
  and GitHub-compatible heading slugs), the rest is HTML chrome, and both of the
  tricky jobs are covered by the gate rather than by trust. The honest counterfactual
  is that `mdbook` would not ask us to write any of it and would ask every
  contributor and every CI image to install it instead;
* no client-side search. A book this size is navigated by its tree and by `grep`, and
  `grep` over Markdown is better than an index over HTML. If the corpus triples this
  becomes a real gap, and it will be someone else's ticket;
* two Markdown-to-anchor implementations — the generator's Rust `slugify` and the
  checker's Python one — which must agree. They are small, they are documented as
  needing to match, and a divergence shows up immediately as a wall of anchor
  failures rather than as a silent mis-link. That is the acceptable shape of this
  duplication: it fails loudly. A third implementation is not acceptable; if a
  second page needs GitHub-style slugs, they move into a shared spec.

## What this forbids

1. **No existing file moves or renames** without amending this ADR first. The
   outward links from ADRs to `src/` and `spikes/results/` are the corpus's most
   valuable property, and they are one `mv` away from being a pile of broken links.
2. **No new documentation dependency** (`mdbook`, `sphinx`, `pandoc`, a JS static
   site generator). The parser is already in `Cargo.lock` for the app; anything else
   is a thing every contributor and every CI image has to install before reading a
   sentence.
3. **No committed HTML.** `docs/_site/` stays gitignored. Committed generated
   output is an artefact that can disagree with its source while looking
   authoritative.
4. **No second list of a knob's defaults.** One home per fact. The site inherits the
   rule the ADRs established, and `docs_check.py`'s default-agreement check is what
   stops the rule becoming folklore.
5. **No Mermaid / PlantUML / image-only diagrams.** ASCII in fenced blocks, per §5.
6. **No content page that fixes code.** The docs epic's rule: findings go to beads
   first, code second, on a separate branch. A commit that touches `docs/guide/*.md`
   and `src/*.rs` is two reviews pretending to be one.
7. **No hand-maintained copy of a generated table.** The keymap tables are
   generated between markers; editing them by hand is overwritten and, if the gate
   is bypassed, undetectable — so the gate is the rule, and `--fix-keymap` is the
   only sanctioned way to change them.
8. **No check count in prose that no capture agrees with.** A number like `149/149`
   is a claim about one run, not a property of the tree, so it has to resolve to a
   committed capture under `spikes/results/` — the newest capture of the spike it
   names, or a capture named in the same sentence. A count whose run was never
   captured says so in the page (`<!-- spike-count: unverified N/M <spike> — why -->`),
   and the marker is checked in both directions: a claim with no capture and no
   marker fails, and a marker that no longer backs a claim fails. Added after
   `looprs-00u.23`: three pages quoted `shutdown_e2e`'s 149 checks
   (`spikes/results/shutdown-e2e-pdl3.log`, the count at `pdl.3`) while the tree
   printed 171 (`spikes/results/shutdown-e2e-00u23.log` is now committed, and the
   three pages name their captures), and before this check nothing in the repo could
   have contradicted them.
9. **No test name quoted in backticks that no function answers to.** A test list is
   a specification only while a reader can find the item they were pointed at, and
   the failure this rules out is silent in both directions: the quoted name is
   typographic, so nothing fails, and the reader who greps and finds nothing
   suspects the grep before suspecting the page. So a backticked `snake_case` name
   of four or more words — the shape this repo's test names have, and no other
   shape Rust has — resolves to a real `fn <name>` in `src/`, `tests/`,
   `examples/`, `spikes/` or `scripts/`. Names that are *deliberately* not
   functions (a prose example of the naming rule; the name of something a page is
   about the absence of, as ADR-0004 counts the deleted viewport helpers at zero
   occurrences) are excused one at a time in `ILLUSTRATIVE_TEST_NAMES` with the
   reason written beside them, and the excuse is checked both ways like the capture
   markers above: a citation with no function and no excuse fails, and an excuse no
   page still uses fails. Renaming a test to match a page is not a fix — the page
   is the thing that was wrong. Added after `looprs-00u.25`, which found the class
   by noticing one name in `docs/guide/contributing.md` was ungreppable; the gate
   written against it found two more, one of which credited the *row* cap with a
   test written about the **byte** cap.
10. **No size in prose that no command prints.** A line count, a byte count or a
    duration quoted in a page is a claim about a measurement, and the measurement
    has to be re-runnable from the page that makes it: print the one-liner beside
    the number (the corpus table's caption row, `--list-corpus`, `wc -l` on the
    file) so the next re-count is a re-run rather than a re-type. A number that
    cannot be re-run is folklore with a comma in it, and folklore drifts silently —
    two figures typed an hour apart can disagree with no diff to argue about. Where
    a row claims to *include* another, the arithmetic itself is now gated: the
    inclusive row cannot be smaller than the row it includes, and a difference
    quoted beside two totals has to equal the subtraction. Added after
    `looprs-00u.27`, which found this ADR's own corpus table stating 7,071 lines
    under `docs/` and 7,056 *including* `spikes/` — fifteen lines of subtraction
    where there should have been an addition, in the one table a reader trusts to
    be the measurement.

---

## Amendment — 2026-10-10: the page's *shape* becomes a checked property (and Hugo is asked again)

- **Trigger:** a report about the built site rather than the prose — *"looks good on normal
  browsers but is squished on mobile with too much white space on the right side"*.
- **Measured by:** [`spikes/docs_layout.py`](../../spikes/docs_layout.py), which loads every
  built page at 360/390/768/1440 and asserts 15 things about the frame. Committed twice, same
  driver, same tree, one side fixed:
  [`spikes/results/docs-layout-control.log`](../../spikes/results/docs-layout-control.log)
  (**2/15**, pre-fix worktree) against
  [`spikes/results/docs-layout.log`](../../spikes/results/docs-layout.log) (**15/15**).
  Re-run: `./scripts/docs.sh build && python3 spikes/docs_layout.py`. The static half — the
  half that can be a gate — is `check_layout_css` in
  [`scripts/docs_check.py`](../../scripts/docs_check.py), now check 8 of that gate.
- **Amends:** **Q1** (asked again with a name attached: *Hugo*. The decision holds; one of the
  two reasons originally given for it is withdrawn as wrong), **Q3** (the artefact could lie
  after all, by a route the ticket did not consider), and **§What this forbids** (items
  11–13 added). Nothing moved, so rule 1 was never exercised.

### 1. What "squished on mobile" turned out to be

Two independent bugs, neither of which fails a build, a linter, or a link checker, and both of
which were invisible on the machine that wrote them:

| what the report said | what the browser measured (pre-fix) | now |
| --- | --- | --- |
| "squished" | the article column resolved to **348px of a 390px** viewport, and 33 pages came out narrower than 80% of the screen | the article is ≥80% of the frame on every page |
| "too much white space on the right" | `body` resolved **`348px 0px 42px`** — three tracks for a template that defines one. The header stopped 42px short of the right edge (that strip *is* the white space) and the entire page tree was crushed into the 42px beside it | one track at ≤900px on every page |
| (not reported; found by measuring) | 25 pages scrolled wider than the phone — worst **3,323px** of document on a 360px screen, because every pipe table in the corpus had rendered as a paragraph of pipes instead of a table | 0 of 198 pages overflow at any width |

**Bug one — a grid area named but not defined.** The narrow media query retiled `body` to one
column with areas `top`/`main`, while `.sidebar` went on asking for `side`. CSS does not reject
that: it synthesises a track for the ghost name and lays the header in one column beside it. The
invariant now enforced (statically, as check 8) is *every `grid-area:` name in the stylesheet
appears in every `grid-template-areas:` in it* — including the `body:has(.sidebar:empty)` rule
for carried repository files, where the temptation is to drop `side` and hide the item instead.
That temptation is the same loophole; the CSS names `side` and collapses the row.

**Bug two — a markdown parser with no extensions on.** `Parser::new(md)` is *no* extensions, and
a pipe table under a parser with no `ENABLE_TABLES` does not fail: it renders as text. Count of
table rows in the corpus, re-runnable:

```sh
grep -rc '^|' docs/*.md docs/guide/*.md docs/adr/*.md README.md | awk -F: '{s+=$2} END {print s}'
# 1024   (as measured 2026-10-10; `wc -l` on the same file list is a different question)
```

All of it reached the reader as `| … |` text. This is a *generator* bug with a docs-shaped
shadow: the ADR's own table of what the corpus contains was one of the pages affected, and the
ADR quoted the size of a corpus whose tables were invisible.

Two more turned up while measuring, and both are amendments to *this* ADR rather than footnotes:

**The artefact could lie, by a route Q3 missed.** Q3 rejected committed HTML because "committed
HTML can be stale relative to its own Markdown, and a reader cannot tell". Uncommitted HTML in
`docs/_site/` had the same property: the build only ever wrote, never removed, so 32 pages whose
sources had gone stayed in the output directory looking like part of the site. `sweep_stale_pages`
now deletes every `.html` the build did not write (first run: 32), which is what makes Q3's claim
true rather than mostly true.

**Two escape lists in one file.** `scripts/docs_check.py` generated the keymap tables through one
Rust-string-unescape helper and the wire-protocol tables through a second one, and only the second
knew `\u{…}`. Ten keymap rows shipped as `yes \u{2014} the first Esc unselects`, three pages
apart from a wire table where the same em dash rendered correctly. There is one list now
(`unescape_rust`, used by both), and the capture above fails if a `\u{…}` reaches a page.

### 2. Decisions

1. **`render_markdown` names its extension options** (`TABLES | STRIKETHROUGH | TASKLISTS`,
   exceeding by `TABLES` what `src/utils/md.rs` turns on for the transcript), and
   `render_page_markdown` **refuses to build a page whose markdown syntax survived** into the
   prose. The contributor rule that follows is CommonMark's, not a style: a table needs a blank
   line above it, because a table cannot start while a paragraph is still running. That rule bit
   this ADR's own sibling — [ADR-0005](0005-shell-output-content-model.md) had a `check.sh` row
   orphaned into the middle of a paragraph by an explanatory note inserted above the table's last
   row — and the guard found it on the first run, which is the argument for the guard.
2. **The build owns its output directory**: `sweep_stale_pages` removes what it did not write.
3. **`check_layout_css` (gate check 8)**: grid areas closed at every breakpoint, a
   `width=device-width` viewport meta in every page template, and no `position: sticky` sticking
   at a remembered number. That last one is not pedantry: the sheet stuck the sidebar at a typed
   `top: 45px` under a header whose box measured **21px**, so the sidebar's first strip was hidden
   under an opaque bar and a gap no diff explains. `--topbar-h` is now one number in one place.
4. **The page tree is folded on a phone, not squeezed onto one.** A 25-link tree above the article
   is not navigation on a 390px screen; the fold is a CSS-only checkbox (`:checked ~ ul.nav`)
   labelled `Contents · <this page>`, so a folded tree still says where you are. CSS only, on
   purpose: `file://` has no script to lean on and `livereload.js` is a `serve`-only file.
5. **One Rust-string-escape list** in the gate, shared by both generated-table renderers.

### 3. Hugo, priced rather than argued

The suggestion was that an existing generator would be more efficient than 1,425 lines of our
own. Q1's criterion was never features — it was *what a contributor has to install before they can
preview a page they just edited*. That criterion still decides it, but pricing Hugo properly
withdraws one of Q1's reasons and confirms the other:

| axis | Hugo v0.167.0, measured on this corpus | this generator |
| --- | --- | --- |
| install | **55 MB binary**, per contributor and per CI image | nothing: `pulldown-cmark` is already in `Cargo.lock` because the app's transcript renderer uses it |
| time to first page | **zero pages** from content + config alone — Hugo emitted no HTML until three layout files existed; then 26 pages in **~40 ms** | one command, ~0.8 s warm, 4.7 s cold with an empty target dir |
| heading slugs | `## 1. The status → column mapping` → `1-the-status--column-mapping`, **byte-identical to GitHub's rule including the double hyphen** | implements the same rule by hand |
| a link from `docs/adr/x.md` to `../guide/operator.md` | emitted verbatim → points at a `.md` in a site of `.html`, dead, until you add `render_hooks = ["link"]` plus a `render-link.html` template | rewritten to resolve inside the site, at any mount prefix |
| a link from an ADR to `../../spikes/results/x.log` or `../src/session/mod.rs` | emitted verbatim; both escape the site root and land on nothing. Making them land costs that render hook **plus** a `static/` mirror of `src/` and `spikes/` — i.e. copying the tree | `_repo/` carried **on demand**: only linked files, 159 files here, which is why a link into 2.3 MB of Rust costs one page |
| the page's own shape | a theme, written by us either way (3 template files minimum here, 0 provided) | the stylesheet + 2 templates, one Rust file |

**Decision: unchanged, for the reason Q1 gave, minus the reason that turned out to be false.** The
GitHub-slug rule is not a Hugo gap and should not have been listed as one; the outward-link gap is
real but closable with two template files and a mirror of the tree. What is not closable without an
install is the install — and ADR-0008's whole argument was that the fast path stays available to
someone with a checkout and no network. Hugo is the right answer to a different question: a docs
corpus that grows search, i18n, versioning, and a theme designed by someone who is not the person
editing the page. If that day comes, this table is where the ticket starts, and the honest framing
then is "the corpus tripled and needs search", not "the generator is ours and it is 1,425 lines".

### What this forbids (three more, in the same list as the others)

11. **No `grid-area:` name that the active template does not define, no page template without a
    `width=device-width` viewport meta, and no sticky offset typed as a literal number.** All
    three are checked by `check_layout_css`, and all three fail *silently and only on a phone*,
    which is the failure class this amendment exists for.
12. **No gate that needs a browser.** The layout rules that can only be checked by a browser live
    in `spikes/` and print a "cannot measure, here is why" exit code 2 when Chromium is absent —
    never a pass. Rule 2 (no new documentation dependency) is the reason the gate stays
    `python3` + the tree, offline.
13. **No assumed markdown extension.** `render_markdown`'s `Options` list is the contract, and the
    build refuses a page whose syntax survived rather than publishing it quietly. The first
    version of that guard checked the raw HTML and missed `<p>\| Command \|` — a check must look at
    the text a reader reads, which is what it now strips code regions and real tables out of.

### What stands

Q2 (nothing moved, and `docs/_site/` stays gitignored), Q3's conclusion (built, not committed) and
its reasoning about diff noise, the ASCII diagram policy, the generated-table markers, and all ten
original prohibitions. Rule 1 was never invoked: no file moved.
