# The improvement sweep

*The protocol for what to do with the things you notice while writing docs.*

Documenting a codebase makes you read files you would otherwise never open. That is
the cheapest possible moment to notice that a hot path re-parses a growing buffer,
that a log destination is hard-coded, that a "2–3.7k line module" cannot be held in
working memory, or that a comment has not matched the code for three releases. The
requirement is to **act on those findings rather than note them**, and the failure
mode to avoid is the docs branch that turns into an unreviewable refactor.

This page is the protocol that keeps those two things apart. It came out of
`looprs-00u.12`, and the nine findings it produced are the worked examples below.

---

## The protocol

**1. Read with a list open.** Every time writing a page requires reading code, the
author records what made them wince, in the list, as they go. The list lives on the
sweep ticket in beads — one line per page read, so a page with no line is
indistinguishable from a page nobody read.

**2. File it before you fix it.** Each finding becomes its own bead under the epic
that produced it, labelled `improvement` and `found-in-sweep`, carrying the four
elements below. The fix happens later, on its own branch, with its own gate run.

**3. No drive-by refactors on a docs branch.** A docs commit that also changes 400
lines of Rust is a docs commit that cannot be reviewed. What a docs branch *may*
carry is the correction of a false statement — the page stops asserting the thing
that turned out not to be true, and the bead keeps the code half. Say which of the
two you did in the commit message.

**4. Verify before you claim.** A finding written into a *documentation page*
("X is O(n) per frame") must be measured before it goes in. An unmeasured
suspicion goes in a ticket, not the site: this repo's rule is that claims carry
numbers, and the site is where people will believe it.

**5. Close the loop.** The sweep ticket closes with counts: findings filed, findings
accepted, findings **rejected with the reason**. A rejected finding with evidence is
a result — it is what stops the next person re-filing it.

## The four required elements

Every finding bead carries all four. A finding without element four is a complaint,
not a ticket.

```text
## Found while documenting
The page being written, the ticket that page belongs to, and the date.

## Evidence
`src/path.rs:NN` or `module::function` — what was seen, plus the command that
showed it and what that command printed.

## What is wrong
One sentence. The defect, not the feeling about the defect.

## What is better
One sentence. The change, in the direction of the measurement.

## How we would know
The measurement, test or profile that distinguishes the two, with the command to
run it. Include the non-vacuity half: what makes this check *fail* on the old code.
```

Element one is what makes the finding re-checkable after the file moves. Element four
is what makes it *an engineering ticket* rather than a gripe: if you cannot say how
you would know, you have a maybe, and maybes go on the list, not on the board.

## What counts as a wince

Four shapes recur, and the fifth is the one people miss:

* **a shape harder than it needs to be** — the module that is 3,709 lines because
  nobody cut it along the responsibility lines already in the file
  (`looprs-00u.18`);
* **a hot path that looks wasteful** — the live preview that re-parses the whole
  unfinished block every frame, at exactly the moment the frame budget is tightest
  (`looprs-00u.14`);
* **a number with no measurement behind it** — "megabytes paid once on the first
  highlight" with no byte count anywhere, and the ADR whose inclusive line count is
  *smaller* than its own part (`looprs-00u.16`, `looprs-00u.27`);
* **a comment or doc that no longer matches the code** — the header block that
  documents a filename casing the program does not produce (`looprs-00u.21`);
* **a doc that claims an enforcement that does not exist** — "this diagram is
  checked by the docs gate", where the gate has three checks and none of them is
  that (`looprs-00u.22`). This is the most expensive kind, because a false
  enforcement claim stops the next reader from verifying the thing it describes.

## What "verified" means for a number in a page

* The number comes from an artifact — a committed capture under
  [`spikes/results/`](../../spikes/results/), a `--list-knobs` print, an `ls -l` —
  and the page names the artifact. **Do not retype numbers across pages.**
  `looprs-00u.23` is what a retyped number looks like three milestones later.
* If you cannot re-run the thing, do not print the number. Write the pointer.
* Quoted identifiers are numbers too: a test name in prose that does not exist
  verbatim in `src/` is a citation to nowhere (`looprs-00u.25`).

## Running the audit

```sh
./scripts/sweep_check.py                        # audit the docs epic in this repo
./scripts/sweep_check.py --since <rev>          # rev = parent of the epic's first docs commit
./scripts/sweep_check.py --no-git               # skip the branch-hygiene check
```

It answers the three questions the acceptance criteria ask, with prints rather than
hope:

1. **Finding shape** — every bead under the epic carrying the improvement labels has
   all four elements, cites at least one repo file, and every cited `path:NN` is
   inside that file's line count. (`How we would know` is exempt from the
   file-must-exist half: it is allowed to name an artifact that does not exist yet,
   since naming it is the point.)
2. **Read coverage** — every page the epic touched has a line in the sweep ticket's
   read log, with either a filed finding or an explicit `read, nothing found`.
3. **Branch hygiene** — no commit in `--since..HEAD` touching the epic's pages also
   touched `src/`, `Cargo.toml` or `Cargo.lock`.

