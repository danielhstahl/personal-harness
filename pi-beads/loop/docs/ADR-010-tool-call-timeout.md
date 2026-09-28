# ADR-010: Every tool call gets a timeout — from the harness, by shadowing the built-ins by name

- **Status:** Accepted as a **decision**, recorded by a spike. It decides the
  mechanism and the shapes; it deliberately ships no wrapper. All line numbers
  below are pi **0.85.1** as vendored in
  `pi-beads/loop/node_modules/@earendil-works/` and were re-read, not
  inherited, for this document.
- **Modules (proposed seam, for the implementing ticket):** `src/tool-timeouts.ts`
  (new: `ToolTimeoutError`, `withToolTimeout()`, env read), `src/agent.ts`
  (`SessionSpec.toolTimeoutMs`, wrapping inside `defaultSessionFactory`
  ~line 862), `src/main.ts` (`LOOP_TOOL_TIMEOUT_MS` next to
  `LOOP_WORK_TIMEOUT_MS`, line 269), `src/render.ts` (one predicate at line
  1446 so a timeout does not render as `ok`).
- **Answers, in one line each:** (1) a `customTools` entry named `bash` **wins**
  — nothing rejects it; (2) `noTools:"builtin"` + rebuilt built-ins **works but
  is rejected**, because it makes this repo's own inventory invariant lie; (3)
  **yes** — hand the inner tool a wrapper-owned `AbortController` and pi's
  `killProcessTree` fires at the wrapper's deadline, provided the wrapper also
  forwards the session's inbound signal.

## Context

The parent report: a tool call spawns a process that never exits, and the whole
run waits on it. The run is not unbounded — it has a per-iteration budget
(`LOOP_WORK_TIMEOUT_MS`, `src/main.ts:269` → `config.workTimeoutMs`,
`src/app.ts:232,810`) that ends in `session.abort()`
(`src/agent.ts:1865,1979,2038,2064`). So today a single wedged `bash` child is
paid for with **the entire iteration**: every model turn already spent is thrown
away because one tool call never came back.

What pi 0.85.1 actually offers, checked rather than assumed:

| knob | exists? | where |
| --- | --- | --- |
| per-tool-call timeout in `CreateAgentSessionOptions` | **no** | `dist/core/sdk.d.ts:10-56` — the whole option list is `cwd, agentDir, modelRuntime, model, thinkingLevel, scopedModels, noTools, tools, excludeTools, customTools, resourceLoader, sessionManager, settingsManager, sessionStartEvent` |
| a tool-options / tool-factory hook | **no** | same list; there is no place to inject per-tool config |
| per-tool timeout in settings | **no** | `dist/core/settings-manager.d.ts:115-116` carries only `httpIdleTimeoutMs` and `websocketConnectTimeoutMs` |
| the tool's own awareness of the signal | **yes** | `ToolDefinition.execute(id, params, signal, onUpdate, ctx)` — `dist/core/extensions/types.d.ts:372` |
| bash's own timeout | **yes, but opt-in per call** | the model must pass `timeout` (seconds): `tools/bash.js:26-28` schema, `:14-25` resolve + cap (`MAX_TIMEOUT_SECONDS = 2_147_483_647/1000`), `:72-77` kill, `:251-258` the `Command timed out after N seconds` / `Command aborted` messages |

There is no "pi config setting" for this. The knob is ours, which means the
wrapper is ours too, and the only question worth answering before building it is
**where a wrapper can be attached so that nothing escapes it.**

