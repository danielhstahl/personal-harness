/**
 * The backend monitor — a read-only read of the inference server, for the fixed
 * chrome of the TUI.
 *
 * The loop asks the server one question at startup (`/health`, see
 * `src/health.ts` and ADR-002) and then goes quiet about it for the rest of a
 * run. The things that actually decide whether a pass is going well are *live*
 * facts about that same machine — how full the KV pool is, whether a slot is
 * free, what the decode rate is, whether the prompt cache is still warm — and
 * every one of them is sitting in a response nobody is reading.
 *
 * This module reads them and turns them into one glanceable line. It is kept
 * apart from everything it describes:
 *
 * - **Read-only, in the strongest sense.** Every request is a `GET` with no
 *   body and no credential beyond what the config already holds, and none of
 *   these endpoints can change anything. Nothing here takes an `issueId`, a
 *   session or a board handle: the monitor cannot *do* anything to the run it
 *   is drawn over.
 * - **Never load-bearing.** A dead server is a line that says `✕ unreachable`,
 *   not a crashed run. Every failure path returns a value, every timer is
 *   unref'd, and the surface draws the last good snapshot with its age attached
 *   rather than pretending a number is current.
 * - **Shaped by what the server actually says.** `metrics` and `cache` are
 *   advertised *inside* the health report (`"metrics": "/metrics"`,
 *   `"cache_counters": "/cache"`), and an endpoint that answers 404 stops
 *   being probed instead of being asked every two seconds forever. Since the
 *   bodies of `/metrics` and `/cache` are the server's business and not ours,
 *   every field is read through a candidate list (the `*_PATHS` constants) and
 *   an unmatched field renders as `—`. The monitor shows what it found, and
 *   {@link BackendMonitor.describe} reports what was there to find, so an
 *   unfamiliar server is a thing you can teach rather than a wall.
 */
import { truncateToWidth, visibleWidth, type Component } from "@earendil-works/pi-tui";

import { healthUrlFor, isRecord } from "./health.ts";
import type { JsonRecord } from "./health.ts";

// ── what the panel needs from a theme ────────────────────────────────────────

/**
 * The colour roles the monitor uses. Each one is also a presenter role, so pi's
 * live theme (`createPresenterTheme()` in `src/render.ts`) satisfies
 * {@link MonitorTheme} as-is: no hand-rolled escapes here either.
 */
export const MONITOR_ROLES = [
  "accent",
  "dim",
  "muted",
  "text",
  "success",
  "warning",
  "error",
  "borderMuted",
] as const;

export type MonitorRole = (typeof MONITOR_ROLES)[number];

/**
 * The only colour the monitor needs: "this value has this meaning". The pi
 * presenter theme satisfies it structurally (see `src/app.ts`), so the panel is
 * themed by the same highlight.js-backed object as the transcript and never
 * picks an ANSI code by hand.
 */
export interface MonitorTheme {
  color(role: MonitorRole, text: string): string;
}

/** The no-colour theme: the plain path, and the test path. */
export const PLAIN_MONITOR_THEME: MonitorTheme = {
  color: (_role: MonitorRole, text: string): string => text,
};

// ── endpoints ────────────────────────────────────────────────────────────────

export const MONITOR_ENDPOINTS = ["health", "metrics", "cache", "models"] as const;
export type EndpointName = (typeof MONITOR_ENDPOINTS)[number];

/** Where to ask. A missing member means "not asked". */
export interface MonitorUrls {
  readonly health?: string;
  readonly metrics?: string;
  readonly cache?: string;
  readonly models?: string;
}

/**
 * The four URLs for an API base, e.g. `http://host:8081/v1` →
 *
 * ```
 * health   http://host:8081/health
 * metrics  http://host:8081/metrics
 * cache    http://host:8081/cache
 * models   http://host:8081/v1/models
 * ```
 *
 * The diagnostics live *beside* the API, not inside it, so the version segment
 * comes off before they go on — and `healthUrlFor()` is what takes it off, so
 * this module and the startup audit can never disagree about where the report
 * is. `/v1/models` keeps the version segment that was actually present:
 * `http://host:8081/gw/v1` has a prefix *and* a version, and a gateway in
 * front of either is common enough that hard-coding the middle would be wrong
 * half the time.
 */
export function urlsForBase(baseUrl: string | undefined): MonitorUrls {
  // The health URL itself is derived by the one function that encodes where the
  // report lives, so the audit and the monitor cannot drift apart about it.
  const health = healthUrlFor(baseUrl);
  if (health === undefined) return {};
  const trimmed = (baseUrl ?? "").trim();
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return {};
  }
  // The version has to come off the *given* base: by the time the health URL is
  // in hand the version is gone, and a v2 server would be asked for v1 models.
  const path = url.pathname.replace(/\/+$/u, "");
  const version = /\/(v\d+)$/iu.exec(path)?.[1] ?? "v1";
  const prefix = path.replace(/\/v\d+$/iu, "").replace(/\/health$/iu, "");
  const origin = url.origin;
  return {
    health,
    metrics: `${origin}${prefix}/metrics`,
    cache: `${origin}${prefix}/cache`,
    models: `${origin}${prefix}/${version}/models`,
  };
}

/** `"/cache_counters"` → `"cache_counters"`: keep our prefix, take their name. */
function leafOf(path: unknown): string | undefined {
  if (typeof path !== "string") return undefined;
  const bare = path.trim().replace(/^\/+/u, "").replace(/\/+$/u, "");
  if (bare === "") return undefined;
  const leaf = bare.split("/").pop() ?? "";
  return leaf === "" ? undefined : leaf;
}

/** The `<origin><prefix>` the four diagnostic paths are hung off, or undefined. */
function apiBaseOf(urls: MonitorUrls): string | undefined {
  const health = urls.health ?? urls.metrics ?? urls.cache ?? urls.models;
  if (health === undefined) return undefined;
  try {
    const url = new URL(health);
    const path = url.pathname
      .replace(/\/health$/iu, "")
      .replace(/\/metrics$/iu, "")
      .replace(/\/cache$/iu, "")
      .replace(/\/models$/iu, "")
      .replace(/\/v\d+$/iu, "");
    return `${url.origin}${path}`;
  } catch {
    return undefined;
  }
}

/**
 * The endpoints the server itself advertised.
 *
 * `GET /health` answers with `"metrics": "/metrics"`, `"cache_counters":
 * "/cache"` and an `endpoints` array naming `/v1/models`. Those are facts from
 * the witness, so they override our guesses — but only the *leaf* of each
 * advertised path, because the path an engine reports is its own and a gateway
 * prefix in front of us is invisible to it. Taking our prefix and the server's
 * name is the only combination that is right from both sides.
 */
export function refineUrls(urls: MonitorUrls, health: JsonRecord | undefined): MonitorUrls {
  const apiBase = apiBaseOf(urls);
  if (health === undefined || apiBase === undefined) return urls;
  // A mutable copy: the resolved set is assembled here and handed back frozen.
  const next: { health?: string; metrics?: string; cache?: string; models?: string } = { ...urls };
  const byLeaf = (value: unknown): string | undefined => {
    const leaf = leafOf(value);
    return leaf === undefined ? undefined : `${apiBase}/${leaf}`;
  };
  const metrics = byLeaf(health.metrics ?? health.metrics_url ?? health.counters);
  if (metrics !== undefined) next.metrics = metrics;
  const cache = byLeaf(health.cache_counters ?? health.cache_url ?? health.cache);
  if (cache !== undefined) next.cache = cache;

  if (Array.isArray(health.endpoints)) {
    const modelPaths = health.endpoints
      .map((entry) => (typeof entry === "string" ? entry.trim() : ""))
      .filter((entry) => /\/models\/?$/u.test(entry));
    if (modelPaths.length === 1) {
      const version = /\/(v\d+)\/models\/?$/u.exec(modelPaths[0] ?? "")?.[1] ?? "v1";
      next.models = `${apiBase}/${version}/models`;
    }
  }
  return next;
}

/**
 * Where to point the monitor.
 *
 * An explicit monitor URL wins; failing that the audited provider's base, since
 * the monitor and the startup audit are looking at the same machine and the
 * monitor should not need a second config key to find it.
 */
export function resolveMonitorUrls(options: {
  readonly monitorUrl?: string;
  readonly healthUrl?: string;
  readonly baseUrl?: string;
}): MonitorUrls {
  const explicit = (options.monitorUrl ?? "").trim();
  if (explicit !== "") return urlsForBase(explicit);
  const health = (options.healthUrl ?? "").trim();
  if (health !== "") return urlsForBase(health);
  return urlsForBase(options.baseUrl);
}

// ── reading one endpoint ─────────────────────────────────────────────────────

