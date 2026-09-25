# ADR-002: Compare the provider's `/health` report with the config before the first ticket

- **Status:** Accepted
- **Modules:** `src/health.ts` (probe), `src/audit.ts` (comparison), `src/startup.ts` (wiring), `src/audit-cli.ts` (`npm run audit`)

## Context

`models.json` is a list of assertions about a machine that is not in this repo.
`contextWindow: 256000` is a claim. `reasoning: true` is a claim.
`thinkingFormat: "chat-template"` is a claim about what a chat template will
accept. When a claim is wrong the run pays for it, and the run is the most
expensive place in this system to learn anything:

- **Too small costs reachable context.** `src/context.ts` stops a run against the
  *declared* window, and pi sizes every turn's answer budget against it too. A
  ticket that would have fitted in the window being paid for stops short and is
  reported as "this ticket does not fit one session" — a false statement about
  the work and a true one about the config.
- **Too big costs a refusal.** A declared window over the server's, or an output
  ceiling over its `max_tokens_cap`, is a 400 — and it arrives after the session
  spent its budget getting large, which is the worst moment for it.
- **Quietly wrong is the expensive one.** A thinking level the server never
  receives; an image `input` over a server with no vision tower; a `strict` tool
  schema on an endpoint with no constrained decoding. Nothing fails loudly. The
  model simply does not do the thing the config says it was told to do, and the
  loop spends a full budget discovering that.

All three are visible in one unauthenticated `GET` before a ticket is claimed.
The server reports its own window, its output cap, its default reasoning effort,
the chat-template kwargs it accepts, whether a vision tower was loaded, which
JSON Schema keywords it enforces versus ignores versus refuses, and whether it is
already sitting on a queue. That is not documentation somebody might have
written down correctly; it is the server, answering now.

Measured against this project's endpoint, the shipped config had: a window 6,144
tokens short of what the server serves (`256000` declared against `262144`); an
output ceiling of `16384` that is *entirely* consumed by pi's own thinking
budget at the server's default level of `xhigh`, leaving nothing for the
`report_done` block the whole loop waits on; and a `chat-template` format
listing only `preserve_thinking`, while the server accepts `reasoning_effort`
and `enable_thinking` through the same kwargs — so every thinking level in the
harness, including the per-pass `LOOP_WORK_THINKING` / `LOOP_SPLIT_THINKING`
knobs, was choosing something nobody received.

## Decision

**Query the server, diff it against the config at startup, report the gaps as
data, and never touch the live config unasked.**

1. **The report lives at the base, not under the version prefix.** Pi sends
   requests to `http://host:8081/v1`; the report is `http://host:8081/health`.
   `healthUrlFor()` is the only place that transformation exists, so nothing
   else in this codebase has an opinion about where the health page is. A base
   that already names `/health` is returned unchanged, which makes the
   derivation idempotent and safe to feed back in.
2. **The comparison is pure.** `src/audit.ts` takes the report and the resolved
   model config and returns findings — no fetch, no filesystem, no clock, no
   environment. Every rule is testable against a fixture with no server, which is
   what lets the suite assert that a rule fires on *this project's real payload*
   rather than on a hypothetical one.
3. **A finding carries its own patch.** Each suggestion names a scope
   (`provider` or `model`), a dotted field, a value and a reason.
   `applySuggestions()` resolves a model that has a raw `models` entry to
   `providers.<p>.models[i].<field>` and one that does not to
   `providers.<p>.modelOverrides.<id>.<field>`, which is what that key exists
   for. Suggestions are deduplicated by path, so two rules can never quietly fight
   over one key with the later value winning.
4. **Every suggestion is a key this version of pi accepts.** A suggestion the
   config loader rejects is worse than no suggestion. The settable `$var` forms
   and the mirrored thinking budgets are pinned by drift tests that read pi's own
   schema and source, so an upstream change fails the build instead of emitting a
   config that will not load.
5. **Missing fields are skipped, never inferred.** Health payloads vary per
   server. A rule with nothing to read returns nothing, and the one summary that
   always exists lists what the report could not speak to (`health-silent-on`)
   instead of implying it was checked. An audit that guesses is worse than one
   that stays quiet, because its output gets acted on.
6. **The run never dies of the audit.** A missing file, an unreachable endpoint or
   a malformed report is a line on the screen. The audit is only ever the reason a
   run *does not start* when the operator switched `LOOP_AUDIT_STRICT` on — a
   decision taken in the environment and honoured rather than second-guessed.
7. **The live config is not rewritten by surprise.** Suggestions print by default.
   `LOOP_AUDIT_WRITE=1` writes `models.json.proposed`;
   `LOOP_AUDIT_WRITE=inplace` patches the real file and keeps `models.json.bak`.
   Printed output is secret-redacted; the written file keeps the credential it
   already had, because it lands beside a file that already holds it under the
   same trust boundary, and a redacted key there would just be a broken config.

## What the rules are for

Each rule exists because of a specific failure mode, not because a field was
available to compare:

