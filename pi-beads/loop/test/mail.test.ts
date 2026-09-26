/**
 * Tests for `src/mail.ts` — the mail adapter.
 *
 * Two halves. The pure half asserts the *bytes*: quoted-printable round-trips,
 * encoded-words fold without breaking, addresses that are not addresses are
 * refused before anything is sent. The wired half runs this exact SMTP client
 * against a fake server on loopback and asserts the whole conversation — which
 * went out, what the server saw, and what came back when the server was rude.
 *
 * The fake server is in this file rather than in a support module because it is a
 * mail-shaped double with no other possible consumer, and because half the value
 * of these tests is that the transcript of the conversation is asserted right
 * next to the conversation.
 *
 * TLS: the transport takes an `upgrade` seam, and the STARTTLS tests substitute
 * it with the plaintext socket, "as if" the handshake succeeded. What those tests
 * prove is the part this module owns — that the plain session is detached before
 * the wrap (so no listener sees encrypted bytes), that EHLO is re-issued, and
 * that the session is then treated as encrypted for the purposes of deciding
 * whether a password may go out. `defaultUpgrade` itself needs a real relay with
 * a real certificate to exercise, which is out of scope here.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MailError,
  createMailer,
  createRecordingTransport,
  createSmtpTransport,
  dotStuff,
  encodeHeaderValue,
  encodeQuotedPrintable,
  foldHeader,
  parseAddress,
  readCapabilities,
  redactCommand,
  renderEmail,
  resolveSmtpConfig,
  type MailEnvelope,
  type MailTransport,
  type SmtpReply,
  type SmtpSetting,
  type SmtpTransportOptions,
} from "../src/mail.ts";
import { startFakeSmtp } from "./mail-relay.ts";

// ── helpers ─────────────────────────────────────────────────────────────────

/** A QP decoder, written here so the encoder cannot be graded on its own words. */
function decodeQuotedPrintable(encoded: string): string {
  const normalised = encoded.replace(/\r\n/gu, "\n");
  const bytes: number[] = [];
  let index = 0;
  while (index < normalised.length) {
    const char = normalised[index] ?? "";
    if (char === "=") {
      if (normalised[index + 1] === "\n") {
        index += 2; // soft break
        continue;
      }
      const hex = normalised.slice(index + 1, index + 3);
      assert.match(hex, /^[0-9A-F]{2}$/u, `bad escape ${JSON.stringify(hex)}`);
      bytes.push(parseInt(hex, 16));
      index += 3;
      continue;
    }
    bytes.push(char.codePointAt(0) ?? 63);
    index += 1;
  }
  return Buffer.from(bytes).toString("utf8");
}

const clock = (ms: number) => (): number => ms;
const fixedUid = (uid: string) => (): string => uid;

/** Split a rendered message into headers and body at the blank line. */
function splitRaw(raw: string): { headers: string[]; body: string } {
  const index = raw.indexOf("\r\n\r\n");
  assert.ok(index >= 0, "a rendered message must separate headers from body with a blank line");
  return {
    headers: raw.slice(0, index).split("\r\n"),
    body: raw.slice(index + 4),
  };
}

// ── addresses ───────────────────────────────────────────────────────────────

test("an address is validated and split into its header and envelope forms", () => {
  assert.deepEqual(parseAddress("dev@example.com"), {
    addrSpec: "dev@example.com",
    header: "dev@example.com",
  });
  assert.deepEqual(parseAddress("  Ada <ada@example.com> "), {
    addrSpec: "ada@example.com",
    header: "Ada <ada@example.com>",
  });
});

test("a string that is not an address is refused instead of going on the wire", () => {
  for (const bad of ["", "   ", "no-at-sign", "two @ signs", "spaces in@x.com", "@example.com", "a@b"]) {
    assert.throws(
      () => parseAddress(bad, "recipient"),
      (error: unknown) => MailError.is(error) && error.kind === "invalid-address",
      `${JSON.stringify(bad)} should not have passed`,
    );
  }
});