/** A parsed response body, in the two shapes `metrics`-style endpoints come in. */
export interface BodySample {
  readonly numbers: ReadonlyMap<string, number>;
  readonly strings: ReadonlyMap<string, string>;
  readonly bools: ReadonlyMap<string, boolean>;
  /** Arrays by path — the pool list and the model list live here. */
  readonly arrays: ReadonlyMap<string, unknown[]>;
  /** Labeled samples, present when the body was Prometheus text. */
  readonly series: readonly MetricSample[];
  /** Top-level key names, so `describe()` can say what the server exposed. */
  readonly keys: readonly string[];
  /**
   * The JSON object as it arrived, when it was JSON. {@link refineUrls} reads the
   * advertised `metrics` / `cache_counters` / `endpoints` paths out of this
   * rather than out of a reconstruction, so a path the flattener never looked at
   * is still visible to the code that needs it.
   */
  readonly raw?: JsonRecord;
}

export interface MetricSample {
  readonly name: string;
  readonly labels: Readonly<Record<string, string>>;
  readonly value: number;
}

export const EMPTY_BODY: BodySample = {
  numbers: new Map(),
  strings: new Map(),
  bools: new Map(),
  arrays: new Map(),
  series: [],
  keys: [],
  raw: undefined,
};

/** Flatten a JSON object into dotted scalar paths; arrays are kept and indexed. */
export function flattenRecord(
  record: unknown,
  into: {
    numbers: Map<string, number>;
    strings: Map<string, string>;
    bools: Map<string, boolean>;
    arrays: Map<string, unknown[]>;
  } = { numbers: new Map(), strings: new Map(), bools: new Map(), arrays: new Map() },
  prefix = "",
): typeof into {
  if (!isRecord(record)) return into;
  for (const [key, value] of Object.entries(record)) {
    const path = prefix === "" ? key : `${prefix}.${key}`;
    if (typeof value === "number" && Number.isFinite(value)) into.numbers.set(path, value);
    else if (typeof value === "string") into.strings.set(path, value);
    else if (typeof value === "boolean") into.bools.set(path, value);
    else if (Array.isArray(value)) {
      into.arrays.set(path, value);
      value.forEach((entry, index) => flattenRecord(entry, into, `${path}.${index}`));
    } else if (isRecord(value)) {
      flattenRecord(value, into, path);
    }
  }
  return into;
}

/**
 * Every readable path in a parsed body: the flattened scalar paths, not the
 * payload's top-level key names. `describe()` works in these because that is
 * the currency the reader spends — a nested `version.api` is a path, and a
 * report of "keys we are not showing" has to speak the same language or it
 * hides the nested ones behind their parent.
 */
export function pathsOf(body: BodySample): string[] {
  return [
    ...body.numbers.keys(),
    ...body.strings.keys(),
    ...body.bools.keys(),
    ...body.arrays.keys(),
  ];
}

/** `key{label="v"} 12.5`, the whole line. */
const METRIC_LINE =
  /^\s*(?<name>[a-zA-Z_:][\w.:]*)(?:\{(?<labels>[^}]*)\})?\s+(?<value>[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?|NaN|Inf|-Inf)\s*$/u;

/**
 * Parse Prometheus text exposition into the same shape as a JSON body.
 *
 * `/metrics` is the one endpoint with a format of its own, and common enough to
 * be worth meeting: without this the monitor shows nothing for a server that is
 * reporting plenty. Same-named series are summed into the scalar view (a total
 * across label sets is the number a glance wants) while the individual series
 * are kept for the per-pool breakdown.
 *
 * Returns `undefined` when nothing parsed, so a caller can report "answered,
 * not in a shape I read" instead of a screen full of zeroes.
 */
export function parsePrometheus(
  text: string,
): { numbers: Map<string, number>; series: MetricSample[] } | undefined {
  const numbers = new Map<string, number>();
  const series: MetricSample[] = [];
  for (const rawLine of text.split("\n")) {
    const line = rawLine.trim();
    if (line === "" || line.startsWith("#")) continue;
    const match = METRIC_LINE.exec(line);
    if (match?.groups === undefined) continue;
    const value = Number(match.groups.value);
    if (!Number.isFinite(value)) continue;
    const labels: Record<string, string> = {};
    const pairs = match.groups.labels ?? "";
    for (const pair of pairs.split(",")) {
      const eq = pair.indexOf("=");
      if (eq <= 0) continue;
      const label = pair.slice(0, eq).trim();
      const raw = pair.slice(eq + 1).trim().replace(/^"|"$/g, "");
      if (label !== "") labels[label] = raw;
    }
    const name = match.groups.name;
    if (name === undefined || name === "") continue;
    series.push({ name, labels, value });
    numbers.set(name, (numbers.get(name) ?? 0) + value);
  }
  return series.length === 0 ? undefined : { numbers, series };
}

/** JSON first, Prometheus second, nothing third. */
export function parseBody(text: string): BodySample | undefined {
  const trimmed = text.trim();
  if (trimmed === "") return undefined;
  try {
    const parsed: unknown = JSON.parse(trimmed);
    if (!isRecord(parsed)) return undefined;
    const flat = flattenRecord(parsed);
    return {
      numbers: flat.numbers,
      strings: flat.strings,
      bools: flat.bools,
      arrays: flat.arrays,
      series: [],
      keys: Object.keys(parsed),
      raw: parsed,
    };
  } catch {
    const prom = parsePrometheus(text);
    if (prom === undefined) return undefined;
    return {
      numbers: prom.numbers,
      strings: new Map(),
      bools: new Map(),
      arrays: new Map(),
      series: prom.series,
      keys: [...prom.numbers.keys()],
    };
  }
}

/**
 * How one endpoint answered, kept across polls so a later paint and
 * {@link BackendMonitor.describe} can say what is going on without asking again.
 */
export interface EndpointState {
  readonly name: EndpointName;
  readonly url?: string;
  /** `absent` = answered 404/405 once and is never asked again. */
  readonly state: "unknown" | "live" | "absent" | "error";
  readonly at?: number;
  readonly latencyMs?: number;
  readonly error?: string;
  readonly failures: number;
  readonly body: BodySample;
  /**
   * True once the endpoint has answered with something readable, even if the
   * most recent attempt failed. The values stay on screen and the age field
   * says how old they are, which is the honest version of "the numbers are
   * fine, the server is not answering right now".
   */
  readonly hasBody: boolean;
}

export function emptyEndpoint(name: EndpointName, url?: string): EndpointState {
  return { name, url, state: "unknown", failures: 0, body: EMPTY_BODY, hasBody: false };
}

const DEFAULT_TIMEOUT_MS = 1_500;
const SNIPPET_CHARS = 160;
/** Status codes that mean "this endpoint does not exist here". */
const ABSENT_STATUSES = new Set([404, 405, 501]);

function snippet(text: string): string {
  const flat = text.replace(/\s+/gu, " ").trim();
  return flat.length > SNIPPET_CHARS ? `${flat.slice(0, SNIPPET_CHARS)}…` : flat;
}

export interface FetchedBody {
  readonly ok: boolean;
  readonly status?: number;
  readonly body?: BodySample;
  readonly error?: string;
  readonly latencyMs: number;
  readonly absent: boolean;
}

/**
 * One `GET`, one deadline, no throw.
 *
 * The same discipline as `probeHealth()` — a bounded request whose failure is a
 * value — with one difference: this one also accepts `text/plain`, because
 * `/metrics` is allowed to answer in the exposition format.
 */
export async function fetchBody(
  url: string,
  options: {
    readonly timeoutMs?: number;
    readonly fetchImpl?: typeof fetch;
    readonly now?: () => number;
    readonly headers?: Readonly<Record<string, string>>;
  } = {},
): Promise<FetchedBody> {
  const now = options.now ?? Date.now;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const doFetch = options.fetchImpl ?? (typeof fetch === "function" ? fetch : undefined);
  const started = now();
  if (doFetch === undefined) {
    return { ok: false, error: "no fetch implementation available", latencyMs: 0, absent: false };
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), Math.max(1, timeoutMs));
  timer.unref?.();
  try {
    const response = await doFetch(url, {
      method: "GET",
      headers: { accept: "application/json, text/plain, */*", ...(options.headers ?? {}) },
      signal: controller.signal,
    });
    const text = await response.text().catch(() => "");
    const latencyMs = Math.max(0, now() - started);
    if (!response.ok) {
      const detail = snippet(text);
      return {
        ok: false,
        status: response.status,
        absent: ABSENT_STATUSES.has(response.status),
        error: `HTTP ${response.status}${detail === "" ? "" : `: ${detail}`}`,
        latencyMs,
      };
    }
    const body = parseBody(text);
    if (body === undefined) {
      return {
        ok: false,
        status: response.status,
        absent: false,
        error: `answered, but not JSON or metrics text (${snippet(text) || "empty body"})`,
        latencyMs,
      };
    }
    return { ok: true, status: response.status, body, latencyMs, absent: false };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return {
      ok: false,
      error: /abort/u.test(message) ? `no answer within ${timeoutMs}ms` : message,
      latencyMs: Math.max(0, now() - started),
      absent: false,
    };
  } finally {
    clearTimeout(timer);
  }
}

