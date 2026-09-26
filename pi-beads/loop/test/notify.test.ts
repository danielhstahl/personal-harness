/**
 * Tests for `src/notify.ts` — the completion notice.
 *
 * Two layers here, and both get tested: the *wording* (a title a person can find
 * a bead by, a body that answers the questions a notice exists to answer) and
 * the *behaviour* (a notifier that keeps its silence after a few failures
 * rather than warning once per bead until somebody notices).
 *
 * The publisher is a recording double: these tests are about what the notice
 * says and when it stops trying. The transport's own behaviour is
 * `test/ntfy.test.ts`'s problem, and the loop's reaction to a delivery report
 * is `test/loop.test.ts`'s.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { buildApp } from "../src/app.ts";
import type { NtfyDelivery, NtfyMessage, NtfyPublisher } from "../src/ntfy.ts";
import { LoopError } from "../src/loop.ts";
import { readEnv } from "../src/main.ts";
import {
  MAX_TITLE_LENGTH,
  completionBody,
  completionNotice,
  completionTitle,
  createNotifier,
  createNullNotifier,
  formatDuration,
  formatTimestamp,
  oneLine,
  type BeadCompletion,
} from "../src/notify.ts";

// ── fixtures ────────────────────────────────────────────────────────────────

const CONTEXT = {
  cwd: "/work/project",
  hostname: "buildhost",
  model: "fake-provider/fake-model",
};

function completion(overrides: Partial<BeadCompletion> = {}): BeadCompletion {
  return {
    issueId: "tst.42",
    title: "Add the colour mode to the parser",
    summary: "Added colour handling to the tokenizer and thread it through the parser.",
    commit: "deadbeefcafebabe0123456789abcdef01234567",
    changedFiles: ["src/colour.ts", "src/parser.ts"],
    nextSteps: ["docs still need the colour section"],
    decisions: ["case-insensitive by default"],
    closeReason: "Done: Added colour handling to the tokenizer and thread it through the parser.",
    handoffKey: "loop:handoff:tst.42",
    iteration: 3,
    workKind: "done",
    elapsedMs: 191_000,
    completedAt: Date.UTC(2025, 8, 26, 11, 3, 7),
    ...overrides,
  };
}

/** A publisher that records what it was asked to publish. */
function recordingPublisher(deliveries: readonly ("delivered" | "failed" | "skipped")[] = ["delivered"]) {
  const published: NtfyMessage[] = [];
  let index = 0;
  const publisher: NtfyPublisher = {
    enabled: true,
    destination: "http://ntfy.test/loop",
    transport: "recorder",
    async publish(message: NtfyMessage): Promise<NtfyDelivery> {
      published.push(message);
      const kind = deliveries[Math.min(index, deliveries.length - 1)] ?? "delivered";
      index += 1;
      if (kind === "failed") {
        return {
          kind: "failed",
          reason: "ntfy replied 502: bad gateway",
          destination: "http://ntfy.test/loop",
          retryable: true,
        };
      }
      if (kind === "skipped") {
        return { kind: "skipped", reason: "nothing to publish" };
      }
      return {
        kind: "delivered",
        messageId: `msg-${index}`,
        destination: "http://ntfy.test/loop",
        transport: "recorder",
      };
    },
  };
  return { publisher, published };
}

// ── the title ───────────────────────────────────────────────────────────────

test("the title carries the bead id, which is what the notice is searched by", () => {
  assert.equal(
    completionTitle(completion()),
    "[pi-beads] tst.42 completed: Added colour handling to the tokenizer and thread it through the parser.",
  );
});

test("the title prefix is configurable, and empty falls back rather than vanishing", () => {
  assert.ok(
    completionTitle(completion(), "nightly").startsWith("[nightly] tst.42"),
    completionTitle(completion(), "nightly"),
  );
  assert.ok(
    completionTitle(completion(), "").startsWith("[pi-beads] tst.42"),
    "an empty prefix means 'not set', not 'no prefix' — a title with no bracket reads like a bug",
  );
});

test("with no summary the title uses the bead's own title", () => {
  const title = completionTitle(completion({ summary: "" }));
  assert.ok(title.includes("Add the colour mode to the parser"), title);
  assert.ok(title.startsWith("[pi-beads] tst.42 completed"), title);
});

test("the title is capped for a lock screen without losing the id", () => {
  const title = completionTitle(completion({ summary: "long ".repeat(80) }));
  assert.ok(title.length <= MAX_TITLE_LENGTH, `title was ${title.length} chars`);
  assert.ok(title.startsWith("[pi-beads] tst.42"), title);
});

// ── the body ────────────────────────────────────────────────────────────────