test("a line break in an address is refused as the injection it is", () => {
  // The shape a model could produce if a summary were ever pasted into a header:
  // a valid address, then a second RCPT command nobody asked for.
  assert.throws(
    () => parseAddress("dev@example.com\r\nRCPT TO:<attacker@evil.test>", "recipient"),
    (error: unknown) =>
      MailError.is(error) &&
      error.kind === "invalid-address" &&
      /injection/u.test(error.message),
  );
});

// ── header encoding ─────────────────────────────────────────────────────────

test("an ASCII header survives as itself; a non-ASCII one becomes encoded-words", () => {
  assert.equal(encodeHeaderValue("tst.2 completed: fixed the parser"), "tst.2 completed: fixed the parser");
  const encoded = encodeHeaderValue("tst.2 completed: naïve café ✓");
  assert.match(encoded, /^=\?UTF-8\?B\?/u);
  const decoded = encoded
    .split(" ")
    .map((word) => Buffer.from(word.replace(/^=\?UTF-8\?B\?/u, "").replace(/\?=$/u, ""), "base64").toString("utf8"))
    .join("");
  assert.equal(decoded, "tst.2 completed: naïve café ✓");
});

test("every encoded-word stays inside the 75-character limit RFC 2047 allows", () => {
  const long = encodeHeaderValue("émoji and accents ".repeat(12));
  for (const word of long.split(" ")) {
    assert.ok(word.length <= 75, `encoded-word too long: ${word.length} ${word}`);
  }
});

test("foldHeader keeps every physical line within the limit and never folds mid-word", () => {
  const folded = foldHeader(
    "Subject",
    "[pi-beads] tst.12 completed: a summary long enough to need folding because nobody writes short summaries",
  );
  const lines = folded.split("\r\n");
  assert.ok(lines.length > 1, "this one should have needed folding");
  for (const line of lines) assert.ok(line.length <= 78, `line too long: ${line.length} ${line}`);
  assert.ok(lines[1]?.startsWith(" "), "a continuation line starts with folding whitespace");
  // No line ends inside an encoded-word.
  for (const line of lines) {
    const opens = (line.match(/=\?UTF-8/gu) ?? []).length;
    const closes = (line.match(/\?=/gu) ?? []).length;
    assert.equal(opens, closes, `an encoded-word was split across a fold: ${line}`);
  }
});

test("a line break in a header value is folded into a space, never into a header", () => {
  const folded = foldHeader("Subject", "one\r\nEvil-Header: yes two");
  const lines = folded.split("\r\n");
  // Without the flattening this renders as `Subject: one` followed by a real
  // `Evil-Header:` header — the classic injection. With it, the text survives as
  // subject text and every line after the first starts with folding whitespace.
  assert.ok(
    lines.slice(1).every((line) => /^[ \t]/u.test(line)),
    `every folded line must start with whitespace: ${JSON.stringify(lines)}`,
  );
  assert.ok(
    lines.every((line) => !line.trimStart().startsWith("Evil-Header:") || line.startsWith(" ")),
    "the injected header must not be a header",
  );
  assert.match(folded, /^Subject: one Evil-Header: yes two$/u);
});

// ── quoted-printable ────────────────────────────────────────────────────────

test("quoted-printable round-trips ASCII, Unicode and punctuation", () => {
  const input = [
    "The parser now handles colour as well as color.",
    "Symbols that must survive: = < > é ✓ 你好",
    "",
    "No trailing content above this line.",
  ].join("\n");
  const encoded = encodeQuotedPrintable(input);
  assert.equal(decodeQuotedPrintable(encoded), input);
  assert.ok(encoded.includes("=3D"), "a literal = must be escaped");
});

test("quoted-printable never emits a line longer than the limit", () => {
  const encoded = encodeQuotedPrintable("word ".repeat(80));
  for (const line of encoded.split("\r\n")) {
    assert.ok(line.length <= 76, `line over 76 chars: ${line.length} ${JSON.stringify(line)}`);
  }
  assert.equal(decodeQuotedPrintable(encoded), "word ".repeat(80));
});

