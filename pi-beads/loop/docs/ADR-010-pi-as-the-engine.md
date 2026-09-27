# ADR-010: pi is the engine, not a plugin host we are bypassing — keep it

- **Status:** Accepted — **keep** `@earendil-works/pi-ai`, `@earendil-works/pi-coding-agent`
  and `@earendil-works/pi-tui` (all pinned `0.85.1`).
  **This ADR changes no code.** It is the evidence and the position that a
  dependency removal should have been written against, plus the boundary rule
  (§5) that keeps the question cheap if it ever has to be asked again.
- **Modules:** the pi surface in `src/agent.ts`, `src/idle.ts`, `src/render.ts`,
  `src/kanban.ts`, `src/monitor.ts`, `src/startup.ts`, and the mirrored
  constants in `src/context.ts` / `src/audit.ts`; the (pi-free) loop core
  `src/orchestrator.ts`, `src/loop.ts`, `src/beads.ts`, `src/finalize.ts`
- **Ticket:** workspace-k1o.1

## Context

The proposal on the table was to drop pi.dev from this loop, on two grounds:
that the loop registers no pi plugins, and that "pi is a thin wrapper around
direct LLM calls". Both were measured before being answered.

The measurements, from this working tree at `0.85.1`:

- `node_modules` unpacks to **553 MB**, of which 434 MB sits under
  `@earendil-works/pi-coding-agent` and ~9 MB more under the top-level
  `pi-ai` and `pi-tui`.
- The compiled JS those pins ship — counting each package once, not the
  duplicate copies nested inside `pi-coding-agent`'s own `node_modules` — is
  **≈105,000 lines**: `pi-coding-agent` 51,151 + `pi-agent-core` 16,335 +
  `pi-ai` 18,285 + `pi-tui` 14,775 + `@earendil-works/chord` 4,670.
- The part of this repo that touches any of it: **209 lines across six files**
  name a `@earendil-works/*` symbol — `agent.ts` 38, `render.ts` 69,
  `idle.ts` 79, `kanban.ts` 11, `monitor.ts` 8, `startup.ts` 4.
  Two more, `context.ts` and `audit.ts`, name pi only in comments and in drift
  tests that read pi's shipped source off disk.
- The loop core names it **zero** times: `orchestrator.ts`, `loop.ts`,
  `beads.ts`, `finalize.ts`, and also `split.ts`, `app.ts`, `main.ts` have no
  `@earendil-works` import at all.

Those two numbers — 209 and ≈105,000 — are the same fact read from opposite
ends, and they are the answer to "thin wrapper". A thin wrapper is smaller than
the code that calls it. Here 209 lines of ours stand where ~105,000 lines of
shipped work sits, and every one of those 209 is a *delegation*, not an
implementation: delete the delegation and the work does not disappear, it
moves here.

The first ground — "no pi plugins" — is nearer the truth and matters more, so it
gets answered precisely. `discoverAndLoadExtensions` is never called from
`src/`. Nothing in this repo's behaviour comes from a file dropped in a
directory. That is a statement about pi's **discovery** mechanism, and we do not
use it. It is not a statement about dependence:

- the loop's entire contract with the model is registered through the same seam a
  plugin would use — `defineTool` at `src/agent.ts:1589` (`report_done`) and
  `src/agent.ts:1628` (`report_split`);
- the prompt is swapped through pi's own resource loader, `DefaultResourceLoader`
  at `src/agent.ts:908-915`;
- the config file is pi's, read at the path pi's own `getAgentDir()` returns
  (`src/startup.ts:27`, `src/startup.ts:227`, `src/agent.ts:864`).

So there is no plugin host being bypassed. There is a runtime being used,
through its public SDK, in-process — which is what ADR-001 §1 decided in the
first place ("we become a fourth mode rather than a subprocess of one").

## Decision

**Keep pi.** It is not the extension layer we skipped; it is the execution
runtime. And confine it, deliberately rather than by accident, to `src/agent.ts`,
`src/idle.ts`, `src/render.ts` and the pi-tui leaf components
(`src/kanban.ts`, `src/monitor.ts`) plus the config read in `src/startup.ts`,
so the loop core never names it.