It is deliberately **not** a step of [`./scripts/check.sh`](../../scripts/check.sh):
it queries beads, and the beads database is not in CI. Run it before closing a sweep
ticket. A check that needs a database nobody ships is still a check you can run; a
check nobody runs is decoration.

## The read log format

On the sweep ticket's notes, one line per page, pipe-separated, so the audit can
parse it and a human can read it in `bd show`:

```text
SWEEP EPIC PAGES: docs/index.md docs/guide/differences.md …

docs/guide/transcript.md  | src/state/scrollback.rs:161,835,1187; src/session/view.rs:56,104 | read, nothing found
docs/guide/operator.md    | src/services/journal.rs:9-14,377                                 | filed: looprs-00u.21
docs/guide/example.md     | src/theme/styles.rs                                               | rejected: the map's directory rows cover it
```

One line per page, three legal outcomes: `filed: <ids>`, `read, nothing found`, and
`rejected: <reason>`. Silence is not an outcome — that is the whole point of step 1.
The `SWEEP EPIC PAGES:` line is what makes "every page the epic wrote" a checked
statement rather than a memory test.

## Worked examples from this epic

| finding | the wince | how the four elements show up |
| --- | --- | --- |
| `looprs-00u.13` | a log in `temp_dir()`, `rolling::never`, default level `debug` | cites `src/main.rs:53-67`; "how we would know" is a 60-second scripted run with before/after sizes in the notes |
| `looprs-00u.14` | a per-frame full re-parse of growing text | cites `src/app.rs` → `SessionView::preview` → `flusher.preview`; the check is µs and bytes per frame *against live-block length* on the corpus `src/measure.rs` already loads. **Closed by the measurement, and the measurement rewrote the finding:** the live slice is the open *paragraph*, not the answer (`Cursor::block` resets at every blank line) — max 3.0 KiB against 56 KiB entries, worst re-parse 313 µs, under the ticket's own 1 ms "no code change" bar. The cost that was real is bytes: 18.8 KB per preview call pooled, ~1.1 MB/s of churn from one streaming view at 60 fps, paid in full by frames whose bytes had not changed. Cache added on the ticket's option 1; hit = 3.9 µs / 1.6 KB = exactly the clone-of-rows floor, miss = +21% for the clone and the `src` copy. Side product: the harness's `flush` column caught syntect's first-highlight-per-context stall (296 ms → 78 ms once more languages were warmed) — `looprs-00u.16`'s finding, evidenced in [`../../spikes/results/live-preview-cost.log`](../../spikes/results/live-preview-cost.log) |
| `looprs-00u.15` | a blanket `unused_dependencies = "allow"`, an `osc52` feature nothing in `src/` uses, `reqwest` for one notifier | "verify first" section: if the blanket exists because cargo misreports, name the crate and the reason in the manifest instead of keeping the blanket |
| `looprs-00u.16` | `OnceLock` moves syntect's whole load into the first highlighted draw | the "what is better" is conditional on the measurement — warm it, shrink it, or record that it is cheap and stop worrying |
| `looprs-00u.17` | four timing-dependent tests the suite pre-forgives | "how we would know" is ≥20 consecutive runs, and the ticket is not done until the "Known flakes" section in `docs/testing.md` is **deleted** |
| `looprs-00u.18` | four 2–3.7k line modules | behaviour-preserving only: "if a split makes a bug obvious, file it as its own ticket rather than fixing it here" |
| `looprs-00u.19` | `role: String // "user" \| "assistant" \| …` | a value set that lives in a comment cannot be documented from code, so the doc would need a second source of truth — the fix makes the enum the source |
| `looprs-00u.20` | `docs/testing.md` promised a transcript entry the code had removed | fixed in the page (docs half), with the residual — nothing asserts the replacement marker's wording — split into `looprs-00u.20.1` |
| `looprs-00u.21` | a header block documenting `-beads.txt` while disk carries `-Beeds.txt` | evidence is `ls -l` output in the ticket, and the reason it matters is the operator typing the documented name and getting "No such file or directory" |

`looprs-00u.22`–`looprs-00u.27` came from the second pass over the pages this epic
itself had just published — which is the part worth copying: **read your own new docs
against the code afterwards**, at the speed of someone who does not believe them.

## Anti-patterns

* **The drive-by fix.** The refactor you "just did while you were in there". It cost
  a review that cannot be done, and it hides the finding in the diff of something
  else.
* **The prose-only note.** Writing "X is O(n) per frame" into a page because the
  code looked like it. Now the site believes it and nobody will measure it.
* **The ticket with no fourth element.** "Logging is a mess" cannot be closed,
  cannot be argued with, and cannot be rejected — it can only be ignored.
* **The unrecorded rejection.** Deciding a wince was nothing and telling nobody.
  Next reader wincing at the same thing spends the same twenty minutes, and files it
  twice.
* **Fixing the test to match the doc.** If the doc quotes a test name and they
  disagree, the doc is the one that is wrong (`looprs-00u.25`).

**See also:** [contributor guide](contributing.md) ·
[testing ladder](../testing.md) ·
[how this documentation is organised](../README.md)