test("trailing whitespace is escaped so a soft break cannot lose it", () => {
  const encoded = encodeQuotedPrintable("trailing spaces   \nnext line");
  for (const line of encoded.split("\r\n").slice(0, -1)) {
    assert.ok(!/[ \t]$/u.test(line), `raw trailing whitespace before a break: ${JSON.stringify(line)}`);
  }
  assert.equal(decodeQuotedPrintable(encoded), "trailing spaces   \nnext line");
});

test("a 4-byte code point survives as four escaped bytes", () => {
  const encoded = encodeQuotedPrintable("🎉 party");
  assert.equal(encoded, "=F0=9F=8E=89 party");
  assert.equal(decodeQuotedPrintable(encoded), "🎉 party");
});

// ── dot stuffing ────────────────────────────────────────────────────────────

test("dotStuff doubles a leading dot and nothing else", () => {
  assert.equal(
    dotStuff("first\r\n.second\r\nthird"),
    "first\r\n..second\r\nthird",
  );
  assert.equal(dotStuff("a.b\r\nnot-a-dot"), "a.b\r\nnot-a-dot");
});

// ── rendering ───────────────────────────────────────────────────────────────

test("a rendered message has the headers a plain-text mail needs, in CRLF", () => {
  const { raw } = renderEmail(
    {
      from: "loop@example.test",
      to: ["dev@example.test", "Ada <ada@example.test>"],
      cc: "lead@example.test",
      subject: "tst.2 completed: the parser change",
      body: "Body text.",
    },
    { now: clock(Date.UTC(2025, 8, 26, 11, 3, 7)), hostname: "buildhost", uid: fixedUid("abc123") },
  );
  const { headers, body } = splitRaw(raw);
  assert.equal(headers[0], "Date: Fri, 26 Sep 2025 11:03:07 GMT");
  assert.equal(headers[1], "From: loop@example.test");
  assert.equal(headers[2], "To: dev@example.test, Ada <ada@example.test>");
  assert.equal(headers[3], "Cc: lead@example.test");
  assert.equal(headers[4], "Subject: tst.2 completed: the parser change");
  assert.equal(headers[5], "Message-ID: <abc123@buildhost>");
  assert.equal(headers[6], "MIME-Version: 1.0");
  assert.equal(headers[7], "Content-Type: text/plain; charset=utf-8");
  assert.equal(headers[8], "Content-Transfer-Encoding: quoted-printable");
  assert.equal(headers[9], "X-Mailer: pi-beads-loop");
  assert.equal(body, "Body text.\r\n");
  assert.equal(/\r[^\n]/u.test(raw), false, "every CR must be followed by LF");
});

test("a non-ASCII subject is encoded, and the body is quoted-printable", () => {
  const { raw } = renderEmail(
    {
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "tst.3 terminé — café",
      body: "Résumé: ça marche ✓",
    },
    { now: clock(0), uid: fixedUid("u1") },
  );
  const { headers, body } = splitRaw(raw);
  const subject = headers.find((line) => line.startsWith("Subject:")) ?? "";
  assert.match(subject, /=\?UTF-8\?B\?/u);
  assert.equal(decodeQuotedPrintable(body).replace(/\n$/u, ""), "Résumé: ça marche ✓");
});

test("custom headers go on, but not over the ones the renderer owns", () => {
  const withCustom = renderEmail(
    {
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "s",
      body: "b",
      headers: { "Auto-Submitted": "auto-generated", "X-Loop-Issue": "tst.9" },
    },
    { now: clock(0), uid: fixedUid("u2") },
  );
  assert.match(withCustom.raw, /Auto-Submitted: auto-generated/u);
  assert.match(withCustom.raw, /X-Loop-Issue: tst\.9/u);

  assert.throws(
    () =>
      renderEmail({
        from: "loop@example.test",
        to: "dev@example.test",
        subject: "s",
        body: "b",
        headers: { "Message-Id": "forged" },
      }),
    (error: unknown) => MailError.is(error) && error.kind === "invalid-header",
    "a shadowed Message-ID would put two on the wire",
  );
  assert.throws(
    () =>
      renderEmail({
        from: "loop@example.test",
        to: "dev@example.test",
        subject: "s",
        body: "b",
        headers: { "Bad Name": "x" },
      }),
    (error: unknown) => MailError.is(error) && error.kind === "invalid-header",
  );
});

