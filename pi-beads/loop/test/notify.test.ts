/**
 * Tests for `src/notify.ts` — the completion notice.
 *
 * Two layers here, and both get tested: the *wording* (a subject a person can
 * search by, a body that answers the questions a notice exists to answer) and the
 * *behaviour* (a notifier that keeps its silence after a few failures rather than
 * warning once per bead until somebody notices).
 *
 * The mailer is a recording double: these tests are about what the notice says and
 * when it stops trying. The transport's own behaviour is `test/mail.test.ts`'s
 * problem, and the loop's reaction to a delivery report is `test/loop.test.ts`'s.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { buildApp } from "../src/app.ts";
import { createMailer, createRecordingTransport, type MailEnvelope } from "../src/mail.ts";
import { LoopError } from "../src/loop.ts";
import { readEnv } from "../src/main.ts";
import {
  MAX_SUBJECT_LENGTH,
  completionBody,
  completionMessage,
  completionSubject,
  createNotifier,
  createNullNotifier,
  formatDuration,
  noticeFooter,
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

/** A mailer that records instead of sending. */
function recordingMailer(deliveries: Array<"delivered" | "failed"> = ["delivered"]) {
  const sent: MailEnvelope[] = [];
  let index = 0;
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    transport: {
      name: "recorder",
      async send(envelope: MailEnvelope) {
        sent.push(envelope);
        const kind = deliveries[Math.min(index, deliveries.length - 1)] ?? "delivered";
        index += 1;
        if (kind === "failed") {
          return {
            kind: "failed",
            reason: "relay said no",
            transport: "recorder",
            retryable: true,
          };
        }
        return {
          kind: "delivered",
          messageId: envelope.messageId,
          transport: "recorder",
          recipients: envelope.recipients,
        };
      },
    },
  });
  return { mailer, sent };
}

// ── the subject ─────────────────────────────────────────────────────────────

test("the subject carries the bead id, which is what the mail gets searched by", () => {
  const subject = completionSubject(completion());
  assert.match(subject, /\btst\.42\b/u, "the bead id is in the subject");
  assert.match(subject, /^\[pi-beads\] /u, "and the prefix says which loop it came from");
  assert.match(subject, /Added colour handling/u, "and the summary says what finished");
});

test("the subject is capped, because a phone shows about one line of it", () => {
  const subject = completionSubject(
    completion({ summary: "a summary that goes on ".repeat(30) }),
  );
  assert.ok(subject.length <= MAX_SUBJECT_LENGTH, `subject was ${subject.length} chars: ${subject}`);
  assert.match(subject, /…$/u, "and it says it was cut, rather than ending mid-word");
});

test("a notice with no summary falls back to the title rather than a blank subject", () => {
  assert.match(completionSubject(completion({ summary: "   " })), /Add the colour mode/u);
  assert.match(
    completionSubject(completion({ summary: "", title: "" })),
    /tst\.42 completed/u,
    "the id alone still identifies it",
  );
});

// ── the body ────────────────────────────────────────────────────────────────

test("the body answers what finished, what changed and what is left", () => {
  const body = completionBody(completion(), CONTEXT);
  assert.match(body, /^tst\.42 — Add the colour mode to the parser/u);
  assert.match(body, /Bead\s+tst\.42/u);
  assert.match(body, /Status\s+closed/u);
  assert.match(body, /2025-09-26 11:03:07 UTC/u);
  assert.match(body, /done, 3m 11s, iteration 3/u);
  assert.match(body, /What it did\n {4}Added colour handling/u);
  assert.match(body, /Commit\s+deadbeefcafebabe/u);
  assert.match(body, /- src\/colour\.ts/u);
  assert.match(body, /- src\/parser\.ts/u);
  assert.match(body, /Decisions taken\n {4}1\. case-insensitive/u);
  assert.match(body, /Still to do\n {4}1\. docs still need/u);
  assert.match(body, /bd show tst\.42/u, "the reader can go and look");
  assert.match(body, /bd recall loop:handoff:tst\.42 --json/u);
  assert.match(body, /git show deadbeefcafebabe/u);
  assert.match(body, /Repo\s+\/work\/project/u);
  assert.match(body, /Model\s+fake-provider\/fake-model/u);
});

test("what the run could not know is said as unknown, never left out", () => {
  const body = completionBody(
    {
      issueId: "tst.7",
      title: "A bead worked by a run that reported nothing",
      summary: "",
    },
    { cwd: "/work/project" },
  );
  assert.match(body, /\(the run reported no summary\)/u);
  assert.match(body, /Commit\s+\(none recorded\)/u);
  assert.match(body, /Files\s+\(none reported\)/u);
  assert.match(body, /Still to do\n {4}\(nothing reported\)/u);
  assert.match(body, /unknown verdict/u);
  assert.match(body, /unknown iteration/u);
  assert.match(body, /When\s+unknown/u);
  assert.match(
    body,
    /no handoff key recorded/u,
    "a missing handoff is a fact the reader needs, not an omission",
  );
});