// ── the field map ────────────────────────────────────────────────────────────
//
// A panel figure is read from a list of candidate dotted paths, across the
// endpoints in order. The lists are long on purpose: the shape of `/metrics` is
// the server's business, and the honest response to not knowing a name is to try
// the plausible ones and render `—` if none exist. What must never happen is
// guessing a *value* for a field nothing witnessed.

const CONTEXT_PATHS = [
  "context",
  "context_window",
  "context_length",
  "max_context",
  "n_ctx",
  "model.context_window",
  "limits.context",
] as const;

const SLOT_CTX_PATHS = ["slot_ctx", "slot_context", "per_slot_ctx", "slot_ctx_positions"] as const;

const SLOTS_PATHS = [
  "slots",
  "max_slots",
  "num_slots",
  "slot_count",
  "parallel",
  "max_parallel",
  "concurrency",
  "max_concurrency",
] as const;

const SLOTS_USED_PATHS = [
  "in_flight",
  "inflight",
  "active_slots",
  "slots_in_use",
  "slots_used",
  "active_requests",
  "running_requests",
  "busy_slots",
] as const;

const QUEUED_PATHS = ["queued", "queue", "waiting", "queue_depth", "pending_requests"] as const;

const KV_CAPACITY_PATHS = [
  "kv_pool_positions",
  "kv_pool_capacity",
  "kv_cache_size",
  "kv_capacity",
  "pool_positions",
  "pool_capacity",
  "cache_capacity",
] as const;

const KV_USED_PATHS = [
  "kv_used",
  "kv_used_positions",
  "kv_positions_used",
  "kv_pool_used",
  "pool_used",
  "used_positions",
  "cache_used",
] as const;

const KV_USAGE_PATHS = ["kv_usage", "kv_utilization", "pool_usage", "kv_cache_usage"] as const;

const MAX_TOKENS_PATHS = [
  "max_tokens_cap",
  "max_tokens",
  "max_output_tokens",
  "max_completion_tokens",
  "limits.max_tokens",
  "output_ceiling",
] as const;

const MAX_TOKENS_DEFAULT_PATHS = [
  "max_tokens_default",
  "defaults.max_tokens",
  "default_max_tokens",
  "default_output_tokens",
] as const;

const DRAFTER_PATHS = [
  "drafter",
  "active_drafter",
  "draft_type",
  "drafter_default",
  "speculative.drafter",
  "spec_decode.drafter",
] as const;

const DRAFTERS_PATHS = ["drafters_available", "drafters", "speculative.drafters"] as const;

/** Cumulative counters: the rate is their slope, never their value. */
const GENERATED_TOKENS_PATHS = [
  "tokens_generated",
  "generated_tokens",
  "generation_tokens_total",
  "decode_tokens_total",
  "output_tokens_total",
  "completion_tokens_total",
  "counters.tokens_generated",
] as const;

/** Already-a-rate, for a server that measures itself. */
const INSTANT_TPS_PATHS = [
  "tokens_per_second",
  "decode_tokens_per_second",
  "current_tps",
  "tps",
  "decode_rate",
] as const;

const REQUESTS_SERVED_PATHS = [
  "requests_served",
  "requests_total",
  "requests_completed",
  "served_requests",
  "completed_requests",
  "total_requests",
  "counters.requests_served",
] as const;

const CACHE_HITS_PATHS = [
  "prompt_cache_hits",
  "cache_hits",
  "cached_requests",
  "cache_hit_total",
  "prompt_cache_hit_total",
  "counters.cache_hits",
  // The bare names are what a cache endpoint calls them when it has no room for
  // a prefix. Last in the list on purpose: inside one source, a name that says
  // what it counts beats one that only says it is a count.
  "cache.hit_count",
  "hits",
] as const;

const CACHE_MISSES_PATHS = [
  "prompt_cache_misses",
  "cache_misses",
  "cache_miss_total",
  "prompt_cache_miss_total",
  "counters.cache_misses",
  "cache.miss_count",
  "misses",
] as const;

const CACHE_RATE_PATHS = [
  "cache_hit_rate",
  "prompt_cache_hit_rate",
  "hit_rate",
  "cache.hit_rate",
] as const;

const CACHE_CAP_PATHS = [
  "prompt_cache.cap_mb",
  "cache.cap_mb",
  "cache_capacity_mb",
  "cap_mb",
] as const;
const CACHE_ENABLED_PATHS = ["prompt_cache.enabled", "cache.enabled"] as const;
const BUSY_PATHS = ["busy", "engine_busy", "server_busy"] as const;
const BUSY_FOR_PATHS = ["busy_for_s", "busy_seconds", "busy_for_seconds", "busy_time_s"] as const;
const MODEL_ID_PATHS = ["model", "model_id", "served_model", "default_model"] as const;

/** Which pool a labeled sample belongs to. */
const POOL_LABELS = ["pool", "kv_pool", "pool_id", "pool_name", "shard", "name"] as const;

const POOL_ARRAY_PATHS = ["kv_pools", "pools", "kv_cache.pools", "cache.pools", "kv.pools"] as const;

/**
 * Every key path the reader knows about, in one place.
 *
 * `describe()` subtracts this from the keys an endpoint actually returned, so
 * "the server told us something the panel is not showing" is a line in a
 * verbose run rather than something discovered by reading the source. This is
 * what makes a field set for a server nobody has seen yet extendable in one
 * edit: run with `LOOP_MONITOR_VERBOSE=1`, read the `not shown` line, add the
 * path. A new path list that is not registered here stays on the `not shown`
 * line forever, which is the loud way to get that wrong.
 */
const KNOWN_PATHS: readonly string[] = [
  ...CONTEXT_PATHS,
  ...SLOT_CTX_PATHS,
  ...SLOTS_PATHS,
  ...SLOTS_USED_PATHS,
  ...QUEUED_PATHS,
  ...KV_CAPACITY_PATHS,
  ...KV_USED_PATHS,
  ...KV_USAGE_PATHS,
  ...MAX_TOKENS_PATHS,
  ...MAX_TOKENS_DEFAULT_PATHS,
  ...DRAFTER_PATHS,
  ...DRAFTERS_PATHS,
  ...GENERATED_TOKENS_PATHS,
  ...INSTANT_TPS_PATHS,
  ...REQUESTS_SERVED_PATHS,
  ...CACHE_HITS_PATHS,
  ...CACHE_MISSES_PATHS,
  ...CACHE_RATE_PATHS,
  ...CACHE_CAP_PATHS,
  ...CACHE_ENABLED_PATHS,
  ...BUSY_PATHS,
  ...BUSY_FOR_PATHS,
  ...MODEL_ID_PATHS,
  ...POOL_LABELS,
  ...POOL_ARRAY_PATHS,
  // readVersions(); these are inline because nothing else reads them.
  "version.api",
  "api_version",
  "api.version",
  "version.engine",
  "engine_version",
  "engine.version",
  "version.match",
  "versions_match",
  "version.matches",
  // The report's own table of contents, consumed by `refineUrls()` rather than
  // by a panel field.
  "health",
  "health_url",
  "metrics",
  "metrics_url",
  "counters",
  "cache",
  "cache_url",
  "cache_counters",
  "endpoints",
  // The model list, consumed by `readModelIds()`.
  "data",
  "models",
  "items",
];

const KNOWN_PATH_SET: ReadonlySet<string> = new Set(KNOWN_PATHS);

/** Lists the reader walks entry by entry, rather than by path. */
const ENTRY_ARRAY_PATHS: ReadonlySet<string> = new Set<string>([
  ...POOL_ARRAY_PATHS,
  "data",
  "models",
  "items",
]);

/** Entry fields the readers of those lists actually pick up. */
const ENTRY_FIELDS_READ: ReadonlySet<string> = new Set<string>([
  "id",
  "model_id",
  "name",
  "pool",
  "pool_id",
  "pool_name",
  "capacity",
  "positions",
  "used",
  "occupied",
  "slots",
  "slots_used",
  "in_use",
]);

/**
 * Whether a flattened path is one the reader consumes: by name, or as a field
 * of an entry inside a list it walks (`data.0.id` is read; `data.0.object`
 * is not). Without this the "not shown" line would list the model names the
 * panel is displaying, which is precisely the kind of wrong answer that makes
 * a diagnostic line ignorable.
 */
export function pathIsRead(path: string): boolean {
  if (KNOWN_PATH_SET.has(path)) return true;
  const parts = path.split(".");
  if (parts.length < 3) return false;
  if (!/^\d+$/u.test(parts[1] ?? "")) return false;
  if (!ENTRY_ARRAY_PATHS.has(parts[0] ?? "")) return false;
  return ENTRY_FIELDS_READ.has(parts[parts.length - 1] ?? "");
}

/** A number found somewhere, and where it came from. */
interface Hit {
  readonly path: string;
  readonly value: number;
  readonly source: EndpointName;
}

type OrderedSources = readonly { readonly name: EndpointName; readonly body: BodySample }[];

/**
 * Candidate-path lookup across sources, source-major: the report about the
 * machine outranks the counters about the moment.
 *
 * The `sources` field is written out rather than declared as a TypeScript
 * parameter property because this project runs on `--experimental-strip-types`,
 * which erases annotations and does not generate the assignment a `private
 * readonly` shorthand needs. Same shape, one honest line.
 */