test("a message with nobody to receive it is refused rather than silently dropped", () => {
  assert.throws(
    () => renderEmail({ from: "loop@example.test", to: [], subject: "s", body: "b" }),
    (error: unknown) => MailError.is(error) && error.kind === "no-recipients",
  );
});

test("an agent-written summary cannot smuggle a second header into the subject", () => {
  // The subject is built from a model's own sentence. If that sentence contains a
  // header break, the break has to die in the flattening, not reach the relay.
  const { raw } = renderEmail(
    {
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "done\r\nRCPT TO:<attacker@evil.test>",
      body: "harmless body",
    },
    { now: clock(0), uid: fixedUid("u3") },
  );
  const { headers } = splitRaw(raw);
  const names = headers.map((line) => (line.split(":")[0] ?? "").trim());
  assert.deepEqual(
    names,
    [
      "Date",
      "From",
      "To",
      "Subject",
      "Message-ID",
      "MIME-Version",
      "Content-Type",
      "Content-Transfer-Encoding",
      "X-Mailer",
    ],
    `nothing but the renderer's own headers may exist: ${JSON.stringify(headers)}`,
  );
  assert.match(
    headers.join(" "),
    /Subject: done RCPT TO:/u,
    "the text is kept as subject text; only the break is gone",
  );
});

// ── SMTP configuration ──────────────────────────────────────────────────────

test("the mail URL is resolved into a full SMTP config, credentials and all", () => {
  const config = resolveSmtpConfig({ url: "smtp://ada%40example.test:s3cr%25et@relay.example.test:587" });
  assert.equal(config.host, "relay.example.test");
  assert.equal(config.port, 587);
  assert.equal(config.implicitTls, false);
  assert.deepEqual(config.auth, { user: "ada@example.test", password: "s3cr%et" });
  assert.equal(config.starttls, "optional");
});

test("smtps:// means implicit TLS, and there is nothing left to upgrade", () => {
  const config = resolveSmtpConfig({ url: "smtps://relay.example.test" });
  assert.equal(config.implicitTls, true);
  assert.equal(config.port, 465, "465 is the implicit-TLS submission port");
  assert.equal(config.starttls, "off");
});

test("a port nobody named follows the security mode, not SMTP's historic 25", () => {
  assert.equal(resolveSmtpConfig({ url: "smtp://relay.example.test" }).port, 587);
  assert.equal(resolveSmtpConfig({ url: "smtp://relay.example.test?starttls=off" }).port, 25);
  assert.equal(resolveSmtpConfig({}).port, 25, "a bare default is a local relay on 25");
  assert.equal(resolveSmtpConfig({ host: "mail.test", starttls: "required" }).port, 587);
  assert.equal(resolveSmtpConfig({ host: "mail.test", port: 2525 }).port, 2525, "an explicit port is never second-guessed");
});

test("the STARTTLS knob answers the three questions it is asked", () => {
  assert.equal(resolveSmtpConfig({ starttls: "required" }).starttls, "required");
  assert.equal(resolveSmtpConfig({ starttls: "OFF" }).starttls, "off");
  assert.equal(resolveSmtpConfig({ starttls: "opportunistic" }).starttls, "optional");
  assert.throws(
    () => resolveSmtpConfig({ starttls: "maybe" }),
    (error: unknown) => MailError.is(error) && error.kind === "invalid-config",
    "a policy nobody recognises must not quietly become 'optional'",
  );
});

test("a credential that is only half there is refused", () => {
  assert.throws(
    () => resolveSmtpConfig({ user: "ada" }),
    (error: unknown) => MailError.is(error) && error.kind === "invalid-config",
  );
  assert.throws(
    () => resolveSmtpConfig({ password: "hunter2" }),
    (error: unknown) => MailError.is(error) && error.kind === "invalid-config",
  );
});

