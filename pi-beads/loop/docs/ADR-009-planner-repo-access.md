# ADR-009: The planner gets eyes — the split pass can read the repository

- **Status:** Accepted. **Amends** the split half of `.5`/`.7`: the planning
  session used to run with no built-in tools at all. The transaction (verbatim
  record of the request, the ledger, the `#index` → id binding) is unchanged.
- **Modules:** `src/agent.ts` (`planSplitSession`, `planningSystemPrompt`,
  `buildSplitPrompt`, the allowlist rule in `defaultSessionFactory`),
  `src/vcs.ts` (`worktreeChanges`), `src/loop.ts` (the before/after check in
  the split unit), `src/main.ts` / `src/app.ts` (`LOOP_SPLIT_REPO_ACCESS`)

## Context

A split request arrived, the planner produced a batch, and the batch was
describable in any repository on earth:

> Add CSV export to the report tool
> 1. Create `src/export/csv.py` … 2. Add tests in `tests/test_export.py` …

This repository has no `src/`, no Python, and no `tests/` directory. The planner
knew none of this because it could know none of it: the split session ran with
`noBuiltinTools: true`. Its whole world was the paragraph the human typed and one
report tool. The tickets it wrote were not wrong about the *request* — they were
unverifiable against the *repository*, and the work session downstream paid for
it: paths that did not exist, acceptance criteria no command could satisfy, a
convention invented wholesale because nobody had looked at the one that was
already there.