test("a commit reused from an earlier attempt says so, because a second notice for one bead is confusing", () => {
  const body = completionBody(completion({ committedAgain: true }), CONTEXT);
  assert.match(body, /reused from an earlier attempt, not committed twice/u);
});

test("the footer names the sender, the recipients and the host that wrote it", () => {
  const footer = noticeFooter(CONTEXT, "loop@example.test", ["dev@example.test", "ada@example.test"]);
  assert.match(footer, /pi-beads-loop on buildhost/u);
  assert.match(footer, /dev@example\.test, ada@example\.test/u);
  assert.match(footer, /delivery failure never stops the loop/u);
});

test("the message carries the machine-readable headers a generated notice should", () => {
  const message = completionMessage(completion(), CONTEXT, {
    from: "loop@example.test",
    to: ["dev@example.test"],
  });
  assert.equal(message.headers["Auto-Submitted"], "auto-generated", "RFC 3834: do not auto-reply to this");
  assert.equal(message.headers["X-Loop-Issue"], "tst.42");
  assert.equal(message.headers["X-Loop-Commit"], "deadbeefcafebabe0123456789abcdef01234567");
  assert.equal(message.headers["X-Loop-Handoff"], "loop:handoff:tst.42");
  assert.equal(message.headers["X-Loop-Verdict"], "done");
});

test("a summary with a line break in it cannot forge a second header into the body footer", () => {
  // The summary is an agent's own sentence. Whatever it contains, the notice must
  // still be one message with the headers this module wrote.
  const hostile = completion({
    summary: "done\r\nX-Loop-Commit: forged\r\nand other things",
  });
  const message = completionMessage(hostile, CONTEXT, {
    from: "loop@example.test",
    to: ["dev@example.test"],
  });
  assert.equal(message.headers["X-Loop-Commit"], "deadbeefcafebabe0123456789abcdef01234567");
  assert.match(message.subject, /^.{0,120}$/su, "the subject is one line");
  const forgedLines = message.body
    .split("\n")
    .filter((line) => /^[A-Za-z-]+:\s*forged$/u.test(line));
  assert.deepEqual(forgedLines, [], "no body line may read as a header we did not write");
});

test("formatDuration reads like a human's estimate, not a millisecond count", () => {
  assert.equal(formatDuration(812), "812ms");
  assert.equal(formatDuration(9_000), "9s");
  assert.equal(formatDuration(191_000), "3m 11s");
  assert.equal(formatDuration(3_720_000), "1h 02m");
  assert.equal(formatDuration(null), null);
  assert.equal(formatDuration(-5), null);
  assert.equal(formatDuration(Number.NaN), null);
});

test("oneLine flattens a paragraph into a subject-sized line", () => {
  assert.equal(oneLine("  many\n\tnested \n lines  "), "many nested lines");
});

// ── the notifier ────────────────────────────────────────────────────────────

test("the notifier hands the notice to the mailer and reports what came back", async () => {
  const { mailer, sent } = recordingMailer(["delivered"]);
  const notifier = createNotifier({ mailer, context: CONTEXT });
  assert.equal(notifier.enabled, true);
  assert.deepEqual(notifier.destination, ["dev@example.test"]);

  const delivery = await notifier.notifyCompletion(completion());
  assert.equal(delivery.kind, "delivered");
  assert.equal(sent.length, 1);
  assert.match(sent[0]?.raw ?? "", /Subject: \[pi-beads\] tst\.42 completed/u);
});

test("a mailer with no recipients disables the notifier before anything is tried", async () => {
  const seen: MailEnvelope[] = [];
  const notifier = createNotifier({
    mailer: createMailer({
      from: "loop@example.test",
      to: [],
      transport: createRecordingTransport((envelope) => seen.push(envelope)),
    }),
    context: CONTEXT,
  });
  assert.equal(notifier.enabled, false);
  const delivery = await notifier.notifyCompletion(completion());
  assert.equal(delivery.kind, "skipped");
  assert.equal(seen.length, 0);
});

