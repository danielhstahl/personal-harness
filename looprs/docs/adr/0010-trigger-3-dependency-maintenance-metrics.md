# ADR-0010 trigger 3, written as four testable metrics — and both `rho` libraries scored against them

- **Ticket:** workspace-8jk.6
- **Status:** Accepted — definitions dated 2026-10-12; the score below is dated the same day and
  expires the moment any input to it moves
- **Defines:** [ADR-0010](0010-pi-rpc-child-vs-embedded-rho-engine.md) §4 trigger 3 — *"rho becomes
  adoptable as a dependency"* — which until this page was four fill-in phrasings rather than four
  tests, and which names one library where two answer to the word: every row below is therefore run
  once per library (see **Subjects**)
- **Measured by:** [`spikes/dependency_maintenance_metrics.py`](../../spikes/dependency_maintenance_metrics.py)
  → [`spikes/results/dependency-maintenance-metrics.log`](../../spikes/results/dependency-maintenance-metrics.log)
  (seconds; no build, no model call, no endpoint)
- **Subjects:** matthewyjiang/rho — whose embeddable crate is `rho-sdk`, the library ADR-0010 was
  *requested* against — and casonadams/rho (`rho`, `rho-engine`, `rho-harness-core`), the library its §3
  was *filed* against. Both are scored on every row, because a definition that is only run against the
  convenient candidate is not a definition.
- **House rule this page obeys:** ADR-0010's own forbids 7 — *do not retype these numbers into another
  page*. ADR-0010 stops restating trigger-3 numbers and links here instead; this page is where they
  live, and the capture is where they come from. Every number below was read off the public record on
  the date above by the driver named at the top, and re-running that driver is how you disagree with it.

---

## Why this memo exists

Trigger 3 as ADR-0010 filed it reads: *a licence file in the published artifact, ≥2 human contributors
each carrying >5% of commits, ≥12 months of history, and a documented event vocabulary that carries a
settle/turn-complete signal looprs can key a loop on.* Four conditions, and of those four, two had no
definition at all behind them. **"Share of commits"** named no denominator, and the denominator is the
whole argument: matthewyjiang/rho has 1238 attributed commits of which 319 are bots, so the top human
is 72.9% of all commits and 98.3% of human commits — the same repo answers a bus-factor trigger two
different ways depending on a convention nobody wrote down. **"History"** named no clock, and the
candidate's repo creation date predates `rho-sdk`'s first crates.io publish by 24 days, so "12 months"
lands on a different calendar day depending on which of three dates you pick. **"Documented"** named no
checkable form, which is how a condition like that gets re-litigated by vibes: *"arguably it has a
settle signal"* is not refutable.

