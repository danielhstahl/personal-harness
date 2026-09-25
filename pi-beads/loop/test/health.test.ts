/**
 * Tests for `src/health.ts` — finding the report and reading it without
 * hanging.
 *
 * The URL derivation is the whole reason the brief said *the `/health` endpoint
 * is at the base URL, not after `/v1`*. Getting it wrong is a 404 on every
 * start, and a startup probe that hangs is worse than one that is missing, so
 * both halves get pinned: where we look, and what we do when nothing answers.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { healthUrlFor, probeHealth } from "../src/health.ts";

test("the health report sits at the base, not under the api version prefix", () => {
  assert.equal(healthUrlFor("http://host:8081/v1"), "http://host:8081/health");
  assert.equal(healthUrlFor("http://host:8081/v1/"), "http://host:8081/health");
  assert.equal(healthUrlFor("https://inference.test/v1"), "https://inference.test/health");
});

test("a proxied or versioned path keeps its prefix", () => {
  assert.equal(healthUrlFor("http://host:8081/gw/v1"), "http://host:8081/gw/health");
  assert.equal(healthUrlFor("https://example.com/a/b/v2"), "https://example.com/a/b/health");
  assert.equal(healthUrlFor("http://host:8081"), "http://host:8081/health");
  assert.equal(healthUrlFor("http://host:8081/"), "http://host:8081/health");
});

test("a base that already names /health is left alone (idempotent)", () => {
  assert.equal(healthUrlFor("http://host:8081/health"), "http://host:8081/health");
  assert.equal(healthUrlFor("http://host:8081/gw/health"), "http://host:8081/gw/health");
  // Feeding a resolved URL back in must not produce /health/health.
  assert.equal(healthUrlFor(healthUrlFor("http://host:9/v1") ?? ""), "http://host:9/health");
});

test("a base that is not a URL gives no answer rather than a wrong one", () => {
  assert.equal(healthUrlFor(undefined), undefined);
  assert.equal(healthUrlFor(""), undefined);
  assert.equal(healthUrlFor("not a url"), undefined);
  assert.equal(healthUrlFor("ftp://host/v1"), undefined, "only http(s) is probed");
});

test("a good report comes back parsed, with the timing", async () => {
  const clock = { t: 1000 };
  const probe = await probeHealth({
    url: "http://x/health",
    now: () => clock.t += 5,
    fetchImpl: (async () =>
      new Response(JSON.stringify({ status: "ok", context: 123 }), { status: 200 })) as typeof fetch,
  });
  assert.equal(probe.ok, true);
  assert.equal(probe.status, 200);
  assert.deepEqual(probe.report, { status: "ok", context: 123 });
  assert.equal(probe.elapsedMs, 5, "the elapsed figure is the probe's, not the caller's clock");
});

test("a non-2xx is a failure with a quotable reason, not a throw", async () => {
  const probe = await probeHealth({
    url: "http://x/health",
    fetchImpl: (async () => new Response("gateway exploded", { status: 502 })) as typeof fetch,
  });
  assert.equal(probe.ok, false);
  assert.equal(probe.status, 502);
  assert.match(probe.error ?? "", /502/);
  assert.match(probe.error ?? "", /gateway exploded/);
  assert.equal(probe.report, undefined);
});

test("a JSON array or scalar is not a report", async () => {
  for (const body of ["[1,2,3]", "\"hello\"", "null"]) {
    const probe = await probeHealth({
      url: "http://x/health",
      fetchImpl: (async () => new Response(body, { status: 200 })) as typeof fetch,
    });
    assert.equal(probe.ok, false, body);
    assert.match(probe.error ?? "", /not a JSON object|was not JSON/);
  }
});

test("a body that is not JSON says so instead of throwing", async () => {
  const probe = await probeHealth({
    url: "http://x/health",
    fetchImpl: (async () => new Response("<html>maintenance</html>", { status: 200 })) as typeof fetch,
  });
  assert.equal(probe.ok, false);
  assert.match(probe.error ?? "", /not JSON/);
});

test("a slow server is cut off at the deadline", async () => {
  const started = Date.now();
  const probe = await probeHealth({
    url: "http://x/health",
    timeoutMs: 20,
    fetchImpl: (async (_url: unknown, init?: RequestInit) =>
      new Promise((_resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("This operation was aborted")), 5_000);
        init?.signal?.addEventListener("abort", () => {
          clearTimeout(timer);
          reject(new Error("This operation was aborted"));
        });
      })) as typeof fetch,
  });
  assert.equal(probe.ok, false);
  assert.match(probe.error ?? "", /no answer within 20ms/);
  assert.ok(Date.now() - started < 1_000, "the deadline actually bit");
});

test("with no injected implementation the global fetch is used", async () => {
  const original = globalThis.fetch;
  let seen = "";
  globalThis.fetch = (async (url: unknown) => {
    seen = String(url);
    return new Response(JSON.stringify({ status: "ok" }), { status: 200 });
  }) as typeof fetch;
  try {
    const probe = await probeHealth({ url: "http://global.test/health" });
    assert.equal(probe.ok, true);
    assert.equal(seen, "http://global.test/health", "the global was called with our URL");
  } finally {
    globalThis.fetch = original;
  }
});

test("the snippet is capped, so a giant error page cannot become a giant log line", async () => {
  const probe = await probeHealth({
    url: "http://x/health",
    fetchImpl: (async () => new Response("x".repeat(10_000), { status: 500 })) as typeof fetch,
  });
  assert.equal(probe.ok, false);
  assert.ok((probe.error ?? "").length < 400, `error was ${(probe.error ?? "").length} chars`);
});