test("a relay that keeps failing is given up on, not warned at once per bead", async () => {
  const { mailer, sent } = recordingMailer(["failed"]);
  const logged: string[] = [];
  const notifier = createNotifier({
    mailer,
    context: CONTEXT,
    maxConsecutiveFailures: 3,
    logger: (line) => logged.push(line),
  });

  assert.equal((await notifier.notifyCompletion(completion())).kind, "failed");
  assert.equal((await notifier.notifyCompletion(completion())).kind, "failed");
  assert.equal((await notifier.notifyCompletion(completion())).kind, "failed");
  const after = await notifier.notifyCompletion(completion());

  assert.equal(after.kind, "skipped", "the fourth bead is not used to discover the relay is down again");
  assert.match(
    after.kind === "skipped" ? after.reason : "",
    /3 completion notices in a row failed to send/u,
  );
  assert.equal(sent.length, 3, "and nothing was attempted after the giving-up");
  assert.match(logged.join("\n"), /switched off for the rest of this run/u);
});

test("the switch-off is heard at the failure that caused it, not on the next bead's turn", async () => {
  const { mailer } = recordingMailer(["failed"]);
  const notifier = createNotifier({ mailer, context: CONTEXT, maxConsecutiveFailures: 3 });
  assert.equal(notifier.enabled, true, "on its way down, mail is still on");

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
    "and 'is mail on?' answers no from here on — a snapshot taken at construction would keep answering yes",
  );
});

test("one success clears the failure streak", async () => {
  const sent: MailEnvelope[] = [];
  const outcomes = ["failed", "failed", "delivered", "failed", "failed", "delivered"];
  let index = 0;
  const notifier = createNotifier({
    mailer: createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: {
        name: "stubborn",
        async send(envelope: MailEnvelope) {
          sent.push(envelope);
          const kind = outcomes[index] ?? "delivered";
          index += 1;
          return kind === "failed"
            ? { kind: "failed", reason: "flaky", transport: "stubborn", retryable: true }
            : {
                kind: "delivered",
                messageId: envelope.messageId,
                transport: "stubborn",
                recipients: envelope.recipients,
              };
        },
      },
    }),
    context: CONTEXT,
    maxConsecutiveFailures: 3,
  });

  for (let i = 0; i < 6; i += 1) {
    const delivery = await notifier.notifyCompletion(completion());
    assert.equal(
      delivery.kind,
      outcomes[i],
      `attempt ${i + 1} should have reported ${outcomes[i]}, not been skipped`,
    );
  }
  assert.equal(sent.length, 6, "a success in the middle means it was never a streak of three");
});

test("a copy is delivered as a copy: in Cc: on the wire, and named as a copy in the footer", async () => {
  const seen: MailEnvelope[] = [];
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    cc: ["lead@example.test"],
    transport: createRecordingTransport((envelope) => seen.push(envelope)),
  });
  const notifier = createNotifier({ mailer, context: CONTEXT });

  const delivery = await notifier.notifyCompletion(completion());

  assert.equal(delivery.kind, "delivered");
  assert.deepEqual(
    notifier.destination,
    ["dev@example.test", "lead@example.test"],
    "the notice goes to both, and says so before it is sent",
  );

  const headers = (seen[0]?.raw ?? "").split("\r\n\r\n")[0] ?? "";
  assert.match(headers, /^To: dev@example\.test$/mu);
  assert.match(headers, /^Cc: lead@example\.test$/mu);
  assert.ok(
    !/To: dev@example\.test, lead@/u.test(headers),
    "the copy is not dressed up as an addressee in the header the whole thread is replied to",
  );
  assert.deepEqual(seen[0]?.recipients, ["dev@example.test", "lead@example.test"]);
});

test("the footer tells a reader who was addressed and who was copied", () => {
  const footer = noticeFooter(
    CONTEXT,
    "loop@example.test",
    ["dev@example.test"],
    ["lead@example.test", "ada@example.test"],
  );
  assert.ok(
    footer.includes("to dev@example.test, cc lead@example.test, ada@example.test"),
    footer,
  );
  assert.ok(
    !noticeFooter(CONTEXT, "loop@example.test", ["dev@example.test"]).includes("cc"),
    "no copies means no mention of copies",
  );
});

test("a null notifier says why it is silent", async () => {
  const notifier = createNullNotifier("LOOP_NOTIFY_EMAIL is unset");
  assert.equal(notifier.enabled, false);
  const delivery = await notifier.notifyCompletion(completion());
  assert.equal(delivery.kind, "skipped");
  assert.match(delivery.kind === "skipped" ? delivery.reason : "", /LOOP_NOTIFY_EMAIL is unset/u);
});

// ── the knobs ───────────────────────────────────────────────────────────────