What follows is the cost side of the ledger, per package, so the recommendation
can be argued with rather than re-litigated.

### 1. `@earendil-works/pi-coding-agent` — the execution runtime

What the loop actually uses, with the call sites:

| Call site | What pi does for us there |
| --- | --- |
| `src/agent.ts:39-47` — `createAgentSession`, `DefaultResourceLoader`, `defineTool`, `getAgentDir`, `ModelRuntime`, `SessionManager`, `SettingsManager` | the whole engine API this loop is built on |
| `src/agent.ts:863` — `await getModelRuntime()`, which calls `ModelRuntime.create()` (`738-749`) | model registry, one per process, and one poisoned-create guard |
| `src/agent.ts:864` — `SettingsManager.create(spec.cwd, getAgentDir())` | reads the user's real `~/.pi/agent` config: provider, default model, default thinking level |
| `src/agent.ts:870` — `resolveThinkingLevelForRun(diskSettings, …)` | the precedence *caller → user default → pi's own*, so no ticket runs at a level nobody chose |
| `src/agent.ts:871` — `SettingsManager.inMemory({ compaction: { enabled: false } })` | compaction off — the amnesia half of ADR-001 |
| `src/agent.ts:877` — `SessionManager.inMemory(spec.cwd)` | nothing persisted to a session file, ever |
| `src/agent.ts:873-916` — `CreateAgentSessionOptions`: `noTools: "builtin"`, `tools: [...]`, `excludeTools: [...]`, `customTools`, `model`, `thinkingLevel`, `resourceLoader` | the tool-surface controls ADR-009 is built on |
| `src/agent.ts:918` — `await createAgentSession(options)` | **the tool-calling loop itself** |
| `src/agent.ts:691-712` — `AgentSessionLike = Pick<AgentSession, "sessionId" \| "sessionFile" \| "messages" \| "prompt" \| "abort" \| "dispose" \| "subscribe" \| "steer">` | the port this repo codes against instead of the class |
| `src/agent.ts:1589`, `1628` — `defineTool` | `report_done`, `report_split` — the only completion signal the loop accepts |

Behind `createAgentSession` today, as shipped: `dist/core/agent-session.js`
**2,830 lines** (queues, `prompt` 821, `steer` 1,016, `followUp` 1,033,
`abort` 1,222, `_willRetryAfterAgentEnd` 429, tool-hook installation 223,
`dispose` + `cleanupSessionResources` 584-598), on top of
`pi-agent-core`'s loop **16,335 lines** (`agent-loop.js` 559,
`harness/runtime/lane.js` 1,574, `runtime/drive/tools.js` 448,
`runtime/drive/response.js` 393, `runtime/drive/structural.js` 928,
`harness/env/nodejs.js` 793), on top of the built-in tools
`core/tools/*.js` **2,525 lines** (`bash.js` 295, `edit.js` 147 +
`edit-diff.js` 420, `read.js` 165, `write.js` 61, `grep.js` 251,
`find.js` 250, `ls.js` 125, `truncate.js` 214, `output-accumulator.js` 183,
`file-mutation-queue.js` 51, `path-utils.js` 98).

Removal means writing that loop — not a sketch of it, a correct version of the
parts this repo leans on hardest:

- **Streaming tool-call accumulation.** A tool call arrives across chunks. A
  half-parsed call read as "no tool call" is a run that silently did nothing,
  and this project has already documented exactly that failure class at the
  provider level (the litellm note in the repository `README.md`). This is the
  single place where a bug is invisible in the logs and fatal to a bead.
- **Dispatch with bounded output.** `truncate.js` + `output-accumulator.js` are
  397 lines because unbounded tool output is not an edge case; a `cat` of a
  generated file ends the run by eating the window ADR-002 exists to police.
- **The file-mutation queue**, so two `edit` calls in one turn cannot race each
  other's read-modify-write.
- **Abort that actually unwinds** an in-flight HTTP request *and* any child the
  `bash` tool started, within `abortGraceMs`, and leaves nothing able to write
  into the tree after `finalize.ts` has staged it. That property is not
  cosmetic: ADR-009's checked-split and ADR-006's lock policy both assume the
  session is *provably* stopped before the next actor touches the tree.