| Rule | The failure it prevents |
| --- | --- |
| `context-window-undersized` / `-oversized` / `-undeclared` | taking the tightest of `context`, `slot_ctx`, `kv_pool_positions` as the real limit; the guard in `src/context.ts` measures the *declared* one |
| `output-cap-exceeded` / `output-starves-the-answer` | a ceiling over the server's cap is a 400; a ceiling under `thinking budget + answer room + verdict room` truncates the `report_done` block. `token_budget_covers_reasoning` decides whether the second check applies at all |
| `thinking-effort-never-sent` / `-enable-never-sent` | under `chat-template`, pi forwards exactly what `chatTemplateKwargs` lists and never reads `supportsReasoningEffort`; `qwen-chat-template` sends no effort at all. Without the kwargs the server's default runs |
| `effort-value-unsupported` | pi bills `xhigh` and `max` at the same budget but sends the *name* it was given, so an unmapped `max` arrives as a string the server cannot resolve; the fix is a `thinkingLevelMap` |
| `reasoning-flag-off` / `-server-silent` | paying for a reasoning model and running it as a plain completion, or claiming one where the server offers no knob |
| `chat-template-kwargs-unknown` / `-var-unknown` | a kwarg the template does not declare is dropped silently: the config looks set and behaves unset |
| `no-tool-calling` / `strict-mode-without-decoding` / `forced-call-disables-thinking` | no tools means no `report_done` means nothing can ever finish; no constrained decoding means `strict` is decoration; a forced call is an unthinking verdict, which is why `report_done` is asked for and never coerced |
| `vision-claimed-tower-missing` | every `read` of a PNG becomes a refused request mid-ticket, after the session walked to the file |
| `sampling-*` | implemented vs accepted-but-ignored vs mirrored-from-the-server-defaults — the last being a freeze rather than a no-op, because a field the request sends always wins |
| `usage-not-requested` / `no-streaming-usage` | the context guard eats `usage` every turn; without it the guard never fires and the wall clock becomes the only clock, which is the failure mode this loop was built to avoid |
| `tool-schema-refused` / `-unenforced` | a refused keyword (`pattern`, `allOf`, `not`, …) fails every request in the pass that registered the tool; an unenforced one (`minimum`, `maximum`, …) is a note about where the guarantee actually lives — for `report_split`'s priority range, in `src/split.ts`, not on the wire |
| `prompt-cache-not-deterministic` / `-cap`, `server-queueing` | a warm cache that is not bit-identical means a re-run is not a repro; a server already queueing is time the work budget is charged for like any other |
| `config-placeholder` | the cheapest finding to print first: an unfilled template explains every other finding as its knock-on |

## Consequences

- Startup costs one `GET` with a 2000 ms default deadline, before the board is
  read. Nothing here generates tokens; the audit cannot inflate a bill.
- `npm run audit` runs the same comparison without starting the loop and exits 1
  on a blocking finding — the same signal the loop acts on under
  `LOOP_AUDIT_STRICT`. It reads its configuration through `readEnv`, because two
  places parsing the same knobs is two places that can disagree about what a run
  means.
- The strict stop is a `LoopError("provider-audit", …)`: a plain fatal line and
  exit 2. The loop did not start, and the reason is the first thing said.
- `renderAudit()` hides the checks that passed unless asked, so a clean start is
  one line and a bad start is ranked worst-first. The passing checks are still
  reachable, because "did the audit actually look at that?" is a question a
  `LOOP_AUDIT_VERBOSE=1` run has to answer.
- The audit reads the harness's own tool schemas through
  `harnessToolSchemas()` in `src/agent.ts`, so the schemas that get compared are
  the schemas that get registered — not a copy that can drift.
- `buildApp` runs the audit inside `run()`, never at build time, and hands every
  finding through the same presenter the work stream uses. The comparison lands in
  the transcript rather than floating above it, and `overrides.audit.fetchImpl`
  keeps the whole path testable without a network.
- The mirrored pi constants (`PI_MIN_ANSWER_TOKENS`, `PI_THINKING_BUDGETS`,
  `piBudgetLevel`, `PI_CHAT_TEMPLATE_VARS`) are a second copy of facts that live
  upstream, and they are drift-tested for exactly that reason. The alternative —
  importing them — is not available: pi does not export the `models.json` schema,
  and the runtime module does not export the config-level validation.

## Rejected alternatives

- **A checklist in the README.** Rejected: a checklist is correct until the server
  changes, and the server is the thing that changes. The report is the same
  information with a witness attached.
- **Auto-patching `models.json`.** Rejected outright. A config edited by a process
  the operator did not ask to edit is a config nobody owns. `inplace` exists
  because an operator may want it, and it keeps a backup so the change is
  reviewable after the fact rather than inferred from a diff that no longer exists.
- **Probing lazily, on the first failed request.** Rejected: by then a session
  exists, a ticket is claimed, and the failure arrives as a model-facing error
  string rather than a config-facing report. The whole value is in knowing before
  the claim.
- **Warnings as fatal by default.** Rejected: a warning that stops a nightly run
  in its tracks is a warning that gets switched off within a day. Non-fatal by
  default keeps it read; `strict` is for the operator who has decided the config
  must be right and wants that enforced.
- **Reading the model through `ModelRuntime` instead of the raw `models.json`.**
  Deferred rather than rejected. The audit needs the raw JSON to produce a
  patchable object and needs the health URL before any session exists, so the
  compared view is built by `deriveModelConfigView()`, which mirrors pi's own
  composition rules — provider-then-model merge, and `input ?? ["text"]` — so it
  reports what a session would send rather than what the file literally contains.
  If the merge ever diverges from pi's, the drift tests are the place to catch it.

## Follow-ups

- Teach the audit to read `/metrics` and `/cache` (both advertised in the same
  report) so a run can be told about cache hit rate rather than just cache
  capability.
- Emit the suggestion set as a machine-readable artifact so an outer harness can
  gate a deploy on it rather than a human reading a terminal.
- Reconsider whether `supportsReasoningEffort` should be dropped from the
  suggestion set entirely: it is correct but inert under `chat-template`, and a
  suggestion that does nothing trains a reader to skip suggestions.
