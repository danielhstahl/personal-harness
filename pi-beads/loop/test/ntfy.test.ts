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
  NTFY_MAX_JSON_BODY_BYTES,
  NTFY_MAX_MESSAGE_BYTES,
  NTFY_MAX_TITLE_LENGTH,
  NTFY_MESSAGE_LIMITS,
  NTFY_MIN_MESSAGE_BYTES,
  NTFY_TOPIC_PATTERN,
  classifyResponse,
  createHttpTransport,
  createNtfyPublisher,
  createNullPublisher,
  fitJsonBytes,
  isHeaderSafe,
  isMessageTooLargeRefusal,
  isNtfyMessageBytes,
  localHostname,
  ntfyMessageFromNotice,
  ntfyPriorityNumber,
  ntfyPublishBody,
  ntfyPublishPayload,
  resolveNtfyTarget,
  shrinkMessageLimit,
  transportHeaders,
  truncateToBytes,
  type NtfyDelivery,
  type NtfyRequestOptions,
  type NtfyTransport,
} from "../src/ntfy.ts";
import { publishedJson, startFakeNtfy } from "./ntfy-server.ts";
import type { Delivery, Notice } from "../src/notify.ts";

// ── the target: where a notice goes ─────────────────────────────────────────

test("a bare topic is joined onto the server, whose default is the hosted one", () => {
  const target = resolveNtfyTarget({ topic: "my-loop" });
  assert.equal(target.base, "https://ntfy.sh");
  assert.equal(target.topic, "my-loop");
  assert.equal(target.destination, "https://ntfy.sh/my-loop");
  assert.equal(
    target.publishUrl,
    "https://ntfy.sh/",
    "the request goes to the root; the topic travels in the body",
  );
});

test("a self-hosted server is one variable, not a different configuration", () => {
  const target = resolveNtfyTarget({ url: "http://192.168.1.20:8080", topic: "loop" });
  assert.equal(target.destination, "http://192.168.1.20:8080/loop");
  assert.equal(target.base, "http://192.168.1.20:8080");
  assert.equal(target.publishUrl, "http://192.168.1.20:8080/");
});

test("a reverse-proxied base keeps its path prefix", () => {
  const target = resolveNtfyTarget({ url: "https://example.test/ntfy/", topic: "loop" });
  assert.equal(
    target.destination,
    "https://example.test/ntfy/loop",
    "nginx-served ntfy lives under a prefix, and the topic belongs under it too",
  );
  assert.equal(
    target.publishUrl,
    "https://example.test/ntfy/",
    "and the JSON publish is POSTed at the prefix, which is the root of the ntfy behind it",
  );
});

test("a full URL as the topic is taken as given, the way the web UI hands it over", () => {
  const target = resolveNtfyTarget({ topic: "https://ntfy.sh/abc123-notify" });
  assert.equal(target.destination, "https://ntfy.sh/abc123-notify");
  assert.equal(target.topic, "abc123-notify");
  assert.equal(target.publishUrl, "https://ntfy.sh/");
});

test("a full URL with a proxy prefix keeps the prefix and takes the last segment as the topic", () => {
  const target = resolveNtfyTarget({ topic: "https://example.test/ntfy/loop" });
  assert.equal(target.topic, "loop");
  assert.equal(target.publishUrl, "https://example.test/ntfy/");
  assert.equal(target.destination, "https://example.test/ntfy/loop");
});

