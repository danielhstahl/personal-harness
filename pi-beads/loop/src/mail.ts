/**
 * pi-beads loop — the mail adapter.
 *
 * **This is the only module in the app that opens a socket to a mail server, and
 * the only one that knows what an RFC 822 message looks like.** Everything above
 * it — `src/notify.ts`, the interpreter, the composition root — hands this
 * module an {@link EmailMessage} and gets back a {@link MailDelivery} saying
 * what happened to it.
 *
 * WHY THE PROTOCOL IS HAND-ROLLED RATHER THAN DEPENDENCY-BORROWED
 * The loop's whole dependency list is three packages, all of them pi. The mail
 * surface this app needs is eight commands (`EHLO`, `STARTTLS`, `AUTH`,
 * `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT`) plus a reply parser, and the tests for
 * it are stronger written against a fake SMTP server on loopback — asserting the
 * exact conversation, redaction included — than against a wrapper around
 * somebody else's client, where the interesting failure modes hide behind an
 * abstraction nobody here is auditing.
 *
 * THREE RULES, EACH WITH A TEST BEHIND IT
 *
 * 1. **Nothing here throws at its caller.** A refused relay, a bad address, a
 *    server that hangs — every one of them arrives as a {@link MailDelivery} of
 *    kind `failed`. Mail is a *report* about the run, never a dependency of it:
 *    work that is done stays done when the notice about it cannot be sent.
 *    (`MailError` is the client's internal vocabulary on its way to becoming a
 *    failed delivery; the mailer catches every one of them.)
 * 2. **Nothing reaches the wire that was not validated first.** An address is
 *    parsed down to its `addr-spec` before it goes into `RCPT TO`, and a header
 *    value carrying a CR or LF is refused or folded. This matters more here than
 *    in a normal mailer: the subject and body are assembled from an
 *    agent-written summary, and a summary containing `\r\nRCPT TO:` is header
 *    injection, not a formatting quirk.
 * 3. **A credential is never written to the log.** `AUTH` goes to the wire and
 *    comes back redacted, by construction: the session logs whatever it is told
 *    to log, and the auth commands are the only ones built with `redact: true`.
 */
import { randomUUID } from "node:crypto";
import { hostname as osHostname } from "node:os";
import { connect as netConnect } from "node:net";
import type { Socket } from "node:net";
import { connect as tlsConnect } from "node:tls";

/** What this mailer calls itself in a header. Not a claim about the transport. */
export const MAILER_ID = "pi-beads-loop";

/**
 * The machine's own name, for the `Message-ID` and for saying where a notice
 * came from.
 *
 * `os.hostname()` is not always a fully-qualified name and is occasionally empty;
 * an empty host half of a `Message-ID` is a message no mail server will accept,
 * so the fallback is a name rather than nothing.
 */
export function localHostname(): string {
  const name = osHostname().trim();
  return name === "" ? "localhost" : name;
}

/**
 * A sender address built from this machine's name, shaped so a relay will take it.
 *
 * A container's hostname is one label (`4d33c207da29`), and most relays refuse a
 * sender whose domain cannot be a domain — a refusal that looks like a broken
 * relay and is really a broken default. `localhost.localdomain` is the reserved
 * stand-in for exactly that case, so the fallback is ugly-and-accepted rather
 * than plausible-and-rejected. Anyone running a real relay sets
 * `LOOP_NOTIFY_FROM` and never sees it.
 */
export function defaultFromAddress(localPart: string = MAILER_ID): string {
  const host = localHostname();
  const domain = host.includes(".") ? host : "localhost.localdomain";
  return `${localPart}@${domain}`;
}

/** Hard cap on a header line, per RFC 5322. */
const MAX_HEADER_LINE = 78;
/** Data bytes per RFC 2047 encoded-word: keeps `=?UTF-8?B?…?=` under 75 chars. */
const ENCODED_WORD_BYTES = 45;

// ── the message ─────────────────────────────────────────────────────────────

/**
 * A message as the layers above think of it: addresses, a subject, a body. No
 * MIME, no encoding, no line endings — those are this module's job, so the
 * layers above cannot get them wrong.
 */
export interface EmailMessage {
  readonly to: string | readonly string[];
  readonly from: string;
  readonly cc?: string | readonly string[];
  readonly replyTo?: string;
  readonly subject: string;
  readonly body: string;
  /** Extra headers. Values must not carry a line break. */
  readonly headers?: Readonly<Record<string, string>>;
}

/** Headers the renderer owns; a custom header may not shadow one of them. */
const RESERVED_HEADERS: readonly string[] = [
  "date",
  "from",
  "to",
  "cc",
  "reply-to",
  "subject",
  "message-id",
  "mime-version",
  "content-type",
  "content-transfer-encoding",
  "x-mailer",
];

/**
 * What went wrong, in the client's own words.
 *
 * It never escapes the module as a throw: `createMailer` turns every one of
 * these into a failed {@link MailDelivery}. It is exported so the transport can
 * be tested at its own level, and so `retryable` can carry a real answer rather
 * than make a reader guess whether "connection reset" deserves another try.
 */
export class MailError extends Error {
  readonly kind: string;
  readonly retryable: boolean;

  constructor(kind: string, message: string, retryable = false) {
    super(message);
    this.name = "MailError";
    this.kind = kind;
    this.retryable = retryable;
  }

  static is(value: unknown): value is MailError {
    return value instanceof MailError;
  }
}

// ── addresses ───────────────────────────────────────────────────────────────

/**
 * An address in the two forms the protocol needs: the one that goes in a header
 * (which may carry a display name) and the one that goes in the envelope
 * (`MAIL FROM`/`RCPT TO`, which may not).
 */
export interface ParsedAddress {
  /** The bare `addr-spec`: `dev@example.com`. */
  readonly addrSpec: string;
  /** What a header should say: the display form if given, else the addr-spec. */
  readonly header: string;
}

