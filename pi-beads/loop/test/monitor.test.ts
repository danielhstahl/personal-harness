/**
 * Tests for `src/monitor.ts` — the read-only window into the inference server.
 *
 * The module has four jobs and each is pinned here separately, because the way
 * one of them breaks is never the way another breaks:
 *
 *   A. **Where to ask.** The URLs must come out of the base the same way the
 *      audit derives them, and the paths the server advertises must win.
 *   B. **What came back.** JSON and Prometheus text are both legitimate for
 *      `/metrics`; an HTML error page is neither, and must not become zeroes.
 *   C. **What it means.** A rate is a slope between two reads of one key; a
 *      reset counter and a renamed counter both have to yield *no* number rather
 *      than a wrong one.
 *   D. **What is drawn.** A fixed order, `—` for what was never witnessed, a
 *      marker for how old the reading is, and nothing that ever reads as a
 *      live figure when it is not.
 *
 * Plus the two surfaces: the monitor may be drawn over the work stream and above
 * the idle prompt, and may not — when released, when plain, when off.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";

import { stripTerminalSequences, visibleWidth } from "@earendil-works/pi-tui";
import { initTheme } from "@earendil-works/pi-coding-agent";

import {
  createNullPresenter,
  createPresenterTheme,
  createWorkPresenter,
} from "../src/render.ts";
import { createIdleMode, type IdleHandle } from "../src/idle.ts";
import { buildApp } from "../src/app.ts";
import { FakeSignals, FakeTerminal, settle } from "./idle-fakes.ts";

import {
  MONITOR_ENDPOINTS,
  MONITOR_MISSING,
  PLAIN_MONITOR_THEME,
  compactNumber,
  createBackendMonitor,
  createNullMonitor,
  emptyEndpoint,
  flattenRecord,
  flowSegments,
  formatAge,
  freshnessOf,
  packSegments,
  parseBody,
  parsePrometheus,
  pathIsRead,
  panelSegments,
  percent,
  rateNumber,
  refineUrls,
  renderFlowLine,
  renderTopPanel,
  resolveMonitorUrls,
  summarise,
  urlsForBase,
  type BackendSnapshot,
  type EndpointState,
} from "../src/monitor.ts";

initTheme("dark", false);

const here = dirname(fileURLToPath(import.meta.url));
const HEALTH: Record<string, unknown> = JSON.parse(
  readFileSync(join(here, "fixtures", "health.json"), "utf8"),
);

// ── helpers ──────────────────────────────────────────────────────────────────

function live(name: EndpointState["name"], body: unknown, at = 1_000): EndpointState {
  const parsed = typeof body === "string" ? parseBody(body) : parseBody(JSON.stringify(body));
  assert.ok(parsed !== undefined, `fixture for ${name} must be parseable`);
  return {
    name,
    url: `http://host:8081/${name}`,
    state: "live",
    at,
    latencyMs: 5,
    failures: 0,
    body: parsed,
    hasBody: true,
  };
}

const EMPTY_SNAPSHOT: BackendSnapshot = {
  at: 0,
  ageMs: 0,
  anyLive: false,
  endpoints: [],
};

/** A clock and scheduler the test drives by hand. */
function fakeTime(start = 1_000) {
  let current = start;
  const timers = new Map<number, { at: number; run: () => void }>();
  let nextId = 1;
  return {
    now: () => current,
    pending: () => timers.size,
    advance(ms: number) {
      current += ms;
    },
    /** Move the clock and fire everything that came due, in order. */
    tick(ms: number) {
      current += ms;
      const due = [...timers.entries()]
        .filter(([, timer]) => timer.at <= current)
        .sort((a, b) => a[1].at - b[1].at);
      for (const [id, timer] of due) {
        timers.delete(id);
        timer.run();
      }
    },
    schedule(run: () => void, ms: number) {
      const id = nextId++;
      timers.set(id, { at: current + ms, run });
      return () => {
        timers.delete(id);
      };
    },
    calls: () => timers.size,
  };
}

/** A `fetch` that answers from a table, and remembers every URL it was asked for. */
function fakeFetch(table: Record<string, { status?: number; body: string; contentType?: string }>) {
  const asked: string[] = [];
  const impl = (async (url: unknown, _init?: RequestInit) => {
    const key = String(url);
    asked.push(key);
    const entry = table[key];
    if (entry === undefined) return new Response("not found", { status: 404 });
    return new Response(entry.body, {
      status: entry.status ?? 200,
      headers: { "content-type": entry.contentType ?? "application/json" },
    });
  }) as unknown as typeof fetch;
  return { impl, asked };
}

const json = (value: unknown): string => JSON.stringify(value);

// ══════════════════════════════════════════════════════════════════════════════
// A. where to ask
// ══════════════════════════════════════════════════════════════════════════════

