/**
 * Tests for `src/audit.ts` — the startup comparison.
 *
 * The fixture pair matters: `test/fixtures/health.json` is a real report from a
 * real inference server, and the config below is the shape this repo actually
 * ships. Every assertion is a claim that the audit *sees* something an operator
 * would otherwise find the expensive way — mid-ticket, after a budget was spent.
 *
 * A rule that can only be demonstrated with a contrived payload is a rule nobody
 * needed; a rule that fires on this pair is one that paid for itself on the
 * first start.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  applySuggestions,
  auditHeader,
  auditProvider,
  deriveModelConfigView,
  displayPath,
  levelsInPlay,
  notableFindings,
  piBudgetLevel,
  PI_CHAT_TEMPLATE_VARS,
  PI_MIN_ANSWER_TOKENS,
  piThinkingBudget,
  PI_THINKING_BUDGETS,
  rawPathFor,
  redactSecrets,
  renderAudit,
  renderFinding,
  summariseAudit,
  VERDICT_ROOM_TOKENS,
} from "../src/audit.ts";
import type { AuditFinding, AuditLevels, AuditReport, JsonRecordLike } from "../src/audit.ts";

const HEALTH: JsonRecordLike = JSON.parse(
  readFileSync(fileURLToPath(new URL("./fixtures/health.json", import.meta.url)), "utf8"),
);

const PROVIDER = "halogen";
const MODEL = "halogen-qwen3.8-flash-next";
const HEALTH_URL = "http://inference.test:8081/health";

/** The config as this repo ships it: a chat-template provider with a 256K window. */
function providerConfig(): JsonRecordLike {
  return {
    baseUrl: "http://inference.test:8081/v1",
    api: "openai-completions",
    apiKey: "hello",
    compat: {
      supportsDeveloperRole: false,
      supportsReasoningEffort: false,
      thinkingFormat: "chat-template",
      chatTemplateKwargs: { preserve_thinking: true },
    },
  };
}

function modelEntry(overrides: JsonRecordLike = {}): JsonRecordLike {
  return {
    id: MODEL,
    reasoning: true,
    maxTokens: 16_384,
    contextWindow: 256_000,
    samplingParams: {
      temperature: 1.0,
      top_p: 0.95,
      top_k: 20,
      min_p: 0.0,
      presence_penalty: 0.0,
    },
    ...overrides,
  };
}

interface AuditOptions {
  readonly model?: JsonRecordLike;
  readonly provider?: JsonRecordLike;
  readonly levels?: AuditLevels;
  readonly toolSchemas?: Record<string, unknown>;
  readonly health?: JsonRecordLike;
}

function auditWith(options: AuditOptions = {}): AuditReport {
  const view = deriveModelConfigView({
    provider: PROVIDER,
    providerConfig: options.provider ?? providerConfig(),
    modelConfig: options.model ?? modelEntry(),
  });
  return auditProvider({
    view,
    target: { provider: PROVIDER, modelId: view.modelId, modelIndex: 0 },
    healthUrl: HEALTH_URL,
    health: options.health === undefined ? HEALTH : (options.health as never),
    levels: options.levels ?? {},
    toolSchemas: options.toolSchemas ?? {},
  });
}

function only(report: AuditReport, id: string): AuditFinding | undefined {
  return report.findings.find((item) => item.id === id);
}

function ids(report: AuditReport): string[] {
  return report.findings.map((item) => item.id);
}

function severity(report: AuditReport, id: string): string | undefined {
  return only(report, id)?.severity;
}

function suggestion(report: AuditReport, id: string, field: string): unknown {
  return only(report, id)?.suggestions?.find((item) => item.field === field)?.value;
}

// ── the headline: what this repo's config gets told ───────────────────────

test("the shipped config is compared against the real report and the real gaps show", () => {
  const report = auditWith();
  // Window declared short, thinking with no room to answer, the level never sent,
  // strict schemas against a server that cannot enforce them.
  for (const id of [
    "context-window-undersized",
    "output-starves-the-answer",
    "thinking-effort-never-sent",
    "thinking-enable-never-sent",
    "strict-mode-without-decoding",
  ]) {
    assert.equal(severity(report, id), "warn", `${id} should warn against this config`);
  }
  assert.equal(report.blocking, false, "none of those is a 'do not start' — they are a 'fix this'");
});