/** A pragmatic RFC 5322 `addr-spec`. Quoted local parts and IP literals are not accepted. */
const ADDR_SPEC =
  /^[A-Za-z0-9._%+!#$&*/=?^`{|}~-]+@[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?)+$/u;

/** Control characters, with CR and LF called out separately in the message. */
const CONTROL_CHARS = /[\u0000-\u0008\u000b\u000c\u000e-\u001f]/u;

/**
 * Parse and validate one address.
 *
 * A display name is allowed and kept for the header; the envelope gets the bare
 * `addr-spec`. A line break in either half is a hard refusal — that is how an
 * unvalidated string turns into a recipient nobody asked for.
 */
export function parseAddress(raw: string, label = "address"): ParsedAddress {
  const text = raw.trim();
  if (text === "") throw new MailError("invalid-address", `${label} is empty`);
  if (/[\r\n]/u.test(text)) {
    throw new MailError(
      "invalid-address",
      `${label} contains a line break, which in a header position is injection`,
    );
  }
  if (CONTROL_CHARS.test(text)) {
    throw new MailError("invalid-address", `${label} contains a control character`);
  }
  const bracketed = /^([^<>]*)<([^<>\s]+)>$/u.exec(text);
  const addrSpec = (bracketed?.[2] ?? text).trim();
  if (!ADDR_SPEC.test(addrSpec)) {
    throw new MailError(
      "invalid-address",
      `${label} ${JSON.stringify(raw)} is not a usable email address ` +
        "(expected something like name@example.com)",
    );
  }
  return { addrSpec, header: text };
}

export function parseAddresses(raw: readonly string[], label: string): ParsedAddress[] {
  return raw.map((entry, index) => parseAddress(entry, `${label}[${index}]`));
}

function addressHeader(addresses: readonly ParsedAddress[]): string {
  return addresses.map((entry) => entry.header).join(", ");
}

/** Normalise `string | string[]`, dropping blanks. A single address is the norm. */
export function toList(value: string | readonly string[] | undefined): string[] {
  if (value === undefined) return [];
  const list = typeof value === "string" ? [value] : [...value];
  return list.map((entry) => String(entry).trim()).filter((entry) => entry !== "");
}

/**
 * The envelope's recipient list: every address once, in the order first given.
 *
 * The envelope and the headers are different documents — an address in `Cc:`
 * still has to be in `RCPT TO` or the copy never arrives — so the two lists get
 * merged here. Deduplicated because a person listed in both fields would
 * otherwise be handed the same notice twice by the relay.
 */
function envelopeRecipients(list: readonly string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const entry of parseAddresses(list, "recipient address")) {
    const key = entry.addrSpec.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(entry.addrSpec);
  }
  return out;
}

// ── header encoding ─────────────────────────────────────────────────────────

/** A header value with line breaks taken out of it. */
function flattenHeaderValue(value: string): string {
  return value.replace(/[\r\n]+/gu, " ").replace(/\t/gu, " ").replace(CONTROL_CHARS, " ");
}

function isAsciiPrintable(value: string): boolean {
  return /^[\x20-\x7e]*$/u.test(value);
}

/** RFC 2047 encoded-word, base64 flavour. */
function encodedWord(chunk: string): string {
  return `=?UTF-8?B?${Buffer.from(chunk, "utf8").toString("base64")}?=`;
}

/**
 * A header value that is safe on the wire: ASCII when it can be, encoded-words
 * when it cannot.
 */
export function encodeHeaderValue(value: string): string {
  const flat = flattenHeaderValue(value);
  if (isAsciiPrintable(flat)) return flat;
  const words: string[] = [];
  let buffer = "";
  let bytes = 0;
  for (const char of flat) {
    const charBytes = Buffer.byteLength(char, "utf8");
    if (buffer !== "" && bytes + charBytes > ENCODED_WORD_BYTES) {
      words.push(encodedWord(buffer));
      buffer = "";
      bytes = 0;
    }
    buffer += char;
    bytes += charBytes;
  }
  if (buffer !== "") words.push(encodedWord(buffer));
  return words.join(" ");
}

/**
 * `Name: value` as one or more physical lines, folded at whitespace.
 *
 * Folding moves whole words, so an encoded-word is never split — a broken
 * `=?UTF-8?B?` across a fold is unreadable rather than merely ugly.
 */
export function foldHeader(name: string, value: string): string {
  const encoded = encodeHeaderValue(value);
  const prefix = `${name}: `;
  if (prefix.length + encoded.length <= MAX_HEADER_LINE) return `${prefix}${encoded}`;

  const lines: string[] = [];
  let line = prefix;
  let continued = false;
  for (const word of encoded.split(" ")) {
    const lead = continued ? " " : "";
    if (continued && line.length + lead.length + word.length > MAX_HEADER_LINE) {
      lines.push(line);
      line = " ";
    }
    line = continued && line !== " " ? `${line} ${word}` : `${line}${lead}${word}`;
    continued = true;
  }
  lines.push(line);
  return lines.join("\r\n");
}

// ── quoted-printable ────────────────────────────────────────────────────────

function hexByte(byte: number): string {
  return `=${byte.toString(16).toUpperCase().padStart(2, "0")}`;
}

/** Escape every byte of one character (a multi-byte char becomes several escapes). */
function escapeChar(char: string): string {
  return [...Buffer.from(char, "utf8")].map((byte) => hexByte(byte)).join("");
}

/**
 * A logical line as units: one per character, an escape sequence counting as one
 * unit that carries its own encoded length.
 */
function qpUnits(line: string): string[] {
  const units: string[] = [];
  for (const char of line) {
    const code = char.codePointAt(0) ?? 0;
    // A space stays literal (it is the whole readability of a plain-text body);
    // a tab is escaped, because a tab that survives a fold or a trim is a
    // formatting bug nobody can see.
    const literal = (code >= 33 && code <= 126) || code === 32;
    units.push(literal ? (char === "=" ? "=3D" : char) : escapeChar(char));
  }
  return units;
}