class Reader {
  private readonly sources: OrderedSources;

  constructor(sources: OrderedSources) {
    this.sources = sources;
  }

  number(paths: readonly string[]): Hit | undefined {
    for (const { name, body } of this.sources) {
      for (const path of paths) {
        const value = body.numbers.get(path);
        if (value !== undefined) return { path, value, source: name };
      }
    }
    return undefined;
  }

  string(paths: readonly string[]): string | undefined {
    for (const { body } of this.sources) {
      for (const path of paths) {
        const value = body.strings.get(path);
        if (value !== undefined && value !== "") return value;
      }
    }
    return undefined;
  }

  boolean(paths: readonly string[]): boolean | undefined {
    for (const { body } of this.sources) {
      for (const path of paths) {
        const value = body.bools.get(path);
        if (value !== undefined) return value;
      }
    }
    return undefined;
  }

  arrayOfObjects(paths: readonly string[]): JsonRecord[] {
    const out: JsonRecord[] = [];
    for (const { body } of this.sources) {
      for (const path of paths) {
        const value = body.arrays.get(path);
        if (!Array.isArray(value)) continue;
        for (const entry of value) if (isRecord(entry)) out.push(entry);
      }
    }
    return out;
  }

  seriesNamed(names: readonly string[]): MetricSample[] {
    const out: MetricSample[] = [];
    for (const { body } of this.sources) {
      for (const sample of body.series) {
        if (names.includes(sample.name)) out.push(sample);
      }
    }
    return out;
  }
}

/** One KV cache pool — only shown separately when a server has more than one. */
export interface KvPool {
  readonly name?: string;
  readonly capacity?: number;
  readonly used?: number;
  readonly slots?: number;
  readonly slotsUsed?: number;
}

/** Everything the panel can say, and the honesty fields that make it credible. */
export interface BackendSnapshot {
  /** When the most recent source answered. */
  readonly at: number;
  /** How long ago that was, as of the build. */
  readonly ageMs: number;
  /** At least one endpoint has answered since the monitor started. */
  readonly anyLive: boolean;
  /** The first error worth quoting, when nothing is answering. */
  readonly error?: string;

  readonly model?: string;
  readonly context?: number;
  readonly slotCtx?: number;
  readonly slots?: number;
  readonly slotsUsed?: number;
  readonly queued?: number;
  readonly kvCapacity?: number;
  readonly kvUsed?: number;
  /** 0..1 (values reported as percent are normalised). */
  readonly kvUsage?: number;
  readonly pools?: readonly KvPool[];
  readonly maxTokens?: number;
  readonly maxTokensDefault?: number;
  readonly drafter?: string;
  readonly drafters?: readonly string[];
  /** Decoded tokens/second — the server's own if it reports one, else our slope. */
  readonly tokensPerSecond?: number;
  readonly requestsPerSecond?: number;
  readonly requestsServed?: number;
  /** Prompt-cache hit rate over the last interval, 0..1. */
  readonly cacheHitRate?: number;
  readonly cacheEnabled?: boolean;
  readonly cacheCapMb?: number;
  readonly busy?: boolean;
  readonly busyForMs?: number;
  readonly apiVersion?: string;
  readonly engineVersion?: string;
  readonly versionsMatch?: boolean;
  /** Round trip of the slowest source in the most recent completed poll. */
  readonly latencyMs?: number;
  readonly endpoints: readonly EndpointState[];
}

/**
 * The last read of each cumulative counter, keyed by the exact field it came
 * from. {@link summarise} takes this back in and returns a new one.
 */
export type RateMemo = Readonly<Record<string, Hit & { readonly at: number }>>;

export interface SummariseInput {
  readonly endpoints: readonly EndpointState[];
  readonly now: number;
  readonly previous?: RateMemo;
}

export interface SummariseResult {
  readonly snapshot: BackendSnapshot;
  readonly memo: RateMemo;
}

/** Source order for every read. The report about the machine outranks the counters. */
const SOURCE_ORDER: readonly EndpointName[] = ["health", "cache", "metrics", "models"];

/**
 * Raw endpoint bodies in, panel numbers out.
 *
 * Pure, and taking the previous counter memo as an argument, because "current
 * t/s" is not something a server hands over on a plate: it is the slope of a
 * cumulative counter between two instants. Doing that diff in one place, with
 * both instants visible, is what keeps a rate honest — a counter that reset
 * between reads yields no rate rather than a negative one, and a field that
 * moved to a different key yields no rate rather than a nonsense one.
 */
export function summarise(input: SummariseInput): SummariseResult {
  const byName = new Map<EndpointName, BodySample>();
  for (const state of input.endpoints) {
    // `hasBody`, not `state === "live"`: a source that answered and has since
    // gone quiet still has facts worth showing, with an age attached.
    if (state.hasBody && state.state !== "absent") byName.set(state.name, state.body);
  }
  const sources: OrderedSources = SOURCE_ORDER.filter((name) => byName.has(name)).map((name) => ({
    name,
    body: byName.get(name) as BodySample,
  }));
  const reader = new Reader(sources);

  // "Live" here means *a reading exists*, not *the last attempt succeeded*: a
  // source that answered and has gone quiet still has facts worth showing, and
  // `ageMs` is what tells the reader how much to trust them.
  const answered = input.endpoints.filter(
    (state) => state.hasBody && state.state !== "absent" && state.at !== undefined,
  );
  const lastAt = answered.length === 0 ? 0 : Math.max(...answered.map((state) => state.at ?? 0));
  const anyLive = answered.length > 0;
  const errored = input.endpoints.filter((state) => state.state === "error");
  const absent = input.endpoints.filter((state) => state.state === "absent");

  const context = reader.number(CONTEXT_PATHS);
  const slotCtx = reader.number(SLOT_CTX_PATHS);
  const slots = reader.number(SLOTS_PATHS);
  const slotsUsed = reader.number(SLOTS_USED_PATHS);
  const queued = reader.number(QUEUED_PATHS);
  const kvCapacityHit = reader.number(KV_CAPACITY_PATHS);
  const kvUsedHit = reader.number(KV_USED_PATHS);
  const kvUsageHit = reader.number(KV_USAGE_PATHS);
  const maxTokens = reader.number(MAX_TOKENS_PATHS);
  const maxTokensDefault = reader.number(MAX_TOKENS_DEFAULT_PATHS);
  const drafter = reader.string(DRAFTER_PATHS);
  const served = reader.number(REQUESTS_SERVED_PATHS);
  const instantTps = reader.number(INSTANT_TPS_PATHS);
  const generated = reader.number(GENERATED_TOKENS_PATHS);
  const hits = reader.number(CACHE_HITS_PATHS);
  const misses = reader.number(CACHE_MISSES_PATHS);
  const cacheRateHit = reader.number(CACHE_RATE_PATHS);
  const cacheCap = reader.number(CACHE_CAP_PATHS);

  // ── rates: the slope since the previous read of the same key ──────────────
  const slope = (key: string, hit: Hit | undefined): number | undefined => {
    if (hit === undefined || lastAt <= 0) return undefined;
    const before = input.previous?.[key];
    if (before === undefined) return undefined;
    // Two different keys are two different quantities; diffing them is a bug
    // dressed as a measurement.
    if (before.path !== hit.path || before.source !== hit.source) return undefined;
    const dt = lastAt - before.at;
    if (!Number.isFinite(dt) || dt <= 0) return undefined;
    const delta = hit.value - before.value;
    // Backwards means the counter was reset (restart, rotation). The honest
    // answer is "no rate this interval", not a giant negative spike.
    if (delta < 0) return undefined;
    return delta / (dt / 1_000);
  };

  const memo: Record<string, Hit & { at: number }> = {};
  const remember = (key: string, hit: Hit | undefined): void => {
    if (hit === undefined || lastAt <= 0) return;
    memo[key] = { ...hit, at: lastAt };
  };
  remember("generated", generated);
  remember("served", served);
  remember("hits", hits);
  remember("misses", misses);

  const generatedRate = slope("generated", generated);
  const requestsRate = slope("served", served);
  const hitsRate = slope("hits", hits);
  const missesRate = slope("misses", misses);

  // A hit rate *over the interval* is the useful one: a lifetime average has
  // been smoothed into uselessness by the cold start of every ticket.
  let cacheHitRate: number | undefined = cacheRateHit?.value;
  if (cacheHitRate !== undefined && cacheHitRate > 1) cacheHitRate = cacheHitRate / 100;
  if (cacheHitRate === undefined && hitsRate !== undefined) {
    const miss =
      missesRate ?? Math.max(0, (requestsRate ?? 0) - hitsRate);
    const total = hitsRate + miss;
    cacheHitRate = total > 0 ? hitsRate / total : undefined;
  }

  const pools = collectPools(sources);
  const multiPool = pools !== undefined && pools.length > 1;
  // A single pool inside an array still has to reach the totals: the panel reads
  // `kvCapacity` first and only looks at `pools` when there is more than one.
  const capacity = kvCapacityHit?.value ?? sumDefined(pools?.map((p) => p.capacity));
  const used = kvUsedHit?.value ?? sumDefined(pools?.map((p) => p.used));
  const kvUsage =
    kvUsageHit !== undefined
      ? kvUsageHit.value > 1
        ? kvUsageHit.value / 100
        : kvUsageHit.value
      : capacity !== undefined && capacity > 0 && used !== undefined
        ? used / capacity
        : undefined;

  const versions = readVersions(sources);
  const latencies = answered
    .map((state) => state.latencyMs)
    .filter((value): value is number => value !== undefined);

  const snapshot: BackendSnapshot = {
    at: lastAt,
    ageMs: anyLive ? Math.max(0, input.now - lastAt) : 0,
    anyLive,
    error: anyLive
      ? undefined
      : errored[0]?.error ??
        (absent.length > 0 ? "no diagnostics endpoint answered" : "nothing answered"),
    model: reader.string(MODEL_ID_PATHS),
    context: context?.value,
    slotCtx: slotCtx?.value ?? slots?.value,
    slots: slots?.value ?? sumDefined(pools?.map((p) => p.slots)),
    slotsUsed: slotsUsed?.value ?? sumDefined(pools?.map((p) => p.slotsUsed)),
    queued: queued?.value,
    kvCapacity: capacity,
    kvUsed: used,
    kvUsage,
    pools: multiPool ? pools : undefined,
    maxTokens: maxTokens?.value,
    maxTokensDefault: maxTokensDefault?.value,
    drafter,
    drafters: readDrafters(sources),
    tokensPerSecond: instantTps?.value ?? generatedRate,
    requestsPerSecond: requestsRate,
    requestsServed: served?.value,
    cacheHitRate,
    cacheEnabled: reader.boolean(CACHE_ENABLED_PATHS),
    cacheCapMb: cacheCap?.value,
    busy: reader.boolean(BUSY_PATHS),
    busyForMs: readSeconds(reader.number(BUSY_FOR_PATHS)),
    apiVersion: versions.api,
    engineVersion: versions.engine,
    versionsMatch: versions.match,
    latencyMs: latencies.length === 0 ? undefined : Math.max(...latencies),
    endpoints: input.endpoints,
  };

  return { snapshot, memo };
}