test("the window finding suggests exactly what the server serves", () => {
  const report = auditWith();
  assert.equal(suggestion(report, "context-window-undersized", "contextWindow"), HEALTH.context);
  assert.equal(HEALTH.context, 262_144);
});

test("a small shortfall is rounding and is not reported", () => {
  // 1% of 262144 is ~2621, so a 1024-token gap is inside the tolerance.
  const near = auditWith({ model: modelEntry({ contextWindow: 262_144 - 1_000 }) });
  assert.equal(severity(near, "context-window-undersized"), undefined);
  const past = auditWith({ model: modelEntry({ contextWindow: 262_144 - 5_000 }) });
  assert.equal(severity(past, "context-window-undersized"), "warn");
});

test("declaring more window than the server holds is an error, not a hint", () => {
  const report = auditWith({ model: modelEntry({ contextWindow: 300_000 }) });
  assert.equal(severity(report, "context-window-oversized"), "error");
  assert.equal(report.blocking, true);
  assert.equal(suggestion(report, "context-window-oversized", "contextWindow"), 262_144);
});

test("the smallest server limit is the window, not the biggest", () => {
  // A 262K window is not usable when the KV pool underneath is 128K.
  const health = { ...HEALTH, kv_pool_positions: 131_072 } as JsonRecordLike;
  const report = auditWith({ model: modelEntry({ contextWindow: 262_144 }), health });
  assert.equal(severity(report, "context-window-oversized"), "error");
  assert.equal(suggestion(report, "context-window-oversized", "contextWindow"), 131_072);
});

test("an undeclared window is a warning that nothing is clamped", () => {
  const report = auditWith({ model: modelEntry({ contextWindow: 0 }) });
  assert.equal(severity(report, "context-window-undeclared"), "warn");
});

// ── the output ceiling and the thinking budget ────────────────────────────

test("a ceiling that only covers thinking leaves the verdict nowhere to go", () => {
  const report = auditWith({ levels: { work: "high" } });
  const found = only(report, "output-starves-the-answer");
  assert.ok(found, "high thinking against a 16384 ceiling must warn");
  const suggested = suggestion(report, "output-starves-the-answer", "maxTokens");
  assert.equal(typeof suggested, "number");
  const ceiling = suggested as number;
  const highBudget = PI_THINKING_BUDGETS.high ?? 0;
  assert.ok(highBudget > 0, "the mirrored table must carry a `high` budget");
  assert.ok(
    ceiling >= highBudget + PI_MIN_ANSWER_TOKENS + VERDICT_ROOM_TOKENS,
    "the suggestion must actually carry the thinking budget plus the answer",
  );
  assert.ok(ceiling <= (HEALTH.max_tokens_cap as number), "and stay under the server's cap");
});

test("a ceiling that carries the level and the answer is left alone", () => {
  const report = auditWith({ model: modelEntry({ maxTokens: 32_768 }), levels: { work: "high" } });
  assert.equal(severity(report, "output-starves-the-answer"), undefined);
  assert.equal(severity(report, "output-cap"), "ok");
});

test("a ceiling over the server's cap is an error that carries a usable number", () => {
  const report = auditWith({ model: modelEntry({ maxTokens: 70_000 }) });
  assert.equal(severity(report, "output-cap-exceeded"), "error");
  assert.ok((suggestion(report, "output-cap-exceeded", "maxTokens") as number) <= 65_536);
});

test("when reasoning does not share the budget, the squeeze check is void", () => {
  const health = { ...HEALTH, token_budget_covers_reasoning: false } as JsonRecordLike;
  const report = auditWith({ health, levels: { work: "high" } });
  assert.equal(severity(report, "output-starves-the-answer"), undefined, "nothing is squeezed out of what");
});

test("with reasoning off the ceiling is only checked against the cap", () => {
  const report = auditWith({ model: modelEntry({ reasoning: false, maxTokens: 4_096 }) });
  assert.equal(severity(report, "output-starves-the-answer"), undefined);
  assert.equal(severity(report, "output-cap"), "ok");
});

// ── the thinking knobs ───────────────────────────────────────────────────

test("a chat-template format with no effort kwarg never sends the level", () => {
  const report = auditWith();
  const found = only(report, "thinking-effort-never-sent");
  assert.ok(found);
  const scoped = found?.suggestions?.find(
    (item) =>
      item.scope === "provider" && item.field === "compat.chatTemplateKwargs.reasoning_effort",
  );
  assert.deepEqual(scoped?.value, { $var: "thinking.effort" });
});