/**
 * A closing physical line may not end in a raw space: some servers trim it and
 * the content is quietly changed. Escaping the last one is enough, and costs two
 * characters, which is why the content limit leaves three of headroom.
 */
function closeQpLine(line: string, softBreak: boolean): string {
  const closed = / $/u.test(line) ? `${line.slice(0, -1)}=20` : line;
  return softBreak ? `${closed}=` : closed;
}

/**
 * One logical line, quoted-printably encoded and soft-wrapped.
 *
 * `=` is always escaped, so an `=` at the end of a physical line is always the
 * soft break and never content. Printable ASCII — spaces included — stays
 * literal: the common case, an English sentence, survives readable in the raw
 * message, which is worth something when a delivery has to be debugged out of a
 * mail log. The one thing that never survives is a trailing raw space, and that
 * is exactly the thing `closeQpLine` refuses to emit.
 */
function encodeQpLine(line: string, maxLineLength: number): string[] {
  // `=` for the soft break, plus the two characters escaping a trailing space
  // costs, so the emitted line never exceeds `maxLineLength`.
  const contentMax = Math.max(16, maxLineLength - 3);
  const out: string[] = [];
  let current = "";
  for (const unit of qpUnits(line)) {
    if (current !== "" && current.length + unit.length > contentMax) {
      out.push(closeQpLine(current, true));
      current = unit;
      continue;
    }
    current += unit;
  }
  out.push(closeQpLine(current, false));
  return out;
}

/**
 * A whole body, quoted-printably encoded with CRLF line endings.
 *
 * Newlines are normalised first: a soft break only means anything in the line
 * endings the protocol actually uses.
 */
export function encodeQuotedPrintable(text: string, maxLineLength = 76): string {
  return text
    .replace(/\r\n|\r|\n/gu, "\n")
    .split("\n")
    .flatMap((line) => encodeQpLine(line, maxLineLength))
    .join("\r\n");
}

/**
 * Dot-stuffing: a wire rule, not a message rule.
 *
 * `DATA` ends at a line holding a single `.`, so a body line that *starts* with
 * one must have it doubled. Quoted-printable already turns a leading `.` into
 * `=2E`, so this is defence against a payload that did not come from
 * {@link renderEmail}.
 */
export function dotStuff(message: string): string {
  const withCrlf = message.replace(/\r\n|\r|\n/gu, "\r\n");
  return withCrlf
    .split("\r\n")
    .map((line) => (line.startsWith(".") ? `.${line}` : line))
    .join("\r\n");
}

export interface RenderOptions {
  readonly now?: () => number;
  readonly hostname?: string;
  readonly uid?: () => string;
}

export interface RenderedEmail {
  readonly raw: string;
  readonly messageId: string;
}

/**
 * Render a message into the bytes a transport sends.
 *
 * Header order is fixed and every line ends CRLF, so two calls with the same
 * clock and the same `uid` produce the same string — which makes "what went
 * out" something a test can assert rather than something to infer.
 */
export function renderEmail(message: EmailMessage, options: RenderOptions = {}): RenderedEmail {
  const now = options.now?.() ?? Date.now();
  const hostname = options.hostname ?? "localhost";
  const uid = options.uid?.() ?? randomUUID();
  const messageId = `${uid}@${hostname}`;

  const from = parseAddress(message.from, "from address");
  const to = parseAddresses(toList(message.to), "to address");
  if (to.length === 0) {
    throw new MailError("no-recipients", "a message with nobody to go to is not a message");
  }
  const cc = parseAddresses(toList(message.cc ?? []), "cc address");
  const replyTo =
    message.replyTo === undefined ? null : parseAddress(message.replyTo, "reply-to address");

  const lines: string[] = [
    `Date: ${new Date(now).toUTCString()}`,
    `From: ${addressHeader([from])}`,
    `To: ${addressHeader(to)}`,
  ];
  if (cc.length > 0) lines.push(`Cc: ${addressHeader(cc)}`);
  if (replyTo !== null) lines.push(`Reply-To: ${replyTo.header}`);
  lines.push(foldHeader("Subject", message.subject));
  lines.push(`Message-ID: <${messageId}>`);
  lines.push("MIME-Version: 1.0");
  lines.push("Content-Type: text/plain; charset=utf-8");
  lines.push("Content-Transfer-Encoding: quoted-printable");
  lines.push(`X-Mailer: ${MAILER_ID}`);

  for (const [name, value] of Object.entries(message.headers ?? {})) {
    const clean = name.trim();
    if (!/^[!-9;-~]+$/u.test(clean)) {
      throw new MailError("invalid-header", `${JSON.stringify(name)} is not a usable header name`);
    }
    if (RESERVED_HEADERS.includes(clean.toLowerCase())) {
      throw new MailError(
        "invalid-header",
        `header ${clean} belongs to the renderer; passing it too would put two on the wire`,
      );
    }
    lines.push(foldHeader(clean, value));
  }

  const body = encodeQuotedPrintable(message.body);
  const raw = `${lines.join("\r\n")}\r\n\r\n${body}${body.endsWith("\r\n") ? "" : "\r\n"}`;
  return { raw, messageId };
}

// ── delivery ────────────────────────────────────────────────────────────────

/**
 * What a send attempt ended as. Every path out of {@link Mailer.send} is one of
 * these three, including the paths where nothing was attempted.
 */
export type MailDelivery =
  | {
      readonly kind: "delivered";
      readonly messageId: string;
      readonly transport: string;
      readonly recipients: readonly string[];
    }
  | { readonly kind: "skipped"; readonly reason: string }
  | {
      readonly kind: "failed";
      readonly reason: string;
      readonly transport: string;
      readonly retryable: boolean;
    };