test("the body answers what finished, what it did, and what is left", () => {
  const body = completionBody(completion(), CONTEXT);
  assert.ok(body.startsWith("tst.42 — Add the colour mode to the parser"), body);
  assert.ok(body.includes("Added colour handling to the tokenizer"), body);
  assert.ok(body.includes("done"), body);
  assert.ok(body.includes("3m 11s"), body);
  assert.ok(body.includes("iteration 3"), body);
  assert.ok(body.includes("deadbeefcafe"), "a short hash: long enough to paste, short enough to read");
  assert.ok(body.includes("2 files"), body);
  assert.ok(body.includes("- src/colour.ts"), body);
  assert.ok(body.includes("next: docs still need the colour section"), body);
  assert.ok(body.includes("bd show tst.42"), body);
  assert.ok(body.includes("bd recall loop:handoff:tst.42"), body);
});

test("a long commit hash is shortened rather than wrapped across the notice", () => {
  const body = completionBody(completion(), CONTEXT);
  assert.ok(!body.includes("0123456789abcdef"), "the tail of the hash is not what anybody reads", body);
});

test("fields the run could not know say so instead of disappearing", () => {
  const body = completionBody(
    {
      issueId: "tst.7",
      title: "",
      summary: "",
      commit: null,
      changedFiles: [],
      nextSteps: [],
      handoffKey: null,
      iteration: null,
      workKind: null,
      elapsedMs: null,
      completedAt: null,
    },
    { cwd: "/srv/app" },
  );
  assert.ok(body.includes("(untitled bead)"), body);
  assert.ok(body.includes("(the run reported no summary)"), body);
  assert.ok(body.includes("no commit recorded"), body);
  assert.ok(body.includes("no files reported"), body);
  assert.ok(body.includes("unknown verdict"), body);
  assert.ok(body.includes("unknown duration"), body);
  assert.ok(body.includes("nothing left to do"), body);
  assert.ok(body.includes("/srv/app"), "the repo is always there to say where the work is");
});

test("a reused commit is labelled as one, because 'committed twice' is the fear", () => {
  const body = completionBody(completion({ committedAgain: true }), CONTEXT);
  assert.ok(body.includes("reused"), body);
  assert.ok(body.includes("not committed twice"), body);
});

test("many files are listed compactly, and a long next-step list says how many more", () => {
  const manyFiles = completion({ changedFiles: Array.from({ length: 9 }, (_, i) => `f${i}.ts`) });
  const fileBody = completionBody(manyFiles, CONTEXT);
  assert.ok(fileBody.includes("9 files"), fileBody);
  assert.ok(!fileBody.includes("f8.ts"), "the list is folded rather than scrolling the notice", fileBody);

  const manySteps = completion({
    nextSteps: ["one", "two", "three", "four", "five"],
  });
  const stepBody = completionBody(manySteps, CONTEXT);
  assert.ok(stepBody.includes("next: one; two; three (+2 more)"), stepBody);
});

test("the notice pairs the title and the body for one publish", () => {
  const notice = completionNotice(completion(), { ...CONTEXT, titlePrefix: "loop" });
  assert.ok(notice.title.startsWith("[loop] tst.42"), notice.title);
  assert.ok(notice.body.startsWith("tst.42 —"), notice.body);
});

test("a summary with a line break in it is flattened, not rendered as a broken notice", () => {
  const body = completionBody(completion({ summary: "first line\nsecond line" }), CONTEXT);
  assert.ok(body.includes("first line second line"), body);
  const title = completionTitle(completion({ summary: "first\nsecond" }));
  assert.ok(!title.includes("\n"), `title kept a newline: ${JSON.stringify(title)}`);
});

// ── the formatters ──────────────────────────────────────────────────────────

test("formatDuration reads like a human's estimate, not a millisecond count", () => {
  assert.equal(formatDuration(812), "812ms");
  assert.equal(formatDuration(3_110_000), "51m 50s");
  assert.equal(formatDuration(3_720_000), "1h 02m");
  assert.equal(formatDuration(null), null);
  assert.equal(formatDuration(-5), null);
});

test("formatTimestamp is short enough for a notice line and still unambiguous", () => {
  assert.equal(formatTimestamp(Date.UTC(2025, 8, 26, 11, 3, 7)), "2025-09-26 11:03 UTC");
  assert.equal(formatTimestamp(null), "unknown");
});

test("oneLine flattens a paragraph into a title-sized line", () => {
  assert.equal(oneLine("  a   b \n c  "), "a b c");
});

// ── the notifier ────────────────────────────────────────────────────────────