describe("monitor urls: the diagnostics live beside the api, not inside it", () => {
  it("spreads the four endpoints off a versioned base", () => {
    const urls = urlsForBase("http://host:8081/v1");
    assert.deepEqual(urls, {
      health: "http://host:8081/health",
      metrics: "http://host:8081/metrics",
      cache: "http://host:8081/cache",
      models: "http://host:8081/v1/models",
    });
  });

  it("keeps a gateway prefix on all four", () => {
    const urls = urlsForBase("https://gw.internal/team-a/v1");
    assert.equal(urls.health, "https://gw.internal/team-a/health");
    assert.equal(urls.metrics, "https://gw.internal/team-a/metrics");
    assert.equal(urls.cache, "https://gw.internal/team-a/cache");
    assert.equal(urls.models, "https://gw.internal/team-a/v1/models");
  });

  it("keeps the version segment that was actually present for /models", () => {
    assert.equal(
      urlsForBase("http://host:8081/v2").models,
      "http://host:8081/v2/models",
      "a v2 server is asked for v2 models",
    );
    assert.equal(
      urlsForBase("http://host:8081").models,
      "http://host:8081/v1/models",
      "a base with no version falls back to the openai convention",
    );
  });

  it("is a no-op on a base that is not a url", () => {
    assert.deepEqual(urlsForBase(undefined), {});
    assert.deepEqual(urlsForBase("not a url"), {});
    assert.deepEqual(urlsForBase("ftp://host/v1"), {});
  });

  it("takes the leaf of an advertised path and keeps our own prefix", () => {
    // The engine reports `/cache_counters`; we are behind `/gw`. Taking the
    // advertised absolute path would drop the gateway; taking our own guess
    // would ignore the witness. Take the name from one, the prefix from the other.
    const urls = refineUrls(urlsForBase("http://gw/team/v1"), {
      cache_counters: "/somewhere/else/cache_counters",
      metrics: "/counters/metrics",
    });
    assert.equal(urls.cache, "http://gw/team/cache_counters");
    assert.equal(urls.metrics, "http://gw/team/metrics");
  });

  it("learns the models path from the advertised endpoint list", () => {
    const urls = refineUrls(urlsForBase("http://host:8081/v1"), {
      endpoints: ["/v1/chat/completions", "/v2/models"],
    });
    assert.equal(urls.models, "http://host:8081/v2/models");
  });

  it("leaves the guesses alone when the report says nothing useful", () => {
    const before = urlsForBase("http://host:8081/v1");
    assert.deepEqual(refineUrls(before, {}), before);
    assert.deepEqual(refineUrls(before, undefined), before);
    // Two model paths is an ambiguous answer, so it is not taken.
    const ambiguous = refineUrls(before, { endpoints: ["/v1/models", "/v2/models"] });
    assert.equal(ambiguous.models, "http://host:8081/v1/models");
  });

  it("resolves precedence: explicit monitor url, then health url, then base", () => {
    assert.equal(
      resolveMonitorUrls({
        monitorUrl: "http://other:9/v1",
        healthUrl: "http://health-only/health",
        baseUrl: "http://base/v1",
      }).health,
      "http://other:9/health",
    );
    assert.equal(
      resolveMonitorUrls({ healthUrl: "http://health-only/health", baseUrl: "http://base/v1" })
        .health,
      "http://health-only/health",
    );
    assert.equal(
      resolveMonitorUrls({ baseUrl: "http://base/v1" }).cache,
      "http://base/cache",
    );
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// B. what came back
// ══════════════════════════════════════════════════════════════════════════════

describe("monitor bodies: json, metrics text, or nothing", () => {
  it("flattens nested json into dotted scalar paths", () => {
    const flat = flattenRecord({
      context: 262144,
      version: { api: "0.11.5", match: true },
      prompt_cache: { enabled: true, cap_mb: 110 },
      drafters_available: ["serial", "mtp"],
    });
    assert.equal(flat.numbers.get("context"), 262144);
    assert.equal(flat.strings.get("version.api"), "0.11.5");
    assert.equal(flat.bools.get("version.match"), true);
    assert.equal(flat.bools.get("prompt_cache.enabled"), true);
    assert.deepEqual(flat.arrays.get("drafters_available"), ["serial", "mtp"]);
  });

  it("indexes objects inside arrays so a model entry is addressable", () => {
    const flat = flattenRecord({ data: [{ id: "a", max_tokens: 128 }, { id: "b" }] });
    assert.equal(flat.strings.get("data.0.id"), "a");
    assert.equal(flat.numbers.get("data.1.max_tokens"), undefined);
  });

  it("parses prometheus exposition, summing same-named series and keeping labels", () => {
    const prom = parsePrometheus(
      [
        "# HELP tokens_generated total tokens decoded",
        "# TYPE tokens_generated counter",
        "tokens_generated 12345",
        'kv_used{pool="a"} 100',
        'kv_used{pool="b"} 50',
        "requests_served{model=\"x\",state=\"ok\"} 7",
      ].join("\n"),
    );
    assert.ok(prom !== undefined);
    assert.equal(prom.numbers.get("tokens_generated"), 12345);
    assert.equal(prom.numbers.get("kv_used"), 150, "same name across label sets sums to the total");
    assert.equal(prom.numbers.get("requests_served"), 7);
    assert.deepEqual(
      prom.series.filter((s) => s.name === "kv_used").map((s) => s.labels.pool),
      ["a", "b"],
    );
  });

  it("drops non-finite samples and junk lines without dropping the whole body", () => {
    const prom = parsePrometheus("good 1\nbroken NaN\ngauge Inf\n   \nnot a metric at all\n");
    assert.deepEqual([...(prom?.numbers.keys() ?? [])], ["good"]);
  });

  it("answers undefined for text that is not metrics at all", () => {
    assert.equal(parsePrometheus("<html>maintenance</html>"), undefined);
    assert.equal(parseBody("<html>maintenance</html>"), undefined);
    assert.equal(parseBody(""), undefined);
    assert.equal(parseBody("[1,2,3]"), undefined, "a top-level array is not a report");
  });

  it("reads metrics text through the same entry point as json", () => {
    const body = parseBody("cache_hits 40\ncache_misses 10\n");
    assert.equal(body?.numbers.get("cache_hits"), 40);
    assert.equal(body?.series.length, 2);
    assert.equal(body?.raw, undefined, "metrics text has no raw json object");
  });

  it("keeps the raw json object so advertised paths survive flattening", () => {
    const body = parseBody(json({ cache_counters: "/cache_counters", context: 10 }));
    assert.equal(body?.raw?.cache_counters, "/cache_counters");
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// C. what it means
// ══════════════════════════════════════════════════════════════════════════════

describe("monitor snapshot: facts from the report", () => {
  it("reads the ceiling figures off the health report", () => {
    const { snapshot } = summarise({ endpoints: [live("health", HEALTH)], now: 2_000 });
    assert.equal(snapshot.context, 262_144);
    assert.equal(snapshot.slots, 4);
    assert.equal(snapshot.slotsUsed, 1, "in_flight is the slots in use");
    assert.equal(snapshot.queued, 0);
    assert.equal(snapshot.kvCapacity, 262_144);
    assert.equal(snapshot.maxTokens, 65_536, "the cap, not the default");
    assert.equal(snapshot.maxTokensDefault, 8_192);
    assert.equal(snapshot.drafter, "mtp");
    assert.deepEqual(snapshot.drafters, ["serial", "mtp"]);
    assert.equal(snapshot.apiVersion, "0.11.5");
    assert.equal(snapshot.versionsMatch, true);
    assert.equal(snapshot.cacheEnabled, true);
    assert.equal(snapshot.cacheCapMb, 110);
  });

  it("is empty-but-safe when no source said anything", () => {
    const { snapshot } = summarise({ endpoints: [], now: 1_000 });
    assert.equal(snapshot.anyLive, false);
    assert.equal(snapshot.context, undefined);
    assert.equal(snapshot.tokensPerSecond, undefined);
    assert.ok(snapshot.error !== undefined, "and it says why nothing is there");
  });

  it("marks a stale reading by its age, not by guessing", () => {
    const { snapshot } = summarise({ endpoints: [live("health", HEALTH, 1_000)], now: 30_000 });
    assert.equal(snapshot.anyLive, true, "it is still the truth about the server");
    assert.equal(snapshot.ageMs, 29_000, "and it says how old it is");
    assert.equal(freshnessOf(snapshot).state, "stale");
  });

  it("keeps showing a source that answered once and has since gone quiet", () => {
    // The endpoint's own state is `error` now; the body it left behind is still
    // shown, ageing. That is the honest version of "server down, numbers as of…".
    const kept: EndpointState = { ...live("health", HEALTH, 1_000), state: "error", error: "ETIMEDOUT" };
    const { snapshot } = summarise({ endpoints: [kept], now: 1_500 });
    assert.equal(snapshot.context, 262_144);
    assert.equal(snapshot.anyLive, true);
  });

  it("does not count a known-absent endpoint as a witness", () => {
    const absent: EndpointState = {
      ...live("cache", { hit_rate: 0.99 }),
      state: "absent",
      hasBody: false,
    };
    const { snapshot } = summarise({ endpoints: [absent], now: 1_000 });
    assert.equal(snapshot.cacheHitRate, undefined);
    assert.equal(snapshot.anyLive, false);
  });
});

describe("monitor rates: a slope between two reads of one key", () => {
  const first = [
    live(
      "metrics",
      {
        tokens_generated: 10_000,
        requests_served: 10,
        prompt_cache_hits: 100,
        prompt_cache_misses: 100,
      },
      1_000,
    ),
  ];
  const second = (patch: Record<string, number>, at: number): EndpointState[] => [
    live(
      "metrics",
      {
        tokens_generated: 10_000 + (patch.tokens ?? 0),
        requests_served: 10 + (patch.requests ?? 0),
        prompt_cache_hits: 100 + (patch.hits ?? 0),
        prompt_cache_misses: 100 + (patch.misses ?? 0),
      },
      at,
    ),
  ];

  it("has no rate on the first read, because there is no slope yet", () => {
    const { snapshot } = summarise({ endpoints: first, now: 1_000 });
    assert.equal(snapshot.tokensPerSecond, undefined);
    assert.equal(snapshot.requestsPerSecond, undefined);
  });

  it("derives tokens/second and requests/second from the interval", () => {
    const { memo } = summarise({ endpoints: first, now: 1_000 });
    const { snapshot } = summarise({
      endpoints: second({ tokens: 4_000, requests: 2 }, 3_000),
      now: 3_000,
      previous: memo,
    });
    assert.equal(snapshot.tokensPerSecond, 2_000, "4000 tokens over 2s");
    assert.equal(snapshot.requestsPerSecond, 1, "2 requests over 2s");
    assert.equal(snapshot.requestsServed, 12, "and the cumulative total is shown too");
  });

  it("prefers a rate the server measured itself", () => {
    const withInstant = [
      live("metrics", { tokens_generated: 10_000, tokens_per_second: 137.5 }, 1_000),
    ];
    const { memo } = summarise({ endpoints: withInstant, now: 1_000 });
    const { snapshot } = summarise({
      endpoints: [live("metrics", { tokens_generated: 12_000, tokens_per_second: 137.5 }, 3_000)],
      now: 3_000,
      previous: memo,
    });
    assert.equal(snapshot.tokensPerSecond, 137.5);
  });

  it("reports no rate when the counter went backwards (a restart is not a speed)", () => {
    const { memo } = summarise({ endpoints: first, now: 1_000 });
    const { snapshot } = summarise({
      endpoints: second({ tokens: -9_000 }, 3_000),
      now: 3_000,
      previous: memo,
    });
    assert.equal(snapshot.tokensPerSecond, undefined);
  });

  it("reports no rate when the figure moved to a different key", () => {
    const { memo } = summarise({ endpoints: first, now: 1_000 });
    const moved = [
      live("metrics", { generated_tokens: 20_000, requests_served: 20 }, 3_000),
    ];
    const { snapshot } = summarise({ endpoints: moved, now: 3_000, previous: memo });
    assert.equal(
      snapshot.tokensPerSecond,
      undefined,
      "diffing `tokens_generated` against `generated_tokens` would be arithmetic, not measurement",
    );
  });

  it("reports no rate when the previous read came from a different endpoint", () => {
    const { memo } = summarise({ endpoints: first, now: 1_000 });
    const fromElsewhere = [
      live("cache", { tokens_generated: 20_000 }, 3_000),
      live("metrics", { requests_served: 10 }, 3_000),
    ];
    const { snapshot } = summarise({ endpoints: fromElsewhere, now: 3_000, previous: memo });
    assert.equal(snapshot.tokensPerSecond, undefined);
  });

  it("computes the cache hit rate over the interval, not over the server's life", () => {
    const { memo } = summarise({ endpoints: first, now: 1_000 });
    const { snapshot } = summarise({
      // 40 hits, 10 misses this interval, on top of 100/100 lifetime.
      endpoints: second({ hits: 40, misses: 10 }, 3_000),
      now: 3_000,
      previous: memo,
    });
    assert.ok(snapshot.cacheHitRate !== undefined);
    assert.ok(Math.abs(snapshot.cacheHitRate - 0.8) < 1e-9, `got ${snapshot.cacheHitRate}`);
  });

  it("normalises a rate the server sent as a percentage", () => {
    const { snapshot } = summarise({
      endpoints: [live("cache", { hit_rate: 87.5 })],
      now: 1_000,
    });
    assert.equal(snapshot.cacheHitRate, 87.5 / 100);
  });
});

describe("monitor kv pools", () => {
  it("sums one pool into the totals without showing a pool list", () => {
    const { snapshot } = summarise({
      endpoints: [
        live("health", {
          kv_pools: [{ name: "main", kv_pool_positions: 1000, kv_used: 500, slots: 2, slots_used: 1 }],
        }),
      ],
      now: 1_000,
    });
    assert.equal(snapshot.kvCapacity, 1_000);
    assert.equal(snapshot.kvUsed, 500);
    assert.equal(snapshot.kvUsage, 0.5);
    assert.equal(snapshot.pools, undefined, "one pool is what the totals already say");
  });

  it("keeps every pool when there is more than one", () => {
    const { snapshot } = summarise({
      endpoints: [
        live("cache", {
          kv_pools: [
            { name: "short", positions: 8_192, used: 8_000, slots: 2, slots_used: 2 },
            { name: "long", positions: 262_144, used: 20_000, slots: 2, slots_used: 0 },
          ],
        }),
      ],
      now: 1_000,
    });
    assert.equal(snapshot.pools?.length, 2);
    assert.deepEqual(snapshot.pools?.map((p) => p.name), ["short", "long"]);
    assert.equal(snapshot.kvCapacity, 270_336);
    assert.equal(snapshot.slots, 4, "slots across pools are the server's real slot count");
    assert.equal(snapshot.slotsUsed, 2);
    const segments = panelSegments(snapshot).ceiling.map((s) => s.label);
    assert.ok(segments.includes("pool short") && segments.includes("pool long"));
  });

  it("builds pools from labelled metrics series", () => {
    const { snapshot } = summarise({
      endpoints: [
        live(
          "metrics",
          'kv_pool_positions{pool="a"} 1000\nkv_used{pool="a"} 250\nkv_pool_positions{pool="b"} 2000\nkv_used{pool="b"} 1_900\n'
            .replace("1_900", "1900"),
        ),
      ],
      now: 1_000,
    });
    assert.deepEqual(snapshot.pools?.map((p) => `${p.name}:${p.capacity}:${p.used}`), [
      "a:1000:250",
      "b:2000:1900",
    ]);
  });

  it("derives usage from used/capacity when the server never reported a percentage", () => {
    const { snapshot } = summarise({
      endpoints: [live("metrics", { kv_used: 300, kv_pool_positions: 1_200 })],
      now: 1_000,
    });
    assert.equal(snapshot.kvUsage, 0.25);
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// D. what is drawn
// ══════════════════════════════════════════════════════════════════════════════

describe("monitor formatting", () => {
  it("compacts token counts to something a glance can read", () => {
    assert.equal(compactNumber(999), "999");
    assert.equal(compactNumber(1_000), "1k");
    assert.equal(compactNumber(8_192), "8.2k");
    assert.equal(compactNumber(65_536), "66k");
    assert.equal(compactNumber(262_144), "262k");
    assert.equal(compactNumber(1_500_000), "1.5M");
    assert.equal(compactNumber(undefined), MONITOR_MISSING);
  });

  it("renders percentages, rates and ages", () => {
    assert.equal(percent(0.5), "50%");
    assert.equal(percent(0.061), "6%");
    assert.equal(percent(undefined), MONITOR_MISSING);
    assert.equal(rateNumber(137.5), "138");
    assert.equal(rateNumber(12.34), "12.3");
    assert.equal(rateNumber(undefined), MONITOR_MISSING);
    assert.equal(formatAge(2_000), "2s");
    assert.equal(formatAge(305_000), "5m05s");
    assert.equal(formatAge(3_720_000), "1h02m");
    assert.equal(formatAge(undefined), MONITOR_MISSING);
  });

  it("tells live, stale and unreachable apart by colour role as well as glyph", () => {
    assert.equal(freshnessOf({ ...EMPTY_SNAPSHOT, anyLive: true, ageMs: 100 }).state, "live");
    assert.equal(freshnessOf({ ...EMPTY_SNAPSHOT, anyLive: true, ageMs: 30_000 }).state, "stale");
    assert.equal(freshnessOf({ ...EMPTY_SNAPSHOT, anyLive: false }).state, "down");
    const stale = freshnessOf({ ...EMPTY_SNAPSHOT, anyLive: true, ageMs: 30_000 });
    assert.equal(stale.role, "warning", "stale is a warning, not a disaster and not fine");
  });
});

describe("monitor panel", () => {
  const snapshot: BackendSnapshot = {
    at: 10_000,
    ageMs: 1_000,
    anyLive: true,
    context: 262_144,
    slots: 4,
    slotsUsed: 1,
    queued: 0,
    kvCapacity: 262_144,
    kvUsed: 240_000,
    kvUsage: 240_000 / 262_144,
    maxTokens: 65_536,
    drafter: "mtp",
    tokensPerSecond: 132.4,
    requestsPerSecond: 1.5,
    requestsServed: 415,
    cacheHitRate: 0.82,
    apiVersion: "0.11.5",
    engineVersion: "0.11.5",
    endpoints: [],
  };

  it("leads with the freshness marker, because everything else depends on it", () => {
    const lines = renderTopPanel(snapshot, null, { width: 200, maxLines: 2 });
    assert.match(lines[0] ?? "", /● 1s/);
    assert.ok(
      (lines[0] ?? "").indexOf("● 1s") < (lines[0] ?? "").indexOf("t/s"),
      "the marker comes before the figures it qualifies",
    );
  });

  it("puts the live figures on the first line and the ceiling on the second", () => {
    const lines = renderTopPanel(snapshot, null, { width: 200, maxLines: 2 });
    const first = lines[0] ?? "";
    const second = lines[1] ?? "";
    for (const field of ["t/s", "kv", "slots", "queued"]) {
      assert.ok(first.includes(field), `load line should carry ${field}`);
    }
    for (const field of ["ctx", "out", "draft", "cache"]) {
      assert.ok(second.includes(field), `ceiling line should carry ${field}`);
    }
  });

  it("shows a field it could not read as —, rather than dropping it from the line", () => {
    // A field that vanishes reflows the line, and a reflowing line is a line the
    // eye has to re-read every frame.
    const lines = renderTopPanel(EMPTY_SNAPSHOT, null, { width: 200, maxLines: 2 });
    const joined = lines.join(" | ");
    const missingCount = (joined.match(new RegExp(MONITOR_MISSING, "gu")) ?? []).length;
    assert.ok(missingCount >= 5, `expected several — placeholders, got: ${joined}`);
    assert.ok(joined.includes("unreachable"), "and it says why");
  });

  it("marks what did not fit instead of silently dropping it", () => {
    const lines = renderTopPanel(snapshot, null, { width: 40, maxLines: 1 });
    assert.equal(lines.length, 1);
    assert.match(lines[0] ?? "", /\+\d+ more/, "the reader is told how much they did not see");
    assert.ok(visibleWidthOf(lines[0] ?? "") <= 40);
  });

  it("never exceeds the width it was given", () => {
    for (const width of [24, 40, 63, 80, 120]) {
      const lines = renderTopPanel(snapshot, null, { width, maxLines: 2 });
      for (const line of lines) {
        assert.ok(visibleWidthOf(line) <= width, `${visibleWidthOf(line)} > ${width}: ${line}`);
      }
    }
  });

  it("changes nothing but colour between the plain and themed renderings", () => {
    const themed = renderTopPanel(snapshot, presenterTheme(), { width: 200, maxLines: 2 });
    const plain = renderTopPanel(snapshot, null, { width: 200, maxLines: 2 });
    assert.deepEqual(themed.map((line) => stripTerminalSequences(line)), plain);
    assert.notDeepEqual(themed, plain, "and the themed one really is coloured");
  });

  it("colours a nearly-full kv pool differently from an empty one", () => {
    const roles = (usage: number): string =>
      panelSegments({ ...snapshot, kvUsage: usage })
        .load.find((segment) => segment.label === "kv")!
        .role;
    assert.equal(roles(0.2), "success");
    assert.equal(roles(0.8), "warning");
    assert.equal(roles(0.95), "error");
  });

  it("warns when every slot is taken", () => {
    const segment = panelSegments({ ...snapshot, slotsUsed: 4, slots: 4 })
      .load.find((s) => s.label === "slots");
    assert.equal(segment?.role, "warning");
  });

  it("gives the flow line the short, live list", () => {
    const labels = flowSegments(snapshot).map((segment) => segment.label);
    assert.deepEqual(labels.slice(0, 4), ["", "t/s", "kv", "slots"]);
    const line = renderFlowLine(snapshot, null, { width: 120 });
    assert.equal(line.length, 1);
    assert.ok((line[0] ?? "").includes("t/s 132"), "the rate carries its own label");
    assert.ok((line[0] ?? "").includes("kv 92%") || (line[0] ?? "").includes("kv 91%"));
    const tight = renderFlowLine(snapshot, null, { width: 30 });
    assert.equal(tight.length, 1);
    assert.ok(visibleWidth(stripTerminalSequences(tight[0] ?? "")) <= 30);
  });
});

function visibleWidthOf(line: string): number {
  return visibleWidth(stripTerminalSequences(line));
}

function presenterTheme() {
  initTheme("dark", false);
  return createPresenterTheme();
}

// ══════════════════════════════════════════════════════════════════════════════
// E. the poller
// ══════════════════════════════════════════════════════════════════════════════

const ALL_FOUR = {
  "http://host:8081/health": {
    body: json({
      context: 262_144,
      slots: 4,
      in_flight: 1,
      metrics: "/counters",
      cache_counters: "/cache_stats",
      endpoints: ["/v1/chat/completions", "/v1/models"],
    }),
  },
  "http://host:8081/counters": { body: "tokens_generated 1000\nrequests_served 7" },
  "http://host:8081/cache_stats": { body: json({ hit_rate: 0.55 }) },
  "http://host:8081/v1/models": { body: json({ data: [{ id: "halogen-qwen3.8-flash-next" }] }) },
};

describe("monitor poller", () => {
  it("reads the report before anything that depends on where it pointed", async () => {
    const time = fakeTime();
    const { impl, asked } = fakeFetch(ALL_FOUR);
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    await monitor.poll();
    assert.equal(asked[0], "http://host:8081/health");
    assert.ok(asked.includes("http://host:8081/counters"), "the advertised metrics path is used");
    assert.ok(
      !asked.includes("http://host:8081/metrics"),
      "the guessed path is never asked once the report has corrected it",
    );
    assert.equal(monitor.snapshot.context, 262_144);
    assert.equal(monitor.snapshot.cacheHitRate, 0.55);
  });

  it("stops asking an endpoint that answered 404, and says so", async () => {
    const time = fakeTime();
    const { impl, asked } = fakeFetch({
      "http://host:8081/health": { body: json({ context: 10 }) },
      "http://host:8081/metrics": { body: json({ tokens_generated: 5 }) },
      "http://host:8081/v1/models": { body: json({ data: [] }) },
      // /cache absent entirely.
    });
    const events: string[] = [];
    const monitor = createBackendMonitor({
      urls: {
        health: "http://host:8081/health",
        metrics: "http://host:8081/metrics",
        cache: "http://host:8081/cache",
        models: "http://host:8081/v1/models",
      },
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
      onEvent: (line) => events.push(line),
    });
    await monitor.poll();
    const firstRound = asked.length;
    await monitor.poll();
    const cacheAsks = asked.filter((url) => url.endsWith("/cache")).length;
    assert.equal(cacheAsks, 1, "asked once, told it is not there, never asked again");
    assert.ok(firstRound >= 3);
    assert.ok(events.some((line) => line.includes("cache") && line.includes("not served")));
    assert.match(monitor.describe().join("\n"), /cache.*not served/);
  });

  it("keeps a source that answered on the panel even when the next attempt fails", async () => {
    const time = fakeTime();
    let fail = false;
    const impl = (async () => {
      if (fail) throw new Error("ECONNREFUSED");
      return new Response(json({ context: 4096 }), { status: 200 });
    }) as unknown as typeof fetch;
    const monitor = createBackendMonitor({
      urls: { health: "http://host:8081/health" },
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    await monitor.poll();
    assert.equal(monitor.snapshot.context, 4_096);
    fail = true;
    time.advance(20_000);
    await monitor.poll();
    assert.equal(monitor.snapshot.context, 4_096, "the last good figure stays up");
    assert.equal(monitor.snapshot.anyLive, true, "there is still a reading");
    assert.equal(freshnessOf(monitor.snapshot).state, "stale", "and it says the reading is old");
  });

  it("is unreachable rather than zero when nothing ever answered", async () => {
    const time = fakeTime();
    const impl = (async () => {
      throw new Error("ECONNREFUSED");
    }) as unknown as typeof fetch;
    const monitor = createBackendMonitor({
      urls: { health: "http://host:8081/health", metrics: "http://host:8081/metrics" },
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    await monitor.poll();
    assert.equal(monitor.snapshot.anyLive, false);
    assert.match(monitor.snapshot.error ?? "", /ECONNREFUSED/);
    const line = monitor.lines(120).join("\n");
    assert.match(line, /✕ unreachable/);
  });

  it("keeps the model list on a longer interval than the live figures", async () => {
    const time = fakeTime();
    const { impl, asked } = fakeFetch(ALL_FOUR);
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
      modelsEveryMs: 10_000,
    });
    monitor.start();
    for (let i = 0; i < 5; i += 1) {
      time.tick(1_000);
      await settle(1);
    }
    monitor.stop();
    const metricsAsks = asked.filter((u) => u.endsWith("/counters")).length;
    const modelAsks = asked.filter((u) => u.endsWith("/models")).length;
    assert.ok(metricsAsks >= 4, `metrics should be polled every interval, got ${metricsAsks}`);
    assert.ok(modelAsks <= 2, `models should be polled far less often, got ${modelAsks}`);
  });

  it("notifies every listener and survives one that throws", async () => {
    const time = fakeTime();
    const { impl } = fakeFetch(ALL_FOUR);
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    let good = 0;
    monitor.subscribe(() => {
      throw new Error("surface exploded");
    });
    const off = monitor.subscribe(() => {
      good += 1;
    });
    await monitor.poll();
    assert.equal(good, 1, "a neighbour that throws does not cost this listener its signal");
    off();
    await monitor.poll();
    assert.equal(good, 1, "and unsubscribing really stops it");
  });

  it("stops polling when told, and leaves nothing armed", async () => {
    const time = fakeTime();
    const { impl, asked } = fakeFetch(ALL_FOUR);
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
    });
    monitor.start();
    await settle(1);
    assert.ok(monitor.running);
    monitor.stop();
    const after = asked.length;
    for (let i = 0; i < 4; i += 1) {
      time.tick(2_000);
      await settle(1);
    }
    assert.equal(asked.length, after, "no request is made after stop()");
    assert.equal(time.pending(), 0, "and no timer is left behind");
    assert.equal(monitor.running, false);
  });

  it("describes what each endpoint is and what it exposed", async () => {
    const time = fakeTime();
    const { impl } = fakeFetch(ALL_FOUR);
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    await monitor.poll();
    const described = monitor.describe().join("\n");
    assert.match(described, /health\s+http:\/\/host:8081\/health\s+answered/);
    assert.match(described, /models\s+http:\/\/host:8081\/v1\/models\s+answered/);
  });

  it("the null monitor is inert in every direction", async () => {
    const monitor = createNullMonitor();
    monitor.start();
    await monitor.poll();
    assert.equal(monitor.running, false);
    assert.deepEqual(monitor.lines(120), []);
    monitor.stop();
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// F. the surfaces it is drawn on
// ══════════════════════════════════════════════════════════════════════════════

/** A monitor the test drives by hand: fixed text, manual `notify`, subscribed count. */
class FakeMonitor implements BackendMonitorLike {
  text = "MONITOR t/s 100";
  listeners = new Set<() => void>();
  started = 0;
  stopped = 0;

  lines(width: number): string[] {
    return [this.text.slice(0, Math.max(0, width))];
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  notify(): void {
    for (const listener of [...this.listeners]) listener();
  }

  get snapshot(): BackendSnapshot {
    return EMPTY_SNAPSHOT;
  }

  start(): void {
    this.started += 1;
  }

  stop(): void {
    this.stopped += 1;
  }

  async poll(): Promise<void> {}

  get running(): boolean {
    return this.started > this.stopped;
  }

  describe(): string[] {
    return ["fake"];
  }
}

interface BackendMonitorLike {
  lines(width: number, maxLines?: number, indent?: string): string[];
  subscribe(listener: () => void): () => void;
  readonly snapshot: BackendSnapshot;
  start(): void;
  stop(): void;
  poll(): Promise<void>;
  readonly running: boolean;
  describe(): string[];
}

function workHarness(options: {
  readonly monitor?: BackendMonitorLike | null;
  readonly placement?: "band" | "top";
  readonly tty?: boolean;
  readonly columns?: number;
}) {
  const term = new FakeTerminal(options.columns ?? 80, 24);
  const time = fakeTime();
  const written: string[] = [];
  const presenter = createWorkPresenter({
    terminal: term as never,
    tty: options.tty ?? true,
    now: time.now,
    schedule: time.schedule,
    coalesceMs: 33,
    heartbeatMs: 500,
    write: (chunk: string) => written.push(chunk),
    monitor: options.monitor ?? null,
    ...(options.placement === undefined ? {} : { monitorPlacement: options.placement }),
  });
  return {
    term,
    time,
    presenter,
    written: () => written.join(""),
    frame: () => presenter.capturePlain(),
  };
}

describe("monitor on the work surface", () => {
  it("rides in the fixed chrome: content, then monitor, then footer", () => {
    const monitor = new FakeMonitor();
    const h = workHarness({ monitor });
    h.presenter.acquire();
    h.presenter.say("working on it");
    h.presenter.flushSync();
    const frame = h.frame();
    const monitorAt = frame.findIndex((line) => line.includes("MONITOR"));
    const footerAt = frame.findIndex((line) => /^\s*issue\b/u.test(line));
    const contentAt = frame.findIndex((line) => line.includes("working on it"));
    assert.ok(monitorAt >= 0, "the monitor is on screen");
    assert.ok(contentAt >= 0 && contentAt < monitorAt, "under it is the transcript");
    assert.ok(monitorAt < footerAt, "and the footer is still last");
  });

  it("can be asked to sit at the top of the work surface instead", () => {
    const monitor = new FakeMonitor();
    const h = workHarness({ monitor, placement: "top" });
    h.presenter.acquire();
    h.presenter.say("working on it");
    h.presenter.flushSync();
    const frame = h.frame();
    const monitorAt = frame.findIndex((line) => line.includes("MONITOR"));
    const contentAt = frame.findIndex((line) => line.includes("working on it"));
    assert.ok(monitorAt < contentAt, "the monitor leads");
  });

  it("leaves when the surface is released, so scrollback ends with content", () => {
    const monitor = new FakeMonitor();
    const h = workHarness({ monitor });
    h.presenter.acquire();
    h.presenter.say("a line of work");
    h.presenter.flushSync();
    h.presenter.release();
    const frame = h.frame();
    assert.ok(
      !frame.some((line) => line.includes("MONITOR")),
      `no stale dashboard after release: ${JSON.stringify(frame)}`,
    );
  });

  it("takes one frame per poll, through the same coalescing window as everything else", () => {
    const monitor = new FakeMonitor();
    const h = workHarness({ monitor });
    h.presenter.acquire();
    h.presenter.flushSync();
    const before = h.presenter.stats().paints;
    monitor.notify();
    h.time.tick(40);
    assert.equal(h.presenter.stats().paints, before + 1, "a poll landing is one repaint");
    monitor.notify();
    monitor.notify();
    h.time.tick(40);
    assert.equal(h.presenter.stats().paints, before + 2, "and two polls in a window are still one frame");
  });

  it("is not printed into a piped log", () => {
    const monitor = new FakeMonitor();
    const h = workHarness({ monitor, tty: false });
    h.presenter.acquire();
    h.presenter.say("plain transcript");
    h.presenter.flushSync();
    const out = h.written();
    assert.ok(out.includes("plain transcript"), "the transcript still goes out");
    assert.ok(!out.includes("MONITOR"), "the panel does not: a log has no screen for it");
  });

  it("changes nothing on the surface when there is no monitor", () => {
    const without = workHarness({ monitor: null });
    without.presenter.acquire();
    without.presenter.say("just the transcript");
    without.presenter.flushSync();
    const frame = without.frame();
    assert.ok(!frame.some((line) => line.includes("MONITOR")));
    assert.ok(frame.some((line) => line.includes("just the transcript")));
    assert.ok(frame.some((line) => /^\s*issue\b/u.test(line)));
  });
});

describe("monitor on the idle surface", () => {
  function idleHarness(options: { readonly monitor?: BackendMonitorLike | null } = {}) {
    const term = new FakeTerminal(90, 24);
    const signals = new FakeSignals();
    const goodbyes: string[] = [];
    const handle: IdleHandle = createIdleMode({
      terminal: term as never,
      signals,
      goodbye: (line: string) => goodbyes.push(line),
      status: { ready: 2, inProgress: 0 },
      ...(options.monitor === null
        ? {}
        : { monitor: options.monitor ?? new FakeMonitor(), monitorLines: 2 }),
    });
    return { term, signals, goodbyes, handle };
  }

  it("is the top line of the idle screen", async () => {
    const monitor = new FakeMonitor();
    const h = idleHarness({ monitor });
    void h.handle.next();
    await settle(2);
    const out = h.term.output;
    assert.ok(out.includes("MONITOR"), "the panel is drawn");
    const monitorIndex = h.term.output.indexOf("MONITOR");
    const statusIndex = h.term.output.indexOf("ready");
    assert.ok(
      monitorIndex < statusIndex,
      "above the board status, which is above the prompt",
    );
    await h.handle.dispose();
  });

  it("repaints when a poll lands, and stops when the surface is gone", async () => {
    const monitor = new FakeMonitor();
    const h = idleHarness({ monitor });
    void h.handle.next();
    await settle(2);
    assert.equal(monitor.listeners.size, 1, "one listener attached while the surface lives");
    const before = h.handle.renderCount;
    monitor.notify();
    await settle(1);
    assert.ok(h.handle.renderCount > before, "a poll repaints the idle screen");
    await h.handle.dispose();
    assert.equal(monitor.listeners.size, 0, "teardown unsubscribed");
    const afterDispose = h.handle.renderCount;
    monitor.notify();
    await settle(1);
    assert.equal(h.handle.renderCount, afterDispose, "nothing repaints after teardown");
  });

  it("boots with no monitor exactly as it always did", async () => {
    const h = idleHarness({ monitor: null });
    void h.handle.next();
    await settle(2);
    assert.ok(
      stripTerminalSequences(h.term.output).includes("ready 2"),
      "the status line is unaffected",
    );
    assert.ok(!h.term.output.includes("MONITOR"));
    await h.handle.dispose();
  });
});

describe("monitor primitives", () => {
  it("names the four endpoints it knows, in the order it asks about them", () => {
    assert.deepEqual([...MONITOR_ENDPOINTS], ["health", "metrics", "cache", "models"]);
  });

  it("treats a fresh endpoint as no evidence at all", () => {
    const blank = emptyEndpoint("cache");
    assert.equal(blank.hasBody, false);
    assert.equal(blank.state, "unknown");
    const { snapshot } = summarise({ endpoints: [blank], now: 1_000 });
    assert.equal(snapshot.anyLive, false);
    assert.equal(snapshot.cacheHitRate, undefined);
  });

  it("packs segments a line at a time and reports what overflowed", () => {
    const segments = [
      { label: "a", text: "1", role: "text" as const },
      { label: "b", text: "2", role: "text" as const },
      { label: "c", text: "3", role: "text" as const },
    ];
    assert.deepEqual(packSegments(segments, null, 200, 2), ["a 1 · b 2 · c 3"]);
    const squeezed = packSegments(segments, null, 12, 1);
    assert.equal(squeezed.length, 1);
    assert.match(squeezed[0] ?? "", /\+1 more$/);
  });

  it("renders the plain theme as no colour at all", () => {
    assert.equal(PLAIN_MONITOR_THEME.color("success", "x"), "x");
    assert.equal(PLAIN_MONITOR_THEME.color("error", ""), "");
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// G. the composition root
// ══════════════════════════════════════════════════════════════════════════════

function emptyBoard() {
  return { listReady: async () => [], listInProgress: async () => [] } as never;
}

function modelsFixtureFile(contents: unknown): { path: string; cleanup: () => void } {
  const dir = mkdtempSync(join(tmpdir(), "loop-monitor-"));
  const path = join(dir, "models.json");
  writeFileSync(path, JSON.stringify(contents, null, 2));
  return { path, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

describe("monitor at the composition root", () => {
  it("finds the server through models.json and runs between start and stop", async () => {
    const asked: string[] = [];
    const fetchImpl = (async (url: unknown) => {
      asked.push(String(url));
      return new Response(json({ context: 4096, slots: 2 }), { status: 200 });
    }) as unknown as typeof fetch;
    const fixture = modelsFixtureFile({
      providers: {
        llamacpp: {
          baseUrl: "http://127.0.0.1:8199/v1",
          models: [{ id: "m1", contextWindow: 4096, maxTokens: 512 }],
        },
      },
    });
    try {
      const app = buildApp({
        cwd: "/repo",
        modelRef: { provider: "llamacpp", id: "m1" },
        providerAudit: { enabled: false },
        monitor: { enabled: true, intervalMs: 10_000 },
        maxIterations: 0,
        overrides: {
          modelsPath: fixture.path,
          beads: emptyBoard(),
          git: undefined as never,
          presenter: createNullPresenter(),
          audit: { fetchImpl },
        },
      });
      await app.run().catch(() => undefined);
      await settle(1);
      assert.ok(
        asked.includes("http://127.0.0.1:8199/health"),
        `the monitor asked the provider's own health endpoint; asked: ${asked.join(", ")}`,
      );
      assert.equal(app.monitor.snapshot.context, 4_096);
      assert.equal(app.monitor.running, false, "and it is stopped when the run is over");
    } finally {
      fixture.cleanup();
    }
  });

  it("is switched off by configuration rather than by a missing server", async () => {
    const asked: string[] = [];
    const fetchImpl = (async (url: unknown) => {
      asked.push(String(url));
      return new Response("{}", { status: 200 });
    }) as unknown as typeof fetch;
    const fixture = modelsFixtureFile({
      providers: { p: { baseUrl: "http://127.0.0.1:1/v1", models: [{ id: "m1" }] } },
    });
    try {
      const app = buildApp({
        cwd: "/repo",
        modelRef: { provider: "p", id: "m1" },
        monitor: { enabled: false },
        maxIterations: 0,
        overrides: {
          modelsPath: fixture.path,
          beads: emptyBoard(),
          git: undefined as never,
          presenter: createNullPresenter(),
          audit: { fetchImpl },
        },
      });
      await app.run().catch(() => undefined);
      assert.deepEqual(asked, [], "an off monitor asks for nothing at all");
      assert.deepEqual(app.monitor.lines(120), []);
    } finally {
      fixture.cleanup();
    }
  });

  it("is a no-op when nothing could name the server", () => {
    const app = buildApp({
      cwd: "/repo",
      providerAudit: { enabled: false },
      maxIterations: 0,
      overrides: {
        modelsPath: "/nonexistent/models.json",
        beads: emptyBoard(),
        git: undefined as never,
        presenter: createNullPresenter(),
        audit: {
          fetchImpl: (async () => {
            throw new Error("should not be reached");
          }) as unknown as typeof fetch,
        },
      },
    });
    assert.deepEqual(app.monitor.lines(120), []);
  });

  it("reads the monitor's own url override ahead of the provider's base", async () => {
    const asked: string[] = [];
    const fetchImpl = (async (url: unknown) => {
      asked.push(String(url));
      return new Response(json({ context: 7 }), { status: 200 });
    }) as unknown as typeof fetch;
    const fixture = modelsFixtureFile({
      providers: { p: { baseUrl: "http://elsewhere.test/v1", models: [{ id: "m1" }] } },
    });
    try {
      const app = buildApp({
        cwd: "/repo",
        modelRef: { provider: "p", id: "m1" },
        providerAudit: { enabled: false },
        monitor: { enabled: true, url: "http://127.0.0.1:8199/v1", intervalMs: 10_000 },
        maxIterations: 0,
        overrides: {
          modelsPath: fixture.path,
          beads: emptyBoard(),
          git: undefined as never,
          presenter: createNullPresenter(),
          audit: { fetchImpl },
        },
      });
      await app.run().catch(() => undefined);
      await settle(1);
      assert.ok(!asked.some((url) => url.includes("elsewhere.test")));
      assert.ok(asked.includes("http://127.0.0.1:8199/health"));
    } finally {
      fixture.cleanup();
    }
  });
});

describe("monitor describe()", () => {
  it("names the keys an endpoint exposed that nothing reads", async () => {
    const time = fakeTime();
    const { impl } = fakeFetch({
      "http://host:8081/health": {
        body: json({
          context: 4_096,
          slots: 2,
          some_brand_new_metric: 12,
          nested: { untouched: true },
        }),
      },
    });
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
      verbose: true,
    });
    await monitor.poll();
    const described = monitor.describe().join("\n");
    assert.match(described, /4 key\(s\) exposed/);
    assert.match(
      described,
      /not shown, no field reads them: some_brand_new_metric, nested\.untouched/,
    );
  });

  it("says so plainly when everything exposed is read", async () => {
    const time = fakeTime();
    const { impl } = fakeFetch({
      "http://host:8081/health": { body: json({ context: 4_096, queued: 0 }) },
    });
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
      verbose: true,
    });
    await monitor.poll();
    assert.match(monitor.describe().join("\n"), /2 key\(s\) exposed, every one of them read/);
  });

  it("keeps the quiet describe() short when nobody asked for detail", async () => {
    const time = fakeTime();
    const { impl } = fakeFetch({
      "http://host:8081/health": { body: json({ context: 1, brand_new: 2 }) },
    });
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    await monitor.poll();
    const described = monitor.describe().join("\n");
    assert.ok(!described.includes("exposed"), "detail only appears when it was asked for");
    assert.match(described, /health\s+http:\/\/host:8081\/health\s+answered/);
  });
});

describe("describe()'s unread report", () => {
  it("does not list a model id the panel is showing", async () => {
    const time = fakeTime();
    const { impl } = fakeFetch({
      "http://host:8081/v1/models": {
        body: json({ object: "list", data: [{ id: "shown-model", object: "model" }] }),
      },
      "http://host:8081/health": { body: json({ endpoints: ["/v1/models"] }) },
    });
    const monitor = createBackendMonitor({
      urls: urlsForBase("http://host:8081/v1"),
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
      verbose: true,
    });
    await monitor.poll();
    const described = monitor.describe().join("\n");
    const modelsLine = described.split("\n").find((line) => line.includes("not shown") && true) ?? "";
    assert.ok(!modelsLine.includes("data.0.id"), `the read id is not reported: ${modelsLine}`);
    assert.ok(pathIsRead("kv_pools.0.capacity"), "pool entry fields count as read");
    assert.ok(pathIsRead("data.0.id"), "model entry ids count as read");
    assert.ok(!pathIsRead("data.0.object"), "unread entry fields still do not");
    assert.ok(!pathIsRead("some.deep.path"), "and unrelated paths stay unread");
  });
});

describe("cycle joining", () => {
  it("gives a second caller the cycle already running instead of a second one", async () => {
    const time = fakeTime();
    let asked = 0;
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const impl = (async () => {
      asked += 1;
      await gate;
      return new Response(json({ context: 42 }), { status: 200 });
    }) as unknown as typeof fetch;
    const monitor = createBackendMonitor({
      urls: { health: "http://host:8081/health" },
      fetchImpl: impl,
      now: time.now,
      schedule: time.schedule,
    });
    const first = monitor.poll();
    const second = monitor.poll();
    await Promise.resolve();
    release();
    await Promise.all([first, second]);
    assert.equal(asked, 1, "one request serves both callers");
    assert.equal(monitor.snapshot.context, 42);
    // And the next cycle is still available: joining is not a one-shot.
    time.advance(5_000);
    await monitor.poll();
    assert.equal(asked, 2, "the joined cycle did not consume the next one");
  });
});
