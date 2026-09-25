/**
 * The startup comparison: what the server says about itself, against what the
 * config tells Pi to believe.
 *
 * `models.json` is a set of assertions — *this window is 256000*, *this model
 * thinks*, *these sampling defaults apply* — and every one of them is a claim
 * about a machine that is not in this repo. A claim that is too small costs
 * reachable context; a claim that is too big costs a 400; a claim that is
 * quietly wrong (an effort knob the server never sees, a vision tower that was
 * not loaded) costs a whole work pass before it shows up. All three are visible
 * in one GET before any ticket is claimed, which is why this module exists and
 * why it runs at startup rather than on demand.
 *
 * Three rules about this file:
 *
 * 1. **It is pure.** No fetch, no filesystem, no clock, no environment. It takes
 *    the health report and the resolved model config as data and returns
 *    findings as data, so every branch below is testable with a fixture and no
 *    server. The I/O lives in `src/health.ts` and `src/startup.ts`.
 * 2. **It reads defensively.** Health payloads differ per server and fields go
 *    missing. A missing field is a *skipped check* or an explicit `info`, never
 *    a crash and never an inferred value — an audit that guesses is worse than
 *    one that stays quiet, because its output gets acted on.
 * 3. **Every suggestion is a key this pi accepts.** A suggestion the config
 *    loader rejects is not a suggestion. The `$var` forms, the compat keys and
 *    the model keys are pinned by the drift tests in `test/audit.test.ts`.
 */
import { formatTokenCount } from "./context.ts";
import { isRecord } from "./health.ts";
import type { JsonRecord } from "./health.ts";

/** A plain JSON object, named for tests and callers. */
export type JsonRecordLike = Record<string, unknown>;

export type AuditSeverity = "error" | "warn" | "info" | "ok";

/** Where a suggestion lands: the provider block, or one model within it. */
export type SuggestionScope = "provider" | "model";

export interface AuditSuggestion {
  readonly scope: SuggestionScope;
  /** Dotted path relative to the scope, e.g. `compat.chatTemplateKwargs.reasoning_effort`. */
  readonly field: string;
  readonly value: unknown;
  readonly why: string;
  /** True → the field should be deleted rather than set. */
  readonly remove?: boolean;
}

export interface AuditFinding {
  readonly id: string;
  readonly severity: AuditSeverity;
  readonly title: string;
  readonly detail: string;
  /** The two sides of the disagreement, in `health.x=1; config.y=2` form. */
  readonly evidence?: string;
  readonly suggestions?: readonly AuditSuggestion[];
}

/** Which thinking levels are in play for the passes this loop is about to run. */
export interface AuditLevels {
  readonly work?: string;
  readonly split?: string;
  /** The level saved in the user's pi settings. */
  readonly savedDefault?: string;
  /** The level the *server* applies when the request sends none. */
  readonly serverDefault?: string;
}

/**
 * The model config as a session will see it: provider block and model entry
 * merged, with pi's own composition defaults applied. This is the "claim" side
 * of the comparison.
 */
export interface ModelConfigView {
  readonly provider: string;
  readonly modelId: string;
  readonly baseUrl: string;
  readonly api: string;
  readonly reasoning: boolean;
  readonly input: readonly string[];
  readonly contextWindow: number;
  readonly maxTokens: number;
  readonly samplingParams: JsonRecord;
  readonly compat: JsonRecord;
  readonly thinkingLevelMap: JsonRecord;
  /** False when the model came from the catalog or a `modelOverrides` entry. */
  readonly hasRawEntry: boolean;
}

/** Where a suggestion's dotted path resolves inside the raw `models.json`. */
export interface ConfigTarget {
  readonly provider: string;
  readonly modelId: string;
  /** Index into `providers.<p>.models`, or undefined → use `modelOverrides`. */
  readonly modelIndex?: number;
}

export interface AuditInput {
  readonly view: ModelConfigView;
  readonly target: ConfigTarget;
  readonly healthUrl: string;
  /** The parsed `/health` payload. Absent → only config-local checks can run. */
  readonly health?: JsonRecord;
  readonly levels?: AuditLevels;
  /** The tool JSON Schemas the harness registers, keyed by tool name. */
  readonly toolSchemas?: Readonly<Record<string, unknown>>;
}

export interface AuditCounts {
  readonly error: number;
  readonly warn: number;
  readonly info: number;
  readonly ok: number;
}

export interface AuditReport {
  readonly provider: string;
  readonly modelId: string;
  /** Where the suggestions land, carried out so a renderer prints real paths. */
  readonly target: ConfigTarget;
  readonly healthUrl: string;
  /** `ok`, `unreachable`, `absent` — the health side's own words. */
  readonly healthState: string;
  readonly findings: readonly AuditFinding[];
  readonly counts: AuditCounts;
  /** Deduplicated by config path, first suggestion wins. */
  readonly suggestions: readonly AuditSuggestion[];
  /** A finding severe enough that a strict run should not start. */
  readonly blocking: boolean;
}

// ── pi constants mirrored here (each one is drift-tested) ──────────────────

/**
 * Mirrors `MIN_ANSWER_TOKENS` in `@earendil-works/pi-ai/api/simple-options`:
 * the answer room pi keeps when thinking shares the response ceiling.
 */
export const PI_MIN_ANSWER_TOKENS = 1_024;

/** Mirrors `DEFAULT_THINKING_BUDGETS` in the same module. */
export const PI_THINKING_BUDGETS: Readonly<Record<string, number>> = {
  minimal: 1_024,
  low: 2_048,
  medium: 8_192,
  high: 16_384,
};

/**
 * Mirrors `clampReasoning`: `xhigh` and `max` are billed at the `high` budget.
 * The *name* sent to the server is unaffected — which is exactly why the
 * effort-value check has to know both forms.
 */
export function piBudgetLevel(level: string): string {
  return level === "xhigh" || level === "max" ? "high" : level;
}

/** The thinking budget pi asks for at a level, or undefined for no level. */
export function piThinkingBudget(level: string | undefined): number | undefined {
  if (level === undefined || level === "" || level === "off") return undefined;
  return PI_THINKING_BUDGETS[piBudgetLevel(level)];
}

/**
 * The `$var` forms this version of pi's `models.json` schema accepts inside
 * `chatTemplateKwargs`. pi-ai resolves more of them at runtime than the config
 * loader will validate, so a suggestion must stay inside this list.
 */
export const PI_CHAT_TEMPLATE_VARS: readonly string[] = ["thinking.enabled", "thinking.effort"];

/**
 * What this loop needs out of an answer that is not thinking: the `report_done`
 * block plus the prose around it. Priced at a kilotoken so a verdict never has
 * to share its last hundred tokens with a sentence.
 */
export const VERDICT_ROOM_TOKENS = 1_024;

/** A declared window may trail the server's by this much before it is a finding. */
const CONTEXT_TOLERANCE_TOKENS = 1_024;
const CONTEXT_TOLERANCE_RATIO = 0.01;

/** The `api` → route a request has to land on. */
const REQUIRED_ROUTE: Readonly<Record<string, string>> = {
  "openai-completions": "/chat/completions",
  "openai-responses": "/responses",
  "azure-openai-responses": "/responses",
  "openai-codex-responses": "/responses",
  "anthropic-messages": "/messages",
  "mistral-conversations": "/conversations",
};

/** Effort names, cheapest first — used to pick the nearest supported value. */
const EFFORT_ORDER: readonly string[] = ["minimal", "low", "medium", "high", "xhigh"];

/**
 * Compat keys some rule actually consulted. Anything set that is not in here
 * comes back as `health-silent-on`: the audit refuses to imply it checked.
 */
const AUDITED_COMPAT_KEYS: readonly string[] = [
  "thinkingFormat",
  "chatTemplateKwargs",
  "supportsReasoningEffort",
  "supportsUsageInStreaming",
  "maxTokensField",
  "supportsStrictMode",
];

// ── defensive readers ────────────────────────────────────────────────────

function dig(source: unknown, path: readonly string[]): unknown {
  let current: unknown = source;
  for (const element of path) {
    // A path element may itself be dotted (`"version.match"`), so each element is
    // split before it is walked. Both spellings mean the same thing here.
    for (const key of element.split(".")) {
      if (!isRecord(current)) return undefined;
      current = current[key];
    }
  }
  return current;
}

function numberAt(source: unknown, path: readonly string[]): number | undefined {
  const raw = dig(source, path);
  if (typeof raw === "number" && Number.isFinite(raw)) return raw;
  if (typeof raw === "string" && raw.trim() !== "" && Number.isFinite(Number(raw))) return Number(raw);
  return undefined;
}

function booleanAt(source: unknown, path: readonly string[]): boolean | undefined {
  const raw = dig(source, path);
  return typeof raw === "boolean" ? raw : undefined;
}

function stringAt(source: unknown, path: readonly string[]): string | undefined {
  const raw = dig(source, path);
  return typeof raw === "string" && raw !== "" ? raw : undefined;
}

