/**
 * Tests for `src/ntfy.ts` — the publish transport.
 *
 * Three layers, each tested at the level where it can actually be wrong:
 *
 * - **Pure logic** — target resolution, header building, byte truncation,
 *   response classification. Cheap, deterministic, no sockets.
 * - **The transport against a real HTTP server on loopback**
 *   (`test/ntfy-server.ts`). The status codes, the timeouts and the shapes of
 *   the requests are the transport's promises, and a fake that only echoes what
 *   we already believe would prove nothing.
 * - **The failure paths**, because every one of them has to come back as a
 *   delivery rather than a throw: a refused topic, a dropped connection, a
 *   server that never answers.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  NTFY_MAX_MESSAGE_BYTES,
  NTFY_MAX_TITLE_LENGTH,
  classifyResponse,
  createHttpTransport,
  createNtfyPublisher,
  createNullPublisher,
  localHostname,
  ntfyHeaders,
  resolveNtfyTarget,
  truncateToBytes,
  type NtfyMessage,
  type NtfyRequestOptions,
  type NtfyTransport,
} from "../src/ntfy.ts";
import { startFakeNtfy } from "./ntfy-server.ts";

// ── the target: where a notice goes ─────────────────────────────────────────

test("a bare topic is joined onto the server, whose default is the hosted one", () => {
  const target = resolveNtfyTarget({ topic: "my-loop" });
  assert.equal(target.base, "https://ntfy.sh");
  assert.equal(target.topic, "my-loop");
  assert.equal(target.destination, "https://ntfy.sh/my-loop");
});

test("a self-hosted server is one variable, not a different configuration", () => {
  const target = resolveNtfyTarget({ url: "http://192.168.1.20:8080", topic: "loop" });
  assert.equal(target.destination, "http://192.168.1.20:8080/loop");
  assert.equal(target.base, "http://192.168.1.20:8080");
});

test("a reverse-proxied base keeps its path prefix", () => {
  const target = resolveNtfyTarget({ url: "https://example.test/ntfy/", topic: "loop" });
  assert.equal(
    target.destination,
    "https://example.test/ntfy/loop",
    "nginx-served ntfy lives under a prefix, and the topic belongs under it too",
  );
});

test("a full URL as the topic is taken as given, the way the web UI hands it over", () => {
  const target = resolveNtfyTarget({ topic: "https://ntfy.sh/abc123-notify" });
  assert.equal(target.destination, "https://ntfy.sh/abc123-notify");
  assert.equal(target.topic, "abc123-notify");
});

test("unusable targets are refused at build time, each with its own reason", () => {
  const cases: Array<{ setting: { url?: string; topic?: string }; why: RegExp }> = [
    { setting: { topic: "" }, why: /no ntfy topic/u },
    { setting: { topic: "http://host.example" }, why: /names no topic/u },
    { setting: { topic: "not a url at all", url: "ftp://host" }, why: /ftp/u },
    { setting: { topic: "loop", url: "not-a-url" }, why: /not a valid URL|scheme/u },
    {
      setting: { topic: "loop", url: "http://user:pass@host" },
      why: /credentials|access token/u,
    },
  ];
  for (const { setting, why } of cases) {
    assert.throws(
      () => resolveNtfyTarget(setting),
      why,
      `expected a refusal matching ${why} for ${JSON.stringify(setting)}`,
    );
  }
});

// ── headers ─────────────────────────────────────────────────────────────────

test("the ntfy headers are built from the message, and unset fields stay unset", () => {
  const headers = ntfyHeaders({
    title: "[pi-beads] tst.42 completed: the parser change",
    body: "body",
    priority: "high",
    tags: ["+1", "beads"],
    click: "https://example.test/bead/42",
  });
  assert.equal(headers.Title, "[pi-beads] tst.42 completed: the parser change");
  assert.equal(headers.Priority, "high");
  assert.equal(headers.Tags, "+1,beads");
  assert.equal(headers.Click, "https://example.test/bead/42");
  assert.equal(headers.Authorization, undefined, "no token configured means no Authorization header");
  assert.equal(headers["Content-Type"], "text/plain; charset=utf-8");
});

test("a header value cannot smuggle a second header in with a line break", () => {
  const headers = ntfyHeaders({
    title: "one\r\nX-Evil: yes\ntwo",
    body: "b",
    priority: "3\nX-More: nope",
    tags: ["ok\r\nInjected: true"],
  });
  assert.equal(headers.Title, "one X-Evil: yes two");
  assert.equal(headers.Priority, "3 X-More: nope");
  assert.equal(headers.Tags, "ok Injected: true");
  for (const value of Object.values(headers)) {
    assert.ok(!/[\r\n]/u.test(value), `header value kept a line break: ${JSON.stringify(value)}`);
  }
});

test("the token lands in the header and nowhere else", async () => {
  const seen: NtfyRequestOptions[] = [];
  const transport: NtfyTransport = {
    name: "recorder",
    async publish(_url: string, options: NtfyRequestOptions) {
      seen.push(options);
      return { statusCode: 200, body: '{"id":"x"}' };
    },
  };
  const publisher = createNtfyPublisher({
    target: resolveNtfyTarget({ url: "http://localhost:9999", topic: "t" }),
    transport,
    token: "super-secret-token",
  });
  const delivery = await publisher.publish({ title: "t", body: "b" });
  assert.equal(delivery.kind, "delivered");
  assert.equal(seen[0]?.headers.Authorization, "Bearer super-secret-token");
  const body = seen[0]?.body ?? "";
  assert.ok(!body.includes("super-secret-token"), "the secret stays out of the message body");
});

// ── the limits ──────────────────────────────────────────────────────────────

test("a body over the byte limit is cut on a character boundary and marked", () => {
  const text = "Ünïcode — ".repeat(200); // multi-byte on purpose
  const cut = truncateToBytes(text, 500);
  assert.ok(Buffer.byteLength(cut, "utf8") <= 500, "stays under the byte limit");
  assert.match(cut, /… \(truncated\)$/u, "and says so, rather than silently arriving short");
  assert.ok(!cut.includes("\uFFFD"), "no replacement character: the cut was on a boundary");
  assert.ok(cut.startsWith("Ünïcode"), "kept the beginning");
});

test("a body that already fits is returned untouched", () => {
  assert.equal(truncateToBytes("short notice", 4096), "short notice");
});

test("the default limits are ntfy's documented ones", () => {
  assert.equal(NTFY_MAX_MESSAGE_BYTES, 4096);
  assert.equal(NTFY_MAX_TITLE_LENGTH, 200);
});

test("a title too long for a phone is clipped without losing the leading id", async () => {
  const publisher = createNtfyPublisher({
    target: resolveNtfyTarget({ url: "http://localhost:9", topic: "t" }),
    transport: stubTransport(),
    maxTitleLength: 40,
  });
  // Same shape the notifier produces: `[prefix] tst.42 completed: …`
  await publisher.publish({ title: `[p] tst.42 completed: ${"x".repeat(300)}`, body: "b" });
  const sent = lastSent();
  assert.ok(sent.title.length <= 40, `title was ${sent.title.length} chars`);
  assert.ok(sent.title.startsWith("[p] tst.42"), "the searchable part survives the clip");
});

// A transport that records the last message it was handed.
let lastMessage: NtfyMessage = { title: "", body: "" };
function lastSent(): NtfyMessage {
  return lastMessage;
}
function stubTransport(): NtfyTransport {
  return {
    name: "stub",
    async publish(_url: string, options: NtfyRequestOptions) {
      lastMessage = { title: options.headers.Title ?? "", body: options.body };
      return { statusCode: 200, body: '{"id":"stub"}' };
    },
  };
}

// ── response classification ─────────────────────────────────────────────────

test("only 4xx-but-not-429 is a clean no", () => {
  assert.deepEqual(classifyResponse(200), { ok: true, retryable: false });
  assert.deepEqual(classifyResponse(204), { ok: true, retryable: false });
  assert.deepEqual(classifyResponse(429), { ok: false, retryable: true }, "rate limit: try later");
  assert.deepEqual(classifyResponse(400), { ok: false, retryable: false });
  assert.deepEqual(classifyResponse(403), { ok: false, retryable: false }, "wrong topic or token");
  assert.deepEqual(classifyResponse(404), { ok: false, retryable: false });
  assert.deepEqual(classifyResponse(500), { ok: false, retryable: true });
  assert.deepEqual(classifyResponse(503), { ok: false, retryable: true });
});

// ── the real transport, against a real server ───────────────────────────────

test("a good publish: POST to the topic, JSON ack read back", async () => {
  const server = await startFakeNtfy();
  try {
    const publisher = createNtfyPublisher({
      target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
      transport: createHttpTransport(),
    });
    const delivery = await publisher.publish({
      title: "[pi-beads] tst.7 completed: fixed it",
      body: "tst.7 — fixed it\nthe details",
      priority: "3",
    });
    assert.equal(delivery.kind, "delivered");
    assert.equal(
      delivery.kind === "delivered" ? delivery.messageId : null,
      "abc123XYZ",
      "the message id is read out of the ack, not invented",
    );
    assert.equal(server.requests.length, 1);
    assert.equal(server.requests[0]?.method, "POST");
    assert.equal(server.requests[0]?.path, "/loop");
    assert.equal(server.requests[0]?.headers.title, "[pi-beads] tst.7 completed: fixed it");
  } finally {
    await server.close();
  }
});

test("a 413 for an oversized message is reported, not retried into a hole", async () => {
  const server = await startFakeNtfy({ status: 413, responseBody: '{"message":"too large"}' });
  try {
    const publisher = createNtfyPublisher({
      target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
      transport: createHttpTransport(),
    });
    const delivery = await publisher.publish({ title: "big", body: "x".repeat(100) });
    assert.equal(delivery.kind, "failed");
    assert.equal(delivery.kind === "failed" ? delivery.retryable : true, false);
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /413/u);
  } finally {
    await server.close();
  }
});

test("a refused topic (403) is a permanent failure; a rate limit (429) is not", async () => {
  for (const [status, retryable] of [
    [403, false],
    [404, false],
    [429, true],
    [500, true],
  ] as const) {
    const server = await startFakeNtfy({ status });
    try {
      const publisher = createNtfyPublisher({
        target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
        transport: createHttpTransport(),
      });
      const delivery = await publisher.publish({ title: "t", body: "b" });
      assert.equal(delivery.kind, "failed", `status ${status} must not report success`);
      assert.equal(
        delivery.kind === "failed" ? delivery.retryable : !retryable,
        retryable,
        `status ${status} retryable`,
      );
    } finally {
      await server.close();
    }
  }
});

test("a server that never answers costs a timeout, and the timeout is retryable", async () => {
  const server = await startFakeNtfy({ hang: true });
  try {
    const publisher = createNtfyPublisher({
      target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
      transport: createHttpTransport(),
      timeoutMs: 120,
    });
    const started = Date.now();
    const delivery = await publisher.publish({ title: "t", body: "b" });
    assert.ok(Date.now() - started >= 100, "the deadline actually waited");
    assert.equal(delivery.kind, "failed");
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /timeout/u);
    assert.equal(delivery.kind === "failed" ? delivery.retryable : false, true);
  } finally {
    await server.close();
  }
});

test("a connection with nothing behind it is a delivery, not an exception", async () => {
  const publisher = createNtfyPublisher({
    // Port 1: nothing listens there on loopback.
    target: resolveNtfyTarget({ url: "http://127.0.0.1:1", topic: "loop" }),
    transport: createHttpTransport(),
    timeoutMs: 500,
  });
  const delivery = await publisher.publish({ title: "t", body: "b" });
  assert.equal(delivery.kind, "failed", "the caller must be told, not surprised");
  assert.equal(delivery.kind === "failed" ? delivery.retryable : false, true);
});

test("the token is never in a logged line", async () => {
  const logged: string[] = [];
  const publisher = createNtfyPublisher({
    target: resolveNtfyTarget({ url: "http://127.0.0.1:1", topic: "loop" }),
    transport: createHttpTransport(),
    token: "hunter2-do-not-log",
    timeoutMs: 300,
    logger: (line) => logged.push(line),
  });
  await publisher.publish({ title: "t", body: "b" });
  assert.ok(logged.length > 0, "the failure was logged");
  for (const line of logged) {
    assert.ok(
      !line.includes("hunter2-do-not-log"),
      `the token leaked into a log line: ${line}`,
    );
  }
});

test("a null publisher is honest about not publishing", async () => {
  const publisher = createNullPublisher("no topic is configured");
  assert.equal(publisher.enabled, false);
  const delivery = await publisher.publish({ title: "t", body: "b" });
  assert.deepEqual(delivery, { kind: "skipped", reason: "no topic is configured" });
});

test("an empty body is skipped rather than published as a blank notice", async () => {
  const publisher = createNtfyPublisher({
    target: resolveNtfyTarget({ url: "http://localhost:9", topic: "t" }),
    transport: stubTransport(),
  });
  const delivery = await publisher.publish({ title: "t", body: "   \n  " });
  assert.equal(delivery.kind, "skipped");
});

test("the local hostname has a fallback, because an empty host in a notice looks like a bug", () => {
  const name = localHostname();
  assert.ok(typeof name === "string" && name.length > 0, `hostname was: ${JSON.stringify(name)}`);
});
