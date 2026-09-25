/**
 * The model-server probe — the only place the loop talks to `/health`.
 *
 * Why this exists: everything the loop needs to know about the server — the
 * context window, the output cap, which thinking knobs the wire actually carries,
 * whether the box is busy — used to live in a hand-maintained `models.json`. That
 * file is a *claim*. When the claim drifts, nothing notices until a run 40 minutes
 * into a ticket: an over-declared `contextWindow` is a 400 at the wall, an
 * under-declared one wastes a fifth of the room, and a thinking level the server
 * never receives looks, from the terminal, exactly like one it did. The server
 * publishes its own truth at `/health`; reading it is one GET.
 *
 * Three rules keep the probe honest:
 *
 * 1. **Read-only.** `GET`, no body, one attempt, a timeout. `/health` is a status
 *    endpoint, and a probe that retries in a loop is a denial-of-service with good
 *    intentions. Callers decide what a failure means.
 * 2. **A field you cannot read is unknown, never zero.** Every typed field is
 *    optional. Deriving `contextWindow: 0` from a missing field would make every
 *    later check pass while the request fails — the worst possible failure,
 *    because it looks like the guard working. `src/autoconfig.ts` turns unknown
 *    fields into notes, not values.
 * 3. **Nothing is inferred from a failure.** This returns a discriminated result,
 *    not a thrown exception and not a default payload. Unreachable is a fact with
 *    a reason attached; the caller falls back to the declared config and says so.
 */

/** The typed slice of `/health` this project reads. Everything is optional: see rule 2. */
export interface ServerHealth {
  /** `"ok"`, `"degraded"`, … Anything that is not `ok` is the operator's business. */
  readonly status?: string;
  /** Which model the server says it is serving. Compared against the configured one. */
  readonly model?: string;

  // ── room to work in ───────────────────────────────────────────────────────
  /** Total context window, in tokens. */
  readonly context?: number;
  /** Per-slot context. May be smaller than `context` when slots share a pool. */
  readonly slot_ctx?: number;
  /** Total KV cache positions across every slot. */
  readonly kv_pool_positions?: number;
  /** Hard ceiling on a single response's token budget. Exceeding it is a 400. */
  readonly max_tokens_cap?: number;
  /** What the server uses when a request sends no token budget. */
  readonly max_tokens_default?: number;

  // ── thinking ──────────────────────────────────────────────────────────────
  /** Effort names the server accepts, e.g. `["low","medium","high","xhigh","none"]`. */
  readonly reasoning_effort_values?: readonly string[];
  readonly reasoning_effort_default?: string;
  /** True when reasoning tokens come out of the same budget as the answer. */
  readonly token_budget_covers_reasoning?: boolean;
  /** Prose rule for answer room, e.g. `"max(1024, 15% of max_tokens)"`. See `parseAnswerRoom`. */
  readonly thinking_answer_room?: string;
  /** Field names the server accepts for capping reasoning, e.g. `["thinking_budget_tokens", …]`. */
  readonly thinking_control_aliases?: readonly string[];
  /** Chat-template kwargs the server honours, e.g. `["reasoning_effort","enable_thinking","preserve_thinking"]`. */
  readonly chat_template_kwargs?: readonly string[];

  // ── capability advertising ────────────────────────────────────────────────
  readonly supported?: readonly string[];
  readonly not_implemented?: readonly string[];
  readonly accepted_but_ignored?: readonly string[];

  // ── the wire shape of tools and structured output ──────────────────────────
  readonly tool_calls?: {
    readonly wire_format?: string;
    readonly streaming?: boolean;
    readonly parallel_tool_calls?: boolean;
    readonly constrained_decoding?: boolean;
    /** A forced tool call skips the thinking phase. Decides `tool_choice: "required"` for ever. */
    readonly forced_call_disables_thinking?: boolean;
  };
  readonly structured_output?: {
    readonly enabled?: boolean;
    readonly refused?: readonly string[];
    readonly accepted_not_enforced?: readonly string[];
    readonly sampling?: string;
  };
  readonly sampling?: {
    readonly implemented?: readonly string[];
    readonly not_implemented?: readonly string[];
  };
  readonly vision?: {
    readonly enabled?: boolean;
    readonly disabled_because?: readonly string[];
  };