test("send the effort as a kwarg and the finding goes away", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).chatTemplateKwargs = {
    preserve_thinking: true,
    reasoning_effort: { $var: "thinking.effort" },
  };
  const report = auditWith({ provider });
  assert.equal(severity(report, "thinking-effort-never-sent"), undefined);
  assert.equal(severity(report, "thinking-effort-sent"), "ok");
});

test("`off` needs enable_thinking or the template decides", () => {
  const report = auditWith();
  assert.equal(severity(report, "thinking-enable-never-sent"), "warn");
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).chatTemplateKwargs = {
    enable_thinking: { $var: "thinking.enabled" },
  };
  assert.equal(severity(auditWith({ provider }), "thinking-enable-never-sent"), undefined);
});

test("qwen-chat-template is called out: it sends no effort and cannot be told to", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).thinkingFormat = "qwen-chat-template";
  const report = auditWith({ provider });
  assert.equal(severity(report, "thinking-effort-never-sent"), "warn");
  const fields = only(report, "thinking-effort-never-sent")?.suggestions?.map((item) => item.field);
  assert.ok(fields?.includes("compat.thinkingFormat"), "it suggests the format to switch to");
});

test("a top-level format that blocks reasoning_effort is reported against the flag", () => {
  const provider = providerConfig();
  delete (provider.compat as JsonRecordLike).thinkingFormat;
  const report = auditWith({ provider });
  assert.equal(severity(report, "thinking-effort-never-sent"), "warn");
  assert.equal(
    only(report, "thinking-effort-never-sent")?.suggestions?.find(
      (item) => item.field === "compat.supportsReasoningEffort",
    )?.value,
    true,
  );
});

test("a kwarg the template does not declare is reported and removable", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).chatTemplateKwargs = {
    preserve_thinking: true,
    keep_thinking_forever: true,
  };
  const report = auditWith({ provider });
  const found = only(report, "chat-template-kwargs-unknown");
  assert.equal(found?.severity, "warn");
  assert.match(found?.evidence ?? "", /keep_thinking_forever/);
  assert.equal(found?.suggestions?.[0]?.remove, true);
});

test("a $var this pi's schema cannot validate is an error", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).chatTemplateKwargs = {
    thinking_amount: { $var: "thinking.budget" },
  };
  const report = auditWith({ provider });
  assert.equal(severity(report, "chat-template-var-unknown"), "error");
});

test("a level name the server has no value for comes with a mapping", () => {
  const report = auditWith({ levels: { work: "max" } });
  assert.equal(severity(report, "effort-value-unsupported"), "warn");
  const mapped = suggestion(report, "effort-value-unsupported", "thinkingLevelMap") as JsonRecordLike;
  assert.equal(mapped.max, "xhigh", "max maps up to the biggest name the server knows");
});

test("a level the server knows needs no mapping", () => {
  const report = auditWith({ levels: { work: "high", split: "medium" } });
  assert.equal(severity(report, "effort-value-unsupported"), undefined);
});

test("a configured level beats the server's default in the maths", () => {
  const cheap = auditWith({ levels: { work: "low", split: "minimal" } });
  assert.equal(
    severity(cheap, "output-starves-the-answer"),
    undefined,
    "low/minimal budgets fit inside 16384 with room to spare",
  );
  const serverDefaultOnly = auditWith({});
  assert.equal(
    severity(serverDefaultOnly, "output-starves-the-answer"),
    "warn",
    "with nothing configured the server's own xhigh is what runs, and it does not fit",
  );
});

test("levelsInPlay prefers what was configured over what the server would default to", () => {
  assert.deepEqual(levelsInPlay({ work: "low", split: "minimal", serverDefault: "xhigh" }), [
    "low",
    "minimal",
  ]);
  assert.deepEqual(levelsInPlay({ serverDefault: "xhigh" }), ["xhigh"]);
  assert.deepEqual(levelsInPlay({ work: "off", split: "off" }), [], "off is not a budget");
  assert.deepEqual(levelsInPlay({ work: "low", split: "low" }), ["low"], "duplicates collapse");
});

test("pi's budget mapping is mirrored: off is nothing, xhigh and max bill at high", () => {
  assert.equal(piThinkingBudget(undefined), undefined);
  assert.equal(piThinkingBudget("off"), undefined);
  assert.equal(piBudgetLevel("max"), "high");
  assert.equal(piBudgetLevel("xhigh"), "high");
  assert.equal(piThinkingBudget("max"), PI_THINKING_BUDGETS.high);
  assert.equal(piThinkingBudget("minimal"), 1_024);
});