function readSeconds(hit: Hit | undefined): number | undefined {
  return hit === undefined ? undefined : hit.value * 1_000;
}

function readDrafters(sources: OrderedSources): readonly string[] | undefined {
  const reader = new Reader(sources);
  for (const { body } of sources) {
    for (const path of DRAFTERS_PATHS) {
      const value = body.arrays.get(path);
      if (!Array.isArray(value)) continue;
      const out = value.filter((entry): entry is string => typeof entry === "string" && entry !== "");
      if (out.length > 0) return out;
    }
  }
  // Prometheus: one series per drafter, `drafters{drafter="mtp"} 1`.
  const series = reader.seriesNamed([...DRAFTERS_PATHS]);
  const names = series
    .map((sample) => sample.labels.drafter ?? sample.labels.name ?? sample.labels.type)
    .filter((name): name is string => name !== undefined && name !== "");
  return names.length > 0 ? names : undefined;
}

function readVersions(
  sources: OrderedSources,
): { api?: string; engine?: string; match?: boolean } {
  const reader = new Reader(sources);
  return {
    api: reader.string(["version.api", "api_version", "api.version"]),
    engine: reader.string(["version.engine", "engine_version", "engine.version"]),
    match: reader.boolean(["version.match", "versions_match", "version.matches"]),
  };
}

function poolLabelFor(labels: Readonly<Record<string, string>>): string | undefined {
  for (const key of POOL_LABELS) {
    const value = labels[key];
    if (value !== undefined && value !== "") return value;
  }
  return undefined;
}

/**
 * The KV pools, from a `kv_pools` array or from `…{pool="…"}` samples.
 *
 * "Slots by KV pool" is a question a multi-pool server can answer, and the one
 * that matters when one pool is full while another idles. A single-pool server
 * gets `undefined` here: its totals already say the whole thing.
 */
export function collectPools(sources: OrderedSources): KvPool[] | undefined {
  const reader = new Reader(sources);
  const explicit = reader.arrayOfObjects(POOL_ARRAY_PATHS);
  if (explicit.length > 0) {
    return explicit.map((entry) => ({
      name: pickString(entry, ["name", "pool", "id", "pool_id"]),
      capacity: pickNumber(entry, [...KV_CAPACITY_PATHS, "capacity", "positions"]),
      used: pickNumber(entry, [...KV_USED_PATHS, "used", "occupied"]),
      slots: pickNumber(entry, [...SLOTS_PATHS, "slots"]),
      slotsUsed: pickNumber(entry, [...SLOTS_USED_PATHS, "slots_used", "in_use"]),
    }));
  }

  const grouped = new Map<string, KvPool & { order: number }>();
  let counter = 0;
  const merge = (label: string, patch: Partial<KvPool>): void => {
    const existing = grouped.get(label);
    if (existing === undefined) grouped.set(label, { name: label, ...patch, order: counter++ });
    else Object.assign(existing, patch);
  };
  const consume = (
    names: readonly string[],
    field: keyof KvPool,
  ): void => {
    for (const sample of reader.seriesNamed(names)) {
      const label = poolLabelFor(sample.labels);
      if (label === undefined) continue;
      merge(label, { [field]: sample.value } as Partial<KvPool>);
    }
  };
  consume(KV_CAPACITY_PATHS, "capacity");
  consume(KV_USED_PATHS, "used");
  consume(SLOTS_PATHS, "slots");
  consume(SLOTS_USED_PATHS, "slotsUsed");
  if (grouped.size === 0) return undefined;
  return [...grouped.values()]
    .sort((a, b) => a.order - b.order)
    .map(({ order: _order, ...pool }) => pool);
}

function pickString(entry: JsonRecord, keys: readonly string[]): string | undefined {
  for (const key of keys) {
    const value = entry[key];
    if (typeof value === "string" && value !== "") return value;
  }
  return undefined;
}

function pickNumber(entry: JsonRecord, keys: readonly string[]): number | undefined {
  for (const key of keys) {
    const value = entry[key];
    if (typeof value === "number" && Number.isFinite(value)) return value;
  }
  return undefined;
}

function sumDefined(values: readonly (number | undefined)[] | undefined): number | undefined {
  if (values === undefined) return undefined;
  const present = values.filter((value): value is number => value !== undefined);
  return present.length === 0 ? undefined : present.reduce((total, value) => total + value, 0);
}

// ── the panel ────────────────────────────────────────────────────────────────

/** What a figure nobody witnessed reads as. Never blank, never `undefined`. */
export const MONITOR_MISSING = "—";

/** One labelled figure. */
export interface PanelSegment {
  readonly label: string;
  readonly text: string;
  readonly role: MonitorRole;
}

/**
 * `262144` → `262k`.
 *
 * A token count spelled out in full is not readable at a glance, and every
 * figure on this panel is a token count of roughly that size.
 */
export function compactNumber(value: number | undefined): string {
  if (value === undefined || !Number.isFinite(value)) return MONITOR_MISSING;
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  const trim = (n: number, digits: number): string =>
    n.toFixed(digits).replace(/\.0+$/u, "").replace(/(\.\d*?)0+$/u, "$1");
  if (abs >= 1e9) return `${sign}${trim(abs / 1e9, 1)}G`;
  if (abs >= 1e6) return `${sign}${trim(abs / 1e6, 1)}M`;
  if (abs >= 10_000) return `${sign}${trim(abs / 1_000, 0)}k`;
  if (abs >= 1_000) return `${sign}${trim(abs / 1_000, 1)}k`;
  return `${sign}${trim(abs, 0)}`;
}

/** `0.612` → `61%`. Percent-scaled input is normalised by the caller. */
export function percent(fraction: number | undefined): string {
  if (fraction === undefined || !Number.isFinite(fraction)) return MONITOR_MISSING;
  return `${Math.round(fraction * 100)}%`;
}

/** A rate, at the precision its own magnitude earns. */
export function rateNumber(value: number | undefined): string {
  if (value === undefined || !Number.isFinite(value)) return MONITOR_MISSING;
  if (Math.abs(value) >= 100) return String(Math.round(value));
  return value.toFixed(1);
}