function stringListAt(source: unknown, path: readonly string[]): string[] {
  const raw = dig(source, path);
  if (!Array.isArray(raw)) return [];
  return raw.filter((item): item is string => typeof item === "string");
}

function recordAt(source: unknown, path: readonly string[]): JsonRecord {
  const raw = dig(source, path);
  return isRecord(raw) ? raw : {};
}

/** Lowercased set, for the many lists a health report spells in mixed case. */
function lower(list: readonly string[]): Set<string> {
  return new Set(list.map((item) => item.trim().toLowerCase()));
}

// ── the "claim" side: resolving what pi will send ─────────────────────────

/**
 * Merge a provider block and one of its model entries into the single view a
 * session gets.
 *
 * Mirrors pi's own composition: a model entry wins over the provider block key
 * by key, `compat` merges shallowly, and the defaults below are pi's — notably
 * `input` defaulting to text only, so a config that says nothing about images
 * is not claiming vision.
 */
export function deriveModelConfigView(options: {
  readonly provider: string;
  readonly providerConfig?: JsonRecord;
  readonly modelConfig?: JsonRecord;
}): ModelConfigView {
  const provider = options.providerConfig ?? {};
  const model = options.modelConfig ?? {};
  const compat = {
    ...(isRecord(provider.compat) ? provider.compat : {}),
    ...(isRecord(model.compat) ? model.compat : {}),
  };
  const input = stringListAt(model, ["input"]);
  return {
    provider: options.provider,
    modelId: stringAt(model, ["id"]) ?? "unknown",
    baseUrl: stringAt(model, ["baseUrl"]) ?? stringAt(provider, ["baseUrl"]) ?? "",
    api: stringAt(model, ["api"]) ?? stringAt(provider, ["api"]) ?? "openai-completions",
    reasoning: booleanAt(model, ["reasoning"]) ?? false,
    input: input.length > 0 ? input : ["text"],
    contextWindow: numberAt(model, ["contextWindow"]) ?? 0,
    maxTokens: numberAt(model, ["maxTokens"]) ?? 0,
    samplingParams: isRecord(model.samplingParams) ? model.samplingParams : {},
    compat,
    thinkingLevelMap: isRecord(model.thinkingLevelMap) ? model.thinkingLevelMap : {},
    hasRawEntry: options.modelConfig !== undefined,
  };
}

// ── helpers the rules share ──────────────────────────────────────────────

interface RuleContext {
  readonly view: ModelConfigView;
  readonly health: JsonRecord;
  readonly levels: AuditLevels;
  readonly toolSchemas: Readonly<Record<string, unknown>>;
  /** The levels that actually decide a budget: configured ones, else the server's. */
  readonly levelsInPlay: readonly string[];
  /** Whether reasoning is drawn from the same token budget as the answer. */
  readonly thinkingSharedWithAnswer: boolean;
}

type Rule = (ctx: RuleContext) => readonly AuditFinding[];

function finding(
  id: string,
  severity: AuditSeverity,
  title: string,
  detail: string,
  extra: { readonly evidence?: string; readonly suggestions?: readonly AuditSuggestion[] } = {},
): AuditFinding {
  return {
    id,
    severity,
    title,
    detail,
    ...(extra.evidence === undefined ? {} : { evidence: extra.evidence }),
    ...(extra.suggestions === undefined ? {} : { suggestions: extra.suggestions }),
  };
}

const ok = (id: string, title: string, detail: string): AuditFinding =>
  finding(id, "ok", title, detail);

function modelSuggestion(field: string, value: unknown, why: string, remove = false): AuditSuggestion {
  return remove ? { scope: "model", field, value, why, remove: true } : { scope: "model", field, value, why };
}

function providerSuggestion(field: string, value: unknown, why: string, remove = false): AuditSuggestion {
  return remove ? { scope: "provider", field, value, why, remove: true } : { scope: "provider", field, value, why };
}

function supportedNames(health: JsonRecord): Set<string> {
  return lower(stringListAt(health, ["supported"]));
}

function chatTemplateKwargNames(health: JsonRecord): Set<string> {
  return lower(stringListAt(health, ["chat_template_kwargs"]));
}

/** Does the server offer any way to be told how much to think? */
function serverOffersThinking(health: JsonRecord): boolean {
  const names = [
    "reasoning_effort",
    "enable_thinking",
    "preserve_thinking",
    "max_thinking_tokens",
    "thinking_budget",
    "thinking_budget_tokens",
    "thinking_token_budget",
    "reasoning",
    "thinking",
  ];
  const supported = supportedNames(health);
  const kwargs = chatTemplateKwargNames(health);
  return names.some((name) => supported.has(name) || kwargs.has(name));
}

/** The `chatTemplateKwargs` entry carrying a pi-controlled `$var`. */
function chatTemplateVar(compat: JsonRecord, wanted: string): string | undefined {
  const kwargs = recordAt(compat, ["chatTemplateKwargs"]);
  for (const [key, value] of Object.entries(kwargs)) {
    if (isRecord(value) && value.$var === wanted) return key;
  }
  return undefined;
}

/**
 * Whether the configured compat actually sends a reasoning effort.
 *
 * Read off pi's `openai-completions` request builder rather than guessed:
 * `chat-template` sends only what `chatTemplateKwargs` lists and never reads
 * `supportsReasoningEffort`; `qwen-chat-template` hard-codes
 * `enable_thinking`/`preserve_thinking` and sends no effort; every other
 * format sends `reasoning_effort` unless the flag is explicitly false.
 * `undefined` means "this format is not analysed here".
 */
function sendsReasoningEffort(compat: JsonRecord): boolean | undefined {
  const format = stringAt(compat, ["thinkingFormat"]) ?? "openai";
  if (format === "chat-template") return chatTemplateVar(compat, "thinking.effort") !== undefined;
  if (format === "qwen-chat-template") return false;
  if (format === "string-thinking") return undefined;
  return booleanAt(compat, ["supportsReasoningEffort"]) !== false;
}

/** `HALOGEN_TOP_K` → `top_k`, so server defaults speak in request-key terms. */
function normaliseDefaultKey(key: string): string {
  const stripped = key.replace(/^[A-Z0-9]+_/u, "").replace(/^HALOGEN_/u, "");
  return stripped.toLowerCase();
}

function serverSamplingDefaults(health: JsonRecord): Map<string, number> {
  const out = new Map<string, number>();
  for (const [key, value] of Object.entries(recordAt(health, ["server_defaults"]))) {
    const numeric = typeof value === "number" ? value : Number(value);
    if (Number.isFinite(numeric)) out.set(normaliseDefaultKey(key), numeric);
  }
  return out;
}

function roundUpTo1024(tokens: number): number {
  return Math.max(1_024, Math.ceil(tokens / 1_024) * 1_024);
}

/** The biggest supported name at or below `level`, for translating a bad one. */
function nearestSupportedEffort(level: string, supported: ReadonlySet<string>): string | undefined {
  const index = EFFORT_ORDER.indexOf(level);
  const window = index < 0 ? EFFORT_ORDER : EFFORT_ORDER.slice(0, index + 1);
  for (let i = window.length - 1; i >= 0; i -= 1) {
    const candidate = window[i];
    if (candidate !== undefined && supported.has(candidate)) return candidate;
  }
  return undefined;
}

/**
 * The levels that decide what this run does.
 *
 * Configured levels win outright; with none configured the server's own default
 * is what every pass will actually run at, and that is the number worth
 * checking the output budget against.
 */
export function levelsInPlay(levels: AuditLevels): string[] {
  const configured = [levels.work, levels.split, levels.savedDefault].filter(
    (level): level is string => level !== undefined && level !== "" && level !== "off",
  );
  const picked = configured.length > 0 ? configured : [levels.serverDefault].filter(
    (level): level is string => level !== undefined && level !== "" && level !== "none",
  );
  return [...new Set(picked)];
}

// ── the rules ────────────────────────────────────────────────────────────

/** `status` is the one field that says whether the rest of the payload means anything. */
const ruleHealthStatus: Rule = ({ health }) => {
  const status = stringAt(health, ["status"]);
  if (status === undefined) {
    return [
      finding(
        "health-status",
        "info",
        "the report carries no `status` field",
        "Everything below was read anyway, but the server never said it was healthy.",
      ),
    ];
  }
  if (status === "ok") {
    return [ok("health-status", "the server reports `status: ok`", "A fair basis for the checks below.")];
  }
  return [
    finding(
      "health-status",
      "warn",
      `the server reports \`status: ${status}\``,
      "This is the server's own word, not the harness's. Starting a work pass against a server that does not call itself healthy is allowed, and usually a bad idea.",
    ),
  ];
};