test("the notifier hands the notice to the publisher and reports what came back", async () => {
  const { publisher, published } = recordingPublisher(["delivered"]);
  const notifier = createNotifier({
    publisher,
    context: CONTEXT,
    priority: "high",
    tags: ["+1"],
    click: "http://board.test/tst.42",
  });
  const delivery = await notifier.notifyCompletion(completion());
  assert.equal(delivery.kind, "delivered");
  assert.equal(published.length, 1);
  assert.equal(published[0]?.title, completionTitle(completion()));
  assert.equal(published[0]?.priority, "high");
  assert.deepEqual(published[0]?.tags, ["+1"]);
  assert.equal(published[0]?.click, "http://board.test/tst.42");
  assert.deepEqual(notifier.destination, ["http://ntfy.test/loop"]);
  assert.equal(notifier.enabled, true);
});

test("a relay with no topic disables the notifier before anything is tried", async () => {
  const { publisher, published } = recordingPublisher();
  const disabledPublisher: NtfyPublisher = { ...publisher, enabled: false };
  const notifier = createNotifier({ publisher: disabledPublisher, context: CONTEXT });
  const delivery = await notifier.notifyCompletion(completion());
  assert.equal(delivery.kind, "skipped");
  assert.equal(published.length, 0);
});

test("a server that keeps failing is given up on, not warned at once per bead", async () => {
  const { publisher, published } = recordingPublisher(["failed"]);
  const logged: string[] = [];
  const notifier = createNotifier({
    publisher,
    context: CONTEXT,
    maxConsecutiveFailures: 3,
    logger: (line) => logged.push(line),
  });

  assert.equal((await notifier.notifyCompletion(completion())).kind, "failed");
  assert.equal((await notifier.notifyCompletion(completion())).kind, "failed");
  assert.equal((await notifier.notifyCompletion(completion())).kind, "failed");
  const after = await notifier.notifyCompletion(completion());

  assert.equal(after.kind, "skipped", "the fourth bead is not used to discover the server is down again");
  assert.match(
    after.kind === "skipped" ? after.reason : "",
    /3 completion notices in a row failed to publish/u,
  );
  assert.equal(published.length, 3, "and nothing was attempted after the giving-up");
  assert.match(logged.join("\n"), /switched off for the rest of this run/u);
});

test("the switch-off is heard at the failure that caused it, not on the next bead's turn", async () => {
  const { publisher } = recordingPublisher(["failed"]);
  const notifier = createNotifier({ publisher, context: CONTEXT, maxConsecutiveFailures: 3 });
  assert.equal(notifier.enabled, true, "on its way down, notices are still on");

  await notifier.notifyCompletion(completion());
  await notifier.notifyCompletion(completion());
  const third = await notifier.notifyCompletion(completion());

  assert.equal(third.kind, "failed");
  assert.ok(
    third.kind === "failed" && /switched off for the rest of this run/u.test(third.reason),
    `the line the loop already prints must carry the switch-off: ${
      third.kind === "failed" ? third.reason : ""
    }`,
  );
  assert.equal(
    notifier.enabled,
    false,
    "and 'are notices on?' answers no from here on — a snapshot taken at construction would keep answering yes",
  );
});

test("one success clears the failure streak", async () => {
  const outcomes = ["failed", "failed", "delivered", "failed", "failed", "delivered"];
  let index = 0;
  const publisher: NtfyPublisher = {
    enabled: true,
    destination: "http://ntfy.test/loop",
    transport: "stub",
    async publish(): Promise<NtfyDelivery> {
      const kind = outcomes[index] ?? "delivered";
      index += 1;
      return kind === "failed"
        ? {
            kind: "failed",
            reason: "flaky",
            destination: "http://ntfy.test/loop",
            retryable: true,
          }
        : {
            kind: "delivered",
            messageId: "m",
            destination: "http://ntfy.test/loop",
            transport: "stub",
          };
    },
  };
  const notifier = createNotifier({ publisher, context: CONTEXT, maxConsecutiveFailures: 3 });

  for (let i = 0; i < 6; i += 1) {
    const delivery = await notifier.notifyCompletion(completion());
    assert.equal(
      delivery.kind,
      outcomes[i],
      `attempt ${i + 1} should have reported ${outcomes[i]}, not been skipped`,
    );
  }
});

test("a null notifier says why it is silent", async () => {
  const notifier = createNullNotifier("no topic is set");
  assert.equal(notifier.enabled, false);
  const delivery = await notifier.notifyCompletion(completion());
  assert.deepEqual(delivery, { kind: "skipped", reason: "no topic is set" });
});

// ── the knobs ───────────────────────────────────────────────────────────────