/** One line for a log: what happened, and where. */
export function deliveryText(delivery: MailDelivery): string {
  switch (delivery.kind) {
    case "delivered":
      return `delivered via ${delivery.transport} to ${delivery.recipients.join(", ")} (id ${delivery.messageId})`;
    case "skipped":
      return `skipped: ${delivery.reason}`;
    case "failed":
      return `failed via ${delivery.transport}: ${delivery.reason}` +
        (delivery.retryable ? " (retryable)" : " (not retryable)");
  }
}

export interface MailEnvelope {
  readonly from: string;
  readonly recipients: readonly string[];
  /** The rendered message: CRLF endings, final CRLF included, unstuffed. */
  readonly raw: string;
  readonly messageId: string;
}

/**
 * The one method a transport has. `raw` is already rendered; a transport that
 * reformats it is out of contract.
 */
export interface MailTransport {
  readonly name: string;
  send(envelope: MailEnvelope): Promise<MailDelivery>;
}

/**
 * A transport that sends nothing and says why, in the same shape as one that does.
 * Handy for a run that must not put a byte on the network but should still report
 * what it would have done.
 */
export function createRecordingTransport(recorder: (envelope: MailEnvelope) => void): MailTransport {
  return {
    name: "recorder",
    async send(envelope: MailEnvelope): Promise<MailDelivery> {
      recorder(envelope);
      return {
        kind: "delivered",
        messageId: envelope.messageId,
        transport: "recorder",
        recipients: envelope.recipients,
      };
    },
  };
}

/** What a caller above the transport sees: an enabled flag, the targets, `send`. */
export interface Mailer {
  readonly enabled: boolean;
  readonly from: string | null;
  /** Every address this mailer will hand to the relay: the addressees and the copies. */
  readonly recipients: readonly string[];
  /** The primary addressees, as distinct from the copies. */
  readonly to: readonly string[];
  /** The copies. Delivered, and named in a `Cc:` header — not blind. */
  readonly cc: readonly string[];
  readonly transport: string;
  send(message: EmailMessage): Promise<MailDelivery>;
}

/**
 * A mailer that sends nothing, ever.
 *
 * The reason is carried so that the *reason* reaches the log rather than the
 * absence of a mailer being the whole message: "no address was set" and "this
 * run has mail switched off" are different answers.
 */
export function createNullMailer(reason = "no mail recipient is configured"): Mailer {
  return {
    enabled: false,
    from: null,
    recipients: [],
    to: [],
    cc: [],
    transport: "none",
    send: async () => ({ kind: "skipped", reason }),
  };
}

export interface MailerOptions {
  readonly from: string;
  readonly to: readonly string[];
  readonly cc?: readonly string[];
  readonly transport: MailTransport;
  readonly hostname?: string;
  readonly now?: () => number;
  readonly uid?: () => string;
  readonly logger?: (line: string) => void;
}

/** Why it did not go, with the kind first: `smtp-550: …` reads as a fact. */
function describeFailure(error: unknown): string {
  if (MailError.is(error)) return `${error.kind}: ${error.message}`;
  return error instanceof Error ? error.message : String(error);
}

/**
 * Bind a transport to an identity and a recipient list.
 *
 * `send` never throws: validation, rendering and the transport call all sit
 * inside the same catch, because the caller is a loop that has already done the
 * work this message is *about*.
 */
export function createMailer(options: MailerOptions): Mailer {
  const to = toList(options.to);
  const cc = toList(options.cc);
  const recipients = [...to, ...cc];
  const log = options.logger ?? (() => undefined);

  return {
    enabled: recipients.length > 0,
    from: options.from,
    recipients,
    to,
    cc,
    transport: options.transport.name,
    async send(message: EmailMessage): Promise<MailDelivery> {
      // `to` is a required field, so an empty one is a statement: this message
      // is addressed to nobody. It is skipped rather than quietly redirected to
      // the mailer's own list, which would deliver a message the caller did not
      // ask to send.
      const messageTo = toList(message.to);
      // The copies are the opposite case: `cc` is optional, so an absent one is
      // "the caller has no opinion" and the configured list applies — minus
      // anybody already addressed, because a person in both `To:` and `Cc:` is a
      // header that contradicts itself.
      const messageCc =
        message.cc === undefined
          ? cc.filter((entry) => !messageTo.includes(entry))
          : toList(message.cc);
      if (messageTo.length === 0) {
        return { kind: "skipped", reason: "no recipient on the message" };
      }
      let envelope: MailEnvelope;
      try {
        const rendered = renderEmail(
          { ...message, to: messageTo, ...(messageCc.length === 0 ? {} : { cc: messageCc }) },
          {
            ...(options.hostname === undefined ? {} : { hostname: options.hostname }),
            ...(options.now === undefined ? {} : { now: options.now }),
            ...(options.uid === undefined ? {} : { uid: options.uid }),
          },
        );
        envelope = {
          from: parseAddress(message.from, "from address").addrSpec,
          // The envelope is addressees *and* copies: the relay needs both, and
          // they are deduplicated here rather than letting one person on the list
          // twice get the same notice twice.
          recipients: envelopeRecipients([...messageTo, ...messageCc]),
          raw: rendered.raw,
          messageId: rendered.messageId,
        };
      } catch (error) {
        const reason = describeFailure(error);
        log(`mail refused before sending: ${reason}`);
        return {
          kind: "failed",
          reason,
          transport: options.transport.name,
          retryable: MailError.is(error) ? error.retryable : false,
        };
      }
      try {
        const delivery = await options.transport.send(envelope);
        log(`mail ${deliveryText(delivery)}`);
        return delivery;
      } catch (error) {
        const reason = describeFailure(error);
        log(`mail failed: ${reason}`);
        return {
          kind: "failed",
          reason,
          transport: options.transport.name,
          retryable: MailError.is(error) ? error.retryable : true,
        };
      }
    },
  };
}

// ── SMTP: configuration ─────────────────────────────────────────────────────