test("a URL that is not a mail URL is refused, and so is a port that is not a port", () => {
  for (const url of ["http://relay.test", "relay.test", "smtp://relay.test:99999"]) {
    assert.throws(
      () => resolveSmtpConfig({ url }),
      (error: unknown) => MailError.is(error) && error.kind === "invalid-config",
      `${url} should not have resolved`,
    );
  }
});

// ── capability parsing, and redaction ───────────────────────────────────────

test("EHLO capabilities are read from both AUTH shapes that exist in the wild", () => {
  const spaced: SmtpReply = {
    code: 250,
    lines: ["relay", "HELP", "STARTTLS", "AUTH PLAIN LOGIN", "8BITMIME", "SIZE 35882568"],
    text: "",
  };
  const caps = readCapabilities(spaced);
  assert.deepEqual(caps.authMechanisms, ["PLAIN", "LOGIN"]);
  assert.equal(caps.supportsStarttls, true);
  assert.equal(caps.supports8bitMime, true);

  const legacy: SmtpReply = { code: 250, lines: ["relay", "AUTH=CRAM-MD5 PLAIN", "STARTTLS"], text: "" };
  assert.deepEqual(readCapabilities(legacy).authMechanisms, ["CRAM-MD5", "PLAIN"]);

  const bare: SmtpReply = { code: 250, lines: ["relay only said hello"], text: "" };
  assert.deepEqual(readCapabilities(bare).authMechanisms, []);
  assert.equal(readCapabilities(bare).supportsStarttls, false);
});

test("every AUTH line is redacted, and nothing else is", () => {
  assert.equal(redactCommand("AUTH PLAIN AGFkYQBzM2NyZXQ="), "AUTH *** (credential redacted)");
  assert.equal(redactCommand("AUTH LOGIN"), "AUTH *** (credential redacted)");
  assert.equal(redactCommand("MAIL FROM:<a@b.test>"), "MAIL FROM:<a@b.test>");
  assert.equal(redactCommand("RCPT TO:<c@d.test>"), "RCPT TO:<c@d.test>");
});

// ── the mailer ──────────────────────────────────────────────────────────────

test("a mailer renders, hands over an envelope, and reports the delivery", async () => {
  const seen: MailEnvelope[] = [];
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    transport: createRecordingTransport((envelope) => seen.push(envelope)),
    now: clock(Date.UTC(2025, 0, 2, 3, 4, 5)),
    uid: fixedUid("mid"),
  });
  const delivery = await mailer.send({
    from: "loop@example.test",
    to: "dev@example.test",
    subject: "tst.4 completed",
    body: "all of it",
  });
  assert.equal(delivery.kind, "delivered");
  assert.equal(seen.length, 1);
  assert.equal(seen[0]?.from, "loop@example.test");
  assert.deepEqual(seen[0]?.recipients, ["dev@example.test"]);
  assert.match(seen[0]?.raw ?? "", /Subject: tst\.4 completed/u);
  if (delivery.kind !== "delivered") throw new Error("expected delivered");
  assert.deepEqual(delivery.recipients, ["dev@example.test"]);
});

test("a bad address is a failed delivery, never a throw at the loop", async () => {
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["not-an-address"],
    transport: createRecordingTransport(() => undefined),
  });
  const delivery = await mailer.send({
    from: "loop@example.test",
    to: "not-an-address",
    subject: "s",
    body: "b",
  });
  assert.equal(delivery.kind, "failed", "the caller must be told, not surprised");
  assert.match(
    delivery.kind === "failed" ? delivery.reason : "",
    /not a usable email address/u,
  );
});

test("a transport that throws becomes a failed delivery with a retryable flag", async () => {
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    transport: {
      name: "grumpy",
      async send(): Promise<never> {
        throw new Error("relay exploded");
      },
    },
  });
  const delivery = await mailer.send({
    from: "loop@example.test",
    to: "dev@example.test",
    subject: "s",
    body: "b",
  });
  assert.equal(delivery.kind, "failed");
  assert.match(delivery.kind === "failed" ? delivery.reason : "", /relay exploded/u);
  assert.equal(delivery.kind === "failed" ? delivery.retryable : false, true, "an unknown transport failure may pass");
});