test("the ntfy knobs are read from the environment, and unset means unset", () => {
  assert.equal(readEnv({}).notify, undefined, "nothing ntfy-related set means no setting at all");

  const configured = readEnv({
    LOOP_NTFY_TOPIC: "loop-notices",
    LOOP_NTFY_URL: "http://192.168.1.20:8080",
    LOOP_NTFY_TOKEN: "ap_abc123",
    LOOP_NTFY_PRIORITY: "high",
    LOOP_NTFY_TAGS: "+1 beads",
    LOOP_NTFY_CLICK: "http://board.test",
    LOOP_NTFY_TITLE_PREFIX: "nightly",
    LOOP_NTFY_TIMEOUT_MS: "4500",
    LOOP_NTFY_MAX_FAILURES: "9",
  });
  assert.equal(configured.notify?.topic, "loop-notices");
  assert.equal(configured.notify?.url, "http://192.168.1.20:8080");
  assert.equal(configured.notify?.token, "ap_abc123");
  assert.equal(configured.notify?.priority, "high");
  assert.deepEqual(configured.notify?.tags, ["+1", "beads"]);
  assert.equal(configured.notify?.click, "http://board.test");
  assert.equal(configured.notify?.titlePrefix, "nightly");
  assert.equal(configured.notify?.timeoutMs, 4_500);
  assert.equal(configured.notify?.maxConsecutiveFailures, 9);
  assert.equal(configured.notify?.enabled, true);
});

test("the topic is the switch", () => {
  assert.equal(
    readEnv({ LOOP_NTFY_URL: "http://host:8080" }).notify?.enabled,
    false,
    "a server with nowhere to publish is off, and says so",
  );
  assert.equal(
    readEnv({ LOOP_NTFY_TOPIC: "  " }).notify,
    undefined,
    "a blank topic is the same as no topic at all — not a setting switched off",
  );
  assert.equal(readEnv({ LOOP_NTFY_TOPIC: "loop" }).notify?.enabled, true);
});

test("LOOP_NTFY_MAX_FAILURES is refused when it is not a whole number of tries", () => {
  for (const bad of ["0", "-2", "1.5", "three"]) {
    assert.throws(
      () => readEnv({ LOOP_NTFY_TOPIC: "loop", LOOP_NTFY_MAX_FAILURES: bad }),
      (error: unknown) => LoopError.is(error) && error.code === "notify-config",
      `LOOP_NTFY_MAX_FAILURES="${bad}" should have been refused`,
    );
  }
  for (const bad of [0, -1, 1.5]) {
    assert.throws(
      () =>
        buildApp({
          cwd: "/tmp",
          notify: { topic: "loop", enabled: true, maxConsecutiveFailures: bad },
        }),
      (error: unknown) => LoopError.is(error) && error.code === "notify-config",
      `maxConsecutiveFailures=${bad} should have been refused`,
    );
  }
});

test("an unusable endpoint is refused when the loop is built, not at the first closed bead", () => {
  const cases: Array<Partial<import("../src/app.ts").NotifySetting>> = [
    { topic: "loop", url: "not-a-url" },
    { topic: "loop", url: "ftp://host" },
    { topic: "http://host.example" },
    { topic: "loop", url: "http://user:pass@host" },
  ];
  for (const setting of cases) {
    assert.throws(
      () => buildApp({ cwd: "/tmp", notify: { enabled: true, ...setting } }),
      (error: unknown) => LoopError.is(error) && error.code === "notify-config",
      `${JSON.stringify(setting)} should have been refused`,
    );
  }
});

test("a nonsense priority is refused, and the five names and 1-5 are not", () => {
  assert.throws(
    () => buildApp({ cwd: "/tmp", notify: { topic: "loop", enabled: true, priority: "urgent!" } }),
    (error: unknown) => LoopError.is(error) && error.code === "notify-config",
  );
  for (const ok of ["1", "3", "5", "min", "low", "default", "high", "urgent", "URGENT"]) {
    const app = buildApp({ cwd: "/tmp", notify: { topic: "loop", enabled: true, priority: ok } });
    assert.equal(app.ports.notify?.enabled, true, `priority "${ok}" should have been accepted`);
  }
});

test("with no topic the loop still builds, and knows it will tell nobody", () => {
  const app = buildApp({ cwd: "/tmp" });
  const notifier = app.ports.notify;
  assert.ok(notifier, "the port exists so the loop's report is a real answer");
  assert.equal(notifier?.enabled, false);
  assert.deepEqual(notifier?.destination, []);
});

test("a configured notifier reaches the loop's ports with the endpoint it was pointed at", () => {
  const app = buildApp({
    cwd: "/tmp",
    notify: { topic: "loop-notices", url: "http://127.0.0.1:8080", enabled: true },
  });
  assert.equal(app.ports.notify?.enabled, true);
  assert.deepEqual(app.ports.notify?.destination, ["http://127.0.0.1:8080/loop-notices"]);
  assert.equal(app.ports.notify?.transport, "ntfy");
});

test("a full topic URL works the same way through the app as through the resolver", () => {
  const app = buildApp({ cwd: "/tmp", notify: { topic: "https://ntfy.sh/abc123", enabled: true } });
  assert.deepEqual(app.ports.notify?.destination, ["https://ntfy.sh/abc123"]);
});