// ── the rest of the surface ──────────────────────────────────────────────

test("a template that was never filled in is an error before anything else", () => {
  const report = auditWith({
    provider: { baseUrl: "http://<yoururl>:8081/v1", api: "openai-completions" },
    model: modelEntry({ id: "<yourmodelid>" }),
  });
  assert.equal(severity(report, "config-placeholder"), "error");
  assert.match(only(report, "config-placeholder")?.detail ?? "", /knock-on/);
});

test("the report and the config disagreeing about which model this is gets said out loud", () => {
  const report = auditWith({ model: modelEntry({ id: "some-other-model" }) });
  assert.equal(severity(report, "model-identity"), "warn");
  assert.match(only(report, "model-identity")?.evidence ?? "", /halogen-qwen3\.8-flash-next/);
});

test("an endpoint that does not serve the configured api's route is an error", () => {
  const report = auditWith({ model: modelEntry({ api: "anthropic-messages" }) });
  assert.equal(severity(report, "endpoint-route"), "error");
  assert.match(only(report, "endpoint-route")?.title ?? "", /\/messages/);
});

test("tools are the completion contract: no tools is an error", () => {
  const health = { ...HEALTH, supported: ["stop", "max_tokens"] } as JsonRecordLike;
  const report = auditWith({ health });
  assert.equal(severity(report, "no-tool-calling"), "error");
});

test("a server with no vision tower and a config that claims images is an error", () => {
  const report = auditWith({ model: modelEntry({ input: ["text", "image"] }) });
  assert.equal(severity(report, "vision-claimed-tower-missing"), "error");
  assert.deepEqual(suggestion(report, "vision-claimed-tower-missing", "input"), ["text"]);
});

test("text-only input against a server with no vision is aligned", () => {
  const report = auditWith();
  assert.equal(severity(report, "vision-claimed-tower-missing"), undefined);
  assert.equal(severity(report, "vision"), "ok");
});

test("strict schemas against no constrained decoding are called out and fixable", () => {
  const report = auditWith();
  assert.equal(
    only(report, "strict-mode-without-decoding")?.suggestions?.[0]?.value,
    false,
  );
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).supportsStrictMode = false;
  assert.equal(severity(auditWith({ provider }), "strict-mode"), "ok");
});

test("a forced tool call that disables thinking is on the record as info", () => {
  const report = auditWith();
  assert.equal(severity(report, "forced-call-disables-thinking"), "info");
  assert.match(only(report, "forced-call-disables-thinking")?.detail ?? "", /report_done/);
});

test("sampling params that restate the server's defaults are reported as a freeze, not a win", () => {
  const report = auditWith();
  assert.equal(severity(report, "sampling-mirrors-server"), "info");
  assert.match(only(report, "sampling-mirrors-server")?.detail ?? "", /freezes/);
});

test("a sampling key the server does not implement is an error with a removal", () => {
  const report = auditWith({
    model: modelEntry({ samplingParams: { temperature: 0.7, top_logprobs: 5 } }),
  });
  assert.equal(severity(report, "sampling-not-implemented"), "error");
  assert.equal(
    only(report, "sampling-not-implemented")?.suggestions?.find((item) => item.field === "samplingParams.top_logprobs")
      ?.remove,
    true,
  );
});

test("a sampling key the server ignores is warned about separately", () => {
  const report = auditWith({ model: modelEntry({ samplingParams: { temperature: 0.7, n: 3 } }) });
  assert.equal(severity(report, "sampling-accepted-but-ignored"), "warn");
});

test("turning streaming usage off blinds the context guard, and is warned about", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).supportsUsageInStreaming = false;
  const report = auditWith({ provider });
  assert.equal(severity(report, "usage-not-requested"), "warn");
  assert.match(only(report, "usage-not-requested")?.detail ?? "", /context/);
});

test("a token-budget field the server does not take is an error", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).maxTokensField = "max_new_tokens";
  const report = auditWith({ provider });
  assert.equal(severity(report, "token-budget-field-unsupported"), "error");
});