/** The endpoint answering is the endpoint the config means. */
const ruleServerIdentity: Rule = ({ health, view }) => {
  const reported = stringAt(health, ["model"]);
  if (reported === undefined) {
    return [
      finding("model-identity", "info", "the report names no model", "Nothing to line the configured id up against."),
    ];
  }
  const configured = view.modelId.toLowerCase();
  const server = reported.toLowerCase();
  // Servers commonly decorate the id with a family or build prefix.
  if (server === configured || server.includes(configured) || configured.includes(server)) {
    return [ok("model-identity", `the endpoint answers as \`${reported}\``, "That matches the configured model id.")];
  }
  return [
    finding(
      "model-identity",
      "warn",
      "the endpoint answers as a different model than the config names",
      "Every number in this report describes the model the server is actually running. If that is not the model the config describes, the comparison is still worth reading — but the config is pointing at the wrong thing.",
      { evidence: `health.model=${reported}; configured ${view.provider}/${view.modelId}` },
    ),
  ];
};

/** The layer that shapes the request and the layer that runs the model agreeing. */
const ruleVersionMatch: Rule = ({ health }) => {
  const match = booleanAt(health, ["version.match"]);
  if (match === undefined) return [];
  const api = stringAt(health, ["version.api"]);
  const engine = stringAt(health, ["version.engine"]);
  if (match) {
    return [ok("version-match", "api and engine versions agree", `${api ?? "?"} / ${engine ?? "?"}`)];
  }
  return [
    finding(
      "version-match",
      "warn",
      "the api and the engine report different versions",
      "Version skew is where a capability report starts lying: the report is served by one build and answered by another.",
      { evidence: `api=${api ?? "?"}; engine=${engine ?? "?"}` },
    ),
  ];
};

/** The route the configured `api` needs is actually served. */
const ruleEndpointRoutes: Rule = ({ health, view }) => {
  const endpoints = stringListAt(health, ["endpoints"]);
  if (endpoints.length === 0) return [];
  const required = REQUIRED_ROUTE[view.api];
  if (required === undefined) {
    return [
      finding(
        "endpoint-route",
        "info",
        `no route rule known for \`api: ${view.api}\``,
        `The report lists ${endpoints.join(", ")}; nothing here checked them against the api type.`,
      ),
    ];
  }
  if (endpoints.some((endpoint) => endpoint.replace(/\/+$/u, "").endsWith(required))) {
    return [ok("endpoint-route", `\`${view.api}\` has its route`, `${required} is served`)];
  }
  return [
    finding(
      "endpoint-route",
      "error",
      `\`${view.api}\` needs ${required} and this endpoint does not serve it`,
      "Every request fails at the URL, before the model sees anything.",
      { evidence: `endpoints=${endpoints.join(", ")}` },
    ),
  ];
};

/**
 * The context window: the claim that decides where the loop stops.
 *
 * Too big and a full request is refused; too small and the run stops early
 * against a wall that is not there. Both go through `src/context.ts`, which
 * measures the *declared* window, and pi's own answer clamp is sized against it
 * too. The server's real limit is the smallest of the window it names, its
 * per-slot context, and its shared KV pool.
 */
const ruleContextWindow: Rule = ({ health, view }) => {
  const declared = view.contextWindow;
  const named = numberAt(health, ["context"]);
  const slot = numberAt(health, ["slot_ctx"]);
  const pool = numberAt(health, ["kv_pool_positions"]);
  const limits = [named, slot, pool].filter((n): n is number => n !== undefined && n > 0);
  const facts = [
    named === undefined ? undefined : `context=${named}`,
    slot === undefined ? undefined : `slot_ctx=${slot}`,
    pool === undefined ? undefined : `kv_pool_positions=${pool}`,
  ].filter((part): part is string => part !== undefined);

  if (limits.length === 0) {
    return [
      finding(
        "context-window",
        "info",
        "the report names no context limit",
        declared > 0
          ? `The declared ${formatTokenCount(declared)} stands unverified.`
          : "Nothing is declared either; the server decides what happens to a long request.",
      ),
    ];
  }
  const serverWindow = Math.min(...limits);

  if (!Number.isFinite(declared) || declared <= 0) {
    return [
      finding(
        "context-window-undeclared",
        "warn",
        "no context window is declared",
        "With nothing declared nothing is clamped: a request grows until the server refuses it, mid-ticket, with no room left inside the run to recover.",
        {
          evidence: facts.join(" "),
          suggestions: [
            modelSuggestion("contextWindow", serverWindow, `the tightest limit the server reports: ${formatTokenCount(serverWindow)} tokens`),
          ],
        },
      ),
    ];
  }

  if (declared > serverWindow) {
    return [
      finding(
        "context-window-oversized",
        "error",
        `the declared window is bigger than the server holds (${formatTokenCount(declared)} > ${formatTokenCount(serverWindow)})`,
        "A request that fills the declared window is refused, not truncated — and it fails at the worst possible moment, after the session has spent its budget getting large.",
        {
          evidence: `config contextWindow=${declared}; ${facts.filter((fact) => !fact.endsWith(String(declared))).join(" ")}`,
          suggestions: [
            modelSuggestion("contextWindow", serverWindow, `the binding limit is ${formatTokenCount(serverWindow)}; declaring more buys a refusal instead of a working tail`),
          ],
        },
      ),
    ];
  }

  const gap = serverWindow - declared;
  const tolerance = Math.max(CONTEXT_TOLERANCE_TOKENS, Math.round(serverWindow * CONTEXT_TOLERANCE_RATIO));
  if (gap > tolerance) {
    return [
      finding(
        "context-window-undersized",
        "warn",
        `${formatTokenCount(gap)} of window is unreachable (${formatTokenCount(declared)} declared, ${formatTokenCount(serverWindow)} served)`,
        "The declared number is the wall: `src/context.ts` stops the run against it and pi sizes the answer budget against it. A ticket that would have fitted in the window you are paying for stops short, and the failure reads as 'this ticket does not fit one session' — a false statement about the work and a true one about the config.",
        {
          evidence: `config contextWindow=${declared}; server window=${serverWindow}; tolerance=${tolerance}`,
          suggestions: [modelSuggestion("contextWindow", serverWindow, "declare the window the server reported")],
        },
      ),
    ];
  }
  return [
    ok(
      "context-window",
      "the declared window matches the server",
      `${formatTokenCount(declared)} declared against ${formatTokenCount(serverWindow)} served`,
    ),
  ];
};

/**
 * The output ceiling: the cap the server refuses to exceed, and the room a
 * thinking pass leaves for the answer.
 *
 * `token_budget_covers_reasoning` is the pivot. When it is true, thinking and
 * the answer are drawn from the same `max_tokens`, so a ceiling only big enough
 * to think with is a ceiling that cannot answer — and in this harness the answer
 * is the `report_done` call the whole loop waits on.
 */