/** What to do about upgrading a plain session with `STARTTLS`. */
export type StarttlsPolicy = "required" | "optional" | "off";

export interface SmtpConfig {
  readonly host: string;
  readonly port: number;
  /** Implicit TLS for the whole session: `smtps://`, the 465 submission port. */
  readonly implicitTls: boolean;
  readonly starttls: StarttlsPolicy;
  readonly auth?: { readonly user: string; readonly password: string };
  readonly timeoutMs: number;
  /**
   * Allow a password on a channel that is not encrypted. Off by default, and
   * "off" is the answer that costs nothing: a relay that genuinely needs it is a
   * relay the operator can name out loud.
   */
  readonly allowInsecureAuth: boolean;
  /** The name announced in `EHLO`/`HELO`. */
  readonly helloHost: string;
}

/** The loose shape the environment gives this module. */
export interface SmtpSetting {
  readonly url?: string;
  readonly host?: string;
  readonly port?: number;
  readonly user?: string;
  readonly password?: string;
  readonly starttls?: string;
  readonly timeoutMs?: number;
  readonly allowInsecureAuth?: boolean;
  readonly helloHost?: string;
}

export const DEFAULT_SMTP_HOST = "127.0.0.1";
export const DEFAULT_SMTP_TIMEOUT_MS = 15_000;

/**
 * The port a mode means when nobody named one.
 *
 * `smtps` is 465. A named relay with transport security is 587, the submission
 * port every provider built since 2008 listens on. And nothing named at all is
 * the documented default: a relay on this machine, which lives on 25.
 */
export function defaultPortFor(mode: {
  implicitTls: boolean;
  starttls: StarttlsPolicy;
  hostNamed?: boolean;
}): number {
  if (mode.implicitTls) return 465;
  if (mode.hostNamed !== true) return 25;
  return mode.starttls === "off" ? 25 : 587;
}

function starttlsPolicy(raw: string | undefined): StarttlsPolicy {
  const value = raw?.trim().toLowerCase();
  if (value === undefined || value === "") return "optional";
  if (value === "required" || value === "require" || value === "must" || value === "1" || value === "true") {
    return "required";
  }
  if (value === "off" || value === "disable" || value === "disabled" || value === "0" || value === "false") {
    return "off";
  }
  if (
    value === "optional" ||
    value === "opportunistic" ||
    value === "try" ||
    value === "auto" ||
    value === "yes"
  ) {
    return "optional";
  }
  throw new MailError(
    "invalid-config",
    `unknown STARTTLS policy ${JSON.stringify(raw)} (expected required, optional or off)`,
  );
}

/**
 * A resolved SMTP configuration from a loose setting bag.
 *
 * `url` wins where it speaks; the discrete knobs fill in the rest. A port that
 * nobody named follows the security mode: `smtp://mail.example.com` with
 * `LOOP_MAIL_STARTTLS=require` means 587 on every submission server built since
 * 2008, while an explicitly configured port is never second-guessed.
 */
export function resolveSmtpConfig(setting: SmtpSetting = {}): SmtpConfig {
  const url = setting.url?.trim();
  let host: string | undefined;
  let port: number | undefined;
  let user: string | undefined;
  let password: string | undefined;
  let implicitTls = false;
  let starttls: StarttlsPolicy | undefined;
  let timeoutMs: number | undefined;
  let allowInsecureAuth: boolean | undefined;

  if (url !== undefined && url !== "") {
    let parsed: URL;
    try {
      parsed = new URL(url);
    } catch {
      throw new MailError("invalid-config", `the mail URL ${JSON.stringify(url)} does not parse`);
    }
    if (parsed.protocol === "smtps:") implicitTls = true;
    else if (parsed.protocol !== "smtp:") {
      throw new MailError(
        "invalid-config",
        `the mail URL must be smtp:// or smtps://, not ${JSON.stringify(parsed.protocol)}`,
      );
    }
    host = parsed.hostname === "" ? undefined : parsed.hostname;
    if (parsed.port !== "") {
      const rawPort = Number(parsed.port);
      if (!Number.isInteger(rawPort) || rawPort <= 0 || rawPort > 65_535) {
        throw new MailError(
          "invalid-config",
          `port ${JSON.stringify(parsed.port)} in the mail URL is not a port`,
        );
      }
      port = rawPort;
    }
    try {
      if (parsed.username !== "") user = decodeURIComponent(parsed.username);
      if (parsed.password !== "") password = decodeURIComponent(parsed.password);
    } catch {
      throw new MailError(
        "invalid-config",
        "the credential in the mail URL is not percent-decodable (a raw % must be %25)",
      );
    }
    const queryStarttls = parsed.searchParams.get("starttls");
    if (queryStarttls !== null) starttls = starttlsPolicy(queryStarttls);
    const queryTls = parsed.searchParams.get("tls");
    if (queryTls === "1" || queryTls === "true") implicitTls = true;
    if (queryTls === "0" || queryTls === "false") implicitTls = false;
    const queryTimeout = parsed.searchParams.get("timeout");
    if (queryTimeout !== null && queryTimeout !== "") {
      const parsedTimeout = Number(queryTimeout);
      if (!Number.isFinite(parsedTimeout) || parsedTimeout <= 0) {
        throw new MailError(
          "invalid-config",
          `timeout ${JSON.stringify(queryTimeout)} in the mail URL is not a number of milliseconds`,
        );
      }
      timeoutMs = Math.trunc(parsedTimeout);
    }
    const queryInsecure = parsed.searchParams.get("allowinsecureauth");
    if (queryInsecure === "1" || queryInsecure === "true") allowInsecureAuth = true;
    if (queryInsecure === "0" || queryInsecure === "false") allowInsecureAuth = false;
  }

  const resolvedStarttls = starttls ?? starttlsPolicy(setting.starttls);
  const namedHost = setting.host?.trim() || host;
  const resolvedHost = namedHost ?? DEFAULT_SMTP_HOST;
  const resolvedPort =
    setting.port ??
    port ??
    defaultPortFor({
      implicitTls,
      starttls: resolvedStarttls,
      hostNamed: namedHost !== undefined && namedHost !== "",
    });
  const resolvedUser = setting.user?.trim() || user;
  const resolvedPassword = setting.password ?? password;
  const hasUser = resolvedUser !== undefined && resolvedUser !== "";
  const hasPassword = resolvedPassword !== undefined && resolvedPassword !== "";
  if (hasUser && !hasPassword) {
    throw new MailError(
      "invalid-config",
      `user ${JSON.stringify(resolvedUser)} was given without a password`,
    );
  }
  if (!hasUser && hasPassword) {
    throw new MailError("invalid-config", "a password was given without a user");
  }

  return {
    host: resolvedHost,
    port: resolvedPort,
    implicitTls,
    // An implicit-TLS session has nothing left to upgrade.
    starttls: implicitTls ? "off" : resolvedStarttls,
    ...(hasUser ? { auth: { user: resolvedUser as string, password: resolvedPassword as string } } : {}),
    timeoutMs: setting.timeoutMs ?? timeoutMs ?? DEFAULT_SMTP_TIMEOUT_MS,
    allowInsecureAuth: setting.allowInsecureAuth ?? allowInsecureAuth ?? false,
    helloHost: setting.helloHost?.trim() || MAILER_ID,
  };
}