That setting was not paranoia for its own sake. The reason the split session had
no tools was the reason the work session had them: a planner that can `edit` is
not a planner. And the bare toolset prompt existed because of a real failure —
left on pi's default prompt the model reached for `bash`, got `Tool "bash" not
found`, and read that as a flaky harness.

What was missing was not the restriction. It was the middle: **"you may look, you
may not touch"** had never been an option. The dial was binary between *nothing*
and *the whole coding toolset*.

## Decision

### 1. The split session gets `read` and `bash` — and never `edit` or `write`

```ts
export const SPLIT_REPO_TOOLS = ["read", "bash"] as const;
export const NEVER_FOR_A_PLANNER = ["edit", "write"] as const;
```

`bash` is the interesting one: `ls`, `git log`, `git show`, `rg`, `cat` are what
it needs to find out what a repository is. The dedicated `grep`/`find`/`ls`
tools are deliberately **not** enabled — `bash` covers them, and one unbounded
recursive search is all it takes for a five-minute planning run to eat its
context window, so the bound is put in the prompt instead of in a tool list that
could not enforce one either way.

`edit`/`write` go in `excludeTools` as well as staying out of the allowlist. The
allowlist already excludes them; the denylist is there so a future name collision
with an extension tool cannot reopen the door quietly.

### 2. The grant and the announcement come from one function

`planSplitSession(customToolNames, { repoAccess })` returns the built-ins, the
excludes **and** the system prompt together, and
`toolInventoryGap()` — which used to fire only for the all-or-nothing case — now
fires for an allowlist too: a narrowed tool set shipped with pi's default prompt
is the same defect pointing the other way (a model told it has `bash` when it
does not, or never told about the tool that was added for it). The factory
refuses the combination rather than running it.

### 3. pi's tool allowlist covers custom tools, and that is a loaded gun

This is the one thing in the change worth reading twice. In pi, `tools: [...]`
filters **every** visible tool — built-in or custom (`sdk.js`: `allowedToolNames`
→ `isAllowedTool`, applied to `allCustomTools` as well as the built-ins). So:

```ts
options.tools = ["read", "bash"];   // with a `report_split` custom tool registered
// → active tools: read, bash. The report tool does not exist as far as the model
//   is concerned. The split can never be reported.
```

Verified against the real session:

```
WITH report in allowlist         => active: [ 'read', 'bash', 'report_split' ]
WITHOUT report in allowlist     => active: [ 'read', 'bash' ]
```

The failure is spectacular in the wrong way: the run would end with "split
produced no structured proposal", pointing at the model rather than at the line
that deleted its answer. So `defaultSessionFactory` appends every custom tool
name to the allowlist, always, with a comment saying why. A caller cannot forget
it, which is the only kind of guarantee worth having here.

### 4. The prompt makes the tickets about the repository

`buildSplitPrompt` now teaches the grounding pass instead of hoping for one:

- **What this project is** — `pwd`, `ls`, `README*`, the manifest
  (`package.json` / `pyproject.toml` / `Cargo.toml` / `Makefile`), `AGENTS.md`.
- **How it is built and checked** — the scripts and targets that really exist,
  the test directory and its naming convention. *Read* the config; do not run
  the suite.
- **Where the request lands** — list the area, read the two or four files that
  own it, so the ticket names real symbols rather than the request's vocabulary.
- **What already exists or is already moving** — `git log --oneline -15` for
  direction, `git status --short` for work in flight. A ticket that re-proposes
  something already landed, or already sitting uncommitted in the tree, is worse
  than no ticket.

And the rules that make it stick:

- Every `description` names the concrete paths the work touches, **from what was
  listed** — not from how the request was phrased.
- Acceptance is checkable *here*: the command that passes, the file that must
  exist, or the behaviour with a named entry point. "Not verifiable in this repo"
  is a signal to look longer, or to make the ticket a `spike`/`decision` whose
  deliverable is the answer.
- Nothing invented. Where the work needs something that does not exist yet, the
  ticket says so *with what was seen*: "create `src/export/csv.ts` (does not
  exist; `src/export/` holds `json.ts` and `ndjson.ts` — follow those)".
- Fit the repo's conventions. Do not propose lint/build/CI steps for a project
  that has none unless the request asks for them.
- **If the request does not fit this repository at all**, report one `decision`
  issue saying what was looked at, what this repository is, and why the request
  does not map onto it. Inventing adjacent work to fill out a batch is the
  failure mode this whole section exists to prevent.

The look is bounded — 5–10 commands, `| head -n 40`, `rg -l` over `rg`, one
directory rather than the tree — because the plan is the deliverable and the
exploration is not.

### 5. The tree is checked, which is what makes the grant safe

A shell in a session that is *told* not to write is a request for good
behaviour. This adds the part that isn't: `GitWriter.worktreeChanges()` reads
`git status --porcelain --untracked-files=normal --no-renames -z`, and the split
unit takes it before the planning session runs and again when the proposal is in
hand.

Any difference: **the split is refused and the run stops `blocked`**, naming what
appeared, what changed and what vanished. The check sits *before* the epic record
and before a single child is created, so a refused split leaves nothing behind on
the board but the request itself, which is exactly what a human needs to see.

Why stop rather than warn? A change made by the planner belongs to no issue. The
next work session would inherit a diff it never reported and never caused — and
if it reported that path among its own, the change would land under its commit
message, which is the one thing the commit stage is built to make impossible. A
warning nobody reads is a difference nobody notices.

Two properties of the check are deliberate:

- **Directory granularity for untracked paths.** `--untracked-files=normal`
  rather than `all`, so an untracked `node_modules/` is one record rather than
  fifty thousand. The consequence is stated in the interface: adding a file
  inside a directory that was *already* untracked is not visible — and that
  directory was already in the difference before the session started, so it is
  pre-existing state, not something the planner did.
- **A failed read is never "clean".** `worktreeChanges()` throws where the
  plan-time status folds a failure into an empty set. If the tree cannot be read
  the loop says so — *"the split proceeds unverified"* — and carries on, which
  is a different sentence from "nothing changed" and is allowed to have
  different consequences.

### 6. `LOOP_SPLIT_REPO_ACCESS`

On by default. `off` restores the sealed planner: no built-ins, the bare
inventory prompt (`bareToolsetSystemPrompt`), and a plan built from the words of
the request alone. It is a **strict** value — `1/true/yes/on` and
`0/false/no/off`, anything else refused at startup with the list of what is
accepted — because a typo (`=of`) silently taking the shell away would look
exactly like a split that was configured to be blind, and that is the one
configuration failure this loop promises not to have.

## What did not change

- **The planner still cannot edit.** No `edit`, no `write`, no web, no
  sub-agents. The inventory line in its system prompt lists every tool it has and
  nothing more, and reaching outside it is stated as the dead end it is.
- **The work pass is untouched**: full tools, pi's own system prompt.
- **The split transaction stands**: verbatim request record, the ledger, the
  `#index`-to-id binding, one `report_split` call, no batch created twice.