test("a reasoning flag off on a server that can think is money going to waste", () => {
  const report = auditWith({ model: modelEntry({ reasoning: false }) });
  assert.equal(severity(report, "reasoning-flag-off"), "warn");
  assert.equal(suggestion(report, "reasoning-flag-off", "reasoning"), true);
});

test("the tool schemas are read against the server's structured-output contract", () => {
  const schemas = {
    report_split: {
      type: "object",
      properties: { priority: { type: "integer", minimum: 0, maximum: 4 } },
    },
  };
  const report = auditWith({ toolSchemas: schemas });
  assert.equal(severity(report, "tool-schema-unenforced"), "info");
  assert.match(only(report, "tool-schema-unenforced")?.evidence ?? "", /minimum/);
});

test("a schema keyword the server refuses outright is an error naming the tool", () => {
  const schemas = {
    report_done: { type: "object", properties: { summary: { type: "string", pattern: "^\\d+$" } } },
  };
  const report = auditWith({ toolSchemas: schemas });
  assert.equal(severity(report, "tool-schema-refused"), "error");
  assert.match(only(report, "tool-schema-refused")?.evidence ?? "", /report_done .*pattern/);
});

test("a property named like a keyword is not mistaken for one", () => {
  const schemas = {
    report_done: { type: "object", properties: { pattern: { type: "string" } } },
  };
  const report = auditWith({ toolSchemas: schemas });
  assert.equal(severity(report, "tool-schema-refused"), undefined);
});

test("a warm cache that is not bit-identical is said, because a re-run is not a repro", () => {
  const report = auditWith();
  assert.equal(severity(report, "prompt-cache-not-deterministic"), "info");
});

test("a server that is queueing at startup is said, because the budget is charged for it", () => {
  const health = { ...HEALTH, queued: 3, in_flight: 4 } as JsonRecordLike;
  const report = auditWith({ health });
  assert.equal(severity(report, "server-queueing"), "info");
  assert.match(only(report, "server-queueing")?.detail ?? "", /wall clock/);
});

test("what the report could not speak to is listed rather than glossed over", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).requiresToolResultName = true;
  const report = auditWith({ provider });
  const silent = only(report, "health-silent-on");
  assert.equal(silent?.severity, "info");
  assert.match(silent?.title ?? "", /requiresToolResultName/);
  assert.doesNotMatch(silent?.title ?? "", /thinkingFormat/, "a key a rule did read is not listed");
});

// ── a config that lines up: the audit should have nothing to complain about ─

test("an aligned config produces no warning and no error", () => {
  const provider = providerConfig();
  (provider.compat as JsonRecordLike).supportsStrictMode = false;
  (provider.compat as JsonRecordLike).chatTemplateKwargs = {
    enable_thinking: { $var: "thinking.enabled" },
    reasoning_effort: { $var: "thinking.effort" },
    preserve_thinking: true,
  };
  const report = auditWith({
    provider,
    model: modelEntry({ contextWindow: 262_144, maxTokens: 32_768, input: ["text"] }),
    levels: { work: "high", split: "low" },
  });
  assert.deepEqual(
    notableFindings(report).filter((item) => item.severity === "error" || item.severity === "warn"),
    [],
    `expected a clean run, got:\n${notableFindings(report)
      .map((item) => `${item.severity} ${item.id}`)
      .join("\n")}`,
  );
});

// ── patching and rendering ───────────────────────────────────────────────

test("suggestions are deduplicated by the path they touch", () => {
  const report = auditWith();
  const paths = report.suggestions.map((item) => `${item.scope}:${item.field}`);
  assert.equal(new Set(paths).size, paths.length, "two rules must not both claim one key");
  assert.ok(paths.length > 0);
});

test("applying the suggestions rewrites the config at exactly the right paths", () => {
  const raw = {
    providers: {
      [PROVIDER]: { ...providerConfig(), models: [modelEntry()] },
    },
  };
  const report = auditWith();
  const patched = applySuggestions(raw, report.suggestions, {
    provider: PROVIDER,
    modelId: MODEL,
    modelIndex: 0,
  }) as JsonRecordLike;
  const provider = (patched.providers as JsonRecordLike)[PROVIDER] as JsonRecordLike;
  const model = (provider.models as JsonRecordLike[])[0] as JsonRecordLike;
  assert.equal(model.contextWindow, 262_144, "the window was raised to what the server serves");
  assert.equal((model.maxTokens as number) > 16_384, true, "the ceiling was lifted");
  const kwargs = ((provider.compat as JsonRecordLike).chatTemplateKwargs as JsonRecordLike) ?? {};
  assert.deepEqual(kwargs.reasoning_effort, { $var: "thinking.effort" });
  assert.deepEqual(kwargs.enable_thinking, { $var: "thinking.enabled" });
  assert.equal(kwargs.preserve_thinking, true, "what was right is left alone");
  assert.equal(raw.providers[PROVIDER] && (raw.providers[PROVIDER] as JsonRecordLike) === provider, false, "the input was not mutated");
});