- **`steer()`**, because the off-ramp (`wrapUpInstruction`, `src/agent.ts:612`)
  is the whole difference between a run that times out with a verdict and one
  that times out with a dirty tree and a shrug.
- **The replayed chain-of-thought shaping.** The README's own measurement —
  "94 tokens without the replayed CoT and 270 with it" — is a property of how
  pi reshapes an assistant turn for the next request. Owning the loop means
  owning that, and re-deriving the number.

**Line count:** ~1,800–2,500 new TypeScript where ~115 lines delegate today
(`defaultSessionFactory` 862-921 = 60, `AgentSessionLike` 691-712 = 22,
model/thinking resolution 738-856 ≈ 33). Nothing is replaced; `src/agent.ts`
grows from 94 KB toward the shape of the thing being removed. The rest of the
file — the prompt builders, the verdict parsing, the context math, the failure
descriptions — is ~2,200 lines and survives either way, because it was always
ours.

### 2. Session persistence and compaction — the one item that shrinks, and it shrinks because we use none of it

`core/session-manager.js` is 1,369 lines; this loop uses `SessionManager.inMemory(cwd)`
(`src/agent.ts:877`) — the *no-persistence* option — and `core/compaction/*.js`
is 1,064 lines, switched off at `src/agent.ts:871` on purpose, because ADR-001's
whole argument is that what crosses an iteration is the issue payload, the
recalled memories and the git state, never a summarised transcript.

Owning the absence costs ~150–250 lines: an array, a `dispose()`, and the
discipline not to grow either. It is the only honest line item in this document
that makes `src/` smaller — and it is worth exactly that, not the 2,000 lines
next door.

### 3. Model registry, provider auth and `models.json` parsing

Read through pi at `src/startup.ts:27` (`getAgentDir`, `SettingsManager`),
`src/startup.ts:89` (`readDefaultModelRef` — *"the way a session reads it"*,
which is the only way that matters, because a resolution rule reimplemented from
the docs and a resolution rule taken from the loader disagree in the cases nobody
documented), `src/startup.ts:227`/`379`, and `src/agent.ts:863-870`
(`ModelRuntime`, `resolveModelForRun`, `resolveThinkingLevelForRun`).

The **format** is pi's, and this repo's top-level `README.md:7` tells every user
to install `models.json` into `~/.pi/agents`. The fields the loop is written
against are pi's: `providers.<p>.baseUrl|api|apiKey|compat.*` —
`supportsDeveloperRole`, `supportsReasoningEffort`, `thinkingFormat`,
`chatTemplateKwargs` — and `models[].reasoning|maxTokens|contextWindow|samplingParams`.
`src/audit.ts` (75 KB, 25 rules, ADR-002) exists to check that file against
the server, and what its suggestions *are* is keys in that schema:
`applySuggestions()` resolves to `providers.<p>.models[i].<field>` or
`providers.<p>.modelOverrides.<id>.<field>`, and
`test/audit.test.ts:690-707` opens `pi-coding-agent/dist/core/model-config.js`
from disk to prove the `$var` set the audit suggests is inside what pi's loader
accepts.

Removal therefore has two halves and the second is the trap:

1. **Write the loading, resolution and auth.** ~700–1,000 lines: schema parse
   and validation, provider resolution, `apiKey`/auth storage, and at least one
   streaming API adapter — pi ships **1,356** lines for `openai-completions`
   alone (`pi-ai/dist/api/openai-completions.js`) and 1,160 for
   `anthropic-messages`, before any of the `compat` matrix.
2. **Keep the file format pi's anyway**, because the README, `models.bkp.json`
   and every existing install already use it. That makes this repo a
   hand-maintained parser for a schema it does not own and cannot version.
   The drift surface does not disappear — it *loses its alarm*. Today an
   upstream rename of `supportsReasoningEffort` or `MIN_ANSWER_TOKENS` fails
   `npm test` at `test/audit.test.ts:672-707` and `test/context.test.ts:124-137`.
   After a fork it is an ignored key in a file nobody re-reads, discovered at
   the cost of one long run.