test("a copy is delivered to the relay and named in a Cc: header, not smuggled into To:", async () => {
  // The bug this guards: `LOOP_NOTIFY_CC` puts the copy in the envelope so the
  // mail arrives, but if the renderer is handed the *combined* list the header
  // says "To: dev, lead" and no `Cc:` is written at all. The copies get the
  // mail as a blind copy and the header lies about who was addressed.
  const seen: MailEnvelope[] = [];
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    cc: ["lead@example.test"],
    transport: createRecordingTransport((envelope) => seen.push(envelope)),
  });
  const delivery = await mailer.send({
    from: "loop@example.test",
    to: ["dev@example.test"],
    subject: "tst.7 completed: the reader fix",
    body: "Body.",
  });

  assert.equal(delivery.kind, "delivered");
  assert.deepEqual(
    delivery.recipients,
    ["dev@example.test", "lead@example.test"],
    "the relay is told about the copy, or it never arrives",
  );

  const { headers } = splitRaw(seen[0]?.raw ?? "");
  assert.equal(headers[2], "To: dev@example.test", "the To: header names the addressees only");
  assert.equal(headers[3], "Cc: lead@example.test", "and the copy is named as a copy");
});

test("an address in both lists is addressed once, and delivered once", async () => {
  const seen: MailEnvelope[] = [];
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test", "lead@example.test"],
    cc: ["lead@example.test", "ada@example.test"],
    transport: createRecordingTransport((envelope) => seen.push(envelope)),
  });
  await mailer.send({
    from: "loop@example.test",
    to: ["dev@example.test", "lead@example.test"],
    subject: "tst.8 completed: the dedupe",
    body: "Body.",
  });

  assert.deepEqual(
    seen[0]?.recipients,
    ["dev@example.test", "lead@example.test", "ada@example.test"],
    "nobody is handed the same notice twice",
  );
  const { headers } = splitRaw(seen[0]?.raw ?? "");
  assert.equal(headers[3], "Cc: ada@example.test", "and the copy list no longer repeats an addressee");
});

test("a message with no recipient is skipped, not failed", async () => {
  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    transport: createRecordingTransport(() => undefined),
  });
  const delivery = await mailer.send({ from: "loop@example.test", to: [], subject: "s", body: "b" });
  assert.equal(delivery.kind, "skipped");
});

function smtpTransportFor(
  port: number,
  configOverrides: SmtpSetting = {},
  transportOverrides: Omit<SmtpTransportOptions, "config"> = {},
): MailTransport {
  return createSmtpTransport({
    config: resolveSmtpConfig({ host: "127.0.0.1", port, starttls: "off", ...configOverrides }),
    ...transportOverrides,
  });
}

// ── the conversation ────────────────────────────────────────────────────────

test("the whole conversation: greet, EHLO, MAIL FROM, RCPT TO, DATA, QUIT", async () => {
  const server = await startFakeSmtp();
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: smtpTransportFor(server.port),
      now: clock(Date.UTC(2025, 8, 26, 12, 0, 0)),
      uid: fixedUid("conv"),
    });
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "tst.7 completed: the parser change",
      body: "Bead tst.7 is closed.\n  Commit 1234abcd",
    });

    assert.equal(delivery.kind, "delivered");
    if (delivery.kind !== "delivered") throw new Error("unreachable");
    assert.equal(
      delivery.messageId,
      "q7f3a@fake.test",
      "the id the relay gave the message is the one reported, not our own guess",
    );

    const verbs = server.received.map((line) => (line.split(" ")[0] ?? "").toUpperCase());
    assert.deepEqual(verbs, ["EHLO", "MAIL", "RCPT", "DATA", "QUIT"]);
    assert.ok(
      server.received.some((line) => /^MAIL FROM:<loop@example\.test> BODY=8BITMIME$/iu.test(line)),
      "8BITMIME is offered, so the envelope must claim it",
      server.received.join(" | "),
    );
    assert.ok(server.received.some((line) => /^RCPT TO:<dev@example\.test>$/iu.test(line)));
    assert.equal(server.messages.length, 1);
    assert.match(server.messages[0] ?? "", /Subject: tst\.7 completed: the parser change/u);
    assert.match(server.messages[0] ?? "", /Content-Transfer-Encoding: quoted-printable/u);
    assert.ok(
      !server.messages[0]?.split("\r\n").includes("."),
      "a lone dot in the payload would have ended DATA early and truncated the message",
    );
  } finally {
    await server.close();
  }
});