const ruleOutputCeiling: Rule = ({ health, view, levelsInPlay: levelsUsed, thinkingSharedWithAnswer }) => {
  const cap = numberAt(health, ["max_tokens_cap"]);
  const declared = view.maxTokens;
  const overCap = stringAt(health, ["max_tokens_over_cap"]);
  const base =
    `max_tokens_cap=${cap ?? "?"}; config maxTokens=${declared}` +
    (overCap === undefined ? "" : `; over-cap response=${overCap}`);

  if (cap === undefined) {
    return [
      finding(
        "output-cap",
        "info",
        "the report names no output cap",
        declared > 0 ? `The declared ${formatTokenCount(declared)} stands unverified.` : "Nothing was declared either.",
      ),
    ];
  }
  if (!Number.isFinite(declared) || declared <= 0) {
    return [
      finding(
        "output-cap-undeclared",
        "warn",
        "no output cap is declared on the model",
        `The server will take up to ${formatTokenCount(cap)} and no more; with nothing declared, pi has no ceiling to size a request against.`,
        {
          suggestions: [modelSuggestion("maxTokens", Math.min(cap, 16_384), `declare a ceiling; the server's is ${formatTokenCount(cap)}`)],
        },
      ),
    ];
  }

  const budgets = levelsUsed
    .map((level) => ({ level, budget: piThinkingBudget(level) }))
    .filter((entry): entry is { level: string; budget: number } => entry.budget !== undefined);
  const worst = budgets.reduce<{ level: string; budget: number } | undefined>(
    (acc, entry) => (acc === undefined || entry.budget > acc.budget ? entry : acc),
    undefined,
  );
  const wanted =
    worst === undefined
      ? declared
      : worst.budget + PI_MIN_ANSWER_TOKENS + VERDICT_ROOM_TOKENS;

  if (declared > cap) {
    const suggested = Math.min(cap, worst === undefined ? cap : roundUpTo1024(wanted));
    return [
      finding(
        "output-cap-exceeded",
        "error",
        `maxTokens ${declared} is over the server's cap of ${cap}`,
        `A request over the cap answers with ${overCap ?? "an error"}, immediately, every time. This is not a slow failure; it is a config that cannot be sent.`,
        {
          evidence: base,
          suggestions: [
            modelSuggestion(
              "maxTokens",
              suggested,
              `under the cap and still room for ${worst === undefined ? "an answer" : `\`${worst.level}\` thinking plus the verdict block`}`,
            ),
          ],
        },
      ),
    ];
  }

  if (!view.reasoning || !thinkingSharedWithAnswer || worst === undefined) {
    return [
      ok(
        "output-cap",
        "maxTokens fits the server's cap",
        worst === undefined
          ? `${declared} <= ${cap}, and no thinking level is in play`
          : `${declared} <= ${cap}, and thinking is not drawn from this budget on this server`,
      ),
    ];
  }

  if (declared >= wanted) {
    return [
      ok(
        "output-cap",
        "the ceiling carries the thinking budget and still leaves room to answer",
        `${declared} >= ${worst.budget} (${worst.level} thinking) + ${PI_MIN_ANSWER_TOKENS} (pi's answer room) + ${VERDICT_ROOM_TOKENS} (verdict)`,
      ),
    ];
  }

  const serverRoom = stringAt(health, ["thinking_answer_room"]);
  const suggested = Math.min(cap, roundUpTo1024(wanted));
  const squeezed = Math.max(0, declared - worst.budget);
  return [
    finding(
      "output-starves-the-answer",
      "warn",
      `at \`${worst.level}\` the ceiling leaves ${formatTokenCount(squeezed)} for the answer (${declared} total, ${worst.budget} of thinking)`,
      `Reasoning and the answer come out of the same ${declared} tokens here. pi budgets ${worst.budget} of thinking at this level and reserves ${PI_MIN_ANSWER_TOKENS} for the answer${serverRoom === undefined ? "" : `; the server's own rule is ${serverRoom}`}. That is not enough room for a \`report_done\` block and the prose around it: the run looks like it is thinking hard, then hands over a truncated verdict, which the harness must read as failed work.`,
      {
        evidence: `${base}; levels in play=${levelsUsed.join(", ") || "none"}`,
        suggestions: [
          modelSuggestion(
            "maxTokens",
            suggested,
            `${worst.budget} thinking + ${PI_MIN_ANSWER_TOKENS} answer room + ${VERDICT_ROOM_TOKENS} verdict room, rounded up (the server takes up to ${formatTokenCount(cap)})`,
          ),
        ],
      },
    ),
  ];
};

/** The `reasoning` flag against the server's demonstrated capability. */
const ruleReasoningDeclared: Rule = ({ health, view }) => {
  const capable = serverOffersThinking(health);
  if (!view.reasoning && capable) {
    return [
      finding(
        "reasoning-flag-off",
        "warn",
        "the server can think and the config says this model cannot",
        "With `reasoning: false` pi sends no thinking knob at all: no level can be requested, `/thinking` has nothing to pick, and `LOOP_WORK_THINKING` is dead. You are paying for a reasoning model and driving it as a plain completion.",
        {
          suggestions: [
            modelSuggestion("reasoning", true, "the report advertises reasoning controls, so the flag has to be on for a level to mean anything"),
          ],
        },
      ),
    ];
  }
  if (view.reasoning && !capable) {
    return [
      finding(
        "reasoning-claimed-server-silent",
        "warn",
        "the config says this model reasons and the report names no way to ask it to",
        "Nothing in the report mentions a reasoning or thinking parameter. Either the server thinks unconditionally, or it does not think at all and every thinking level in the config is decoration.",
      ),
    ];
  }
  return [
    ok(
      "reasoning-flag",
      view.reasoning ? "reasoning is on and the server offers controls" : "reasoning is off and the server offers none",
      view.reasoning ? "The levels in play are meaningful." : "There is nothing to level up.",
    ),
  ];
};

/** Is a requested thinking level actually delivered to the server? */
const ruleThinkingEffortChannel: Rule = ({ health, view }) => {
  if (!view.reasoning) return [];
  const kwargs = chatTemplateKwargNames(health);
  const asKwarg = kwargs.has("reasoning_effort");
  const asTopLevel = supportedNames(health).has("reasoning_effort");
  if (!asKwarg && !asTopLevel) return [];

  const format = stringAt(view.compat, ["thinkingFormat"]) ?? "openai";
  const flag = booleanAt(view.compat, ["supportsReasoningEffort"]);
  const why =
    "With no effort in the request the server runs *its* default level, and every knob above it — `/thinking`, `LOOP_WORK_THINKING`, `LOOP_SPLIT_THINKING` — is choosing something nobody receives.";

  if (sendsReasoningEffort(view.compat) === true) {
    return [
      ok(
        "thinking-effort-sent",
        `the requested level reaches the server (\`${format}\`)`,
        "A change of level changes what is sent.",
      ),
    ];
  }

  if (format === "chat-template" && asKwarg) {
    const suggestions: AuditSuggestion[] = [
      providerSuggestion(
        "compat.chatTemplateKwargs.reasoning_effort",
        { $var: "thinking.effort" },
        "the server takes `reasoning_effort` through the chat template, and pi's `chat-template` format forwards exactly what this map lists",
      ),
    ];
    if (flag === false) {
      suggestions.push(
        providerSuggestion(
          "compat.supportsReasoningEffort",
          true,
          "informational: the `chat-template` path never reads this flag — it only gates the top-level `reasoning_effort` field — but leaving it false alongside a chat-template format invites exactly this confusion",
        ),
      );
    }
    return [
      finding(
        "thinking-effort-never-sent",
        "warn",
        `\`thinkingFormat: "${format}"\` with no effort kwarg: the requested level is never sent`,
        `${why} The report gives the server's own default as \`${stringAt(health, ["reasoning_effort_default"]) ?? "?"}\`, and that is what every pass in this loop runs at whatever the config asks for.`,
        { evidence: `server chat_template_kwargs=${[...kwargs].join(", ")}`, suggestions },
      ),
    ];
  }

  if (format === "qwen-chat-template") {
    return [
      finding(
        "thinking-effort-never-sent",
        "warn",
        "`qwen-chat-template` hard-codes enable_thinking/preserve_thinking and sends no effort",
        `${why} \`chat-template\` with named kwargs is the format that forwards an effort and says out loud which kwargs it is sending.`,
        {
          suggestions: [
            providerSuggestion("compat.thinkingFormat", "chat-template", "so the effort can be forwarded and the kwargs named explicitly"),
            providerSuggestion("compat.chatTemplateKwargs.reasoning_effort", { $var: "thinking.effort" }, "the effort the selected level stands for"),
          ],
        },
      ),
    ];
  }

  return [
    finding(
      "thinking-effort-never-sent",
      "warn",
      "the config blocks `reasoning_effort` and the server supports it",
      why,
      {
        evidence: `api supports reasoning_effort=${asTopLevel}; compat.supportsReasoningEffort=${String(flag)}`,
        suggestions: [
          providerSuggestion("compat.supportsReasoningEffort", true, "the server lists the field, and this flag is what turns the top-level field on"),
        ],
      },
    ),
  ];
};

/** The `off` level needs a switch to be able to switch: `enable_thinking`. */
const ruleThinkingEnableChannel: Rule = ({ health, view }) => {
  if (!view.reasoning) return [];
  if ((stringAt(view.compat, ["thinkingFormat"]) ?? "openai") !== "chat-template") return [];
  if (!chatTemplateKwargNames(health).has("enable_thinking")) return [];
  if (chatTemplateVar(view.compat, "thinking.enabled") !== undefined) {
    return [
      ok("thinking-enable-sent", "`off` can actually turn thinking off", "an `enable_thinking` kwarg is wired to pi's thinking.enabled"),
    ];
  }
  return [
    finding(
      "thinking-enable-never-sent",
      "warn",
      "nothing in the request can switch thinking off",
      "The server takes `enable_thinking` through the chat template and the config does not send it. A `off` level will still get a thinking model: the template decides, and the template is not being told.",
      {
        suggestions: [
          providerSuggestion(
            "compat.chatTemplateKwargs.enable_thinking",
            { $var: "thinking.enabled" },
            "so `off` means off and every other level means on",
          ),
        ],
      },
    ),
  ];
};

/** Kwarg names the template has never heard of, and `$var` names pi cannot resolve. */
const ruleChatTemplateKwargsKnown: Rule = ({ health, view }) => {
  const configured = Object.keys(recordAt(view.compat, ["chatTemplateKwargs"]));
  if (configured.length === 0) return [];
  const advertised = chatTemplateKwargNames(health);
  const findings: AuditFinding[] = [];

  if (advertised.size === 0) {
    findings.push(
      finding(
        "chat-template-kwargs-unadvertised",
        "info",
        "the report lists no supported `chat_template_kwargs`",
        `The config sets ${configured.join(", ")}; whether the template takes them is unverified.`,
      ),
    );
  } else {
    const unknown = configured.filter((key) => !advertised.has(key.toLowerCase()));
    if (unknown.length > 0) {
      findings.push(
        finding(
          "chat-template-kwargs-unknown",
          "warn",
          `the template is not reported to take: ${unknown.join(", ")}`,
          "A kwarg the template does not declare is dropped silently: the config looks set and behaves unset, which is the worst of the two.",
          {
            evidence: `unknown=${unknown.join(", ")}; advertised=${[...advertised].join(", ")}`,
            suggestions: unknown.map((key) =>
              providerSuggestion(
                `compat.chatTemplateKwargs.${key}`,
                null,
                "not in the server's list; drop it or spell it the way the template says",
                true,
              ),
            ),
          },
        ),
      );
    } else {
      findings.push(
        ok("chat-template-kwargs-known", "every configured kwarg is one the template declares", configured.join(", ")),
      );
    }
  }

  const kwargsRecord = recordAt(view.compat, ["chatTemplateKwargs"]);
  const badVars: string[] = [];
  for (const key of configured) {
    const value = kwargsRecord[key];
    if (isRecord(value) && typeof value.$var === "string" && !PI_CHAT_TEMPLATE_VARS.includes(value.$var)) {
      badVars.push(`${key}: {"$var": "${value.$var}"}`);
    }
  }
  if (badVars.length > 0) {
    findings.push(
      finding(
        "chat-template-var-unknown",
        "error",
        `this pi's config schema rejects: ${badVars.join(", ")}`,
        `The \`$var\` values a chatTemplateKwargs entry may hold are ${PI_CHAT_TEMPLATE_VARS.join(", ")}. Anything else fails the models.json schema, and a provider entry that fails validation is worse than one with a missing setting.`,
      ),
    );
  }
  return findings;
};

/** Effort *names* the server has no value for. */
const ruleEffortValues: Rule = ({ health, view, levelsInPlay: levelsUsed }) => {
  if (!view.reasoning) return [];
  const supported = lower(stringListAt(health, ["reasoning_effort_values"]));
  if (supported.size === 0) return [];

  const unmapped: { level: string; value: string }[] = [];
  for (const level of levelsUsed) {
    const mapped = view.thinkingLevelMap[level];
    const value = typeof mapped === "string" ? mapped : level;
    if (!supported.has(value.toLowerCase())) unmapped.push({ level, value });
  }
  if (unmapped.length === 0) {
    return [
      ok(
        "effort-values",
        "every effort name this loop can send is one the server has a value for",
        `${levelsUsed.join(", ")} against ${[...supported].join(", ")}`,
      ),
    ];
  }
  const merged: JsonRecord = { ...view.thinkingLevelMap };
  for (const entry of unmapped) {
    const replacement = nearestSupportedEffort(entry.value, supported);
    if (replacement !== undefined) merged[entry.level] = replacement;
  }
  return [
    finding(
      "effort-value-unsupported",
      "warn",
      `the server has no value for: ${unmapped.map((entry) => `${entry.level} → \`${entry.value}\``).join(", ")}`,
      `The server accepts ${[...supported].join(", ")}. Note that pi bills \`xhigh\` and \`max\` at the same thinking budget but sends the *name* it was given, so an unmapped \`max\` arrives as a string this server cannot resolve. A \`thinkingLevelMap\` translates pi's level names into this endpoint's.`,
      {
        evidence: `reasoning_effort_values=${[...supported].join(", ")}`,
        suggestions:
          Object.keys(merged).length > 0
            ? [modelSuggestion("thinkingLevelMap", merged, "pi's level names, translated to this server's")]
            : [],
      },
    ),
  ];
};

/** Tool calling — without it this harness has no completion contract at all. */
const ruleToolCalling: Rule = ({ health }) => {
  const supported = supportedNames(health);
  if (supported.size === 0) return [];
  if (!supported.has("tools")) {
    return [
      finding(
        "no-tool-calling",
        "error",
        "the endpoint reports no tool support",
        "This loop's completion contract is a `report_done` tool call, and a work pass cannot edit a repo without tools. Every run against this endpoint would end `unstructured-verdict`.",
      ),
    ];
  }
  const parallel = booleanAt(health, ["tool_calls.parallel_tool_calls"]);
  const streaming = booleanAt(health, ["tool_calls.streaming"]);
  return [
    ok(
      "tool-calling",
      "tools are supported",
      `streaming=${String(streaming ?? "unreported")}, parallel=${String(parallel ?? "unreported")}`,
    ),
  ];
};

/** `strict` in a tool definition means nothing without constrained decoding. */
const ruleStrictMode: Rule = ({ health, view }) => {
  const constrained = booleanAt(health, ["tool_calls.constrained_decoding"]);
  if (constrained === undefined) return [];
  if (constrained) {
    return [ok("strict-mode", "constrained decoding is available", "pi's `strict` tool schemas are enforceable here")];
  }
  if (booleanAt(view.compat, ["supportsStrictMode"]) === false) {
    return [ok("strict-mode", "strict tool schemas are off, and the server has no constrained decoding", "aligned")];
  }
  return [
    finding(
      "strict-mode-without-decoding",
      "warn",
      "pi sends `strict` tool schemas and this server has no constrained decoding",
      "The flag gets ignored, or gets the tool definitions rejected. Neither is what the config means: it means *hold the model to this shape*, and nothing here can do that. Say `false` and let the harness's own validators do the job — which is what `report_done` and `report_split` already do.",
      {
        evidence: "tool_calls.constrained_decoding=false; compat.supportsStrictMode unset (pi's default is true)",
        suggestions: [providerSuggestion("compat.supportsStrictMode", false, "with no constrained decoding, `strict` cannot be honoured")],
      },
    ),
  ];
};

/** A forced call is the wrong shape for a verdict that has to be reasoned about. */
const ruleForcedCalls: Rule = ({ health, view }) => {
  if (!view.reasoning) return [];
  if (booleanAt(health, ["tool_calls.forced_call_disables_thinking"]) !== true) return [];
  return [
    finding(
      "forced-call-disables-thinking",
      "info",
      "a forced `tool_choice` on this server turns thinking off for that request",
      "So do not force `report_done`. The loop's shape — ask for the tool, keep a fenced-JSON fallback, never coerce — is the right one here: a coerced verdict is an unthinking one, and the harness would rather take an honest `done: false` than a forced `done: true`.",
    ),
  ];
};

/** Vision: the claim that this session can see. */
const ruleVision: Rule = ({ health, view }) => {
  const enabled = booleanAt(health, ["vision.enabled"]);
  if (enabled === undefined) return [];
  const wantsImages = view.input.includes("image");

  if (wantsImages && !enabled) {
    const because = stringListAt(health, ["vision.disabled_because"]);
    return [
      finding(
        "vision-claimed-tower-missing",
        "error",
        "the config declares image input and this server has no vision tower",
        "A `read` on a PNG, a screenshot, an image in a tool result — each becomes a refused request in the middle of a ticket, after the session spent its budget walking to the file. Declare text-only input and the harness never offers an image." +
          (because.length > 0 ? ` The server says why: ${because.join("; ")}.` : ""),
        { suggestions: [modelSuggestion("input", ["text"], "no vision tower, so no image input")] },
      ),
    ];
  }
  if (!wantsImages && enabled) {
    return [
      finding(
        "vision-unused",
        "info",
        "the server has vision and the config asks for text only",
        "Nothing is wrong; the capability is simply not being used. Add `\"image\"` to `input` if this loop should read screenshots.",
      ),
    ];
  }
  if (!wantsImages) {
    return [ok("vision", "image input is not claimed and the server has no vision", "aligned")];
  }
  const maxPixels = numberAt(health, ["vision.max_pixels"]);
  const multiple = numberAt(health, ["vision.size_multiple"]);
  return [
    ok(
      "vision",
      "image input is claimed and the server has a vision tower",
      `max_pixels=${maxPixels ?? "?"}, size_multiple=${multiple ?? "?"}`,
    ),
  ];
};

/**
 * Sampling: which knobs the server has, which ones it ignores, and which ones
 * the config is pinning to a value the server would have supplied anyway.
 *
 * The server's rule is stated in the report and worth repeating here because the
 * findings turn on it: a field the request sends always wins, and a default
 * fills only a field the request omits. So mirroring the defaults is not a
 * no-op — it is a freeze. The day the server's default moves, the config keeps
 * the old value and nobody chose either.
 */
const ruleSamplingParams: Rule = ({ health, view }) => {
  const configured = Object.keys(view.samplingParams);
  if (configured.length === 0) return [];
  const defaults = serverSamplingDefaults(health);
  const implemented = lower(stringListAt(health, ["sampling.implemented"]));
  const notImplementedList = stringListAt(health, ["sampling.not_implemented"]);
  const notImplemented = new Set(
    notImplementedList.map((name) => name.toLowerCase().split(" ")[0] ?? name),
  );
  const ignored = lower(stringListAt(health, ["accepted_but_ignored"]));

  // The server's explicit "we read this and do nothing with it" outranks a
  // not-implemented note that happens to name the same key ("n > 1" is about a
  // value, not the field): the ignore list is the statement about the field.
  const dropped = configured.filter((key) => ignored.has(key.toLowerCase()));
  const blocked = configured.filter(
    (key) => notImplemented.has(key.toLowerCase()) && !dropped.includes(key),
  );
  const mirrored: string[] = [];
  const overridden: string[] = [];
  const unknown: string[] = [];

  for (const key of configured) {
    if (blocked.includes(key) || dropped.includes(key)) continue;
    const value = view.samplingParams[key];
    const serverDefault = defaults.get(key.toLowerCase());
    if (implemented.size > 0 && !implemented.has(key.toLowerCase()) && !defaults.has(key.toLowerCase())) {
      unknown.push(key);
      continue;
    }
    if (serverDefault !== undefined && typeof value === "number" && value === serverDefault) {
      mirrored.push(`${key}=${value}`);
    } else if (serverDefault !== undefined) {
      overridden.push(`${key}=${JSON.stringify(value)} (server default ${serverDefault})`);
    }
  }

  const findings: AuditFinding[] = [];
  if (blocked.length > 0) {
    findings.push(
      finding(
        "sampling-not-implemented",
        "error",
        `the server does not implement: ${blocked.join(", ")}`,
        "A parameter on the not-implemented list is at best refused and at worst silently reshaped the response. Neither is what a config that names it means.",
        {
          evidence: `not_implemented=${notImplementedList.join(", ")}`,
          suggestions: blocked.map((key) =>
            modelSuggestion(`samplingParams.${key}`, null, "the server does not implement this; remove it", true),
          ),
        },
      ),
    );
  }
  if (dropped.length > 0) {
    findings.push(
      finding(
        "sampling-accepted-but-ignored",
        "warn",
        `accepted and thrown away: ${dropped.join(", ")}`,
        "The server reads these fields and does nothing with them. A config that sets one looks like a decision that was made.",
        {
          suggestions: dropped.map((key) =>
            modelSuggestion(`samplingParams.${key}`, null, "accepted but ignored by this server; remove it rather than keep the illusion", true),
          ),
        },
      ),
    );
  }
  if (unknown.length > 0) {
    findings.push(
      finding(
        "sampling-unknown",
        "warn",
        `neither implemented nor defaulted: ${unknown.join(", ")}`,
        "The report has no record of this parameter being honoured. Send it and hope is a config strategy with a long failure tail.",
        { evidence: `sampling.implemented=${[...implemented].join(", ") || "?"}` },
      ),
    );
  }
  if (overridden.length > 0) {
    findings.push(
      finding(
        "sampling-overrides-server",
        "info",
        `the config overrides the server's tuning: ${overridden.join(", ")}`,
        "A field the request sends always wins, so these values — not the server's — are what the model decodes from. That is fine when it was chosen and confusing when it was inherited; make sure somebody here chose it.",
      ),
    );
  }
  if (mirrored.length > 0 && overridden.length === 0 && blocked.length === 0 && dropped.length === 0 && unknown.length === 0) {
    findings.push(
      finding(
        "sampling-mirrors-server",
        "info",
        `samplingParams restate the server's defaults: ${mirrored.join(", ")}`,
        "Harmless today, and it documents intent. The catch is that it freezes them: a field you send always wins, so when the server's default changes your config keeps the old number and nobody re-chose it. Drop them if you would rather follow the server.",
      ),
    );
  }
  return findings;
};

/** A greedy run gets the prompt-lookup path; a sampled run does not. Worth naming. */
const ruleGreedyDecode: Rule = ({ health }) => {
  const temperature = numberAt(health, ["server_defaults", "HALOGEN_TEMPERATURE"]);
  const lookup = recordAt(health, ["prompt_lookup"]);
  if (temperature === undefined || Object.keys(lookup).length === 0) return [];
  const applies = stringAt(health, ["prompt_lookup", "applies_to"]);
  if (temperature !== 0) {
    return [
      finding(
        "greedy-decode-unused",
        "info",
        "prompt lookup is on the table but this server defaults to sampled decoding",
        `The report says prompt lookup ${applies ?? "applies only to greedy requests"}. A request that sends temperature 0 gets the greedy path and the acceleration; nothing in the harness asks for that, and it should not for coding work — this is here so the trade is on the record, not to invite a temperature 0 default.`,
      ),
    ];
  }
  return [
    finding(
      "greedy-decode",
      "info",
      "the server defaults to greedy decoding",
      `Prompt lookup applies (${applies ?? "as reported"}), so requests that send no temperature decode greedily and fast. Same caveat as every mirror: a request that sends temperature > 0 leaves this path.`,
    ),
  ];
};

/** The token-budget field name: what pi sends against what the server accepts. */
const ruleTokenBudgetField: Rule = ({ health, view }) => {
  const aliases = lower(stringListAt(health, ["token_budget_aliases"]));
  if (aliases.size === 0) return [];
  const configured = stringAt(view.compat, ["maxTokensField"]);
  if (configured === undefined) {
    return [
      finding(
        "token-budget-field",
        "info",
        `pi auto-detects the token-budget field; this server takes ${[...aliases].join(", ")}`,
        "Nothing to fix. Pinning `compat.maxTokensField` is worth doing only if a request ever comes back saying the field it got was not the field it sent.",
      ),
    ];
  }
  if (aliases.has(configured.toLowerCase())) {
    return [ok("token-budget-field", `\`${configured}\` is one the server accepts`, aliases.size + " aliases reported")];
  }
  return [
    finding(
      "token-budget-field-unsupported",
      "error",
      `\`maxTokensField: "${configured}"\` is not an alias this server takes`,
      `It accepts ${[...aliases].join(", ")}. Every request will ask for a field that does not exist and get the server's own default ceiling instead — which is not the ceiling the config declares.`,
      {
        evidence: `token_budget_aliases=${[...aliases].join(", ")}`,
        suggestions: [
          providerSuggestion("compat.maxTokensField", [...aliases][0], "one the server actually accepts"),
        ],
      },
    ),
  ];
};

/**
 * Streaming usage: the loop's context guard is fed by `usage.input +
 * usage.cacheRead` on every turn (see `src/context.ts`). If the server never
 * sends those numbers, the guard sees zero and never fires, and the only clock
 * left is the wall clock — which is the failure mode this loop was built to avoid.
 */
const ruleStreamingUsage: Rule = ({ health, view }) => {
  const supported = supportedNames(health);
  const supportsStreamOptions = supported.has("stream_options");
  const configured = booleanAt(view.compat, ["supportsUsageInStreaming"]);
  const guard = "the `context-exhausted` guard in src/context.ts";

  if (supportsStreamOptions && configured === false) {
    return [
      finding(
        "usage-not-requested",
        "warn",
        "the server can report streaming usage and the config turns the request off",
        `Without usage in the stream, ${guard} has nothing to measure: it will never fire, and every over-long run falls through to the wall-clock timeout instead of stopping early.`,
        {
          suggestions: [
            providerSuggestion("compat.supportsUsageInStreaming", true, "the server accepts stream_options, and the loop reads usage every turn"),
          ],
        },
      ),
    ];
  }
  if (!supportsStreamOptions && configured !== false) {
    return [
      finding(
        "no-streaming-usage",
        "warn",
        "the report does not list `stream_options`, so per-turn usage may never arrive",
        `${guard} depends on per-turn usage. If the stream carries no usage, the guard stays silent and the work timeout becomes the only clock — the one thing in this harness that ends a run without a verdict. If requests fail outright, set \`supportsUsageInStreaming: false\` so pi stops asking; if they succeed but report nothing, treat the guard as unavailable.`,
      ),
    ];
  }
  return [ok("streaming-usage", "per-turn usage is available to the context guard", "stream_options is accepted")];
};

/**
 * The harness's own tool schemas, read against the server's structured-output
 * contract: which JSON Schema keywords it enforces, which it accepts and ignores,
 * and which it refuses outright.
 */
const ruleToolSchemas: Rule = ({ health, toolSchemas }) => {
  const refused = lower(stringListAt(health, ["structured_output.refused"]));
  const notEnforced = lower(stringListAt(health, ["structured_output.accepted_not_enforced"]));
  const entries = Object.entries(toolSchemas);
  if (entries.length === 0 || (refused.size === 0 && notEnforced.size === 0)) return [];

  /**
   * Annotation keywords. They sit in the `accepted_not_enforced` list because the
   * server does not act on them, but nothing was ever claiming it would: a
   * `description` is documentation, not a constraint. Reporting one per field
   * would bury the `minimum` that actually matters.
   */
  const ANNOTATIONS = new Set(["description", "title", "examples", "$comment", "$schema"]);

  /**
   * Containers whose *keys* are property names rather than schema keywords.
   * Without this a property called `format` would read as a refused keyword.
   */
  const NAMING_CONTAINERS = new Set(["properties", "patternProperties", "$defs", "definitions"]);

  const hits: { tool: string; path: string; keyword: string; kind: "refused" | "unenforced" }[] = [];
  const walk = (node: unknown, tool: string, path: string[]): void => {
    if (Array.isArray(node)) {
      node.forEach((item, index) => walk(item, tool, [...path, `[${index}]`]));
      return;
    }
    if (!isRecord(node)) return;
    for (const [key, value] of Object.entries(node)) {
      if (NAMING_CONTAINERS.has(key)) {
        if (isRecord(value)) {
          for (const [name, child] of Object.entries(value)) walk(child, tool, [...path, key, name]);
        }
        continue;
      }
      const needle = key.toLowerCase();
      if (refused.has(needle)) hits.push({ tool, path: path.join(".") || "(root)", keyword: key, kind: "refused" });
      else if (notEnforced.has(needle) && !ANNOTATIONS.has(needle)) {
        hits.push({ tool, path: path.join(".") || "(root)", keyword: key, kind: "unenforced" });
      }
      walk(value, tool, [...path, key]);
    }
  };
  for (const [tool, schema] of entries) walk(schema, tool, []);

  const refusedHits = hits.filter((hit) => hit.kind === "refused");
  if (refusedHits.length > 0) {
    return [
      finding(
        "tool-schema-refused",
        "error",
        `the server refuses these schema keywords: ${[...new Set(refusedHits.map((hit) => hit.keyword))].join(", ")}`,
        `A schema carrying a refused keyword fails the request that carries it — which is every request in the pass that registered the tool. Rewrite the constraint as prose in the tool description plus a validator in the tool's own execute, which is the pattern ${refusedHits.map((hit) => hit.tool).join(", ")} would follow.`,
        {
          evidence: refusedHits.map((hit) => `${hit.tool} ${hit.path} → ${hit.keyword}`).join("; "),
        },
      ),
    ];
  }
  if (hits.length > 0) {
    return [
      finding(
        "tool-schema-unenforced",
        "info",
        `accepted and not enforced: ${[...new Set(hits.map((hit) => hit.keyword))].join(", ")}`,
        "These constrain nothing on the wire: the model can emit outside the bound and the server will let it through. That is only safe because the harness checks the same bounds itself — which it does for `report_split`'s priority range — so this is a note about where the guarantee actually lives, not a request to change anything.",
        { evidence: hits.map((hit) => `${hit.tool} ${hit.path} → ${hit.keyword}`).join("; ") },
      ),
    ];
  }
  return [ok("tool-schema", "no schema keyword this server refuses or ignores", entries.map(([tool]) => tool).join(", "))];
};

/** Prompt caching: what it buys this loop, and what it does not. */
const rulePromptCache: Rule = ({ health }) => {
  const enabled = booleanAt(health, ["prompt_cache.enabled"]);
  if (enabled === undefined) return [];
  const identical = booleanAt(health, ["prompt_cache.bitwise_identical_to_cold"]);
  const capMb = numberAt(health, ["prompt_cache.cap_mb"]);
  const findings: AuditFinding[] = [];

  if (!enabled) {
    findings.push(
      finding(
        "prompt-cache-off",
        "info",
        "prompt caching is off",
        "Every turn re-pays the whole prefill: system prompt, tool definitions and replayed history. On a loop that deliberately never compacts, that is the whole request cost every turn. Nothing in the client config can fix it, but it is the number to ask the operator about.",
      ),
    );
  }
  if (identical === false) {
    findings.push(
      finding(
        "prompt-cache-not-deterministic",
        "info",
        "a warm cache is not bit-identical to a cold run",
        "A cached reply and a cold reply can differ. Fine for work; not fine for a repro — do not diff two runs, or two attempts at the same ticket, and expect the difference to mean something about the code.",
      ),
    );
  }
  if (enabled && identical !== false && capMb === undefined) return [];
  if (enabled && capMb !== undefined) {
    findings.push(
      finding(
        "prompt-cache-cap",
        "info",
        `the prompt cache is capped at ${capMb} MB`,
        "The cap bounds how much prompt can stay warm. The loop's prefix is a system prompt plus tool definitions replayed every turn, so a small cap mostly holds; what it will not hold is a long history across a big ticket. Expect cache reads to stop growing near the cap rather than to break.",
      ),
    );
  }
  return findings;
};

/** Live capacity at startup: queue time is inside the work budget, not outside it. */
const ruleCapacity: Rule = ({ health }) => {
  const slots = numberAt(health, ["slots"]);
  if (slots === undefined) return [];
  const queued = numberAt(health, ["queued"]) ?? 0;
  const inFlight = numberAt(health, ["in_flight"]) ?? 0;
  const busy = booleanAt(health, ["busy"]);

  if (queued > 0) {
    return [
      finding(
        "server-queueing",
        "info",
        `the server is already queueing (${queued} waiting, ${inFlight}/${slots} slots busy)`,
        "Anything this run asks for waits behind that. The work budget is a wall clock, so queue time is charged to the ticket exactly like inference time — which is worth knowing before reading a timeout as slow work.",
        { evidence: `busy=${String(busy ?? "?")} queued=${queued} in_flight=${inFlight} slots=${slots}` },
      ),
    ];
  }
  if (slots <= 1) {
    return [
      finding(
        "single-slot",
        "info",
        "one serving slot",
        "The split pass and the work pass serialise against each other, and so does a human at `/model` in the same server. Fine for one loop at a time; a bottleneck the moment there are two.",
      ),
    ];
  }
  return [ok("capacity", `${slots} slots free at startup`, `in_flight=${inFlight}, queued=${queued}`)];
};

/**
 * What the audit did *not* check.
 *
 * A report that says nothing about a compat flag leaves the setting unverified,
 * and a summary that hides that is a summary that gets trusted too much.
 */
const ruleHealthSilentOn: Rule = ({ health, view }) => {
  const silent = Object.keys(view.compat).filter((key) => !AUDITED_COMPAT_KEYS.includes(key));
  if (silent.length === 0) return [];
  const reportMentions = silent.filter((key) => {
    const needle = key.toLowerCase();
    const flat = JSON.stringify(health).toLowerCase();
    return flat.includes(needle) || flat.includes(needle.replace(/([a-z])([A-Z])/gu, "$1_$2").toLowerCase());
  });
  if (reportMentions.length > 0) {
    return [
      finding(
        "health-silent-on",
        "info",
        `the report does not speak to: ${silent.join(", ")}`,
        `These are set in the config and no rule here consulted them against the report (${reportMentions.length} of them appear somewhere in the payload, but not in a field this audit reads). They stand exactly as configured, unverified.`,
      ),
    ];
  }
  return [
    finding(
      "health-silent-on",
      "info",
      `the report says nothing about: ${silent.join(", ")}`,
      "No rule here could check them against the server. They stand exactly as configured — unverified, not validated.",
    ),
  ];
};

// ── config-only checks (runnable without a server) ────────────────────────

const PLACEHOLDER_MARKERS = ["<", ">", "yoururl", "yourmodel", "changeme", "todo", "replace_me"];

/**
 * A config that still contains its own template.
 *
 * Detected before anything else because it explains every other finding: an
 * endpoint that is unreachable and a model that does not exist are usually the
 * same fact, and the fact is that nobody filled the template in.
 */
export function ruleConfigPlaceholders(view: ModelConfigView): readonly AuditFinding[] {
  const suspect = (value: string): boolean => {
    const needle = value.toLowerCase();
    return PLACEHOLDER_MARKERS.some((marker) => needle.includes(marker));
  };
  const offenders: string[] = [];
  if (view.baseUrl === "" || suspect(view.baseUrl)) offenders.push(`baseUrl="${view.baseUrl}"`);
  if (view.modelId === "unknown" || suspect(view.modelId)) offenders.push(`model id="${view.modelId}"`);
  if (offenders.length === 0) {
    return [ok("config-placeholders", "no template placeholders left in the target", `provider=${view.provider}`)];
  }
  return [
    finding(
      "config-placeholder",
      "error",
      `the config still holds its template: ${offenders.join(", ")}`,
      "Nothing downstream can work from this, and every other check will report a knock-on of the same cause. Fill the values in and re-run.",
    ),
  ];
}

// ── the auditor ──────────────────────────────────────────────────────────

const HEALTH_RULES: readonly Rule[] = [
  ruleHealthStatus,
  ruleServerIdentity,
  ruleVersionMatch,
  ruleEndpointRoutes,
  ruleContextWindow,
  ruleOutputCeiling,
  ruleReasoningDeclared,
  ruleThinkingEffortChannel,
  ruleThinkingEnableChannel,
  ruleChatTemplateKwargsKnown,
  ruleEffortValues,
  ruleToolCalling,
  ruleStrictMode,
  ruleForcedCalls,
  ruleVision,
  ruleSamplingParams,
  ruleGreedyDecode,
  ruleTokenBudgetField,
  ruleStreamingUsage,
  ruleToolSchemas,
  rulePromptCache,
  ruleCapacity,
  ruleHealthSilentOn,
];

type MutableCounts = { -readonly [K in keyof AuditCounts]: number };

const emptyCounts = (): MutableCounts => ({ error: 0, warn: 0, info: 0, ok: 0 });

/**
 * Compare a health report with the config a session will run on.
 *
 * No I/O, no clock, no environment: give it the two payloads and it gives back
 * findings. With no report at all it still says so and still runs the checks
 * that need no server, because the most common reason a server is unreachable
 * is a config that was never filled in.
 */
export function auditProvider(input: AuditInput): AuditReport {
  const levels = input.levels ?? {};
  const health = input.health;
  const view = input.view;
  const healthOrEmpty: JsonRecord = health ?? {};
  const usedLevels = levelsInPlay({
    ...levels,
    serverDefault: levels.serverDefault ?? stringAt(healthOrEmpty, ["reasoning_effort_default"]),
  });

  const findings: AuditFinding[] = [...ruleConfigPlaceholders(view)];
  if (health === undefined) {
    findings.push(
      finding(
        "health-unread",
        "warn",
        "no health report to compare against",
        `Nothing was read from ${input.healthUrl}, so the window, the caps and the thinking knobs are all still unverified. Only the config-local checks ran.`,
      ),
    );
  } else {
    const ctx: RuleContext = {
      view,
      health,
      levels,
      toolSchemas: input.toolSchemas ?? {},
      levelsInPlay: usedLevels,
      thinkingSharedWithAnswer: booleanAt(health, ["token_budget_covers_reasoning"]) ?? true,
    };
    for (const rule of HEALTH_RULES) findings.push(...rule(ctx));
  }

  const counts = emptyCounts();
  for (const item of findings) counts[item.severity] += 1;

  const seen = new Set<string>();
  const suggestions: AuditSuggestion[] = [];
  for (const item of findings) {
    for (const suggestion of item.suggestions ?? []) {
      const key = `${suggestion.scope}:${suggestion.field}`;
      if (seen.has(key)) continue;
      seen.add(key);
      suggestions.push(suggestion);
    }
  }

  return {
    provider: view.provider,
    modelId: view.modelId,
    target: input.target,
    healthUrl: input.healthUrl,
    healthState: health === undefined ? "unread" : stringAt(health, ["status"]) ?? "reported",
    findings,
    counts,
    suggestions,
    blocking: counts.error > 0,
  };
}

// ── turning suggestions into a config ────────────────────────────────────

function isArraySegment(segment: string): boolean {
  return /^\[\d+\]$/u.test(segment);
}

function childOf(container: unknown, segment: string): unknown {
  if (Array.isArray(container) && isArraySegment(segment)) {
    return container[Number(segment.slice(1, -1))];
  }
  if (isRecord(container)) return container[segment];
  return undefined;
}

function containerFor(existing: unknown, nextSegment: string): JsonRecord | unknown[] {
  if (Array.isArray(existing) || isRecord(existing)) return existing;
  return isArraySegment(nextSegment) ? [] : {};
}

function assignInto(container: unknown, segment: string, value: unknown): void {
  if (Array.isArray(container) && isArraySegment(segment)) {
    const index = Number(segment.slice(1, -1));
    if (value === undefined) {
      if (index >= 0 && index < container.length) container.splice(index, 1);
      return;
    }
    container[index] = value;
    return;
  }
  if (isRecord(container)) {
    if (value === undefined) delete container[segment];
    else container[segment] = value;
  }
}

function setAtPath(root: unknown, segments: readonly string[], value: unknown): void {
  if (segments.length === 0) return;
  const head = segments[0];
  if (head === undefined) return;
  if (segments.length === 1) {
    assignInto(root, head, value);
    return;
  }
  const existing = childOf(root, head);
  const next = containerFor(existing, segments[1] ?? "");
  if (existing !== next) assignInto(root, head, next);
  setAtPath(next, segments.slice(1), value);
}

/**
 * Where a suggestion lands in the raw `models.json`.
 *
 * A model with its own entry in `providers.<p>.models` is patched in place.
 * One that came from the bundled catalog or was never declared goes under
 * `modelOverrides`, which is what that key exists for: metadata for a model
 * this provider block does not itself declare.
 */
export function rawPathFor(target: ConfigTarget, suggestion: AuditSuggestion): string[] {
  const base = ["providers", target.provider];
  if (suggestion.scope === "provider") return [...base, ...suggestion.field.split(".")];
  const rest = suggestion.field.split(".");
  return target.modelIndex === undefined
    ? [...base, "modelOverrides", target.modelId, ...rest]
    : [...base, "models", `[${target.modelIndex}]`, ...rest];
}

/** The same path the way a human reads it: `providers.llamacpp.models[0].maxTokens`. */
export function displayPath(target: ConfigTarget, suggestion: AuditSuggestion): string {
  return rawPathFor(target, suggestion).reduce<string>(
    (acc, segment) => (isArraySegment(segment) ? `${acc}${segment}` : acc === "" ? segment : `${acc}.${segment}`),
    "",
  );
}

function cloneJson(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value)) as unknown;
}