// ── SMTP: the session ───────────────────────────────────────────────────────

export interface SmtpReply {
  readonly code: number;
  /** Every line of the reply, continuation lines included, code stripped. */
  readonly lines: readonly string[];
  readonly text: string;
}

/** The slice of `net.Socket` / `tls.TLSSocket` this client uses. */
export interface SocketLike {
  on(event: "data", listener: (chunk: Buffer) => void): unknown;
  on(event: "error", listener: (error: Error) => void): unknown;
  on(event: "close", listener: () => void): unknown;
  removeAllListeners(event: "data" | "error" | "close"): unknown;
  write(data: string): unknown;
  end(): unknown;
  destroy(error?: Error): unknown;
}

/** `AUTH` and everything after it is a credential; nothing else is. */
export function redactCommand(line: string): string {
  const verb = (line.split(" ")[0] ?? "").toUpperCase();
  return verb === "AUTH" ? "AUTH *** (credential redacted)" : line;
}

interface Waiter {
  readonly resolve: (reply: SmtpReply) => void;
  readonly reject: (error: MailError) => void;
  readonly timer: ReturnType<typeof setTimeout>;
}

/**
 * One SMTP conversation: bytes in, replies out, one command outstanding at a
 * time.
 *
 * Multi-line replies (`250-line` then `250 line`) are folded here so a caller
 * never has to know they exist. Every wait carries a timer, because a mail server
 * that stops answering is common enough that "hang forever" is not an acceptable
 * answer for a loop with work to do.
 */
class SmtpSession {
  private buffer = "";
  private partial: string[] = [];
  private waiter: Waiter | null = null;
  private failure: MailError | null = null;
  /** Node strips types; it does not implement parameter properties. Assign by hand. */
  private readonly socket: SocketLike;
  private readonly timeoutMs: number;
  private readonly log: (line: string) => void;

  constructor(socket: SocketLike, timeoutMs: number, log: (line: string) => void) {
    this.socket = socket;
    this.timeoutMs = timeoutMs;
    this.log = log;
    socket.on("data", (chunk: Buffer) => this.consume(chunk));
    socket.on("error", (error: Error) =>
      this.abandon(new MailError("connection-failed", `the mail socket failed: ${error.message}`, true)),
    );
    socket.on("close", () =>
      this.abandon(
        new MailError(
          "connection-closed",
          "the mail server closed the connection mid-conversation",
          true,
        ),
      ),
    );
  }

  /**
   * Stop listening without destroying the socket — the STARTTLS handover.
   *
   * The TLS layer wraps this same fd. Leaving plaintext `data` listeners attached
   * would have them handed the encrypted bytes, which is both a corrupt log and a
   * corrupt reply parser, so the handover is always detach-then-wrap.
   */
  detach(): SocketLike {
    try {
      this.socket.removeAllListeners("data");
      this.socket.removeAllListeners("error");
      this.socket.removeAllListeners("close");
    } catch {
      // A socket that will not let go is nobody's problem from here.
    }
    this.clearWaiter(
      new MailError("connection-reset", "the plain session ended before the TLS session began", true),
    );
    return this.socket;
  }

  close(): void {
    try {
      this.socket.destroy();
    } catch {
      // Nothing is learned from a socket that cannot be closed.
    }
  }

  /** Wait for a reply nobody asked for — the greeting. */
  expect(codes: readonly number[], what: string): Promise<SmtpReply> {
    if (this.failure !== null) return Promise.reject(this.failure);
    return new Promise<SmtpReply>((resolve, reject) => {
      const timer = setTimeout(() => {
        const error = new MailError(
          "timeout",
          `the mail server sent nothing within ${this.timeoutMs}ms while waiting for ${what}`,
          true,
        );
        this.abandon(error);
        reject(error);
      }, this.timeoutMs);
      this.waiter = { resolve, reject, timer };
    }).then((reply) => {
      if (!codes.includes(reply.code)) {
        throw new MailError(
          `smtp-${reply.code}`,
          `unexpected reply to ${what}: ${reply.code} ${reply.text}`,
          reply.code >= 400 && reply.code < 500,
        );
      }
      return reply;
    });
  }

  /** Send a line and wait for its reply. */
  async command(
    line: string,
    codes: readonly number[],
    what: string,
    options: { readonly redact?: boolean } = {},
  ): Promise<SmtpReply> {
    this.log(`C: ${options.redact === true ? redactCommand(line) : line}`);
    this.socket.write(`${line}\r\n`);
    return this.expect(codes, what);
  }