  // ── is the box free? ──────────────────────────────────────────────────────
  readonly busy?: boolean;
  readonly slots?: number;
  readonly queued?: number;
  readonly in_flight?: number;
  readonly busy_for_s?: number;
  readonly version?: {
    readonly api?: string;
    readonly engine?: string;
    readonly match?: boolean;
  };
  readonly engine?: {
    readonly responds?: boolean;
    readonly probe_s?: number;
  };

  /** The payload as parsed, for anything this type has not been taught yet. */
  readonly raw: Readonly<Record<string, unknown>>;
}

export type HealthErrorKind =
  | "not-configured"
  | "unreachable"
  | "timeout"
  | "http"
  | "shape";

export class HealthError extends Error {
  readonly kind: HealthErrorKind;
  readonly detail: string;
  /** HTTP status, when the failure has one. */
  readonly status?: number;

  constructor(kind: HealthErrorKind, message: string, detail = "", status?: number) {
    super(message);
    this.name = "HealthError";
    this.kind = kind;
    this.detail = detail;
    this.status = status;
  }

  static is(error: unknown): error is HealthError {
    return (
      error instanceof HealthError ||
      (typeof error === "object" &&
        error !== null &&
        (error as { name?: string }).name === "HealthError")
    );
  }
}

export type HealthProbeResult =
  | { readonly ok: true; readonly health: ServerHealth; readonly latencyMs: number; readonly url: string }
  | { readonly ok: false; readonly error: HealthError; readonly url: string | null };

/** The injectable fetch shape: a subset of `globalThis.fetch`, so tests never touch a socket. */
export type FetchLike = (
  url: string,
  init: { method: string; signal?: AbortSignal; headers?: Record<string, string> },
) => Promise<{
  ok: boolean;
  status: number;
  text(): Promise<string>;
}>;

export interface HealthProbeOptions {
  /** Full health endpoint URL, e.g. `http://llm.home:8081/health`. Absent → not configured. */
  readonly url?: string;
  /** How long to wait for the report. Default 5s: a health endpoint that takes longer is telling you something. */
  readonly timeoutMs?: number;
  readonly fetchImpl?: FetchLike;
  readonly now?: () => number;
}

const DEFAULT_PROBE_TIMEOUT_MS = 5_000;

/**
 * Turn a model base URL into the health URL for the same box.
 *
 * The alternative — a second URL in the environment, describing the same box as
 * the first — is a drift bug waiting to be filed: change `models.json` and
 * nothing changes the probe, which then cheerfully reports the limits of the
 * machine nobody is talking to. So the probe asks the provider where the box is
 * and derives from that, with an explicit URL kept only as an override for the
 * topology this guess cannot express.
 *
 * A trailing API path (`/v1`, `/v1/chat/completions`) is replaced rather than
 * appended to, and any prefix in front of it is preserved: a box mounted at
 * `/gpu1/v1` reports at `/gpu1/health`.
 */
export function healthUrlFromBaseUrl(baseUrl: string): string | null {
  try {
    const url = new URL(baseUrl);
    const stripped = url.pathname.replace(
      /\/v\d+(?:\/(?:chat\/completions|completions|responses))?\/?$/iu,
      "",
    );
    const base = stripped.replace(/\/+$/, "");
    url.pathname = `${base}/health`;
    url.search = "";
    url.hash = "";
    return url.toString();
  } catch {
    return null;
  }
}

/** Where the probe's address came from, for the startup line. */
export type ProbeSource = "configured" | "provider";

export type ProbeTarget =
  | { readonly url: string; readonly source: ProbeSource; readonly from?: string }
  | { readonly url: null; readonly reason: string };

