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
| Markdown under `docs/` (after this epic's pages) | **7,071 lines** as first published; **7,281 lines** re-measured 2026-10-09 in the improvement sweep |
| …including `spikes/` (which the site carries) | **7,565 lines** in that same re-measurement |
| How either number is obtained | `find docs spikes -name '*.md' -not -path 'docs/_site/*' \| xargs wc -l \| tail -1`. Re-run it; do not re-type it. The pair originally written here was "7,071 / 7,056", which cannot both be true — the superset was 15 lines *smaller* than its own part. Corrected by the sweep (`looprs-00u.27`) |
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
from `SUMMARY.md`, so a page cannot exist outside the tree.

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
  [`scripts/docs_check.py`](../../scripts/docs_check.py), which no upstream
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