  /** Write a DATA payload: dot-stuffed, and terminated with the lone dot. */
  writeData(payload: string, log: (line: string) => void = this.log): void {
    const stuffed = dotStuff(payload);
    log(`C: DATA <${stuffed.length} bytes>`);
    const framed = `${stuffed}${stuffed.endsWith("\r\n") ? "" : "\r\n"}`;
    this.socket.write(`${framed}.\r\n`);
  }

  private clearWaiter(error: MailError): void {
    const waiter = this.waiter;
    if (waiter === null) return;
    this.waiter = null;
    clearTimeout(waiter.timer);
    waiter.reject(error);
  }

  private abandon(error: MailError): void {
    if (this.failure === null) this.failure = error;
    this.clearWaiter(error);
  }

  private consume(chunk: Buffer): void {
    this.buffer += chunk.toString("utf8");
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline).replace(/\r$/u, "");
      this.buffer = this.buffer.slice(newline + 1);
      const match = /^(\d{3})([ -])(.*)$/u.exec(line);
      if (match === null) {
        this.log(`S: unparsable line ${JSON.stringify(line)}`);
        this.abandon(
          new MailError(
            "protocol-error",
            `the mail server sent a line that is not an SMTP reply: ${JSON.stringify(line)}`,
          ),
        );
        return;
      }
      const code = Number(match[1]);
      this.partial.push(match[3] ?? "");
      if (match[2] === "-") continue;
      const lines = [...this.partial];
      this.partial = [];
      this.log(`S: ${code} ${lines.join(" / ")}`);
      const waiter = this.waiter;
      // An unsolicited reply is logged and dropped: inventing a resolution for a
      // command nobody has outstanding is worse than ignoring it.
      if (waiter === null) continue;
      this.waiter = null;
      clearTimeout(waiter.timer);
      waiter.resolve({ code, lines, text: lines.join(" ") });
    }
  }
}

// ── SMTP: capabilities ──────────────────────────────────────────────────────

export interface SmtpCapabilities {
  readonly extensions: ReadonlyMap<string, string>;
  readonly authMechanisms: readonly string[];
  readonly supportsStarttls: boolean;
  readonly supports8bitMime: boolean;
}

/**
 * Read the EHLO extension lines.
 *
 * `AUTH PLAIN LOGIN` and the legacy `AUTH=PLAIN LOGIN` are both real and both
 * out there; a client that parses only one fails against a server that works.
 */
export function readCapabilities(reply: SmtpReply): SmtpCapabilities {
  const extensions = new Map<string, string>();
  const mechanisms = new Set<string>();
  for (const line of reply.lines.slice(1)) {
    const match = /^([A-Za-z0-9-]+)(?:[ =](.*))?$/u.exec(line.trim());
    if (match === null) continue;
    const name = (match[1] ?? "").toUpperCase();
    const params = match[2] ?? "";
    extensions.set(name, params);
    if (name === "AUTH") {
      for (const mechanism of params.split(/[\s,]+/u)) {
        if (mechanism !== "") mechanisms.add(mechanism.toUpperCase());
      }
    }
  }
  return {
    extensions,
    authMechanisms: [...mechanisms],
    supportsStarttls: extensions.has("STARTTLS"),
    supports8bitMime: extensions.has("8BITMIME"),
  };
}

// ── SMTP: the transport ─────────────────────────────────────────────────────

export interface SmtpTransportOptions {
  readonly config: SmtpConfig;
  /** Test seam: where the socket comes from. */
  readonly connect?: (options: {
    host: string;
    port: number;
    timeoutMs: number;
  }) => Promise<SocketLike>;
  /** Test seam: how a plain socket becomes a TLS one. */
  readonly upgrade?: (socket: SocketLike, servername: string) => Promise<SocketLike>;
  readonly logger?: (line: string) => void;
}

function defaultConnect(options: { host: string; port: number; timeoutMs: number }): Promise<SocketLike> {
  return new Promise<SocketLike>((resolve, reject) => {
    const socket = netConnect({ host: options.host, port: options.port });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(
        new MailError(
          "timeout",
          `could not reach the mail relay at ${options.host}:${options.port} within ${options.timeoutMs}ms`,
          true,
        ),
      );
    }, options.timeoutMs);
    socket.once("connect", () => {
      clearTimeout(timer);
      resolve(socket as unknown as SocketLike);
    });
    socket.once("error", (error: Error) => {
      clearTimeout(timer);
      reject(
        new MailError(
          "connection-failed",
          `could not reach the mail relay at ${options.host}:${options.port}: ${error.message}`,
          true,
        ),
      );
    });
  });
}

function defaultUpgrade(socket: SocketLike, servername: string): Promise<SocketLike> {
  return new Promise<SocketLike>((resolve, reject) => {
    let settled = false;
    const tlsSocket = tlsConnect(
      { socket: socket as unknown as Socket, servername, minVersion: "TLSv1.2" },
      () => {
        settled = true;
        tlsSocket.removeListener("error", onError);
        resolve(tlsSocket as unknown as SocketLike);
      },
    );
    function onError(error: Error): void {
      if (settled) return;
      reject(new MailError("tls-failed", `STARTTLS to ${servername} failed: ${error.message}`, true));
    }
    tlsSocket.once("error", onError);
  });
}

/**
 * The SMTP transport: one connection per send, greeting to `QUIT`.
 *
 * One connection per message is wasteful by mail-client standards and right by
 * this app's: a completion notice is one message per bead, beads land minutes
 * apart, and a pooled connection held open across a work run would be a
 * long-lived thing with a state machine nobody here is watching.
 */