The other option — invent our own config format — leaves every person who
installs this thing reading two incompatible sets of documentation about a file
that lives in the same `~/.pi/agents` directory. That is a fork with extra
steps, not a simplification.

**Line count:** ~700–1,000 new, replacing 4 lines in `startup.ts` and ~33 in
`agent.ts`. Plus the drift guards, which stop being guards.

### 4. `@earendil-works/pi-tui` — rendering, raw mode, diffing, keybindings

| Call site | The pi thing | Shipped size |
| --- | --- | --- |
| `src/render.ts:32-38` — `AssistantMessageComponent`, `getMarkdownTheme` | the assistant block: markdown + thinking presentation + highlighter | `pi-tui/components/markdown.js` 810, `highlight.js` 10.7.3 |
| `src/render.ts:301-341` — `AssistantBlock` wraps pi's component verbatim ("pi's own arguments: thinking visible, pi's markdown theme, pi's pad of 1") | the whole highlighted half of the screen | — |
| `src/render.ts:97` — `Symbol.for("@earendil-works/pi-coding-agent:theme")` | pi's **live** theme object, so the loop's colours *are* the user's pi theme, not a copy of it | — |
| `src/render.ts:1168`, `src/idle.ts:713` — `new TuiMainScreen(...)` | the main-screen diff renderer | `tui-main-screen.js` 593 |
| `src/render.ts:1047-1072` with `786` (`app.tools.expand`) | the keybinding registry, and pi's own expand key so the key that expands tool output in `pi` expands it here | `keybindings.js` 232, `keys.js` 1,173 |
| `src/idle.ts:712` — `ProcessTerminal` | raw-mode stdin, resize, bracketed paste | `terminal.js` 459, `stdin-buffer.js` 365 |
| `src/idle.ts:749-773` — `CustomEditor`, `CombinedAutocompleteProvider`, `getSelectListTheme` | the multi-line editor: cursor, kill ring, word navigation, undo, slash-command completion | `components/editor.js` 2,040, `autocomplete.js` 664 |
| `src/kanban.ts:37`, `src/monitor.ts:34` — `truncateToWidth`, `visibleWidth`, `type Component` (`KanbanComponent` at `kanban.ts:974`, `MonitorComponent` at `monitor.ts:1933`; 17 call sites between the two files) | the width model: wide CJK, combining marks, ANSI-run-safe truncation | `utils.js` 1,199 |

Removal writes all of it: raw-mode input handling (partial escape sequences,
paste, resize), the damage-and-diff repaint with the coalescing window the
README documents as `coalesceMs` (33 ms), a Markdown → ANSI renderer with
tables, code blocks and thinking blocks, a highlight.js binding driven by a
*theme* object, and a text editor with an autocomplete provider.

**Estimate: 2,500–4,000 lines**, replacing ~79 lines in `idle.ts`, ~40 of
`render.ts` (the `AssistantBlock` + theme acquisition), and 19 across
`kanban.ts`/`monitor.ts`. The presenter logic in `render.ts` — the block
column, the footer, the HUD band placement, the coalescing — is ~1,850 lines
and survives either way; what dies is the *delegation to the highlighter*,
which is the part ADR-001 specifically refuses to own.

That is worth naming plainly: **dropping `pi-tui` reverses ADR-001 §2**, which
demoted `src/format.ts` and its hand-rolled ANSI palette precisely because the
rule was "highlighting is inherited, not reimplemented" and "the user reads
this all day." If that decision is being reversed, it should be reversed in
writing, in a decision that says the requirement changed — not smuggled in as a
dependency removal.

### 5. Where the boundary sits if pi is kept

pi is allowed in **six** files, and each one is a different kind of permission:

- `src/agent.ts` — the engine. The only place a session exists.
- `src/idle.ts` — the human's keyboard. ADR-001's "idle like a normal pi
  session" is paid for here with `CustomEditor` and pi's keybindings.
