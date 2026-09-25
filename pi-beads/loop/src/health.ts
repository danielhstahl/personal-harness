/**
 * The provider's own report of itself.
 *
 * Every number this loop cares about — the context window, the output ceiling,
 * which thinking knobs exist, whether vision is wired up — is a fact about a
 * server, and a config file is only a *claim* about that fact. The server is the
 * one witness worth checking, and most OpenAI-compatible inference servers publish
 * a report of exactly those facts on a `health` endpoint.
 *
 * The report lives at the **base** of the URL, not under the API version prefix:
 * `http://host:8081/v1` is the API Pi sends requests to and
 * `http://host:8081/health` is the report about that server. {@link healthUrlFor}
 * is the only place that transformation is encoded, so nothing else in this
 * codebase has an opinion about where the health page lives.
 *
 * Reading the report is the only side effect in this file, and it is deliberately
 * shy: one GET, one timeout, no retries, no credential probing. A startup check
 * that can hang the run is worse than no startup check, so every failure path
 * returns a value rather than throwing.
 */

/** A JSON object, as read off the wire. */
export type JsonRecord = Record<string, unknown>;

/** True for a plain JSON object (not an array, not null). */
export function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The health URL for an API base URL.
 *
 * Handles the three shapes that actually show up in configs:
 *
 * - `http://host:8081/v1`      → `http://host:8081/health`
 * - `http://host:8081/v1/`     → `http://host:8081/health`
 * - `http://host:8081/gw/v1`  → `http://host:8081/gw/health`
 *
 * A base that already names `/health` is returned unchanged (idempotent, so a
 * caller may pass a resolved URL back in). Anything that is not a valid URL, or
 * not http(s), gives `undefined` — which the caller reports rather than guesses.
 */
export function healthUrlFor(baseUrl: string | undefined): string | undefined {
  const trimmed = (baseUrl ?? "").trim();
  if (trimmed === "") return undefined;

  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return undefined;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return undefined;

  const path = url.pathname.replace(/\/+$/u, "");
  if (path.toLowerCase().endsWith("/health")) {
    url.pathname = path === "" ? "/health" : path;
    url.search = "";
    url.hash = "";
    return url.toString();
  }
  // Drop the API version segment; the report is served beside it, not inside it.
  const withoutVersion = /\/v\d+$/u.test(path) ? path.replace(/\/v\d+$/u, "") : path;
  url.pathname = `${withoutVersion}/health`;
  url.search = "";
  url.hash = "";
  return url.toString();
}

export interface HealthProbeOptions {
  readonly url: string;
  /** Wall clock for the whole request. Default 2000ms. */
  readonly timeoutMs?: number;
  readonly fetchImpl?: typeof fetch;
  readonly headers?: Readonly<Record<string, string>>;
  /** Injectable clock for the elapsed figure. */
  readonly now?: () => number;
}

/**
 * The result of one GET. There is no throwing out of here: an unreachable server
 * is a *finding*, not a crash, and the caller has a loop to start either way.
 */
export interface HealthProbe {
  readonly url: string;
  readonly ok: boolean;
  readonly elapsedMs: number;
  readonly status?: number;
  /** Parsed report body, present only on a 2xx JSON object. */
  readonly report?: JsonRecord;
  readonly error?: string;
}

const DEFAULT_TIMEOUT_MS = 2_000;
/** Enough of a body to quote a reason out of, not enough to leak a payload. */
const BODY_SNIPPET_CHARS = 240;

function snippet(text: string): string {
  const flat = text.replace(/\s+/gu, " ").trim();
  return flat.length > BODY_SNIPPET_CHARS ? `${flat.slice(0, BODY_SNIPPET_CHARS)}…` : flat;
}

/** One GET, one deadline, one parse. Never throws. */
export async function probeHealth(options: HealthProbeOptions): Promise<HealthProbe> {
  const now = options.now ?? Date.now;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const started = now();
  const fail = (error: string, status?: number): HealthProbe => ({
    url: options.url,
    ok: false,
    elapsedMs: now() - started,
    ...(status === undefined ? {} : { status }),
    error,
  });

  const doFetch: typeof fetch | undefined =
    options.fetchImpl ?? (typeof fetch === "function" ? fetch : undefined);
  if (doFetch === undefined) return fail("no fetch implementation available");

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), Math.max(1, timeoutMs));
  timer.unref?.();

  try {
    const response = await doFetch(options.url, {
      method: "GET",
      headers: { accept: "application/json", ...(options.headers ?? {}) },
      signal: controller.signal,
    });
    const body = await response.text().catch(() => "");
    if (!response.ok) {
      const detail = snippet(body);
      return fail(
        `HTTP ${response.status}${detail === "" ? "" : `: ${detail}`}`,
        response.status,
      );
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(body);
    } catch {
      return fail(`response was not JSON (${snippet(body) || "empty body"})`, response.status);
    }
    if (!isRecord(parsed)) return fail("health report was not a JSON object", response.status);
    return { url: options.url, ok: true, elapsedMs: now() - started, status: response.status, report: parsed };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const aborted = /abort/u.test(message);
    return fail(aborted ? `no answer within ${timeoutMs}ms` : message);
  } finally {
    clearTimeout(timer);
  }
}