export function createSmtpTransport(options: SmtpTransportOptions): MailTransport {
  const config = options.config;
  const connect = options.connect ?? defaultConnect;
  const upgrade = options.upgrade ?? defaultUpgrade;
  const log = options.logger ?? (() => undefined);
  const where = `${config.host}:${config.port}${config.implicitTls ? " (implicit TLS)" : ""}`;

  /** EHLO, or HELO if the server will not do EHLO, which some do not. */
  async function hello(session: SmtpSession): Promise<SmtpCapabilities> {
    try {
      const reply = await session.command(`EHLO ${config.helloHost}`, [250], "EHLO");
      return readCapabilities(reply);
    } catch (error) {
      log(
        `EHLO refused (${error instanceof Error ? error.message : String(error)}); ` +
          "falling back to HELO with no capabilities",
      );
      await session.command(`HELO ${config.helloHost}`, [250], "HELO");
      return {
        extensions: new Map(),
        authMechanisms: [],
        supportsStarttls: false,
        supports8bitMime: false,
      };
    }
  }

  /**
   * Upgrade a plain session in place, returning a session over TLS plus the
   * capabilities of the upgraded conversation (a server may advertise different
   * ones after STARTTLS, and `AUTH` is usually only among them).
   */
  async function upgradeSession(
    session: SmtpSession,
  ): Promise<{ session: SmtpSession; caps: SmtpCapabilities }> {
    await session.command("STARTTLS", [220], "STARTTLS");
    const plain = session.detach();
    let secure: SocketLike;
    try {
      secure = await upgrade(plain, config.host);
    } catch (error) {
      // The plain socket is ours to clean up: an upgrade that failed leaves it
      // wrapped, half-read and belonging to nobody.
      try {
        plain.destroy();
      } catch {
        // Nothing to do but stop trying.
      }
      throw error;
    }
    // RFC 3207: after the handshake both sides discard the cached conversation
    // and the client re-issues EHLO. No greeting is sent, and waiting for one
    // would be a timeout wearing a hat.
    const next = new SmtpSession(secure, config.timeoutMs, log);
    const caps = await hello(next);
    return { session: next, caps };
  }

  async function authenticate(
    session: SmtpSession,
    caps: SmtpCapabilities,
    secure: boolean,
  ): Promise<void> {
    const auth = config.auth;
    if (auth === undefined) return;
    if (!secure && config.allowInsecureAuth !== true) {
      throw new MailError(
        "insecure-auth",
        `refusing to send a password to ${where} over an unencrypted connection. Use smtps://, ` +
          "set LOOP_MAIL_STARTTLS=require, or say you mean it with LOOP_MAIL_INSECURE_AUTH=1.",
      );
    }
    const mechanisms = caps.authMechanisms;
    if (mechanisms.length > 0 && !mechanisms.includes("PLAIN") && !mechanisms.includes("LOGIN")) {
      throw new MailError(
        "auth-unsupported",
        `${where} offers ${mechanisms.join(", ")}, none of which this client can do`,
      );
    }
    if (mechanisms.length === 0 || mechanisms.includes("PLAIN")) {
      const token = Buffer.from(`\u0000${auth.user}\u0000${auth.password}`, "utf8").toString("base64");
      await session.command(`AUTH PLAIN ${token}`, [235], "AUTH PLAIN", { redact: true });
      return;
    }
    await session.command("AUTH LOGIN", [334], "AUTH LOGIN", { redact: true });
    await session.command(Buffer.from(auth.user, "utf8").toString("base64"), [334], "the login user", {
      redact: true,
    });
    await session.command(
      Buffer.from(auth.password, "utf8").toString("base64"),
      [235],
      "the login password",
      { redact: true },
    );
  }

  return {
    name: `smtp:${where}`,
    async send(envelope: MailEnvelope): Promise<MailDelivery> {
      let session: SmtpSession | null = null;
      let secure = config.implicitTls;
      try {
        if (config.host.trim() === "") {
          throw new MailError("invalid-config", "no mail relay host is configured");
        }
        const socket = await connect({
          host: config.host,
          port: config.port,
          timeoutMs: config.timeoutMs,
        });
        session = new SmtpSession(socket, config.timeoutMs, log);
        await session.expect([220], "the greeting");
        let caps = await hello(session);

        if (!secure && config.starttls !== "off") {
          if (!caps.supportsStarttls) {
            if (config.starttls === "required") {
              throw new MailError(
                "starttls-unavailable",
                `${where} did not advertise STARTTLS, and LOOP_MAIL_STARTTLS=require will not send ` +
                  "over a channel it cannot encrypt",
              );
            }
            log(`${where} does not offer STARTTLS; continuing in the clear (nothing secret is being sent)`);
          } else {
            const upgraded = await upgradeSession(session);
            session = upgraded.session;
            caps = upgraded.caps;
            secure = true;
          }
        }

        await authenticate(session, caps, secure);

        const body = caps.supports8bitMime ? " BODY=8BITMIME" : "";
        await session.command(`MAIL FROM:<${envelope.from}>${body}`, [250], "MAIL FROM");
        for (const recipient of envelope.recipients) {
          await session.command(`RCPT TO:<${recipient}>`, [250, 251], `RCPT TO:${recipient}`);
        }
        await session.command("DATA", [354], "DATA");
        session.writeData(envelope.raw);
        const queued = await session.expect([250, 251], "the queued message id");
        // `250 2.0.0 <abc123@relay> Queued mail received` is the usual form; a
        // relay that only says "Ok" leaves us with the id we generated, which is
        // still the id in the message's own `Message-ID` header.
        const queuedId = /<([^<>\s]+)>/u.exec(queued.lines[0] ?? "")?.[1];
        const messageId = queuedId ?? envelope.messageId;
        try {
          await session.command("QUIT", [221], "QUIT");
        } catch {
          // A server that will not say goodbye has still taken the message.
        }
        return {
          kind: "delivered",
          messageId,
          transport: `smtp:${where}`,
          recipients: [...envelope.recipients],
        };
      } catch (error) {
        session?.close();
        throw error;
      } finally {
        session?.close();
      }
    },
  };
}