/**
 * Apply suggestions to a raw config, returning a new object.
 *
 * The caller decides what to do with the result; nothing here writes a file or
 * refuses a suggestion. Suggestions on the same path collapse to the first one,
 * because two rules disagreeing about the same key must not produce a patch that
 * silently picks the later value.
 */
export function applySuggestions(
  raw: unknown,
  suggestions: readonly AuditSuggestion[],
  target: ConfigTarget,
): JsonRecord {
  const cloned = cloneJson(isRecord(raw) ? raw : {}) as JsonRecord;
  const applied = new Set<string>();
  for (const suggestion of suggestions) {
    const key = `${suggestion.scope}:${suggestion.field}`;
    if (applied.has(key)) continue;
    applied.add(key);
    setAtPath(cloned, rawPathFor(target, suggestion), suggestion.remove ? undefined : suggestion.value);
  }
  return cloned;
}

const SECRET_KEYS = /^(apikey|api_key|token|secret|password|authorization)$/i;

/**
 * A copy safe to print.
 *
 * A literal API key in a startup log is a leaked API key, and this runs on every
 * start. An `$ENVVAR` / `${ENVVAR}` reference is not a secret and is kept,
 * because that is the whole point of writing it that way.
 */
export function redactSecrets(value: unknown): unknown {
  if (Array.isArray(value)) return value.map((item) => redactSecrets(item));
  if (!isRecord(value)) return value;
  const out: JsonRecord = {};
  for (const [key, item] of Object.entries(value)) {
    if (SECRET_KEYS.test(key)) {
      out[key] =
        typeof item === "string" && (item.startsWith("$") || item.startsWith("{") || item === "")
          ? item
          : "«redacted»";
    } else {
      out[key] = redactSecrets(item);
    }
  }
  return out;
}