/** How old a reading is: `2s`, `4m12s`, `1h02m`. */
export function formatAge(ms: number | undefined): string {
  if (ms === undefined || !Number.isFinite(ms) || ms < 0) return MONITOR_MISSING;
  const total = Math.floor(ms / 1_000);
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes}m${String(total % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, "0")}m`;
}

export type Freshness = "live" | "stale" | "down";

/**
 * Whether the read itself is trustworthy, coloured before anything else.
 *
 * This is the segment everything else depends on being believed. A rate that
 * has not moved in thirty seconds and a rate that is live are otherwise
 * identical, and a monitor that cannot tell you which one it is showing is a
 * monitor worth ignoring.
 */
export function freshnessOf(
  snapshot: BackendSnapshot,
  options: { readonly staleAfterMs?: number } = {},
): { readonly state: Freshness; readonly glyph: string; readonly role: MonitorRole } {
  if (!snapshot.anyLive) return { state: "down", glyph: "✕", role: "error" };
  if (snapshot.ageMs > (options.staleAfterMs ?? 7_000)) {
    return { state: "stale", glyph: "○", role: "warning" };
  }
  return { state: "live", glyph: "●", role: "success" };
}

function usageRole(usage: number | undefined): MonitorRole {
  if (usage === undefined) return "muted";
  if (usage >= 0.9) return "error";
  if (usage >= 0.75) return "warning";
  return "success";
}

/**
 * The panel's figures, in two groups that mean two different things.
 *
 * `load` is what the server is doing this second; `ceiling` is the room it has,
 * what it is wearing, and who is answering. The split is the reading order:
 * first "is it busy, is it full, is it fast", then the slower-moving facts you
 * check when the first line surprises you.
 */
export function panelSegments(snapshot: BackendSnapshot): {
  load: PanelSegment[];
  ceiling: PanelSegment[];
} {
  const fresh = freshnessOf(snapshot);
  const load: PanelSegment[] = [
    // The freshness marker leads the panel and is never dropped: it is what
    // makes every other number on the line either trustworthy or not.
    freshnessSegment(snapshot, fresh),
    { label: "t/s", text: rateNumber(snapshot.tokensPerSecond), role: "accent" },
  ];

  const kvText =
    snapshot.kvUsage === undefined
      ? MONITOR_MISSING
      : snapshot.kvCapacity !== undefined && snapshot.kvUsed !== undefined
        ? `${percent(snapshot.kvUsage)} ${compactNumber(snapshot.kvUsed)}/${compactNumber(snapshot.kvCapacity)}`
        : percent(snapshot.kvUsage);
  load.push({ label: "kv", text: kvText, role: usageRole(snapshot.kvUsage) });

  const slotsFull =
    snapshot.slotsUsed !== undefined &&
    snapshot.slots !== undefined &&
    snapshot.slotsUsed >= snapshot.slots;
  load.push({
    label: "slots",
    text:
      snapshot.slotsUsed === undefined && snapshot.slots === undefined
        ? MONITOR_MISSING
        : `${snapshot.slotsUsed ?? MONITOR_MISSING}/${snapshot.slots ?? MONITOR_MISSING}`,
    role: slotsFull ? "warning" : "text",
  });
  load.push({
    label: "queued",
    text: snapshot.queued === undefined ? MONITOR_MISSING : String(Math.max(0, Math.round(snapshot.queued))),
    role: (snapshot.queued ?? 0) > 0 ? "warning" : "text",
  });
  if (snapshot.busy === true) {
    load.push({
      label: "busy",
      text: snapshot.busyForMs === undefined ? "yes" : formatAge(snapshot.busyForMs),
      role: "warning",
    });
  }

  const ceiling: PanelSegment[] = [
    { label: "ctx", text: compactNumber(snapshot.context), role: "text" },
    { label: "out", text: compactNumber(snapshot.maxTokens), role: "text" },
    { label: "draft", text: snapshot.drafter ?? MONITOR_MISSING, role: "accent" },
  ];
  if (snapshot.cacheEnabled === false) {
    ceiling.push({ label: "cache", text: "off", role: "muted" });
  } else {
    ceiling.push({
      label: "cache",
      text: snapshot.cacheHitRate === undefined ? MONITOR_MISSING : `${percent(snapshot.cacheHitRate)} hit`,
      role: snapshot.cacheHitRate === undefined ? "muted" : snapshot.cacheHitRate >= 0.5 ? "success" : "warning",
    });
  }
  if (snapshot.requestsPerSecond !== undefined || snapshot.requestsServed !== undefined) {
    ceiling.push({
      label: "req",
      text: [
        snapshot.requestsPerSecond === undefined
          ? MONITOR_MISSING
          : `${rateNumber(snapshot.requestsPerSecond)}/s`,
        snapshot.requestsServed === undefined
          ? ""
          : `(${compactNumber(snapshot.requestsServed)} served)`,
      ]
        .filter((piece) => piece !== "")
        .join(" "),
      role: "text",
    });
  }
  for (const pool of snapshot.pools ?? []) {
    const usage =
      pool.capacity !== undefined && pool.capacity > 0 && pool.used !== undefined
        ? pool.used / pool.capacity
        : undefined;
    ceiling.push({
      label: `pool ${pool.name ?? "?"}`,
      text: [
        percent(usage),
        pool.slots === undefined && pool.slotsUsed === undefined
          ? ""
          : `${pool.slotsUsed ?? MONITOR_MISSING}/${pool.slots ?? MONITOR_MISSING} slots`,
      ]
        .filter((piece) => piece !== "")
        .join(" "),
      role: usageRole(usage),
    });
  }
  if (snapshot.model !== undefined) {
    ceiling.push({ label: "model", text: snapshot.model, role: "text" });
  }
  if (snapshot.apiVersion !== undefined || snapshot.engineVersion !== undefined) {
    ceiling.push({
      label: "api",
      text:
        snapshot.apiVersion !== undefined && snapshot.engineVersion !== undefined
          ? `${snapshot.apiVersion}/${snapshot.engineVersion}`
          : snapshot.apiVersion ?? snapshot.engineVersion ?? MONITOR_MISSING,
      role: snapshot.versionsMatch === false ? "warning" : "text",
    });
  }
  if (snapshot.latencyMs !== undefined) {
    ceiling.push({ label: "rt", text: `${Math.round(snapshot.latencyMs)}ms`, role: "muted" });
  }
  return { load, ceiling };
}

/** The state marker, with the age of the newest reading glued to it. */
function freshnessSegment(
  snapshot: BackendSnapshot,
  fresh: { readonly glyph: string; readonly role: MonitorRole; readonly state: Freshness },
): PanelSegment {
  if (fresh.state === "down") {
    return { label: "", text: `${fresh.glyph} unreachable`, role: fresh.role };
  }
  return { label: "", text: `${fresh.glyph} ${formatAge(snapshot.ageMs)}`, role: fresh.role };
}

/** The flow line's short list: the live half of panel, one line, always. */
export function flowSegments(snapshot: BackendSnapshot): PanelSegment[] {
  const fresh = freshnessOf(snapshot);
  const segments: PanelSegment[] = [
    freshnessSegment(snapshot, fresh),
    { label: "t/s", text: rateNumber(snapshot.tokensPerSecond), role: "accent" },
    { label: "kv", text: percent(snapshot.kvUsage), role: usageRole(snapshot.kvUsage) },
    {
      label: "slots",
      text:
        snapshot.slotsUsed === undefined && snapshot.slots === undefined
          ? MONITOR_MISSING
          : `${snapshot.slotsUsed ?? MONITOR_MISSING}/${snapshot.slots ?? MONITOR_MISSING}`,
      role: "text",
    },
  ];
  if (snapshot.cacheHitRate !== undefined || snapshot.cacheEnabled === false) {
    segments.push({
      label: "cache",
      text: snapshot.cacheEnabled === false ? "off" : percent(snapshot.cacheHitRate),
      role: "text",
    });
  }
  segments.push({ label: "upd", text: formatAge(snapshot.ageMs), role: fresh.role });
  if (snapshot.error !== undefined && !snapshot.anyLive) {
    segments.push({ label: "", text: snapshot.error, role: "error" });
  }
  return segments;
}

/** One segment, styled. `theme === null` is the plain path: same text, no colour. */
function paintSegment(segment: PanelSegment, theme: MonitorTheme | null): string {
  if (theme === null) return segment.label === "" ? segment.text : `${segment.label} ${segment.text}`;
  const value = theme.color(segment.role, segment.text);
  return segment.label === "" ? value : `${theme.color("dim", `${segment.label} `)}${value}`;
}

/**
 * Greedy-pack segments into at most `maxLines` lines, marking what did not fit.
 *
 * A panel that silently drops fields is worse than one that says `+3 more`: the
 * first leaves the reader wondering whether the server said nothing, and that
 * question is the whole reason this module exists.
 */
export function packSegments(
  segments: readonly PanelSegment[],
  theme: MonitorTheme | null,
  width: number,
  maxLines: number,
  indent = "",
): string[] {
  const safeWidth = Math.max(8, Math.trunc(width));
  const sep = theme === null ? " · " : theme.color("dim", " · ");
  const lines: string[] = [];
  let current = "";
  let hidden = 0;
  const push = (line: string): boolean => {
    if (lines.length >= maxLines) {
      hidden += 1;
      return false;
    }
    lines.push(line);
    return true;
  };
  for (const segment of segments) {
    const piece = paintSegment(segment, theme);
    // An unlabelled segment is a marker on the thing that follows it in
    // reading order, so it joins with a space rather than a bullet.
    const glue = segment.label === "" && lines.length + current.length > 0 ? " " : sep;
    if (current === "") {
      if (lines.length >= maxLines) {
        hidden += 1;
        continue;
      }
      current = `${indent}${piece}`;
      continue;
    }
    const candidate = `${current}${glue}${piece}`;
    if (visibleWidth(candidate) <= safeWidth) {
      current = candidate;
      continue;
    }
    if (!push(current)) continue;
    current = `${indent}${piece}`;
  }
  if (current !== "") push(current);
  if (hidden > 0) {
    const marker = theme === null ? `+${hidden} more` : theme.color("dim", `+${hidden} more`);
    if (lines.length > 0) {
      // Give the marker its own room instead of letting the truncation eat the
      // end of it: `+3 mo…` tells you less than `… +3 more`.
      const room = Math.max(4, safeWidth - visibleWidth(marker) - 1);
      lines[lines.length - 1] =
        `${truncateToWidth(lines[lines.length - 1] ?? "", room, "…")} ${marker}`;
    } else {
      lines.push(truncateToWidth(`${indent}+${hidden} more`, safeWidth, "…"));
    }
  }
  return lines.map((line) => truncateToWidth(line, safeWidth, "…"));
}

/**
 * The two-line panel: `load` over `ceiling`, both greedy-packed to the width.
 *
 * `theme === null` produces exactly the same text with no colour, which is what
 * the plain path prints and what the tests read.
 */
export function renderTopPanel(
  snapshot: BackendSnapshot,
  theme: MonitorTheme | null,
  options: {
    readonly width: number;
    readonly maxLines?: number;
    readonly staleAfterMs?: number;
    readonly prefix?: string;
  },
): string[] {
  const maxLines = Math.max(1, options.maxLines ?? 2);
  const groups = panelSegments(snapshot);
  const indent = options.prefix === undefined ? "" : `${options.prefix} `;
  if (maxLines === 1) {
    return packSegments(groups.load, theme, options.width, 1, indent);
  }
  const load = packSegments(groups.load, theme, options.width, maxLines - 1, indent);
  const ceiling = packSegments(groups.ceiling, theme, options.width, maxLines - load.length, " ".repeat(
    Math.max(indent.length - 1, 0),
  ));
  return [...load, ...ceiling];
}

/** The one-line form: the live half of the panel, for a band that must not grow. */
export function renderFlowLine(
  snapshot: BackendSnapshot,
  theme: MonitorTheme | null,
  options: { readonly width: number; readonly staleAfterMs?: number; readonly prefix?: string },
): string[] {
  const indent = options.prefix === undefined ? "" : `${options.prefix} `;
  return packSegments(flowSegments(snapshot), theme, options.width, 1, indent);
}

// ── the monitor ──────────────────────────────────────────────────────────────

/**
 * What a surface consumes: a source of already-rendered lines, plus a way to be
 * told when they changed.
 *
 * Pull, not push. The TUI's render pass is synchronous and must never wait on a
 * network, so the monitor keeps its last good snapshot and the component reads
 * it at render time; `subscribe` exists so a surface can *ask to be repainted*
 * when a poll lands, not so the data can arrive mid-frame.
 */
export interface MonitorSource {
  /**
   * The panel: 1..`maxLines` styled lines, truncated to `width`. Empty when off.
   * `indent` is a leading gutter so a surface can line the panel up with the
   * blocks under it without re-wrapping it afterwards.
   */
  lines(width: number, maxLines?: number, indent?: string): string[];
  /** Register a repaint hook. The returned function unregisters it. */
  subscribe(listener: () => void): () => void;
  readonly snapshot: BackendSnapshot;
}

export interface BackendMonitorOptions {
  readonly urls: MonitorUrls;
  /** Poll cadence. Default 2000ms — fast enough to see a trend, slow enough to ignore. */
  readonly intervalMs?: number;
  /** Per-request deadline. Default 1500ms. */
  readonly timeoutMs?: number;
  /** `/v1/models` changes when someone restarts something, so it asks less often. */
  readonly modelsEveryMs?: number;
  readonly headers?: Readonly<Record<string, string>>;
  readonly fetchImpl?: typeof fetch;
  readonly now?: () => number;
  /** Timer injection; returns the cancel function. */
  readonly schedule?: (run: () => void, ms: number) => () => void;
  /** `null` renders without colour, which is what the plain path wants. */
  readonly theme?: MonitorTheme | null;
  readonly maxLines?: number;
  readonly staleAfterMs?: number;
  /** Report what each endpoint exposed, once, so an unknown server is teachable. */
  readonly verbose?: boolean;
  readonly onEvent?: (line: string) => void;
}

export interface BackendMonitor extends MonitorSource {
  /** Begin polling. Idempotent. */
  start(): void;
  /** Stop polling. Idempotent; leaves the last snapshot readable. */
  stop(): void;
  /** Poll now, out of band with the cadence. */
  poll(): Promise<void>;
  readonly running: boolean;
  /** One line per endpoint: what it is, where it is, and what it exposed. */
  describe(): string[];
}

/**
 * A cycle is three or four small `GET`s. At one per second that is a rounding
 * error against a box that is busy serving a 262k-context decode, and it is the
 * difference between a panel that looks live and one that looks like a
 * screenshot. The model list is exempt: it changes when somebody restarts
 * something.
 */
const DEFAULT_INTERVAL_MS = 1_000;
const DEFAULT_MODELS_EVERY_MS = 30_000;

function defaultSchedule(run: () => void, ms: number): () => void {
  const timer = setTimeout(run, ms);
  timer.unref?.();
  return (): void => {
    clearTimeout(timer);
  };
}

const EMPTY_SNAPSHOT: BackendSnapshot = {
  at: 0,
  ageMs: 0,
  anyLive: false,
  endpoints: [],
};

/**
 * The poller.
 *
 * Three rules keep it out of the way of the thing it watches:
 *
 * 1. **Never overlapping.** One cycle at a time; a cycle that overruns its
 *    interval is followed by the next one starting when it finished, not piled
 *    on top of itself.
 * 2. **Never asked twice what the server already answered.** A 404 is a
 *    permanent answer for that endpoint and stops it being polled, so a server
 *    with no `/cache` is not hit 30 times a minute by something that will never
 *    get a different reply.
 * 3. **Never the reason a frame waits.** Everything the panel draws is already
 *    in memory before the frame starts.
 */
export function createBackendMonitor(options: BackendMonitorOptions): BackendMonitor {
  const now = options.now ?? Date.now;
  const schedule = options.schedule ?? defaultSchedule;
  const fetchImpl = options.fetchImpl ?? (typeof fetch === "function" ? fetch : undefined);
  const intervalMs = Math.max(250, options.intervalMs ?? DEFAULT_INTERVAL_MS);
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const modelsEveryMs = Math.max(intervalMs, options.modelsEveryMs ?? DEFAULT_MODELS_EVERY_MS);
  const theme = options.theme === undefined ? PLAIN_MONITOR_THEME : options.theme;
  const maxLines = Math.max(1, options.maxLines ?? 2);
  const staleAfterMs = options.staleAfterMs;
  const verbose = options.verbose === true;
  const emit = options.onEvent ?? ((_line: string): void => undefined);

  let urls: MonitorUrls = { ...options.urls };
  const states = new Map<EndpointName, EndpointState>();
  const nextPollAt = new Map<EndpointName, number>();
  for (const name of MONITOR_ENDPOINTS) {
    states.set(name, emptyEndpoint(name, urls[name]));
    nextPollAt.set(name, 0);
  }
  const listeners = new Set<() => void>();

  let snapshot: BackendSnapshot = EMPTY_SNAPSHOT;
  let memo: RateMemo | undefined;
  let running = false;
  /** The cycle in flight, if any — joined by `poll()` rather than doubled. */
  let cycleInFlight: Promise<void> | undefined;
  let cancelTimer: (() => void) | null = null;
  let booted = false;
  /** The model ids the models endpoint listed, for the `models` segment. */
  let modelIds: readonly string[] = [];

  function snapshotWith(): BackendSnapshot {
    const result = summarise({ endpoints: [...states.values()], now: now(), previous: memo });
    memo = result.memo;
    snapshot = { ...result.snapshot, model: result.snapshot.model ?? modelIds[0] };
    return snapshot;
  }

  function notify(): void {
    for (const listener of [...listeners]) {
      try {
        listener();
      } catch {
        // A surface that throws on a repaint is not the monitor's problem, and
        // must not stop the other listeners or the next poll.
      }
    }
  }

  function applyResult(name: EndpointName, url: string, fetched: FetchedBody): void {
    const previous = states.get(name) ?? emptyEndpoint(name, url);
    if (fetched.ok && fetched.body !== undefined) {
      states.set(name, {
        name,
        url,
        state: "live",
        at: now(),
        latencyMs: fetched.latencyMs,
        failures: 0,
        body: fetched.body,
        hasBody: true,
      });
      return;
    }
    if (fetched.absent) {
      // "Not here" is a permanent answer: keep whatever we knew before, mark it
      // absent, and stop asking.
      states.set(name, {
        ...previous,
        name,
        url,
        state: "absent",
        error: fetched.error ?? "not found",
        failures: previous.failures + 1,
        hasBody: false,
        body: EMPTY_BODY,
      });
      emit(`monitor: ${name} is not served at ${url} (${fetched.error ?? "404"}); that field stays blank`);
      return;
    }
    // Everything else is worth asking about again: the body we had is kept so
    // the panel keeps showing it, ageing.
    states.set(name, {
      ...previous,
      name,
      url,
      state: "error",
      error: fetched.error ?? "request failed",
      failures: previous.failures + 1,
    });
  }

  /** The model ids, read from the shapes `/v1/models` actually comes in. */
  function readModelIds(body: BodySample): readonly string[] {
    const out: string[] = [];
    for (const key of ["data", "models", "items"]) {
      const list = body.arrays.get(key);
      if (!Array.isArray(list)) continue;
      for (const entry of list) {
        if (typeof entry === "string" && entry !== "") out.push(entry);
        else if (isRecord(entry)) {
          const id = entry.id ?? entry.model_id ?? entry.name;
          if (typeof id === "string" && id !== "") out.push(id);
        }
      }
    }
    if (out.length === 0) {
      const direct = body.strings.get("model");
      if (direct !== undefined) out.push(direct);
    }
    return [...new Set(out)];
  }

  async function pollOne(name: EndpointName): Promise<void> {
    const url = urls[name];
    if (url === undefined) return;
    const state = states.get(name) ?? emptyEndpoint(name, url);
    if (state.state === "absent") return;
    if (fetchImpl === undefined) {
      states.set(name, { ...state, state: "error", error: "no fetch implementation available" });
      return;
    }
    const fetched = await fetchBody(url, {
      timeoutMs,
      fetchImpl,
      now,
      ...(options.headers === undefined ? {} : { headers: options.headers }),
    });
    applyResult(name, url, fetched);
    if (name === "health" && fetched.ok && fetched.body !== undefined) {
      const before = urls;
      urls = refineUrls(urls, fetched.body.raw);
      // A refined URL can un-hide an endpoint we were about to guess at.
      if (urls.metrics !== before.metrics || urls.cache !== before.cache || urls.models !== before.models) {
        for (const key of ["metrics", "cache", "models"] as const) {
          const current = states.get(key);
          if (current !== undefined && urls[key] !== current.url) {
            // The old URL was ours, not the server's: forget an `absent` that
            // was really just a wrong guess.
            if (current.state === "absent") {
              states.set(key, emptyEndpoint(key, urls[key]));
            } else {
              states.set(key, { ...current, url: urls[key] });
            }
          }
        }
      }
    }
    if (name === "models" && fetched.ok && fetched.body !== undefined) {
      modelIds = readModelIds(fetched.body);
    }
  }

  function dueNames(): EndpointName[] {
    const at = now();
    const due: EndpointName[] = [];
    for (const name of MONITOR_ENDPOINTS) {
      if (urls[name] === undefined) continue;
      if ((states.get(name)?.state ?? "unknown") === "absent") continue;
      if ((nextPollAt.get(name) ?? 0) > at) continue;
      due.push(name);
    }
    return due;
  }

  async function runCycle(): Promise<void> {
    try {
      if (!booted) {
        // Health first, because it is the one endpoint that tells us where the
        // others are: its report advertises `metrics` and `cache_counters`, and
        // asking the guesses first would get a 404 stamped on a path that only
        // looked wrong.
        booted = true;
        if (urls.health !== undefined) {
          await pollOne("health");
          nextPollAt.set("health", now() + intervalMs);
        }
      }
      await Promise.all(
        dueNames().map(async (name) => {
          await pollOne(name);
          nextPollAt.set(name, now() + (name === "models" ? modelsEveryMs : intervalMs));
        }),
      );
    } finally {
      // Deliberately no `throw`: a cycle that could not read anything is a
      // snapshot that says so, and every caller of `cycle()` treats a resolved
      // promise as "a reading was taken", not as "the server answered".
    }
    snapshotWith();
    notify();
  }

  /**
   * One cycle, joined rather than doubled. A call while a cycle is running
   * waits for that one instead of starting a second, which is both the
   * no-overlap rule and what makes `poll()` worth awaiting: a caller can have
   * the first reading in hand before it prints anything about it.
   */
  function cycle(): Promise<void> {
    if (cycleInFlight !== undefined) return cycleInFlight;
    const running = runCycle().finally(() => {
      if (cycleInFlight === running) cycleInFlight = undefined;
    });
    cycleInFlight = running;
    return running;
  }

  function arm(delayMs: number): void {
    if (cancelTimer !== null) cancelTimer();
    cancelTimer = schedule(() => {
      cancelTimer = null;
      void cycle().finally(() => {
        if (running) arm(intervalMs);
      });
    }, Math.max(0, delayMs));
  }

  return {
    start(): void {
      if (running) return;
      running = true;
      // The first read is kicked off rather than scheduled: by the time the
      // surface has painted its first frame the panel should already be
      // describing the server, not about to ask it.
      void cycle().finally(() => {
        if (running) arm(intervalMs);
      });
    },
    stop(): void {
      running = false;
      if (cancelTimer !== null) {
        cancelTimer();
        cancelTimer = null;
      }
    },
    poll(): Promise<void> {
      return cycle();
    },
    get running(): boolean {
      return running;
    },
    lines(width: number, limit?: number, indent?: string): string[] {
      const allowed = Math.max(1, limit ?? maxLines);
      return renderTopPanel(snapshot, theme, {
        width,
        maxLines: allowed,
        ...(staleAfterMs === undefined ? {} : { staleAfterMs }),
        ...(indent === undefined || indent === "" ? {} : { prefix: indent }),
      });
    },
    subscribe(listener: () => void): () => void {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    get snapshot(): BackendSnapshot {
      return snapshot;
    },
    describe(): string[] {
      const lines: string[] = [];
      for (const name of MONITOR_ENDPOINTS) {
        const state = states.get(name) ?? emptyEndpoint(name, urls[name]);
        const where = state.url ?? "(no url)";
        const timing = state.latencyMs === undefined ? "" : ` ${Math.round(state.latencyMs)}ms`;
        const detail =
          state.state === "live"
            ? `answered${timing}`
            : state.state === "absent"
              ? `not served (${state.error ?? "404"})`
              : state.state === "error"
                ? `failed: ${state.error ?? "unknown"} (${state.failures}x)`
                : "not asked";
        const exposed = state.hasBody ? pathsOf(state.body) : [];
        const unread = exposed.filter((key) => !pathIsRead(key));
        lines.push(`  ${name.padEnd(7)} ${where}  ${detail}`);
        if (!verbose || exposed.length === 0) continue;
        lines.push(
          `    ${exposed.length} key(s) exposed` +
            (unread.length === 0 ? ", every one of them read" : ""),
        );
        if (unread.length > 0) {
          const listed = unread.slice(0, 30).join(", ");
          const rest = unread.length > 30 ? ` (+${unread.length - 30} more)` : "";
          lines.push(`    not shown, no field reads them: ${listed}${rest}`);
        }
      }
      const shown = [
        ...MONITOR_ENDPOINTS.filter((name) => urls[name] === undefined).map((name) => `${name}`),
      ];
      if (shown.length > 0) lines.push(`  no url configured for: ${shown.join(", ")}`);
      return lines;
    },
  };
}

/**
 * A pi-tui component over a {@link MonitorSource}.
 *
 * Render-only: it holds no timer, reads no socket and takes no key. The
 * snapshot is pulled at render time, so a resize re-wraps it for free and a
 * slow server can never stall a frame.
 */
export class MonitorComponent implements Component {
  readonly kind = "monitor" as const;
  private readonly source: MonitorSource;
  private readonly maxLines: number;
  private readonly indent: string;

  constructor(source: MonitorSource, maxLines = 2, indent = "") {
    this.source = source;
    this.maxLines = maxLines;
    this.indent = indent;
  }

  render(width: number): string[] {
    const available = Math.max(8, width - visibleWidth(this.indent));
    return this.source.lines(available, this.maxLines, this.indent);
  }

  invalidate(): void {
    /* nothing cached between frames */
  }
}

/**
 * The monitor for a run that must not show one.
 *
 * Same shape as the real thing, so the composition root never needs an `if`
 * where a monitor might or might not exist — the same trick
 * `createNullPresenter()` plays in `src/render.ts`.
 */
export function createNullMonitor(reason?: string): BackendMonitor {
  return {
    start(): void {},
    stop(): void {},
    async poll(): Promise<void> {},
    get running(): boolean {
      return false;
    },
    lines(): string[] {
      return [];
    },
    subscribe(): () => void {
      return () => undefined;
    },
    get snapshot(): BackendSnapshot {
      return EMPTY_SNAPSHOT;
    },
    describe(): string[] {
      return [`  monitor: off${reason === undefined ? "" : ` — ${reason}`}`];
    },
  };
}