test("every recipient gets its own RCPT TO", async () => {
  const server = await startFakeSmtp();
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["one@example.test", "Two <two@example.test>"],
      transport: smtpTransportFor(server.port),
    });
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: ["one@example.test", "Two <two@example.test>"],
      subject: "two of you",
      body: "b",
    });
    assert.equal(delivery.kind, "delivered");
    const rcpts = server.received.filter((line) => /^RCPT TO:/iu.test(line));
    assert.deepEqual(rcpts, ["RCPT TO:<one@example.test>", "RCPT TO:<two@example.test>"]);
  } finally {
    await server.close();
  }
});

test("BODY=8BITMIME is only claimed when the relay offers it", async () => {
  const server = await startFakeSmtp({ caps: ["HELP"] });
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: smtpTransportFor(server.port),
    });
    await mailer.send({ from: "loop@example.test", to: "dev@example.test", subject: "s", body: "b" });
    assert.ok(
      server.received.some((line) => /^MAIL FROM:<loop@example\.test>$/iu.test(line)),
      "no 8BITMIME claim on a relay that never offered it",
      server.received.join(" | "),
    );
  } finally {
    await server.close();
  }
});

test("a refused recipient stops the send and is reported, not swallowed", async () => {
  const server = await startFakeSmtp({
    rejectRecipient: { address: "gone@example.test", code: 550, message: "5.1.1 No such user" },
  });
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["gone@example.test"],
      transport: smtpTransportFor(server.port),
    });
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: "gone@example.test",
      subject: "s",
      body: "b",
    });
    assert.equal(delivery.kind, "failed");
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /550/u);
    assert.equal(
      server.messages.length,
      0,
      "nobody's message body was sent after the recipient was refused",
    );
  } finally {
    await server.close();
  }
});

test("AUTH PLAIN goes out when it is configured, and the password never reaches the log", async () => {
  const server = await startFakeSmtp({ caps: ["STARTTLS", "AUTH PLAIN"], requireAuth: true });
  const logged: string[] = [];
  try {
    const transport = createSmtpTransport({
      config: resolveSmtpConfig({
        host: "127.0.0.1",
        port: server.port,
        user: "ada",
        password: "s3cr3t",
        starttls: "off",
        allowInsecureAuth: true,
      }),
      logger: (line) => logged.push(line),
    });    const delivery = await transport.send({
      from: "loop@example.test",
      recipients: ["dev@example.test"],
      raw: "Subject: s\r\n\r\nb\r\n",
      messageId: "auth-test",
    });
    assert.equal(delivery.kind, "delivered", JSON.stringify(delivery));
    const authLine = server.received.find((line) => /^AUTH PLAIN /iu.test(line));
    assert.ok(authLine, "the AUTH PLAIN command must have been sent");
    const decoded = Buffer.from(authLine.replace(/^AUTH PLAIN /iu, ""), "base64").toString("utf8");
    assert.equal(decoded, "\u0000ada\u0000s3cr3t", "the SASL PLAIN token is \\0user\\0pass");
    const log = logged.join("\n");
    assert.ok(!log.includes("s3cr3t"), `the password must not appear in the log: ${log}`);
    assert.match(log, /AUTH \*\*\* \(credential redacted\)/u);
  } finally {
    await server.close();
  }
});

