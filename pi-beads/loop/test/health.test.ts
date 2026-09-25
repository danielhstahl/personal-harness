/**
 * Tests for `src/health.ts` — the probe that reads `/health`.
 *
 * The subject here is not whether the parse works; it is what the probe does when
 * the world is unkind. A model box that is down, slow, behind a captive portal,
 * or answering 200 with HTML must each produce a *distinct, stated* failure with
 * the URL in it, because the alternative — one generic "probe failed" — sends
 * someone to read code before they know whether to check the network or the
 * config. And one rule is checked over all of them: the probe never mutates, never
 * retries, and never POSTs. A status endpoint probed in a retry loop is a
 * denial-of-service attempt with good intentions.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  HealthError,
  healthUrlFromBaseUrl,
  parseServerHealth,
  probeServerHealth,
  resolveProbeTarget,
  type FetchLike,
} from "../src/health.ts";

const fixtureBody = readFileSync(new URL("./fixtures/health-halogen.json", import.meta.url), "utf8");

interface FakeResponse {
  status?: number;
  body?: string;
  error?: Error;
  /** Never settle; only reject when the abort signal fires. */
  hang?: boolean;
}

function fakeFetch(response: FakeResponse): {
  impl: FetchLike;
  calls: { url: string; method: string; headers?: Record<string, string> }[];
} {
  const calls: { url: string; method: string; headers?: Record<string, string> }[] = [];
  const impl: FetchLike = (url, init) => {
    calls.push({ url, method: init.method, headers: init.headers });
    if (response.error !== undefined) return Promise.reject(response.error);
    if (response.hang === true) {
      return new Promise((_resolve, reject) => {
        init.signal?.addEventListener("abort", () => {
          const error = new Error("This operation was aborted");
          error.name = "AbortError";
          reject(error);
        });
      });
    }
    const status = response.status ?? 200;
    return Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      text: async () => response.body ?? "",
    });
  };
  return { impl, calls };
}

const URL_OK = "http://box.test:8081/health";

test("the captured report parses into typed fields", () => {
  const health = parseServerHealth(JSON.parse(fixtureBody));
  assert.equal(health.status, "ok");
  assert.equal(health.context, 262_144);
  assert.equal(health.slots, 4);
  assert.equal(health.slot_ctx, 262_144);
  assert.equal(health.kv_pool_positions, 262_144);
  assert.equal(health.max_tokens_cap, 65_536);
  assert.equal(health.max_tokens_default, 8_192);
  assert.deepEqual(health.chat_template_kwargs, [
    "reasoning_effort",
    "enable_thinking",
    "preserve_thinking",
  ]);
  assert.equal(health.thinking_answer_room, "max(1024, 15% of max_tokens)");
  assert.equal(health.vision?.enabled, false);
  assert.equal(health.engine?.responds, true);
  // The raw payload is kept, so a field this type has not been taught yet is
  // still reachable rather than silently discarded.
  assert.equal(health.raw["rope_scaling"], "yarn");
});

test("a payload with none of the fields we came for is refused as not-a-health-report", () => {
  // The captive-portal case: an HTTP 200 with a body that is a login page in
  // JSON clothing. Refusing it here is what stops every derived field coming back
  // `unknown` and every guard passing vacuously.
  const fake = fakeFetch({ body: JSON.stringify({ title: "Sign in", ok: true }) });
  return probeServerHealth({ url: URL_OK, fetchImpl: fake.impl }).then((result) => {
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.equal(result.error.kind, "shape");
    assert.match(result.error.message, /not a \/health report/);
  });
});

test("a JSON array is not a health report either", () => {
  assert.throws(() => parseServerHealth([1, 2, 3]), (error: unknown) => HealthError.is(error));
});

test("a missing field stays unknown rather than becoming zero", () => {
  // The rule the whole derivation stands on: `context: 0` would make every later
  // guard pass while the request fails. Unknown must be absent, not small.
  const health = parseServerHealth({ status: "ok", context: "262144" });
  assert.equal(health.context, undefined, "a string is not a number we will trust");
  assert.equal(health.slots, undefined);
  assert.equal(health.busy, undefined);
});

test("a probe with no configured url says so instead of pretending", () => {
  return probeServerHealth({ url: "   " }).then((result) => {
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.equal(result.error.kind, "not-configured");
    assert.equal(result.url, null);
  });
});

test("a successful probe reports the report and its latency, and asks with GET", async () => {
  const fake = fakeFetch({ body: fixtureBody });
  let clock = 1_000;
  const result = await probeServerHealth({
    url: URL_OK,
    fetchImpl: fake.impl,
    now: () => {
      clock += 37;
      return clock;
    },
  });
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.equal(result.health.context, 262_144);
  assert.equal(result.latencyMs, 37);
  assert.equal(result.url, URL_OK);
  assert.equal(fake.calls.length, 1, "one attempt; a probe must not retry");
  assert.equal(fake.calls[0]?.method, "GET", "a health endpoint is read, never written to");
  assert.equal(fake.calls[0]?.headers?.accept, "application/json");
});