Three candidate answers were tried on the real SDK, not on the README. Every
observation quoted below came from one runnable probe,
**`pi-beads/loop/tools/tool-timeout-probe.mjs`** (tracked, sitting with the
repo's other `tools/*.mjs` demos), and can be re-run with
`node tools/tool-timeout-probe.mjs` from `pi-beads/loop/`. Sections: **Q1**
shadowing, **Q2** `noTools:"builtin"` + factories, **K1** kill, **K2**
inbound abort, **K3** a non-cooperating tool, **K4** the wrapper inside a real
session, **K5** partial output, **P0–P3** prompt/registry fidelity.

## Decision

**Shadow the built-ins by name: register a wrapper of pi's own factory-produced
definition under the same tool name through `customTools`, and let the wrapper
own the `AbortController`.** Everything else in `SessionSpec` — `builtinTools`,
`excludeTools`, `noBuiltinTools`, the system-prompt pairing — keeps its present
meaning.

### 1. Q1 — a `customTool` named `bash` shadows the built-in. It is not rejected, and not ignored.

pi's tool registry is **last-write-wins by name, with customs written last.**
`AgentSession._refreshToolRegistry()`:

```text
dist/core/agent-session.js:2112-2118  allCustomTools = [ ...extension tools,
                                 ...this._customTools.map(d → {d, sourceInfo: <sdk:name>}) ]
                                 .filter(isAllowedTool)
dist/core/agent-session.js:2119-2127  definitionRegistry = base built-in definitions (<builtin:name>)
dist/core/agent-session.js:2128-2133  for (const tool of allCustomTools) definitionRegistry.set(name, …)   ← customs overwrite
dist/core/agent-session.js:2155-2158  toolRegistry = Map(built-ins); for (t of customs) toolRegistry.set(…)  ← customs overwrite, executable path
dist/core/agent-session.js:2159       this._toolRegistry = toolRegistry
```

`setActiveToolsByName` (`agent-session.js:659-672`) then builds
`agent.state.tools` from that registry, so the shadow is the thing that runs.

Observed (probe **Q1** — `customTools: [a tool named "bash"]`, default
built-in set, no allowlist):

```text
getAllTools names: read,bash,edit,write,powershell,grep,find,ls   (one `bash`, not two)
bash source: sdk | path: <sdk:bash>
active tools: read,bash,edit,write
active bash execute -> [{"type":"text","text":"SHADOW-BASH"}]
SHADOW WINS? true
system prompt mentions bash: true   →  "- bash: marker snippet for bash"
```

Three consequences worth writing down:

- **Silence is the hazard, not the shadow.** `DefaultResourceLoader.detectExtensionConflicts`
  (`resource-loader.js:839-873`, called at `:462`) only compares
  *extension-vs-extension* registrations (`ext.tools` per extension path). A
  `customTools` array handed to the SDK never enters that check, so a
  same-name collision with a built-in — and a collision **between two of our own
  custom tools** — is never reported. Observed (probe **P3**): two
  customs both named `bash`, last one silently wins, its description served,
  its `execute` run. Naming is the whole contract here; a wrapped built-in must
  therefore be the *only* definition under that name at the point it is passed
  in, and §6 puts the wrapping in one place so that stays true.
- **It composes with the harness's existing allowlist logic.** `tools` /
  `excludeTools` are name-keyed (`isAllowedTool`, `agent-session.js:2110`), so
  a shadowed `bash` is still allowed by an allowlist that names `bash` and
  still excluded by `excludeTools: ["bash"]`. Observed (probe **P4**):
  `tools:["read","bash","report_done"], excludeTools:["edit","write"]` +
  shadows → `active tools: read,bash,report_done`.
- **The prompt follows the winning definition.** `_toolPromptSnippets` /
  `_toolPromptGuidelines` are built from the winning entry
  (`agent-session.js:2135-2146`), and `buildSystemPrompt` lists a tool *only*
  if a snippet exists for it (`system-prompt.js:41-43`: "A tool appears in
  Available tools only when the caller provides a one-line snippet"). A shadow
  that forgets `promptSnippet` therefore silently deletes the
  `- bash: Execute bash commands …` line. Observed (P2) with a snippet-less
  shadow: the only surviving `bash` line is
  `- Use bash for file operations like ls, rg, find`, which is *template*-
  generated from the active set (`system-prompt.js:56-70`), not from the tool.
  The model would keep a guideline for a tool whose description it never saw.
  The wrapper must therefore carry the real definition's metadata, which is
  free if it spreads it — see the checklist in §6.

### 2. Q2 — `noTools:"builtin"` + builtins rebuilt from pi's factories: **viable mechanically, rejected.**

Mechanically it works. `noTools:"builtin"` disables the built-in selection and
leaves custom tools enabled, so four factory-made definitions named
`read`/`bash`/`edit`/`write` can stand in for them. Observed (probe **Q2**):

```text
getAllTools names: read,bash,powershell,edit,write,grep,find,ls,report_done,report_split
  bash source: sdk | desc len: 248      read source: sdk | desc len: 303
active tools: read,bash,report_done,report_split        (edit/write present as definitions, not active)
prompt lines:  "- read: Read file contents"  "- bash: Execute bash commands (ls, grep, find, etc.)"
```

Note that pi's tool *description* text is unchanged, because it is the same
factory output. So the damage is not in pi's prose. **It is entirely in this
repo's inventory machinery**, which is the machinery that exists to keep pi's
prose honest:

- `SessionSpec.builtinTools` (`src/agent.ts:718-727`) becomes decorative. The
  factory maps it to `options.tools` (`:890-903`) or to
  `options.noTools = "builtin"` (`:884-889`). Under this option the honest
  description of the work session becomes "no built-in tools, plus four custom
  tools named after built-in tools" — a sentence that has to be said everywhere
  the field is read.
- `toolInventoryGap()` (`src/agent.ts:1291-1350`) branches on
  `noBuiltinTools === true`, and that branch's contract is
  `bareToolsetSystemPrompt` (`src/agent.ts:1249-1283`), whose whole text is
  *"This session has no built-in tools: no shell, no `bash`, no `read`…"*.
  Registering `bash` and `read` as customs while that branch is active is the
  exact lie the function was written to refuse. The fix would be a third state
  — "built-ins re-registered as customs" — i.e. weakening the check to
  accommodate the mechanism. That is backwards, and it is why this option is
  rejected rather than merely disfavoured.
- `planSplitSession` / `splitToolOptions` (`src/agent.ts:1378-1412`,
  `:1897-1912`) and `SPLIT_REPO_TOOLS` (`:1354`) all speak in built-in
  names. `NEVER_FOR_A_PLANNER = ["edit","write"]` would still protect the tree
  (exclusion is name-keyed, and probe **P4** shows it applied to shadows), but it would
  be protecting against *our own registrations* rather than against pi's
  built-ins: drop one wrapper and the guarantee is gone with it, silently, in
  the direction that matters.
- The rebuild also has a **fidelity** cost that has nothing to do with either
  mechanism and everything to do with how carefully it is done:
  `_buildRuntime` gives the built-ins their settings
  (`agent-session.js:2183-2195`) — `read: { autoResizeImages:
  settingsManager.getImageAutoResize() }`, `bash: { commandPrefix:
  settingsManager.getShellCommandPrefix(), shellPath:
  settingsManager.getShellPath() }`. A shadow built from
  `createBashToolDefinition(cwd)` with a bare cwd silently drops the shell
  prefix and the shell path. Under mechanism A this is one checklist (§6);
  under B it is four tools the harness now owns outright, plus a
  `defaultActiveToolNames` assumption
  (`agent-session.js:2208-2211`) to re-verify forever.

**Rejected.** Mechanism A gets the same kill behaviour (§3) with zero changes to
`toolInventoryGap`, `planSplitSession`, `SPLIT_REPO_TOOLS` or any prompt text.

### 3. Q3 — a wrapper-owned `AbortController` really does kill the child, and forwarding the inbound signal keeps Ctrl-C / `session.abort()` intact.

pi kills the whole process tree whenever the signal it was *given* fires — it
does not matter who owns that signal:

```text
dist/core/tools/bash.js:65-68   const onAbort = () => { if (child.pid) killProcessTree(child.pid); };
dist/core/tools/bash.js:72-77   own timeout → timedOut = true; killProcessTree(child.pid)
dist/core/tools/bash.js:82-87   signal.aborted ? onAbort() : signal.addEventListener("abort", onAbort, {once:true})
dist/core/tools/bash.js:90-96   waitForChildProcess(child) → throw "aborted" / `timeout:${timeout}`
```

`read`, `edit` and `write` observe the same signal too
(`tools/read.js:42-53`, `tools/edit.js:99-102`, `tools/write.js:37-40`), so
the controller reaches every built-in, not just the shell.

**Therefore: the wrapper must pass its own controller's signal, not the
session's, to the inner `execute`.** Measured (probe **K1**), wrapper cap
1000 ms around `sleep 45 && echo never`:

```text
K1  elapsed 1004 ms → { timedOut: true, timeoutMs: 1000, elapsedMs: 1004 }
    `ps -C sleep` immediately after return: []      and 2.5 s later: []
```

No orphan. Same command, session-registered wrapper inside a real
`createAgentSession` (K4): 1504 ms, no `sleep` left.

**Forwarding is required, not a nicety.** The chain that makes Ctrl-C work is:
`AgentSession.abort()` (`agent-session.js:1222-1228`) → `agent.abort()`
(`pi-agent-core/dist/agent.js:202-204`) → the run's
`abortController.signal` (`agent.js:198-200`) → `executeToolCalls` →
`executePreparedToolCall(prepared, signal, …)` →
`prepared.tool.execute(id, args, signal, …)`
(`pi-agent-core/dist/agent-loop.js:139`, `:460-470`). Once the wrapper
substitutes its own controller, that chain terminates *at the wrapper*. If the
wrapper does not listen to the inbound signal and re-fire its own controller,
Ctrl-C stops reaching the child at all and the child outlives the run.
Measured (K2, wrapper cap 60 s so only the inbound abort can act; inbound
aborted at 800 ms):

```text
K2  threw "Command aborted" at 802 ms; `ps -C sleep` → []
```

**Race, don't await.** A wrapper that merely awaits the inner call is only as
good as the inner tool's cooperation. The wrapper needs
`Promise.race([inner, timer])`, with the timer winning by returning — and the
inner promise given a `.catch(() => undefined)` so that a late rejection after
we have already returned does not surface as an unhandled rejection. Measured
(K3, a tool that ignores the abort signal entirely, inner = 10 000 ms,
wrapper = 700 ms):

```text
K3  returned at 702 ms with { timedOut: true }
```

The residual is honest and should be stated: for a **non-cooperating
in-process** tool, the inner promise keeps running until it finishes — JS offers
no preemption. What the wrapper buys is not "the hung work is stopped" but "**the
hung work can no longer hold the run**", plus, for anything that spawns
processes, the tree kill that was the actual complaint.

**Partial output is recoverable and should not be thrown away.** The wrapper
tees `onUpdate` into a local buffer (bash streams snapshots through it), so the
timeout result can show what the command produced before it wedged. Measured
(K5, `echo first-line; echo second-line; sleep 40`, cap 1500 ms):

```text
K5  partialChars: 23 → "… Partial output:\nfirst-line\nsecond-line\n"
```

That partial output is what makes a timeout debuggable instead of a wall. Its
limits are worth stating too: the tee only sees what the inner tool chose to
push through `onUpdate` — for bash that is throttled to ~100 ms
(`BASH_UPDATE_THROTTLE_MS`, `tools/renderers/bash.js:15`, consumed at
`tools/bash.js:187`) — and the final flush a tool normally does on a clean
return never happens here, because the kill arrives first. So the last fraction
of a second of output can be missing, and there is no way to ask a killed call
for its final bytes. A partial tail beats a blank one.

### 4. The knob: `LOOP_TOOL_TIMEOUT_MS`

- **Name:** `LOOP_TOOL_TIMEOUT_MS`. Milliseconds, matching every other
  timeout knob in the harness (`LOOP_WORK_TIMEOUT_MS`, `LOOP_GIT_LOCK_WAIT_MS`,
  `LOOP_AUDIT_TIMEOUT_MS`, `LOOP_MONITOR_TIMEOUT_MS`,
  `LOOP_NTFY_TIMEOUT_MS`).
- **Read** with `main.ts`'s existing `number()` helper
  (`src/main.ts:33-38`), so empty and non-finite both mean "unset". Pass it
  down beside `workTimeoutMs` (`src/app.ts:232,810`) as `toolTimeoutMs`.
- **Unset (or empty, non-finite, `<= 0`) → no per-tool cap.** That is today's
  behaviour exactly: the wrapper is not installed, `execute` is pi's, and
  nothing about a call changes. The default must be *off* rather than a number
  chosen here, because no value in this file knows how long a real `npm ci`
  takes in the repository it is pointed at. A cap nobody asked for is a cap
  that fails a working run at 2 a.m.
- **Set → the cap applies to every tool call in the session**: the built-ins
  the session has, and the harness's own `report_done` / `report_split`.
  The report tools are pure validation and will never reach it, and they are
  wrapped anyway, because a wrapper with a documented hole is a wrapper you
  have to remember.
- **Relation to the run budget.** `LOOP_TOOL_TIMEOUT_MS` is nested inside
  `LOOP_WORK_TIMEOUT_MS`. Nothing enforces the ordering. If someone sets
  the per-tool cap larger than the run budget, the per-tool cap never fires and
  the run keeps being paid for the way it is paid today. Worth a one-line
  startup warning, not a refusal.
- **It does not rewrite the model's own `timeout` argument.** bash's per-call
  `timeout` still works, and the earlier of the two wins: if the model says
  `timeout: 5`, bash's own path fires first and produces pi's ordinary
  `Command timed out after 5 seconds` error — **not** a `details.timedOut`
  result, because that path is pi's, not the wrapper's. `LOOP_TOOL_TIMEOUT_MS`
  is a backstop against calls that never return, not a replacement for
  well-chosen per-call timeouts.
- **Clamp** as elsewhere: `Math.max(1, ms)`
  (`src/health.ts:120-121`, `src/monitor.ts:439-440`).
- **`LOOP_TOOL_TIMEOUT_MS` is a cap, not a floor.** A fast tool is not slowed
  to it, and it is not a deadline for the *run*.

### 5. The shape of a timed-out tool call

Two artifacts, deliberately: a **distinct error class** for the control flow
inside the wrapper, and a **result object** for everything outside it.

```ts
/** Thrown by the wrapper's timer branch; caught by the wrapper itself. */
export class ToolTimeoutError extends Error {
  readonly timedOut = true;
  constructor(
    readonly toolName: string,
    readonly timeoutMs: number,
    readonly elapsedMs: number,
    readonly partialOutput: string,
  ) {
    super(`tool ${toolName} timed out after ${timeoutMs}ms`);
    this.name = "ToolTimeoutError";
  }
}
```

and what the model, the transcript and the runner actually receive:

```ts
{
  content: [{
    type: "text",
    text:
      "Tool call `bash` timed out after 15000ms and was killed. " +
      "Partial output before the kill:\n<streamed output, if any>\n" +
      "The process tree was terminated. Re-run with a smaller command " +
      "or an explicit `timeout` if this is expected to be slow.",
  }],
  details: {
    timedOut: true,
    tool: "bash",
    timeoutMs: 15000,
    elapsedMs: 15004,
    partialChars: 23,
  },
}
```

**The wrapper returns this. It does not throw it.** This is the load-bearing
decision in the whole ADR, and the reason is typed:

- `AgentToolResult` has **no `isError` field**
  (`pi-agent-core/dist/types.d.ts:317-331` — `content`, `details`, `usage`,
  `addedToolNames`, `terminate`). The error flag is set by *throwing*, not by
  the return value.
- And the throw path **discards `details`**:
  `executePreparedToolCall` catches and calls
  `createErrorToolResult(error.message)`
  (`pi-agent-core/dist/agent-loop.js:480-486`), which builds
  `{ content: [{type:"text", text: message}], details: {} }`
  (`agent-loop.js:526-531`).

So throwing buys `isError: true` and costs `details.timedOut` — and
`details.timedOut` is precisely the field later tickets need, because it is the
only thing that distinguishes *"the harness capped this call"* from *"the tool
failed on its own"*, which is the difference between a run that should be
retried and a run that should not. Return the object.

**The cost of that choice, and where to pay it.** With a returned result, the
transcript's `isError` stays `false`, and this harness's renderer derives its
status from that flag:
`block.finish(record?.result, record?.isError === true)`
(`src/render.ts:1446`, into `finish(result, isError)` at `:452-455`). Left
alone, a timeout would render as `ok`, which is exactly the kind of result
this codebase does not ship. The fix is one predicate in a module the harness
owns — treat `details?.timedOut === true` as an error there too — and not a
fight with the SDK.

If some later ticket genuinely needs a true `isError` in the transcript *and*
the details, pi does have the hook, and it should be named here so nobody has
to rediscover it: `AgentSession.afterToolCall`
(`agent-session.js:244-270`) replaces `content`, `details` **and** `isError`
(`AfterToolCallResult`, `pi-agent-core/dist/types.d.ts:62-77`), reached from
an extension's `tool_result` handler
(`dist/core/extensions/types.d.ts:939-940`). It is not needed for this knob —
the wrapper can return the whole result — and standing up an extension to flip
one boolean is more machinery than the render.ts predicate. It is the escape
hatch if the requirement grows.

### 6. Where the seam goes

So the implementing ticket does not have to re-decide anything:

1. **`src/tool-timeouts.ts`** — the only module that owns the timeout, as
   `src/gitlock.ts` owns spawn-and-kill (ADR-006). Exports `ToolTimeoutError`,
   `withToolTimeout(definition, timeoutMs): ToolDefinition` implementing the
   K1/K2/K3 shape: build a controller, tee `onUpdate`, forward the inbound
   signal, race the inner call against the timer, return the §5 result on the
   timeout branch, rethrow the inner error otherwise, keep a
   `.catch(() => undefined)` on the losing promise, `clearTimeout` and drop
   the inbound listener in a `finally`.
2. **`SessionSpec.toolTimeoutMs?: number`** and wrapping **inside
   `defaultSessionFactory`** (`src/agent.ts:862`), immediately before
   `options.customTools` is assembled (`:879`). The shadow definitions are
   injected *there*, not smuggled through the caller-visible
   `spec.customTools`. That keeps `toolInventoryGap()` (`:1291`) looking at
   exactly `spec.builtinTools` + the caller's own report tools, so the
   invariant it guards — *the prompt names every tool the session has* — is
   untouched, and `bareToolsetSystemPrompt` / `planningSystemPrompt` need no
   new clause. `planSplitSession`, `splitToolOptions`, `SPLIT_REPO_TOOLS`,
   `NEVER_FOR_A_PLANNER`: unchanged. The prompt override travels separately —
   `spec.systemPromptOverride` becomes a `DefaultResourceLoader` handed to
   `createAgentSession` (`src/agent.ts:907-916`), a different axis from the
   tool registry — so wrapping a tool and replacing a prompt can never see each
   other, and neither has to be re-verified because the other changed.
3. **Fidelity checklist** when rebuilding the built-in definitions, mirroring
   `agent-session.js:2183-2195`, reading from the **same `SettingsManager`
   instance the harness hands to `createAgentSession`** (today that is the
   `SettingsManager.inMemory(...)` built at `src/agent.ts:871`;
   `SettingsManager.inMemory` does not read disk, so any other source is a
   divergence):
   - `createReadToolDefinition(cwd, { autoResizeImages: sm.getImageAutoResize() })`
   - `createBashToolDefinition(cwd, { commandPrefix: sm.getShellCommandPrefix(), shellPath: sm.getShellPath() })`
   - `createEditToolDefinition(cwd)`, `createWriteToolDefinition(cwd)`
   - all four exported from the package root and confirmed importable.
   - **Spread, don't rebuild**: `{ ...def, execute: wrapped }`. The fields pi
     forwards into the runtime are the fixed list at
     `tools/tool-definition-wrapper.js:2-13` (`name`, `label`,
     `description`, `parameters`, `constrainedSampling`,
     `prepareArguments`, `executionMode`), plus `promptSnippet` /
     `promptGuidelines` read at `agent-session.js:2135-2146`. A spread keeps
     every one of them. Verified (probe **P1**): all four built-ins
     shadowed with spread-through wrappers → **`system prompt identical to
     baseline: true`**, `bash description identical: true`, `bash source: sdk
     <sdk:bash>`, `active tools: read,bash,edit,write` (identical to the
     unshadowed baseline).
4. **Which tools get wrapped**: the names in `spec.builtinTools`, or pi's
   default four when it is unset (`agent-session.js:2208-2211`), plus every
   entry of `spec.customTools`. In a `noBuiltinTools` session there is nothing
   built-in to shadow, so only the customs are wrapped.
5. **The hole, named.** The wrapper covers what it registered. A tool activated
   later through `setActiveToolsByName` (`agent-session.js:659`, exposed to
   extensions at `:2047`, and `refreshTools: () => this._refreshToolRegistry()`
   at `:2048`) is **not** wrapped. Today the harness never calls it and loads no
   extensions, so the hole is empty. If that ever changes, wrap at that seam too
   — do not assume §6.4 covered a tool that appeared after the session opened.

## What did not change

- **Nothing was implemented.** This file is the whole diff (plus one README
  index line). `src/agent.ts`, `src/main.ts`, `src/render.ts` are untouched;
  `npm run typecheck` is unchanged.
- **The run budget stands.** `LOOP_WORK_TIMEOUT_MS` and its `session.abort()`
  remain the outer bound. §4 is a second, finer bound inside it, not a
  replacement: a model turn that hangs, or a stream that stalls, is still the
  run timeout's job.
- **pi still owns the tools' behaviour.** The wrapper changes *when a call is
  given up on*, not what `bash`/`read`/`edit`/`write` do. It calls pi's own
  `execute`; it does not reimplement a shell.
- **The prompts stay.** Same default system prompt, same split prompts, same
  inventory pairing. P1 shows a faithful shadow produces a byte-identical
  prompt; anything that changes the prompt is a different decision and needs
  its own ADR.
- **The bash tool's per-call `timeout` parameter is untouched** and still the
  better tool when the model knows what it is doing.

## Alternatives considered, and what killed each one

**Inject `timeout` into the bash args instead of owning a controller.**
Rejected: it only exists for `bash` (`tools/bash.js:26-28`); `read`, `edit`,
`write`, `grep`, `find`, `ls` and our report tools have no such parameter, so
"every call has a cap" dies immediately. It also rewrites the model's
arguments, which is a different kind of intervention than wrapping a call.

**`noTools: "builtin"` + builtins rebuilt and registered as customs.**
Rejected in §2: it works, and it makes `toolInventoryGap()`'s
`noBuiltinTools` branch — and the prose in
`bareToolsetSystemPrompt` that branch exists to keep honest — describe the
opposite of the session it guards.

**`baseToolsOverride`.** The most "intended-looking" hook there is, and it is
not reachable: it exists on `AgentSessionConfig`
(`dist/core/agent-session.d.ts:127-133`, consumed at
`agent-session.js:2186-2194, 2208-2211`), but `createAgentSession` never
passes it through (`sdk.js:253-267`) and neither does
`createAgentSessionFromServices` (`agent-session-services.js:116-135`), and
`CreateAgentSessionOptions` has no such field (`sdk.d.ts:10-56`). Even
`createAllToolDefinitions` / `allToolNames` are not exported from the package
root (verified at runtime: `'createAllToolDefinitions' in pi → false`,
`'allToolNames' in pi → false`, `'createToolDefinition' in pi → false`).
Reaching `baseToolsOverride` means constructing `new AgentSession(...)` by
hand and taking on `initialActiveToolNames`/`defaultActiveToolNames`
(`agent-session.js:2208-2211`) ourselves — replacing the factory this repo's
whole amnesia story (`SessionManager.inMemory` + compaction off,
`src/agent.ts:~862-880`) is built on, to avoid wrapping four definitions.
Not the trade. Re-check if a future pi version plumbs it.

**Extension `tool_call` / `tool_result` interception.** Rejected for the
timeout itself: `beforeToolCall` can *refuse* a call
(`BeforeToolCallResult.block`, `pi-agent-core/dist/types.d.ts:40-48`) and
`afterToolCall` can *annotate* one after it settles
(`agent-session.js:244-270`). Neither can interrupt a call that never settles,
which is the entire problem. (Their value for the *error flag* is noted in §5.)

**Watch the clock in the interpreter (`src/loop.ts`) instead of in the tool.**
Rejected: the interpreter's only lever over a running session is
`session.abort()`, which loses the iteration — the same price as today's run
timeout, paid with more code. Keeping the timeout below the session boundary is
what makes the difference between losing a call and losing a run.

**A global watchdog over all child processes.** Rejected: pi already tracks
detached child pids (`tools/bash.js:61-62` `trackDetachedChildPid`, `:99-101`
untrack in `finally`), and a sweeper that kills outside a tool call cannot say
*which call* timed out. The transcript would have a killed process and no
story, which is worse than a slow call you can see.

**A per-tool knob (`LOOP_BASH_TIMEOUT_MS` etc.).** Deferred, not rejected. The
wrapper takes an explicit `timeoutMs` per definition, so per-tool values are a
config-shape change and nothing else. Ship one knob, see whether one value is
actually wrong for one tool before inventing a table.

## Reproducing the evidence

```sh
cd pi-beads/loop && node tools/tool-timeout-probe.mjs
```

That single file runs every check cited above. It builds real sessions against
the real `ModelRuntime` (model resolution only — no prompt is sent, so no
network and no provider key is exercised) and calls `execute` directly. The
kill checks count surviving `sleep` processes with `ps -C sleep` before the
call, immediately after it returns, and again 2.5 s later. Output shape, from
a run of the probe:

```text
Q1  SHADOW WINS? true                     bash source: sdk | path: <sdk:bash>
Q2  active tools: read,bash,report_done,report_split
K1  elapsed 1003 ms → details.timedOut=true; `ps -C sleep` → [] and 2.5 s later → []
K2  threw "Command aborted" at 807 ms;    sleep procs → []
K3  stubborn inner (10 s) returned at 703 ms with details.timedOut=true
K4  registered tool is the wrapper: true; elapsed 1503 ms; no `sleep` left
K5  partialChars: 23 → "… Partial output:\nfirst-line\nsecond-line\n"
P1  SYSTEM PROMPT IDENTICAL TO BASELINE: true
P2  snippet-less shadow → the `- bash: …` line is gone from Available tools
P3  two customs both named `bash` → "SECOND BASH" served, no diagnostic
```

One probe gotcha worth recording, because it will bite whoever writes the real
wrapper: calling `createBashToolDefinition(...).execute(...)` with a stub ctx
throws `Cannot read properties of undefined (reading 'getSessionId')` at
`tools/bash.js:128` — the bash tool reads `ctx.sessionManager` when
`exposeSessionEnvironment` is on (the default). **The wrapper must pass the
ctx it received through unchanged**; synthesising a thin one breaks bash, and
a wrapper that drops ctx would break it the same way inside the session.

## Open questions for the implementing ticket

1. Should a run that hit a tool timeout get a distinct outcome/notice, or is
   the tool result plus the render fix enough? (§5 makes the transcript
   queryable either way: `details.timedOut`.)
2. Startup warning when `LOOP_TOOL_TIMEOUT_MS >= LOOP_WORK_TIMEOUT_MS`, or
   stay silent? Decided: warning, not refusal — but the wording has to say the
   per-tool cap will simply never fire.
3. Whether the split session should get a **tighter** cap than the work
   session. `SPLIT_REPO_TOOLS` includes `bash` and the planner has no business
   running anything long; a per-session value would come straight from
   `planSplitSession`, which already returns the whole tool decision.