/**
 * Decide what to probe. Pure, so the precedence is testable rather than
c * incidentally right.
 *
 * Explicit wins — if you typed a URL you meant it. Otherwise the provider's own
 * `baseUrl` is the answer, because that is the address the run will actually
 * use, and a probe of any other box is worse than no probe: it is a number from
 * somewhere else, printed where you expect a number from here.
 */
export function resolveProbeTarget(input: {
  configuredUrl?: string;
  providerBaseUrl?: string;
  /** Why there is no provider base URL, when there isn't one. */
  providerNote?: string;
}): ProbeTarget {
  const configured = input.configuredUrl?.trim();
  if (configured !== undefined && configured !== "") {
    return { url: configured, source: "configured" };
  }
  const base = input.providerBaseUrl?.trim();
  if (base === undefined || base === "") {
    return {
      url: null,
      reason:
        input.providerNote ??
        "no health endpoint is configured and no model base URL is known, so there " +
        "is no box to ask; using the declared model config",
    };
  }
  const derived = healthUrlFromBaseUrl(base);
  if (derived === null) {
    return {
      url: null,
      reason: `the provider's base URL "${base}" is not a URL this probe can derive a health endpoint from`,
    };
  }
  return { url: derived, source: "provider", from: base };
}


function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() !== "" ? value : undefined;
}

function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function bool(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

/** Strings only, dropping anything that is not a string rather than coercing it. */
function strList(value: unknown): readonly string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const out = value.filter((entry): entry is string => typeof entry === "string");
  return out.length > 0 ? out : undefined;
}

/**
 * Validate a parsed `/health` body into {@link ServerHealth}. Pure, and exported
 * so the fixture test exercises it without a fetch.
 *
 * `reason` keys are kept with the value so the notes the caller writes can name
 * *why* a thing is off (this server reports `vision.disabled_because` — a reason
 * is worth more in a log line than a boolean).
 */
export function parseServerHealth(body: unknown): ServerHealth {
  if (!isPlainObject(body)) {
    throw new HealthError("shape", "the health payload is not a JSON object");
  }
  // A payload with none of the fields we came for is not this server's health
  // report — a captive-portal page, a proxy error body, an HTML 200. Say that
  // here rather than letting every derived field come back unknown.
  const recognisable = ["status", "context", "slots", "supported", "max_tokens_cap"];
  if (!recognisable.some((key) => key in body)) {
    throw new HealthError(
      "shape",
      "the health payload has none of the fields this probe reads " +
        `(${recognisable.join(", ")}); it is not a /health report`,
    );
  }

  const toolCalls = isPlainObject(body["tool_calls"]) ? body["tool_calls"] : undefined;
  const structured = isPlainObject(body["structured_output"]) ? body["structured_output"] : undefined;
  const sampling = isPlainObject(body["sampling"]) ? body["sampling"] : undefined;
  const vision = isPlainObject(body["vision"]) ? body["vision"] : undefined;
  const version = isPlainObject(body["version"]) ? body["version"] : undefined;
  const engine = isPlainObject(body["engine"]) ? body["engine"] : undefined;

  return {
    status: str(body["status"]),
    model: str(body["model"]),
    context: num(body["context"]),
    slot_ctx: num(body["slot_ctx"]),
    kv_pool_positions: num(body["kv_pool_positions"]),
    max_tokens_cap: num(body["max_tokens_cap"]),
    max_tokens_default: num(body["max_tokens_default"]),
    reasoning_effort_values: strList(body["reasoning_effort_values"]),
    reasoning_effort_default: str(body["reasoning_effort_default"]),
    token_budget_covers_reasoning: bool(body["token_budget_covers_reasoning"]),
    thinking_answer_room: str(body["thinking_answer_room"]),
    thinking_control_aliases: strList(body["thinking_control_aliases"]),
    chat_template_kwargs: strList(body["chat_template_kwargs"]),
    supported: strList(body["supported"]),
    not_implemented: strList(body["not_implemented"]),
    accepted_but_ignored: strList(body["accepted_but_ignored"]),
    tool_calls: toolCalls
      ? {
          wire_format: str(toolCalls["wire_format"]),
          streaming: bool(toolCalls["streaming"]),
          parallel_tool_calls: bool(toolCalls["parallel_tool_calls"]),
          constrained_decoding: bool(toolCalls["constrained_decoding"]),
          forced_call_disables_thinking: bool(toolCalls["forced_call_disables_thinking"]),
        }
      : undefined,
    structured_output: structured
      ? {
          enabled: bool(structured["enabled"]),
          refused: strList(structured["refused"]),
          accepted_not_enforced: strList(structured["accepted_not_enforced"]),
          sampling: str(structured["sampling"]),
        }
      : undefined,
    sampling: sampling
      ? {
          implemented: strList(sampling["implemented"]),
          not_implemented: strList(sampling["not_implemented"]),
        }
      : undefined,
    vision: vision
      ? {
          enabled: bool(vision["enabled"]),
          disabled_because: strList(vision["disabled_because"]),
        }
      : undefined,
    busy: bool(body["busy"]),
    slots: num(body["slots"]),
    queued: num(body["queued"]),
    in_flight: num(body["in_flight"]),
    busy_for_s: num(body["busy_for_s"]),
    version: version
      ? { api: str(version["api"]), engine: str(version["engine"]), match: bool(version["match"]) }
      : undefined,
    engine: engine ? { responds: bool(engine["responds"]), probe_s: num(engine["probe_s"]) } : undefined,
    raw: body,
  };
}