test("the mail knobs are read from the environment, and unset means unset", () => {
  assert.equal(readEnv({}).notify, undefined, "nothing mail-related set means no setting at all");

  const configured = readEnv({
    LOOP_NOTIFY_EMAIL: "dev@example.test, ada@example.com",
    LOOP_NOTIFY_CC: "lead@example.test",
    LOOP_NOTIFY_FROM: "ci@example.test",
    LOOP_NOTIFY_SUBJECT_PREFIX: "nightly",
    LOOP_MAIL_URL: "smtps://mail.example.test:465",
    LOOP_MAIL_STARTTLS: "required",
    LOOP_MAIL_TIMEOUT_MS: "4500",
    LOOP_MAIL_INSECURE_AUTH: "1",
  });
  assert.deepEqual(configured.notify?.to, ["dev@example.test", "ada@example.com"]);
  assert.deepEqual(configured.notify?.cc, ["lead@example.test"]);
  assert.equal(configured.notify?.enabled, true);
  assert.equal(configured.notify?.from, "ci@example.test");
  assert.equal(configured.notify?.subjectPrefix, "nightly");
  assert.equal(configured.notify?.url, "smtps://mail.example.test:465");
  assert.equal(configured.notify?.starttls, "required");
  assert.equal(configured.notify?.timeoutMs, 4_500);
  assert.equal(configured.notify?.allowInsecureAuth, true);
});

test("an address list is split the way a person types one", () => {
  const config = readEnv({ LOOP_NOTIFY_EMAIL: "a@x.test;b@x.test   c@x.test" });
  assert.deepEqual(config.notify?.to, ["a@x.test", "b@x.test", "c@x.test"]);
});

test("an address is checked when the loop is built, not at the first closed bead", () => {
  // Typo discovered now, in the terminal it was typed in, rather than as a
  // per-bead warning in a log nobody is reading.
  assert.throws(
    () =>
      buildApp({
        cwd: "/tmp",
        notify: { to: ["dev-at-example-dot-test"], enabled: true },
      }),
    (error: unknown) => LoopError.is(error) && error.code === "notify-config",
  );
  assert.throws(
    () =>
      buildApp({
        cwd: "/tmp",
        notify: { to: ["dev@example.test"], url: "smtp://mail.test:notaport" },
      }),
    (error: unknown) => LoopError.is(error) && error.code === "notify-config",
  );
});

test("LOOP_NOTIFY_MAX_FAILURES sets the threshold, and a value that is not one is refused", () => {
  assert.equal(
    readEnv({ LOOP_NOTIFY_EMAIL: "a@x.test", LOOP_NOTIFY_MAX_FAILURES: "9" }).notify
      ?.maxConsecutiveFailures,
    9,
  );
  assert.equal(
    readEnv({ LOOP_NOTIFY_EMAIL: "a@x.test" }).notify?.maxConsecutiveFailures,
    undefined,
    "unset is the notifier's own default, not a value invented here",
  );

  // `number()` would have dropped all of these into the default, which reads as a
  // setting that took effect. A knob that is set must mean what it says.
  for (const bad of ["0", "-2", "1.5", "three", " ", "999999999999999999999"]) {
    if (bad.trim() === "") continue;
    assert.throws(
      () => readEnv({ LOOP_NOTIFY_EMAIL: "a@x.test", LOOP_NOTIFY_MAX_FAILURES: bad }),
      (error: unknown) => LoopError.is(error) && error.code === "notify-config",
      `LOOP_NOTIFY_MAX_FAILURES="${bad}" should have been refused`,
    );
  }

  // A nonsense threshold reaches buildApp the same way — refused, not clamped.
  for (const bad of [0, -1, 1.5]) {
    assert.throws(
      () =>
        buildApp({
          cwd: "/tmp",
          notify: { to: ["a@x.test"], enabled: true, maxConsecutiveFailures: bad },
        }),
      (error: unknown) => LoopError.is(error) && error.code === "notify-config",
      `maxConsecutiveFailures=${bad} should have been refused`,
    );
  }
});

test("with no address the loop still builds, and knows it will tell nobody", () => {
  const app = buildApp({ cwd: "/tmp" });
  const notifier = app.ports.notify;
  assert.ok(notifier, "the port exists so the loop's report is a real answer");
  assert.equal(notifier?.enabled, false);
  assert.equal(notifier?.destination.length, 0);
});

test("a configured notifier reaches the loop's ports with the relay it was pointed at", () => {
  const app = buildApp({
    cwd: "/tmp",
    notify: { to: ["dev@example.test"], host: "relay.example.test", port: 2525 },
  });
  assert.equal(app.ports.notify?.enabled, true);
  assert.deepEqual(app.ports.notify?.destination, ["dev@example.test"]);
  assert.match(app.ports.notify?.transport ?? "", /relay\.example\.test:2525/u);
});