So: write the definitions down, then run them, and let the score fall where it falls. The expectation
this memo was written to *test*, not to confirm, was that the intended library passes the licence
definition and fails the bus-factor and history ones — i.e. that the maintenance objection survives the
identity correction even though the licence objection does not. [It did not survive intact](#what-the-correction-actually-changed);
the honest reading is in the last section.

## How to re-score it

```sh
python3 spikes/dependency_maintenance_metrics.py            # both candidates
python3 spikes/dependency_maintenance_metrics.py --id matthewyjiang
```

Global conventions, all of them chosen to make the trigger **harder** to fire rather than easier,
because trigger 3 is a door in a decision whose whole point is that the door stays priced:

- **12 months = 365 days**, measured to the day the driver runs, not to a date written into a page.
- Every leg that cannot read its input prints `not measured` and **can never print PASS**. No network is
  a leg that did not run, not a leg that passed.
- The candidate is read from **what a consumer receives**: the published `.crate` artifact, the rendered
  docs.rs page, the project's own documentation pages. Not from a README summary, not from a ticket.

---

## Definition 1 — "share of commits": the human denominator, both printed

**The convention.** A contributor's share is measured against **human-authored commits only**. A
contributor is a bot when GitHub's own contributors endpoint reports `type != "User"` — that is
`github-actions[bot]`, `dependabot[bot]`, `devin-ai-integration[bot]`, `pullfrog[bot]` — and bots are
counted, printed and excluded from the denominator.

**Why this one.** Automation can neither earn a maintainer's share nor dilute one. The all-commits
denominator does the second: a repo whose CI force-pushes a release bot 298 times pushes a *real*
second maintainer under a 5%-of-all bar without anything happening in the project at all. A bus factor
is a claim about people, so the denominator is people. **Both denominators are printed on every run**
so a future run cannot move the trigger by changing the convention quietly; where the two disagree, the
page says which one the verdict used.

## Definition 2 — "human contributor": the >5% bar is the whole test

**The convention.** A contributor counts toward the **≥2** requirement iff they are `type == User`
*and* carry **>5%** of the human denominator. Two things this settles deliberately:

- **A drive-by commit does not count toward the ≥2.** It is counted in the denominator — it is real
  human work and removing it would inflate everyone else's share — but one patch from a passing
  stranger is not a second person who can carry the library. There is no separate "active contributor"
  rule invented underneath the bar: **the bar is the test**, so the count cannot be quietly padded by
  whoever scores it.
- **Bus factor = the number of humans above the bar**, and the trigger asks for ≥2. Below 20 attributed
  human commits the share is printed and the leg reports `not measured`: a percentage over a handful of
  commits is a rounding artefact, not a maintenance signal.

## Definition 3 — "history": three clocks, the shortest one binds

**The convention.** Three clocks are read, all to the day of evaluation:

1. the repo's GitHub `created_at`,
2. the repo's first GitHub release,
3. the first crates.io publish of the crate a consumer actually links.

**All three must clear 365 days**, i.e. the *youngest* clock binds. Extra clocks — the family's first
publish, the library's first ≥1.0.0 publish — are printed as context and are not scored, and a
**liveness guard** refuses a dead repo the spend of a history it stopped making: last push within 90
days.

**Why "shortest binds".** The ticket's ambiguity was real — matthewyjiang/rho's repo predates
`rho-sdk`'s first publish by 24 days, so the three clocks differ by weeks against a bar measured in
months. Two conventions fix that: take the friendliest clock (a repo can be old while its artifact is
young, which proves nothing), or require every clock (the artifact *and* the repository must both have
existed a year). This memo takes the second, for the same reason it takes a strict licence rule: a
definition that lets the scorer pick the clock is the definition this page exists to replace.

## Definition 4 — "documented settle vocabulary": five sub-tests, all required

The condition is *"a documented event vocabulary carrying a settle/turn-complete signal **looprs can key
a loop on**"*. Keying a loop on it is the load-bearing part, and it decomposes into five things that can
each be looked at. The terminal event set is a **declared claim under test** — `RunEvent`'s
`Completed`, `Failed` and `Cancelled` for rho-sdk, `RpcEvent::TurnEnd` and
`Presenter::print_turn_completed` for the other one — and the machine tries to falsify each part of it:

| sub-test | what it asks | how it is answered |
| --- | --- | --- |
| **S1 named** | does each claimed name exist in the vocabulary the host actually receives? | the name is found in the **published artifact's** enum or trait, and the file carrying it is printed. A rename upstream fails this row rather than ageing into a wrong sentence. |
| **S2 published** | is the name part of the published, versioned API surface? | the item is rendered on docs.rs (`id="variant.X"` / `id="method.X"`), URL printed. |
| **S3 prose-documented** | did a human write down *when* it fires? | a prose page in the candidate's own docs (README or `docs/**.md`) names the terminal set and states the condition. rustdoc proves an identifier shipped; prose proves a contract was written. |
| **S4 delivered** | does the signal actually arrive on the stream the host reads, or somewhere else? | the embed capture's **own event census**: which kinds, how many, and whether a terminal one is among them. |
| **S5 guaranteed** | does it arrive on *every* path a run can end — success, failure, cancellation, and the ugly ones? | FAIL if the candidate's own documentation disclaims it, or if a committed capture shows an arm where a run stopped and no terminal event arrived. Not reached when S4 already failed: there is nothing to guarantee a delivery of. |

S4 and S5 are what stop the condition meaning *"the spelling exists"*. A loop advance keyed on an event
needs the event on the arrival path **and** on the failure paths, because the failure paths are exactly
when a loop needs to advance.

---

## The score sheet, dated 2026-10-12

Read from GitHub's repo/releases/contributors APIs, crates.io's version history, the published `.crate`
artifacts, the candidates' docs.rs pages and documentation pages, and the two committed embed captures.
The capture, not this table, is the source.

| definition | matthewyjiang/rho (`rho-sdk` 5.9.0) | casonadams/rho (`rho` 0.12.0) |
| --- | --- | --- |
| **licence** | **FAIL** — the crate a consumer links ships no licence file; `rho-providers` neither; only the CLI crate `rho-coding-agent` carries a `LICENSE`, and a sibling crate's file does not travel inside another crate's artifact. Manifest says `license = "MIT"`; GitHub reads the repo as MIT. | **FAIL** — no licence file in any of the three published artifacts, no `LICENSE` at the repo root, and GitHub's detector reads NONE. |
| **bus factor** | **FAIL** — 1238 attributed commits, 919 human (319 bot: 298 + 17 + 3 + 1). matthewyjiang carries 903 = 72.9% of all, 98.3% of human. One human above the bar under either denominator. | **FAIL** — 865 attributed, 824 human (41 bot). casonadams carries 823 = 95.1% of all, 99.9% of human. One human above the bar under either denominator. |
| **history** | **FAIL** — repo 2026-06-21 (3.7 months), first release `rho-agent-v0.1.0` same day, first `rho-sdk` publish 2026-07-15 (2.9 months, binding). Liveness passes (pushed 2 days before the run). | **FAIL** — repo 2026-08-19 (1.8 months), first release and first publish both 2026-09-02 (1.3 months, binding); no ≥1.0.0 publish has ever happened. Liveness passes. |
| **settle vocabulary** | **FAIL, by one sub-test** — S1 named, S2 published, S3 prose-documented and S4 delivered all pass; **S5 fails**, and the disproof is the candidate's own contract text: *"The runtime does not guarantee delivery of exactly one terminal event (`Completed`, `Cancelled`, or `Failed`) for every run."* The embed capture agrees: a dropped run produced 0 terminal events in 5 trials, a panic inside a tool closed the stream with no terminal event, and a host that stops reading leaves the session saying `Running` with nothing arriving. Verdict: 4 of the 5 sub-tests. | **FAIL, by three** — S1 and S2 pass (`RpcEvent::TurnEnd` renders on docs.rs; `Presenter::print_turn_completed` exists, with an empty default body, so a host that does not override it is handed nothing at that moment). S3 fails: six published pages, none of them states when a turn completes. S4 fails: the embed arm's census accounts for **398 of 398** host callbacks and not one is a turn-complete — completion reached the host as `run_turn`'s **return value**. S5 is not reached. Verdict: 2 of the 5 sub-tests. |
| **trigger 3 (all four)** | **NOT MET** | **NOT MET** |

**When each row could next say PASS.** Two of the four rows are calendar-bound and can be re-run rather
than re-argued: history cannot clear before **2027-07-15** for matthewyjiang/rho (twelve months after
`rho-sdk`'s first publish) and **2027-09-02** for casonadams/rho. Bus factor is not calendar-bound: at
the current attribution the second human would need **32** more attributed commits in the first repo and
**43** in the second to clear the 5% bar alone — which is what "a second person who carries this library"
has to look like before the row moves. Licence is the cheapest row to fix and the most embarrassing to
leave: a `LICENSE` file in the library crate's published artifact, one file, and it only counts once it
ships. Settle vocabulary flips for rho-sdk when its own docs stop disclaiming the guarantee, and the
disclaimer is quoted in the capture with its URL, so the flip is a diff upstream, not a reading of them.

---

## What the correction actually changed

As filed, ADR-0010 §4 trigger 3 ended with *"Today: 0, 1, 7 weeks, and no."* — four numbers measured
against casonadams/rho, for a decision that was requested about a different library. Those four numbers
are now gone from the ADR, and this page is where their replacements live, so the history is stated
once, here. Re-measured against the
library the decision was about:

- **The licence objection does not die with the identity correction — it sharpens.** The expectation this
  memo was written to test was that the intended library passes this row. It does not. matthewyjiang/rho
  does have a repo-root `LICENSE` (GitHub reads MIT, the manifest says MIT) and it is exactly the shape
  ADR-0010 forbids treating as a formality: **the crate a consumer links ships no licence text**, and the
  one artifact in the workspace that does ship a `LICENSE` is the CLI binary crate, and that file
  arrives with nobody who links the library. A declared-but-unreceived licence is the same gap in a
  nicer repo.
  This is the memo's main disagreement with the ticket that asked for it, and it is stated plainly for
  that reason.
- **The bus-factor objection survives the correction, and the correction makes the numbers better for the
  candidate without moving the verdict.** The record's *"95.1% of 865 commits by one human"* becomes 98.3%
  of 919 human commits (72.9% of all, once 319 bot commits are set aside). The intended library is a
  bigger, busier project than the record thought — and it is still one person, under either denominator,
  with a second human at 16 commits.
- **History gets worse for the record, not for the candidate.** *"7 weeks"* was the wrong repo's age; the
  right repo is roughly twice that old. It is still 2.9 months on the binding clock against a 12-month
  bar, so the row fails for the same reason and the honest note is that the date the correction was made
  is simply early — 2027-07-15 is the first day this row is worth re-running.
- **The settle row is the one place the identity correction genuinely helps the candidate, and it is the
  row that needed definitions most.** As filed, trigger 3's fourth condition was answered with a flat
  *"no"*: turn completion is the `run_turn` return value and the events are a presenter callback.
  Scored for `rho-sdk` it is four of five sub-tests and a **named, published, deliberate limitation** —
  the SDK states its terminal event is best-effort, that the authoritative signal is the awaitable
  outcome, and that end-of-stream must never be read as success. That is not a pass; looprs keys its loop
  advance on the stream, including on the child-died path, so a best-effort settle event is precisely the
  hole looprs cannot drive a loop through. But it is a *different* objection: one upstream can close in a
  release, versus one that is a property of the library's shape.

**And the conclusion, without hedging: trigger 3 is NOT MET by either library.** Zero of four definitions
pass for both. The maintenance objection to **either** `rho` library — one person, three
months of published artifact, and a
licence you do not receive — survives the identity correction whole, and the correction adds a finding
nobody had: the library this decision was *about* fails the licence test too. Nothing in this memo
re-opens ADR-0010; it gives trigger 3 a bar that can be re-run instead of re-argued, which is the only
thing a trigger that is meant to survive its author can be made of.

## What this memo does not decide

- **It does not re-price the engine.** ADR-0010's other findings — the boundary at a hundredth of one
  percent of a pass, the blast wall, the persistence and corpus rows, the wire-vocabulary re-derivation —
  are not trigger-3 rows and no row here touches them. Trigger 3 is a *maintenance* test and always was;
  meeting all four rows would re-open the question, not answer it.
- **It does not amend ADR-0010's §3 or decision 3**, which carry the wrong library's numbers. That is
  workspace-8jk.8's job; this page supplies the definitions and the score it links to.
- **Its inputs are dated and third-party.** GitHub's contributor attribution is GitHub's (email→account,
  default branch, capped at 500 contributors); crates.io's version dates are crates.io's; the
  documentation scan reads the README plus up to 30 `docs/**.md` pages (`LOOPRS_DOCS_MAX`) from the
  default branch, which is a sample of a docs site, not a mirror of it. Every one of those is printed
  with its URL in the capture so a wrong input is a visible wrong input.
- **It scores libraries, not crates.** Where "the library a consumer links" is one crate among several in
  a workspace (`rho-sdk` among three; `rho` among three), the licence and history rows are scored on that
  crate and the family is printed as context — because a dependency is what `cargo` hands you, not what
  the repository contains.