test("a password is never sent over a channel that is not encrypted", async () => {
  const server = await startFakeSmtp({ caps: ["HELP"] });
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: smtpTransportFor(server.port, {
        user: "ada",
        password: "s3cr3t",
        starttls: "off",
      }),
    });
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "s",
      body: "b",
    });
    assert.equal(delivery.kind, "failed");
    assert.equal(delivery.kind === "failed" ? delivery.retryable : true, false, "this will not fix itself");
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /insecure-auth/u);
    assert.ok(
      !server.received.some((line) => /^AUTH/iu.test(line)),
      "the refusal must happen before the credential is put anywhere near the wire",
    );
  } finally {
    await server.close();
  }
});

test("STARTTLS required against a relay that will not do it stops rather than downgrades", async () => {
  const server = await startFakeSmtp({ caps: ["HELP", "8BITMIME"] });
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: smtpTransportFor(server.port, { starttls: "required" }),
    });
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "s",
      body: "b",
    });
    assert.equal(delivery.kind, "failed");
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /did not advertise STARTTLS/u);
  } finally {
    await server.close();
  }
});

test("STARTTLS re-issues EHLO over the secure channel, because capabilities change there", async () => {
  const server = await startFakeSmtp({ caps: ["STARTTLS", "AUTH PLAIN"], requireAuth: true });
  try {
    const transport = smtpTransportFor(
      server.port,
      { starttls: "required", user: "ada", password: "s3cr3t" },
      {
        // The TLS layer itself is substituted here: what is under test is the
        // session handover — detach the plain listeners, wrap, re-EHLO — not the
        // handshake, which needs a relay with a real certificate.
        upgrade: async (socket) => socket,
      },
    );
    const delivery = await transport.send({
      from: "loop@example.test",
      recipients: ["dev@example.test"],
      raw: "Subject: s\r\n\r\nb\r\n",
      messageId: "starttls-test",
    });
    assert.equal(delivery.kind, "delivered", JSON.stringify(delivery));
    const verbs = server.received.map((line) => (line.split(" ")[0] ?? "").toUpperCase());
    assert.deepEqual(verbs, ["EHLO", "STARTTLS", "EHLO", "AUTH", "MAIL", "RCPT", "DATA", "QUIT"]);
  } finally {
    await server.close();
  }
});

test("a relay that stops answering costs a timeout, and the timeout is retryable", async () => {
  const server = await startFakeSmtp({ hangOnData: true });
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: smtpTransportFor(server.port, { timeoutMs: 150 }),
    });
    const started = Date.now();
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "s",
      body: "b",
    });
    const waited = Date.now() - started;
    assert.equal(delivery.kind, "failed");
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /timeout/u);
    assert.equal(delivery.kind === "failed" ? delivery.retryable : false, true);
    assert.ok(waited < 5_000, `the deadline should have fired promptly, waited ${waited}ms`);
  } finally {
    await server.close();
  }
});

test("a relay that hangs up mid-conversation is reported as what it is", async () => {
  const server = await startFakeSmtp({ dropAfterGreeting: true });
  try {
    const mailer = createMailer({
      from: "loop@example.test",
      to: ["dev@example.test"],
      transport: smtpTransportFor(server.port, { timeoutMs: 300 }),
    });
    const delivery = await mailer.send({
      from: "loop@example.test",
      to: "dev@example.test",
      subject: "s",
      body: "b",
    });
    assert.equal(delivery.kind, "failed");
    assert.match(delivery.kind === "failed" ? delivery.reason : "", /closed the connection/u);
  } finally {
    await server.close();
  }
});

test("nothing is listening: the connection failure is a delivery outcome, not an exception", async () => {
  // Take a port that was open and is now closed, so the refusal is a real
  // ECONNREFUSED rather than a made-up number.
  const gone = await startFakeSmtp();
  const port = gone.port;
  await gone.close();

  const mailer = createMailer({
    from: "loop@example.test",
    to: ["dev@example.test"],
    transport: smtpTransportFor(port, { timeoutMs: 300 }),
  });
  const delivery = await mailer.send({
    from: "loop@example.test",
    to: "dev@example.test",
    subject: "s",
    body: "b",
  });
  assert.equal(delivery.kind, "failed");
  assert.match(delivery.kind === "failed" ? delivery.reason : "", /could not reach the mail relay/u);
  assert.equal(delivery.kind === "failed" ? delivery.retryable : false, true);
});