- `src/render.ts` — the presenter. ADR-001's rendering rule.
- `src/kanban.ts`, `src/monitor.ts` — **leaf components**. Three symbols each
  (`truncateToWidth`, `visibleWidth`, `Component`), no behaviour, no session,
  no client. The best-shaped dependency in the repo.
- `src/startup.ts` — the config read. `getAgentDir()` + `SettingsManager`,
  nothing more.

It is already absent — by design, not by luck — from `orchestrator.ts`,
`loop.ts`, `beads.ts`, `finalize.ts`, `split.ts`, `app.ts` and `main.ts`:
zero `@earendil-works` imports in all seven. The rules that keep it that way:

1. **The core speaks ports, not pi.** `SessionFactory` (`agent.ts:733`) and
   `AgentSessionLike` (`agent.ts:691-712`) sit between the runner and the SDK,
   and `asAgentPort`/`asSessionPort` (`agent.ts:2407`, `2411`) make `tsc`
   check the conformance: a pi rename stops the build instead of firing
   mid-iteration.
2. **Nothing that decides touches pi.** The orchestrator is pure —
   `test/orchestrator.test.ts:921` pins that every one of its imports is
   `import type`. `finalize.ts` commits through `vcs.ts`; `beads.ts` shells to
   `bd`. If a decision ever needs a pi call, something upstream has already
   gone wrong.
3. **Rendering and input may use pi, and should** — that is ADR-001, and the
   highlighter, the theme and the keybinding names come free with it.
4. **`models.json` stays pi's format**, and the drift tests that read pi's
   shipped source stay as the tripwire.

The point of drawing that line is that it makes this ADR reversible at a
credible price: if pi ever has to go, the change is confined to those six
files and their tests. The state machine, the board adapter, the git writer,
the finalizer, the notice, the monitor's reader and the audit's rules come
along unchanged. That optionality is what this decision buys, and writing the
rule down is the only cost.

**Follow-up (deliberately not this ticket):** a boundary test — assert the
seven core files contain no `@earendil-works` reference, and that
`kanban.ts`/`monitor.ts` import nothing from pi beyond
`truncateToWidth`/`visibleWidth`/`Component`. Today the boundary is a fact;
the test is what stops it becoming a memory.

### 6. What keeping pi buys, in this loop's own terms

1. The tool-calling loop, the abort/timeout plumbing and the `steer()`
   off-ramp in ~115 lines of factory and port code instead of two thousand.
2. "No context carried between iterations" as a **structural** fact —
   `SessionManager.inMemory()` per run plus a per-iteration `dispose()` —
   which ADR-001 names as the property the whole design rests on.
3. Output that looks like pi, which was a stated requirement rather than a
   preference: ADR-001, §2.
4. `models.json` compatibility with every existing pi install, and an audit
   (ADR-002) whose suggested keys are ones the loader will accept because a
   test opens the loader and checks.
5. The provider/compat surface — `thinkingFormat: "chat-template"`,
   `chatTemplateKwargs`, `preserve_thinking` — as a *config file* the README
   can instruct a human to edit, instead of semantics hand-implemented in a
   bespoke client.

### 7. What dropping pi buys

- **Install size.** 553 MB of `node_modules`, 434 MB of it under
  `pi-coding-agent` — and 284 MB of that is `@esbuild` platform binaries
  pulled by `@earendil-works/chord@0.85.1`'s `esbuild@0.28.1` pin, none of
  which this loop executes. Real, and the largest single number in this
  document. It is also 284 MB of disk on a machine that is about to spend
  twenty minutes and real tokens working one bead.
- **No upstream drift.** Also real, and already paid for differently: three
  pins at exactly `0.85.1`, plus drift tests that convert an upstream rename
  into a red `npm test` at boot of the suite. "Own it and find out in month
  three" is not the better version of this.
- **Full ownership of loop semantics.** Also already ours, at every point this
  loop has needed it: `noTools: "builtin"`, the allowlist/`excludeTools`
  split of ADR-009 §3, the system-prompt override through
  `DefaultResourceLoader`, custom tools through `defineTool`. Nothing in this
  repo's behaviour has ever been blocked by pi. There is no feature here
  waiting on a permission pi would not grant.