test("a non-2xx answer is reported with its status", async () => {
  const fake = fakeFetch({ status: 503, body: "engine is loading" });
  const result = await probeServerHealth({ url: URL_OK, fetchImpl: fake.impl });
  assert.equal(result.ok, false);
  if (result.ok) return;
  assert.equal(result.error.kind, "http");
  assert.equal(result.error.status, 503);
  assert.match(result.error.message, /503/);
});

test("a body that is not JSON is a shape failure naming the parse error", async () => {
  const fake = fakeFetch({ body: "<html><body>proxy</body></html>" });
  const result = await probeServerHealth({ url: URL_OK, fetchImpl: fake.impl });
  assert.equal(result.ok, false);
  if (result.ok) return;
  assert.equal(result.error.kind, "shape");
});

test("a connection refused is unreachable, with the reason kept", async () => {
  const fake = fakeFetch({ error: Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:8081"), {
    name: "FetchError",
  }) });
  const result = await probeServerHealth({ url: URL_OK, fetchImpl: fake.impl });
  assert.equal(result.ok, false);
  if (result.ok) return;
  assert.equal(result.error.kind, "unreachable");
  assert.match(result.error.message, /ECONNREFUSED/);
});

test("a box that never answers times out rather than hanging the run", async () => {
  const fake = fakeFetch({ hang: true });
  const started = Date.now();
  const result = await probeServerHealth({ url: URL_OK, fetchImpl: fake.impl, timeoutMs: 25 });
  assert.equal(result.ok, false);
  if (result.ok) return;
  assert.equal(result.error.kind, "timeout");
  // Not the deadline exactly — but it must not have waited for the socket's own
  // idea of patience, which is measured in minutes.
  assert.ok(Date.now() - started < 2_000, "the probe's own timeout fired");
});

test("no fetch implementation in the runtime is stated, not thrown", async () => {
  const saved = globalThis.fetch;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  delete (globalThis as any).fetch;
  try {
    const result = await probeServerHealth({ url: URL_OK });
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.equal(result.error.kind, "unreachable");
  } finally {
    globalThis.fetch = saved;
  }
});

test("healthUrlFromBaseUrl replaces the API path and keeps any mount prefix", () => {
  assert.equal(healthUrlFromBaseUrl("http://box.test:8081/v1"), "http://box.test:8081/health");
  assert.equal(
    healthUrlFromBaseUrl("https://llm.example.com/v1/chat/completions"),
    "https://llm.example.com/health",
  );
  // A box mounted under a prefix reports under the same prefix. Stripping to the
  // origin would ask the wrong path and call the 404 a dead box.
  assert.equal(healthUrlFromBaseUrl("http://box.test/gpu1/v1"), "http://box.test/gpu1/health");
  assert.equal(healthUrlFromBaseUrl("http://box.test:8081"), "http://box.test:8081/health");
  assert.equal(healthUrlFromBaseUrl("http://box.test:8081/v1?x=1#y"), "http://box.test:8081/health");
  assert.equal(healthUrlFromBaseUrl("not a url"), null);
});

test("resolveProbeTarget: what was configured wins, otherwise the provider answers", () => {
  assert.deepEqual(resolveProbeTarget({ configuredUrl: "http://mgmt/health" }), {
    url: "http://mgmt/health",
    source: "configured",
  });
  assert.deepEqual(resolveProbeTarget({ providerBaseUrl: "http://box:8081/v1" }), {
    url: "http://box:8081/health",
    source: "provider",
    from: "http://box:8081/v1",
  });
  // Explicit beats derived, because a URL somebody typed is a deliberate act.
  const both = resolveProbeTarget({
    configuredUrl: "http://mgmt/h",
    providerBaseUrl: "http://box:8081/v1",
  });
  if (both.url === null) throw new Error("a configured url must produce a target");
  assert.equal(both.source, "configured");
  // Neither: no probe, and the provider's own reason is carried through.
  const nothing = resolveProbeTarget({ providerNote: "pi has no configured default model" });
  assert.equal(nothing.url, null);
  assert.equal(nothing.reason, "pi has no configured default model");
  // A base URL that cannot be parsed says so rather than probing somewhere odd.
  const junk = resolveProbeTarget({ providerBaseUrl: "nonsense" });
  assert.equal(junk.url, null);
  if (junk.url === null) assert.match(junk.reason, /not a URL this probe can derive/);
});