test("a model with no raw entry is patched through modelOverrides", () => {
  const suggestion = { scope: "model" as const, field: "contextWindow", value: 1, why: "x" };
  const target = { provider: "catalog", modelId: "catalog-model" };
  assert.deepEqual(rawPathFor(target, suggestion), [
    "providers",
    "catalog",
    "modelOverrides",
    "catalog-model",
    "contextWindow",
  ]);
  assert.equal(
    displayPath(target, suggestion),
    "providers.catalog.modelOverrides.catalog-model.contextWindow",
  );
  const patched = applySuggestions({}, [suggestion], target) as JsonRecordLike;
  const override = ((patched.providers as JsonRecordLike).catalog as JsonRecordLike).modelOverrides as JsonRecordLike;
  assert.deepEqual((override["catalog-model"] as JsonRecordLike).contextWindow, 1);
});

test("a remove suggestion deletes the key rather than nulling it", () => {
  const raw = { providers: { p: { models: [{ id: "m", samplingParams: { top_logprobs: 3, seed: 1 } }] } } };
  const patched = applySuggestions(
    raw,
    [{ scope: "model", field: "samplingParams.top_logprobs", value: null, why: "gone", remove: true }],
    { provider: "p", modelId: "m", modelIndex: 0 },
  ) as JsonRecordLike;
  const sampling = (((patched.providers as JsonRecordLike).p as JsonRecordLike).models as JsonRecordLike[])[0]!;
  const params = sampling.samplingParams as JsonRecordLike;
  assert.equal("top_logprobs" in params, false);
  assert.equal(params.seed, 1, "its neighbour survives");
});

test("a literal API key never reaches the printable form", () => {
  const redacted = redactSecrets({
    providers: { p: { apiKey: "sk-super-secret", baseUrl: "http://x", headers: { Authorization: "Bearer zzz" } } },
  }) as JsonRecordLike;
  const provider = (redacted.providers as JsonRecordLike).p as JsonRecordLike;
  assert.equal(provider.apiKey, "«redacted»");
  assert.equal((provider.headers as JsonRecordLike).Authorization, "«redacted»");
  assert.equal(provider.baseUrl, "http://x");
});

test("an env reference is not a secret and survives redaction", () => {
  const redacted = redactSecrets({ apiKey: "$MY_KEY", token: "${OTHER}" }) as JsonRecordLike;
  assert.equal(redacted.apiKey, "$MY_KEY");
  assert.equal(redacted.token, "${OTHER}");
});

