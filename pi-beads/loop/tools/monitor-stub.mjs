#!/usr/bin/env node
/**
 * A stub of the inference backend's four diagnostic endpoints, for exercising
 * `src/monitor.ts` without a GPU box.
 *
 *   node tools/monitor-stub.mjs
 *
 * Serves /health (the repo's own fixture), /metrics (Prometheus text that
 * moves), /cache (JSON that moves) and /v1/models, then drives a monitor
 * against it and prints the panel — themed, plain, and at 60 columns — plus
 * what `describe()` says about each endpoint.
 *
 * Not a test. It is what the "live check" paragraph of
 * docs/ADR-003-backend-monitor.md was run from.
 */
import http from "node:http";
import { readFileSync } from "node:fs";
import { createBackendMonitor, urlsForBase } from "../src/monitor.ts";
import { initTheme, rawKeyHint } from "@earendil-works/pi-coding-agent";
import { createPresenterTheme } from "../src/render.ts";

const health = readFileSync("test/fixtures/health.json", "utf8");
let served = 412;
let tokens = 980_000;
let hits = 1900, misses = 240;
const server = http.createServer((req, res) => {
  const url = req.url || "";
  if (url === "/health") { res.writeHead(200, { "content-type": "application/json" }); return res.end(health); }
  if (url === "/metrics") {
    served += 1; tokens += 1370; hits += 9; misses += 1;
    res.writeHead(200, { "content-type": "text/plain; version=0.0.4" });
    return res.end([
      "# HELP tokens_generated Tokens decoded",
      "# TYPE tokens_generated counter",
      `tokens_generated ${tokens}`,
      `requests_served ${served}`,
      `prompt_cache_hits ${hits}`,
      `prompt_cache_misses ${misses}`,
      `kv_used ${Math.round(160_000 + Math.random() * 40_000)}`,
      `queued 0`,
    ].join("\n"));
  }
  if (url === "/cache") {
    res.writeHead(200, { "content-type": "application/json" });
    return res.end(JSON.stringify({ hits, misses, hit_rate: hits / (hits + misses), entries: 612, cap_mb: 110 }));
  }
  if (url === "/v1/models") {
    res.writeHead(200, { "content-type": "application/json" });
    return res.end(JSON.stringify({ object: "list", data: [{ id: "halogen-qwen3.8-flash-next", object: "model" }] }));
  }
  res.writeHead(404, { "content-type": "text/plain" });
  res.end("nope");
});
await new Promise((r) => server.listen(8199, r));

initTheme("dark", false);
const theme = createPresenterTheme();
const monitor = createBackendMonitor({
  urls: urlsForBase("http://127.0.0.1:8199/v1"),
  theme,
  intervalMs: 400,
  verbose: true,
});
monitor.start();
await new Promise((r) => setTimeout(r, 300));
console.log("\n=== panel (2 lines, 118 cols) ===");
console.log(monitor.lines(118).join("\n"));
await new Promise((r) => setTimeout(r, 900));
console.log("\n=== panel later ===");
console.log(monitor.lines(118).join("\n"));
console.log("\n=== plain (stripped) ===");
import("@earendil-works/pi-tui").then(({ stripTerminalSequences }) => {
  console.log(monitor.lines(118).map((l) => stripTerminalSequences(l)).join("\n"));
  console.log("\n=== narrow (60 cols) ===");
  console.log(monitor.lines(60).map((l) => stripTerminalSequences(l)).join("\n"));
  console.log("\n=== describe ===");
  console.log(monitor.describe().join("\n"));
  monitor.stop();
  server.close();
});