/** The patched config, redacted, ready to print or write. */
export function renderProposedConfig(raw: unknown, report: AuditReport, target: ConfigTarget): string {
  return `${JSON.stringify(redactSecrets(applySuggestions(raw, report.suggestions, target)), null, 2)}\n`;
}

// ── rendering ────────────────────────────────────────────────────────────

const MARKER: Readonly<Record<AuditSeverity, string>> = {
  error: "ERR ",
  warn: "WARN",
  info: "info",
  ok: "ok  ",
};

function compact(value: unknown, max = 160): string {
  let text: string;
  try {
    text = JSON.stringify(value) ?? String(value);
  } catch {
    text = String(value);
  }
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** One finding, with its evidence and its patches, as one multi-line block. */
export function renderFinding(item: AuditFinding, target: ConfigTarget): string {
  const lines = [`${item.id}: ${item.title}`, `  ${item.detail}`];
  if (item.evidence !== undefined && item.evidence !== "") lines.push(`  evidence: ${item.evidence}`);
  for (const suggestion of item.suggestions ?? []) {
    lines.push(
      suggestion.remove
        ? `  → remove ${displayPath(target, suggestion)}`
        : `  → set ${displayPath(target, suggestion)} = ${compact(suggestion.value)}`,
    );
    lines.push(`    ${suggestion.why}`);
  }
  return lines.join("\n");
}

/** The findings worth a human's attention, newest problems first. */
export function notableFindings(
  report: AuditReport,
  options: { readonly showOk?: boolean } = {},
): AuditFinding[] {
  const rank: Readonly<Record<AuditSeverity, number>> = { error: 0, warn: 1, info: 2, ok: 3 };
  return report.findings
    .filter((item) => options.showOk === true || item.severity !== "ok")
    .slice()
    .sort((left, right) => rank[left.severity] - rank[right.severity]);
}

/** The one-line tally a run ends on. */
export function summariseAudit(report: AuditReport): string {
  return (
    `${report.counts.error} error, ${report.counts.warn} warn, ${report.counts.info} info, ${report.counts.ok} ok ` +
    `(${report.findings.length} check(s), ${report.suggestions.length} suggested change(s))`
  );
}

/** The header line: who was compared against what, and how the report answered. */
export function auditHeader(report: AuditReport, probeMs?: number): string {
  const timing = probeMs === undefined ? "" : ` (${probeMs}ms)`;
  return (
    `provider audit — ${report.provider}/${report.modelId} against ${report.healthUrl}${timing} [${report.healthState}]`
  );
}

/** The whole report as plain lines: header, findings, tally. */
export function renderAudit(
  report: AuditReport,
  options: { readonly showOk?: boolean; readonly probeMs?: number } = {},
): string[] {
  const lines = [auditHeader(report, options.probeMs)];
  for (const item of notableFindings(report, { showOk: options.showOk })) {
    lines.push(`  ${MARKER[item.severity]} ${renderFinding(item, report.target)}`.replace(/\n/gu, "\n       "));
  }
  lines.push(summariseAudit(report));
  if (report.counts.error > 0) {
    lines.push("  the errors above are why a strict run would not start; fix the config before the work does");
  }
  return lines;
}