test("rendering hides the checks that passed unless they were asked for", () => {
  const report = auditWith();
  const plain = renderAudit(report).join("\n");
  assert.ok(!plain.includes("ok    "), "passing checks are not shown by default");
  const verbose = renderAudit(report, { showOk: true }).join("\n");
  assert.ok(verbose.includes("ok  "), "with showOk the passing checks appear");
  assert.match(plain, /provider audit — halogen\//);
  assert.match(plain, /suggested change/);
});

test("the rendered finding carries the evidence and the patch on separate lines", () => {
  const report = auditWith();
  const item = only(report, "context-window-undersized");
  assert.ok(item);
  const text = renderFinding(item, report.target);
  assert.match(text, /context-window-undersized: /);
  assert.match(text, /evidence: config contextWindow=256000/);
  assert.match(text, /→ set providers\.halogen\.models\[0\]\.contextWindow = 262144/);
});

test("notableFindings puts the errors before the warnings before the notes", () => {
  const report = auditWith({ model: modelEntry({ contextWindow: 300_000, input: ["text", "image"] }) });
  const list = notableFindings(report);
  const severities = list.map((item) => item.severity);
  const firstOk = severities.indexOf("ok");
  const lastProblem = severities.lastIndexOf("warn");
  assert.ok(lastProblem < firstOk || firstOk === -1, "problems are ranked above passes");
  assert.equal(notableFindings(auditWith({})).some((item) => item.severity === "ok"), false);
});

test("the header and the summary read as one report", () => {
  const report = auditWith();
  assert.equal(auditHeader(report, 12), `provider audit — ${PROVIDER}/${MODEL} against ${HEALTH_URL} (12ms) [ok]`);
  assert.match(summariseAudit(report), /\d+ error, \d+ warn, \d+ info, \d+ ok \(\d+ check\(s\), \d+ suggested change\(s\)\)/);
  assert.deepEqual(ids(report).length, report.findings.length);
});

// ── no server: the audit still says something true ────────────────────────

test("with no report at all, only the config-local checks run", () => {
  const view = deriveModelConfigView({
    provider: PROVIDER,
    providerConfig: providerConfig(),
    modelConfig: modelEntry(),
  });
  const report = auditProvider({
    view,
    target: { provider: PROVIDER, modelId: MODEL, modelIndex: 0 },
    healthUrl: HEALTH_URL,
  });
  assert.deepEqual(ids(report).sort(), ["config-placeholders", "health-unread"].sort());
  assert.equal(report.healthState, "unread");
  assert.equal(severity(report, "health-unread"), "warn");
  assert.match(only(report, "health-unread")?.detail ?? "", /unverified/);
});

test("the real config in the repo still trips the placeholder check", () => {
  // The repo ships templates on purpose; the audit's job is to say so plainly.
  const shipped = JSON.parse(
    readFileSync(fileURLToPath(new URL("../../../models.json", import.meta.url)), "utf8"),
  ) as JsonRecordLike;
  const providers = (shipped.providers ?? {}) as JsonRecordLike;
  const name = Object.keys(providers)[0] ?? "llamacpp";
  const cfg = providers[name] as JsonRecordLike;
  const entry = (cfg.models as JsonRecordLike[])[0] as JsonRecordLike;
  const view = deriveModelConfigView({ provider: name, providerConfig: cfg, modelConfig: entry });
  const report = auditProvider({
    view,
    target: { provider: name, modelId: view.modelId, modelIndex: 0 },
    healthUrl: "http://x/health",
    health: HEALTH,
  });
  assert.equal(severity(report, "config-placeholder"), "error", "<yoururl> / <yourmodelid> must not pass");
});

// ── drift guards: the mirrored pi constants still match pi ────────────────

test("the mirrored pi budgets still match pi-ai's own (drift guard)", () => {
  const source = readFileSync(
    fileURLToPath(import.meta.resolve("@earendil-works/pi-ai/api/simple-options")),
    "utf8",
  );
  const minAnswer = source.match(/MIN_ANSWER_TOKENS = (\d+)/u);
  const budgets = source.match(/DEFAULT_THINKING_BUDGETS = \{([^}]+)\}/u);
  assert.ok(minAnswer, "pi's MIN_ANSWER_TOKENS moved or vanished");
  assert.ok(budgets, "pi's DEFAULT_THINKING_BUDGETS moved or vanished");
  assert.equal(PI_MIN_ANSWER_TOKENS, Number(minAnswer[1]));
  const budgetText = budgets[1] ?? "";
  for (const [level, value] of Object.entries(PI_THINKING_BUDGETS)) {
    const entry: RegExpMatchArray | null = budgetText.match(new RegExp(`${level}: (\\d+)`, "u"));
    assert.ok(entry, `pi no longer declares a \`${level}\` thinking budget`);
    assert.equal(value, Number(entry[1]), `the mirrored \`${level}\` budget drifted`);
  }
});

test("the chat-template $var list still matches this pi's config schema (drift guard)", () => {
  // The config schema is not an exported subpath, so it is read by path. If pi
  // moves or renames it, this fails — which is the point: the suggestion set
  // must stay inside what the loader accepts.
  const source = readFileSync(
    fileURLToPath(
      new URL(
        "../node_modules/@earendil-works/pi-coding-agent/dist/core/model-config.js",
        import.meta.url,
      ),
    ),
    "utf8",
  );
  const line = source.match(/\$var: Type\.Union\(\[([^\]]+)\]\)/u);
  assert.ok(line, "pi's chatTemplateKwargs $var schema was not found where this audit says it lives");
  const declared: string[] = [...(line[1] ?? "").matchAll(/Type\.Literal\("([^"]+)"\)/gu)].map((m) => m[1] ?? "");
  assert.deepEqual(PI_CHAT_TEMPLATE_VARS, declared);
});