- **Nothing cleans up after the planner.** The refused change is left exactly as
  it is found — reverting it would be the loop making an unowned change of its
  own. The stop message says which command to run.

## Operations

The stop reads:

```
the planning session changed the working tree (appeared or changed:
?? planner_left_this.txt). A split is a plan, not a patch: nothing owns that
change, and the next work session would inherit a diff it never reported. The
batch was not created. Put the tree back the way it was (git status, then git
checkout -- <path>, or git restore --staged <path> if it was staged) and ask
for the split again.
```

What the operator sees on a normal run is the difference in the tickets: real
paths, the repo's own test command in `acceptance`, and no invented scaffolding.

## Alternatives considered

**Leave the planner blind.** Rejected first, and hardest to defend: the whole
value of a split is that it decides *where* work lands, and that question cannot
be answered from a sentence.

**Full coding tools, "please don't edit".** Rejected: an enforced-by-prompt-only
restriction on a session with `edit` and `write` is a wish, and this loop does
not carry wishes about side effects.

**A purpose-built read-only inspection tool instead of `bash`** (`ls_repo`,
`read_file`, `git_log`). Rejected: it is a smaller hammer only if the planner
never needs the bigger one, and the moment it needs `git log` with a pathspec, or
`rg` with a flag nobody foresaw, the tool becomes a shell with worse ergonomics.
`bash` plus a checked tree is both more useful and better guaranteed.

**Warn instead of refusing on a changed tree.** Rejected: the change is not
cosmetic, and continuing hands the next agent an unowned diff. Stopping is the
same instinct that ends a run on a half-written handoff.

**Revert the change and carry on.** Rejected: the loop destroying files it did not
create, chosen by a heuristic about who made them, is a worse class of bug than
the one it would be papering over.

**Give the split session the board too (`bd` in bash).** Deferred: it is a real
idea — a planner that can see the existing beads cannot re-propose them — but it
also hands the planner a write path to the board itself, which needs its own
checked contract. Not today.

## Pinning tests

- agent — the split session gets `builtinTools: ["read","bash"]` and
  `excludeTools: ["edit","write"]`; its custom report tool is inside the
  allowlist it is given; `noBuiltinTools` is false.
- agent — the system prompt's inventory line is exactly
  `` `read`, `bash`, `report_split` ``, it names the absence of
  `edit`/`write`, and it says the loop checks the tree.
- agent — `toolInventoryGap` refuses an allowlist with no override, and one
  whose prompt does not name every tool.
- agent — `LOOP_SPLIT_REPO_ACCESS=off` restores the bare planner (inventory is
  the report tool alone; the task prompt has no "Look before you plan").
- agent — the grounding prompt: real paths, checkable acceptance, no invention,
  `git log`/`git status` for existing and in-flight work, the honest
  "does not fit this repository" answer, bounded commands.
- loop — a planner that leaves a file behind: `blocked`, the file named, **no
  epic and no children created**.
- loop — `git status` cannot be read: warnings say *before the split* and
  *proceeds unverified*, and the split still lands.
- loop — `treeDelta`: same list is nothing, a failed read on either side is
  "undecidable", and the report caps its list and names the fix.