/**
 * One GET, one timeout, no retry.
 *
 * Returns a result rather than throwing so the caller has to say what it is doing
 * about a missing report. Every failure path carries the reason the human would
 * want: the URL, the status code, the parse error.
 */
export async function probeServerHealth(options: HealthProbeOptions = {}): Promise<HealthProbeResult> {
  const url = options.url?.trim();
  if (url === undefined || url === "") {
    return {
      ok: false,
      url: null,
      error: new HealthError(
        "not-configured",
        "no health endpoint is configured, so the server's own limits are unknown; " +
          "using the declared model config",
      ),
    };
  }

  const fetchImpl = options.fetchImpl ?? (globalThis.fetch as unknown as FetchLike | undefined);
  if (fetchImpl === undefined) {
    return {
      ok: false,
      url,
      error: new HealthError("unreachable", "no fetch implementation is available in this runtime"),
    };
  }

  const timeoutMs = options.timeoutMs ?? DEFAULT_PROBE_TIMEOUT_MS;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  timer.unref?.();
  const started = options.now?.() ?? Date.now();

  try {
    const response = await fetchImpl(url, {
      method: "GET",
      signal: controller.signal,
      headers: { accept: "application/json" },
    });
    const latencyMs = Math.max(0, (options.now?.() ?? Date.now()) - started);
    if (!response.ok) {
      return {
        ok: false,
        url,
        error: new HealthError(
          "http",
          `the health endpoint answered ${response.status}, which is not a report`,
          "",
          response.status,
        ),
      };
    }
    const text = await response.text();
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch (error) {
      return {
        ok: false,
        url,
        error: new HealthError(
          "shape",
          `the health endpoint did not return JSON: ${error instanceof Error ? error.message : String(error)}`,
        ),
      };
    }
    try {
      return { ok: true, health: parseServerHealth(parsed), latencyMs, url };
    } catch (error) {
      if (HealthError.is(error)) return { ok: false, url, error };
      throw error;
    }
  } catch (error) {
    if (error instanceof Error && (error.name === "AbortError" || controller.signal.aborted)) {
      return {
        ok: false,
        url,
        error: new HealthError(
          "timeout",
          `the health endpoint did not answer within ${timeoutMs}ms; ` +
            "that is itself information about the box",
        ),
      };
    }
    return {
      ok: false,
      url,
      error: new HealthError(
        "unreachable",
        `the health endpoint could not be reached: ${error instanceof Error ? error.message : String(error)}`,
      ),
    };
  } finally {
    clearTimeout(timer);
  }
}

export { DEFAULT_PROBE_TIMEOUT_MS };