None of those three pays for §1–§4.

## `pi-plain/Dockerfile` is not evidence either way

The untracked `pi-plain/Dockerfile` installs
`@earendil-works/pi-coding-agent` globally and ends at
`ENTRYPOINT ["pi"]`: bare pi, no loop, no beads, no orchestrator, no ADRs, no
`bd`. It is the *other* experiment on this machine — "what does it look like
to run pi with nothing wrapped around it" — and it is a question this repo has
already answered differently, in the README's own state machine.

It is not evidence that removing pi from this loop is cheap. It is evidence
that pi runs, which was never the disputed part. Both things are true: bare pi
is a fine product, and this loop is not bare pi. If someone now wants the
plain container *instead of* the loop, that is a change of intent, not a
dependency change, and it belongs in its own issue.

## Alternatives considered

**Drop `pi-ai` + `pi-coding-agent`, keep `pi-tui`.** Splits the cost only on
paper. `AssistantMessageComponent` — the thing that owns the markdown renderer
and, through it, the highlighter — lives in `pi-coding-agent`, so keeping the
rendering means keeping the package that contains the engine. What is left to
drop is the engine, which is the 2,000-line half.

**Drop everything and write a client.** §1 + §3 + §4 ≈ 5,000–7,500 new lines,
and the config format becomes ours, which contradicts the README every person
installing this thing reads, and voids the drift guards (§3). The install-size
win is the only clean one, and it is bought with the entire bill.

**Keep pi but hide it from the leaf components** (copy `truncateToWidth` /
`visibleWidth` into `kanban.ts` / `monitor.ts`). The opposite of a boundary:
three more local functions to maintain, and one less guarantee that the board
truncates the way the rest of the screen truncates. Width bugs between two HUDs
on the same frame are exactly the class of bug the shared helper exists to
prevent.

**Keep pi and go all the way into its plugin host** (extensions on disk,
`discoverAndLoadExtensions`, skills). Rejected, and for the same reason the
orchestrator is pure: a loop whose behaviour can be changed by a file in a
directory is a loop whose trace cannot be replayed. `report_done` lives in
`src/agent.ts` so that reading this repo tells you what a completed run is.

**Revisit when.** The recommendation flips on any of: pi's in-memory session
semantics stop being available (compaction forced on, `inMemory` removed or
given persistence side effects); a provider or API family this deployment
needs cannot be expressed in `models.json`; the loop needs behaviour pi's
session refuses *structurally* rather than by configuration; or ADR-001's
"must look like pi" requirement is dropped, in which case §4 and the `pi-tui`
half of the boundary become separately revisitable. Each of those is its own
ticket with its own numbers — none of them is "we removed a thin wrapper".

## Pinning tests

None added: this ADR changes no code (`src/` and `test/` are untouched;
`npm run typecheck` is unchanged). It cites the tests that already hold the
properties it relies on, so the argument can be re-checked rather than
re-argued:

- `test/orchestrator.test.ts:921` — the orchestrator has no runtime imports
  at all: the strongest existing form of §5's "the core never names pi".
- `test/audit.test.ts:672-688` — the mirrored `MIN_ANSWER_TOKENS` and
  thinking budgets still match `@earendil-works/pi-ai/api/simple-options`.
- `test/audit.test.ts:690-707` — the `chatTemplateKwargs` `$var` set still
  matches `pi-coding-agent/dist/core/model-config.js`, i.e. the audit cannot
  suggest a key this pi would reject. If §3's config is ever forked, these are
  the first tests to die — which is the point.
- `test/context.test.ts:124-137` — `CONTEXT_SAFETY_TOKENS` still mirrors pi's;
  the wall `src/context.ts` measures to moves with it.
- `test/agent.test.ts` — the session contract through the `SessionFactory` /
  `AgentSessionLike` port, never through the SDK class.
- `test/render.test.ts`, `test/idle.test.ts`, `test/kanban.test.ts`,
  `test/monitor.test.ts` — the `pi-tui` surface: components, keybindings,
  width handling.

Follow-up: the boundary test named in §5.