test("a topic ntfy would reject is refused here instead, with the rule in the message", () => {
  for (const topic of ["has space", "sl/ash", "über", "", "a".repeat(65)]) {
    if (topic === "") continue; // handled by the empty-topic case below
    assert.throws(
      () => resolveNtfyTarget({ topic, url: "http://localhost:9" }),
      /not a valid ntfy topic name/u,
      `"${topic}" should not have made it to the wire`,
    );
  }
  assert.ok(NTFY_TOPIC_PATTERN.test("loop-notices-1_2"), "the shape this repo uses is legal");
  assert.ok(
    !NTFY_TOPIC_PATTERN.test("loop.notices"),
    "and a dot is not legal — ntfy's own topicRegex says so, and this one mirrors it",
  );
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

// ── the publish envelope: the notice, as ntfy reads it ────────────────────────

test("the whole notice travels as JSON, and unset fields stay unset", () => {
  const payload = ntfyPublishPayload(
    { title: "[pi-beads] tst.42 completed: the parser change", body: "body text" },
    "loop",
  );
  assert.deepEqual(payload, {
    topic: "loop",
    title: "[pi-beads] tst.42 completed: the parser change",
    message: "body text",
  });
});

test("the body is a publish document: topic, message, and the presentation as fields", () => {
  const wire = ntfyPublishBody(
    {
      title: "[pi-beads] tst.42 completed: the parser change",
      body: "line one\nline two",
      priority: "high",
      tags: ["+1", "beads"],
      click: "https://example.test/bead/42",
    },
    "loop",
  );
  const parsed = JSON.parse(wire) as Record<string, unknown>;
  assert.equal(parsed.topic, "loop", "ntfy takes the topic out of the body, not the URL");
  assert.equal(parsed.message, "line one\nline two", "the body is the message, newlines and all");
  assert.equal(
    parsed.priority,
    4,
    "the name the header API took has to become the number the JSON field is typed as",
  );
  assert.deepEqual(parsed.tags, ["+1", "beads"], "a list, not the comma-joined header form");
  assert.equal(parsed.click, "https://example.test/bead/42");
});

test("every priority spelling becomes an integer, because ntfy's field is an int", () => {
  assert.equal(ntfyPriorityNumber("min"), 1);
  assert.equal(ntfyPriorityNumber("low"), 2);
  assert.equal(ntfyPriorityNumber("default"), 3);
  assert.equal(ntfyPriorityNumber("high"), 4);
  assert.equal(ntfyPriorityNumber("URGENT"), 5);
  assert.equal(ntfyPriorityNumber(" 3 "), 3);
  assert.throws(() => ntfyPriorityNumber("urgentish"), /not an ntfy priority/u);
  assert.throws(() => ntfyPriorityNumber("9"), /not an ntfy priority/u);
});

// ── the seam: a `Notice` becomes an ntfy publish ───────────────────────────

/**
 * The claim `NtfyPublisher extends NoticePublisher` is checked against: the
 * publisher is handed a plain notice and nothing else, and everything ntfy is
 * expected to make of it.
 */
test("a notice with hints becomes ntfy's own message shape", () => {
  const notice: Notice = {
    title: "[pi-beads] tst.42 completed: the parser change",
    body: "line one\nline two",
    hints: { priority: "high", tags: ["+1", "beads"], click: "https://example.test/bead/42" },
  };
  assert.deepEqual(ntfyMessageFromNotice(notice), {
    title: "[pi-beads] tst.42 completed: the parser change",
    body: "line one\nline two",
    priority: "high",
    tags: ["+1", "beads"],
    click: "https://example.test/bead/42",
  });
});

test("a notice with no hints is just a title and a body — no invented ntfy fields", () => {
  assert.deepEqual(
    ntfyMessageFromNotice({ title: "t", body: "b" }),
    { title: "t", body: "b" },
    "absent hints must not become empty ones, or every publish carries a pointless field",
  );
  assert.deepEqual(
    ntfyMessageFromNotice({ title: "t", body: "b", hints: { priority: "  " } }),
    { title: "t", body: "b" },
    "a blank hint is unset, not a value",
  );
});

test("a tags hint written the way an environment variable arrives is split here", () => {
  // `LOOP_NTFY_TAGS="+1 beads"` reaches a hint as one string. The transport
  // that knows tags is a list is the transport that splits it.
  assert.deepEqual(ntfyMessageFromNotice({ title: "t", body: "b", hints: { tags: "+1 beads" } }).tags, [
    "+1",
    "beads",
  ]);
});

test("a hint this transport does not read is reported, not swallowed", async () => {
  const logged: string[] = [];
  const transport: NtfyTransport = {
    name: "recorder",
    async publish(): Promise<{ statusCode: number; body: string }> {
      return { statusCode: 200, body: '{"id":"x"}' };
    },
  };
  const publisher = createNtfyPublisher({
    target: resolveNtfyTarget({ url: "http://localhost:9", topic: "t" }),
    transport,
    logger: (line) => logged.push(line),
  });
  await publisher.publish({ title: "a", body: "b", hints: { pirority: "high" } });
  await publisher.publish({ title: "c", body: "d", hints: { pirority: "high" } });

  const complaints = logged.filter((line) => line.includes("pirority"));
  assert.equal(complaints.length, 1, `one typo, one complaint: ${JSON.stringify(logged)}`);
  assert.match(complaints[0] ?? "", /ignored the notice hint/u);
  assert.match(complaints[0] ?? "", /priority, tags, click/u, "and it says what it does read");
  assert.equal((await publisher.publish({ title: "e", body: "f" })).kind, "delivered");
});

test("the message limits the config layer checks against are this transport's own", () => {
  // `src/app.ts` validates `LOOP_NTFY_MAX_MESSAGE_BYTES` with these numbers;
  // if they ever stopped being the numbers the publisher uses, a config that
  // passes the check would not be a config that works.
  assert.equal(NTFY_MESSAGE_LIMITS.minBytes, NTFY_MIN_MESSAGE_BYTES);
  assert.equal(NTFY_MESSAGE_LIMITS.maxBytes, NTFY_MAX_MESSAGE_BYTES);
  assert.equal(NTFY_MESSAGE_LIMITS.defaultBytes, NTFY_MAX_MESSAGE_BYTES);
  assert.equal(isNtfyMessageBytes(NTFY_MIN_MESSAGE_BYTES), true);
  assert.equal(isNtfyMessageBytes(NTFY_MAX_MESSAGE_BYTES), true);
  assert.equal(isNtfyMessageBytes(NTFY_MIN_MESSAGE_BYTES - 1), false, "the floor is the floor");
  assert.equal(isNtfyMessageBytes(NTFY_MAX_MESSAGE_BYTES + 1), false);
  assert.equal(isNtfyMessageBytes(2048.5), false, "half a byte is not a byte count");
  assert.equal(isNtfyMessageBytes("2048"), false, "and neither is one written as a string");
});

test("the delivery this transport reports IS the notice layer's Delivery, not a cousin", () => {
  // Compile-time claim: `NtfyDelivery` is an alias of `Delivery`, so the loop's
  // switch on `kind` covers everything a transport can answer with.
  const fromNtfy: NtfyDelivery = {
    kind: "failed",
    reason: "nope",
    destination: "http://x/y",
    retryable: true,
  };
  const asNoticeLayer: Delivery = fromNtfy;
  assert.equal(asNoticeLayer.kind, "failed");
});

test("nothing the work wrote goes in a header — the header set is three constants and a token", () => {
  const headers = transportHeaders({ token: "s3cret" });
  assert.deepEqual(
    Object.keys(headers).sort(),
    ["Authorization", "Content-Type", "User-Agent"],
    "Title/Priority/Tags/Click are fields in the body now, not headers",
  );
  assert.equal(headers["Content-Type"], "application/json");
  assert.equal(transportHeaders().Authorization, undefined, "no token configured means no header");
  assert.ok(isHeaderSafe("pi-beads-loop"), "the defaults are header-safe");
  assert.ok(!isHeaderSafe("bell\u0007"), "control characters are not");
  assert.ok(!isHeaderSafe("— em dash —"), "and neither is anything outside ASCII");
});

test(
  "REGRESSION: a title outside Latin-1 publishes instead of dying at the header layer",
  async () => {
    // What this is a test for: the title is an agent-written sentence, and
    // Node refuses a character a header cannot carry with
    // `Invalid character in header content ["Title"]`. Under the header-only
    // wire format that meant a bead whose summary happened to contain an em
    // dash, a curly quote, a tick or any CJK could not be announced, while a
    // plain-ASCII summary of the same shape announced fine — a failure in the
    // *content*, which is the worst place to put one.
    const titles = [
      "[pi-beads] tst.42 completed: rewrote the parser — faster, mostly",
      "[pi-beads] tst.42 completed: ✓ all green",
      "[pi-beads] tst.42 completed: 日本語のタイトル",
      "[pi-beads] tst.42 completed: shipped it \u{1F389}",
      "[pi-beads] tst.42 completed: a \u2018curly quoted\u2019 summary",
    ];
    const server = await startFakeNtfy();
    try {
      const publisher = createNtfyPublisher({
        target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
        transport: createHttpTransport(),
      });
      for (const title of titles) {
        const delivery = await publisher.publish({ title, body: `body for ${title}` });
        assert.equal(delivery.kind, "delivered", `"${title}" was not delivered: ${JSON.stringify(delivery)}`);
        assert.equal(
          publishedJson(server.requests[server.requests.length - 1]).title,
          title,
          "arrives verbatim, not escaped, not dropped, not refused",
        );
      }
    } finally {
      await server.close();
    }
  },
);

test("a line break in the title is flattened: two lines is a broken list entry", () => {
  const payload = ntfyPublishPayload({ title: "one\r\nX-Evil: yes\ntwo", body: "b" }, "loop");
  assert.equal(payload.title, "one X-Evil: yes two");
  const wire = ntfyPublishBody({ title: "a\nb", body: "c\nd" }, "loop");
  assert.ok(!/[\r\n]/u.test(wire), "every newline in the request body is JSON-escaped");
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

test(
  "a token that cannot be a header value is named as a config error, not ERR_INVALID_CHAR",
  async () => {
    const logged: string[] = [];
    const publisher = createNtfyPublisher({
      target: resolveNtfyTarget({ url: "http://localhost:9999", topic: "t" }),
      transport: stubTransport(),
      token: "bear\u{1F512}er",
      logger: (line) => logged.push(line),
    });
    const delivery = await publisher.publish({ title: "t", body: "b" });
    assert.equal(delivery.kind, "failed");
    const reason = delivery.kind === "failed" ? delivery.reason : "";
    assert.match(reason, /cannot be sent in an HTTP header/u, `reason was: ${reason}`);
    assert.match(reason, /LOOP_NTFY_TOKEN/u, "and it names the setting to fix");
    assert.equal(
      delivery.kind === "failed" ? delivery.retryable : true,
      false,
      "asking again will not fix a typo, so do not pretend it might",
    );
    for (const line of logged) {
      assert.ok(!line.includes("bear\u{1F512}er"), `the token leaked into a log line: ${line}`);
    }
  },
);

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
  assert.equal(NTFY_MAX_JSON_BODY_BYTES, 8192, "ntfy reads a JSON publish with 2x the message limit");
  assert.equal(NTFY_MAX_TITLE_LENGTH, 200);
});

test("escaping is paid for: a body that fits as text may not fit as JSON", () => {
  // 4000 newlines is 4000 bytes of message and 8002 bytes of JSON (8000 for
  // the escapes, two for the quotes around the field).
  const newlines = "\n".repeat(4000);
  assert.equal(Buffer.byteLength(newlines, "utf8"), 4000);
  assert.equal(Buffer.byteLength(JSON.stringify(newlines), "utf8"), 8002);
  const cut = fitJsonBytes(newlines, 5000);
  assert.ok(Buffer.byteLength(JSON.stringify(cut), "utf8") <= 5000);
  assert.match(cut, /… \(truncated\)$/u, "and the cut is marked");
  assert.equal(fitJsonBytes("short", 5000), "short", "one that fits is returned untouched");
});

test("a notice whose JSON form is too big for the server is clamped, not rejected", async () => {
  // A body made of nothing but quotes: 4096 bytes of message, 8192 bytes once
  // every one of them is escaped, and the envelope has to fit on top of that.
  const hostile = '"'.repeat(4096);
  const publisher = createNtfyPublisher({
    target: resolveNtfyTarget({ url: "http://localhost:9", topic: "t" }),
    transport: stubTransport(),
  });
  const delivery = await publisher.publish({ title: "t", body: hostile });
  assert.equal(delivery.kind, "delivered");
  const sent = lastSent();
  assert.ok(
    Buffer.byteLength(sent.body, "utf8") <= NTFY_MAX_JSON_BODY_BYTES,
    `the whole document was ${Buffer.byteLength(sent.body, "utf8")} bytes`,
  );
  const message = String((JSON.parse(sent.body) as Record<string, unknown>).message);
  assert.match(message, /… \(truncated\)$/u, "and the reader is told, not left counting quotes");
});

// ── the attachment boundary ─────────────────────────────────────────────

test(
  "REGRESSION: nothing lands on ntfy's message limit, because there it stops being a message",
  async () => {
    // `util.Peek` reports `LimitReached: read == limit`, and `handlePublishBody`
    // takes the text-message path only when the limit was NOT reached. A notice
    // truncated to exactly `limit-message-bytes` is therefore routed to
    // `handleBodyAsAttachment`, which answers `40014 attachments not allowed`
    // on a server with no attachment store — ntfy's default. The previous code
    // hit that exactly: cut to `4096 - marker`, put the marker back, 4096.
    // A body with no line break in it is the worst case, because the "prefer
    // the last whole line" rule has nothing to cut on.
    const server = await startFakeNtfy();
    try {
      const publisher = createNtfyPublisher({
        target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
        transport: createHttpTransport(),
      });
      const long = "x".repeat(6000);
      const delivery = await publisher.publish({ title: "t", body: long });
      assert.equal(
        delivery.kind,
        "delivered",
        `a long single-line notice must not be an attachment: ${JSON.stringify(delivery)}`,
      );
      const sent = String(publishedJson(server.requests[0]).message);
      const bytes = Buffer.byteLength(sent, "utf8");
      assert.ok(
        bytes < NTFY_MAX_MESSAGE_BYTES,
        `message was ${bytes} bytes; at 4096 ntfy reads an attachment`,
      );
      assert.equal(server.requests[0]?.asAttachment, false, "the server agrees it was a message");
      assert.match(sent, /… \(truncated\)$/u, "and the cut is still marked");
    } finally {
      await server.close();
    }
  },
);

test("every message the payload builder produces is strictly under the limit", () => {
  for (const size of [0, 1, 4095, 4096, 4097, 8192, 100_000]) {
    const payload = ntfyPublishPayload({ title: "t", body: "y".repeat(size) }, "loop");
    const bytes = Buffer.byteLength(payload.message, "utf8");
    assert.ok(
      bytes < NTFY_MAX_MESSAGE_BYTES,
      `a ${size}-byte body produced a ${bytes}-byte message, at or over the limit`,
    );
  }
});

test("a tighter server is discovered once and the smaller cap is kept", async () => {
  // ntfy does not publish the `limit-message-bytes` it is running, so a
  // self-hosted box on 1 KiB is unknowable in advance. One refusal is enough
  // to learn it: the cap comes down and stays down, so the next bead does not
  // pay for the same discovery again.
  const logged: string[] = [];
  const server = await startFakeNtfy({ messageLimitBytes: 1024 });
  try {
    const publisher = createNtfyPublisher({
      target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
      transport: createHttpTransport(),
      logger: (line) => logged.push(line),
    });
    const long = "z".repeat(4000);

    const first = await publisher.publish({ title: "first", body: long });
    assert.equal(first.kind, "delivered", JSON.stringify(first));
    const afterFirst = server.requests.length;
    assert.ok(afterFirst > 1, "the refusal should have caused a retry, not a give-up");
    assert.ok(
      publisher.messageBytes < NTFY_MAX_MESSAGE_BYTES,
      `the cap should have moved; it is still ${publisher.messageBytes}`,
    );
    assert.ok(
      logged.some((line) => /attachment/iu.test(line) && /cap/iu.test(line)),
      `the shrink should be in the log: ${logged.join(" | ")}`,
    );

    const second = await publisher.publish({ title: "second", body: long });
    assert.equal(second.kind, "delivered", JSON.stringify(second));
    assert.equal(
      server.requests.length,
      afterFirst + 1,
      "the learned cap means the second notice costs one request",
    );
    const lastMessage = String(publishedJson(server.requests[server.requests.length - 1]).message);
    assert.ok(
      Buffer.byteLength(lastMessage, "utf8") < 1024,
      "and it is inside what the server actually takes",
    );
  } finally {
    await server.close();
  }
});

test("shrinking stops when there is nothing useful left to try", () => {
  assert.equal(shrinkMessageLimit(4096), 2048);
  assert.equal(shrinkMessageLimit(512), 256);
  assert.equal(
    shrinkMessageLimit(NTFY_MIN_MESSAGE_BYTES),
    null,
    "below the floor this is a fragment, not a notice",
  );
});

test("which refusals shrinking cures, and which it does not", () => {
  assert.equal(
    isMessageTooLargeRefusal(
      400,
      '{"code":40014,"error":"invalid request: attachments not allowed"}',
    ),
    true,
  );
  assert.equal(
    isMessageTooLargeRefusal(
      400,
      '{"code":40014,"httpMessage":"invalid request: attachments not allowed"}',
    ),
    true,
  );
  assert.equal(isMessageTooLargeRefusal(413, ""), true);
  assert.equal(isMessageTooLargeRefusal(400, "request body too large"), true);
  // Wrong topic, wrong token, rate limit: sending less fixes none of these.
  assert.equal(isMessageTooLargeRefusal(403, '{"code":40030,"error":"forbidden"}'), false);
  assert.equal(isMessageTooLargeRefusal(400, "invalid request: topic invalid"), false);
  assert.equal(isMessageTooLargeRefusal(429, "too many requests"), false);
  assert.equal(isMessageTooLargeRefusal(500, "boom"), false);
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

// A transport that records the last request it was handed, with the publish
// envelope read back out of it.
let lastRequest: NtfyRequestOptions = { headers: {}, body: "", timeoutMs: 0 };
function lastSent(): { title: string; body: string; url: string } {
  const parsed = JSON.parse(lastRequest.body) as { title?: string };
  return {
    title: typeof parsed.title === "string" ? parsed.title : "",
    body: lastRequest.body,
    url: lastUrl,
  };
}
let lastUrl = "";
function stubTransport(): NtfyTransport {
  return {
    name: "stub",
    async publish(url: string, options: NtfyRequestOptions) {
      lastRequest = options;
      lastUrl = url;
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

test("a good publish: POST the envelope to the server root, JSON ack read back", async () => {
  const server = await startFakeNtfy();
  try {
    const publisher = createNtfyPublisher({
      target: resolveNtfyTarget({ url: server.base, topic: "loop" }),
      transport: createHttpTransport(),
    });
    const delivery = await publisher.publish({
      title: "[pi-beads] tst.7 completed: fixed it",
      body: "tst.7 — fixed it\nthe details",
      hints: { priority: "3" },
    });
    assert.equal(delivery.kind, "delivered");
    assert.equal(
      delivery.kind === "delivered" ? delivery.messageId : null,
      "abc123XYZ",
      "the message id is read out of the ack, not invented",
    );
    assert.equal(server.requests.length, 1);
    assert.equal(server.requests[0]?.method, "POST");
    assert.equal(
      server.requests[0]?.path,
      "/",
      "ntfy routes a JSON publish on the root path; the topic is not in the URL",
    );
    assert.equal(
      server.requests[0]?.topic,
      "loop",
      "and the server got the topic out of the body, which is where it looked",
    );
    assert.equal(server.requests[0]?.headers["content-type"], "application/json");
    const sent = publishedJson(server.requests[0]);
    assert.equal(sent.title, "[pi-beads] tst.7 completed: fixed it");
    assert.equal(sent.priority, 3, "the numeric form the field is typed as");
    assert.match(String(sent.message), /the details/u);
    // The report still names the topic URL rather than the request URL, because
    // that is the thing a human recognises as "where the notices go".
    assert.equal(
      delivery.kind === "delivered" ? delivery.destination : "",
      `${server.base}/loop`,
    );
  } finally {
    await server.close();
  }
});

test("a JSON publish aimed at the topic URL is refused, the way ntfy refuses it", async () => {
  // The fake is stricter than the real server on purpose: ntfy would accept
  // `POST /loop` with a JSON body and turn the whole document into the text of
  // the notification. A client that did that delivers garbage, so the test that
  // it never happens is worth more than the test that garbage looks like.
  const server = await startFakeNtfy();
  try {
    const response = await fetch(`${server.base}/loop`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ topic: "loop", message: "would have arrived as JSON text" }),
    });
    assert.equal(response.status, 400);
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
