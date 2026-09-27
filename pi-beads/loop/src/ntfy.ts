/**
 * `src/ntfy.ts` — publishing a notice to ntfy.
 *
 * ntfy's publish API is the whole feature: an HTTP `POST` with the message in
 * the request body. There is no handshake, no session, no protocol to keep
 * alive, and the topic model means the loop never needs to know who is
 * listening. That is why this replaced email — see
 * [ADR-007](../docs/ADR-007-ntfy-notices.md) — and it is why this module is
 * about a hundred lines of transport instead of thirteen hundred: there is
 * nothing here that the previous transport needed and ntfy does not.
 *
 * **The wire format is a JSON envelope posted to the server root, not headers
 * posted to the topic URL.** That was not the original shape, and the original
 * shape broke: the title is an agent-written sentence, and a sentence can hold
 * a character HTTP headers cannot carry. Node refuses one at the socket layer
 * with `Invalid character in header content ["Title"]` — so a bead whose
 * summary contained an em dash, a curly quote, a `✓` or anything outside
 * Latin-1 could not be announced, while a bead whose summary was plain ASCII was
 * announced fine. The failure was *in the content*, which is the worst place
 * for it: intermittent, invisible to review, and blaming the notice for the
 * wording of the work.
 *
 * ntfy's documented JSON publish takes the whole message — `topic`, `title`,
 * `message`, `priority`, `tags`, `click` — as a UTF-8 JSON body at the server
 * **root** (`POST https://ntfy.sh/`, not `POST https://ntfy.sh/mytopic`),
 * which is what a notification body is: text. Anything the work wrote can go in
 * it verbatim. The only header left in this module is the one that *has* to be a
 * header — `Authorization: Bearer …` — plus the constant `Content-Type` and
 * `User-Agent`, none of which an agent ever writes.
 *
 * Three limits worth designing around, all from ntfy's own defaults
 * (`limit-message-bytes: 4096`, and its JSON reader's 2× headroom):
 *
 * - The message is truncated rather than letting the server refuse it, and the
 *   truncation says so. A notice that arrives slightly short is a better
 *   outcome than a notice that arrives never.
 * - **The limit is exclusive, not inclusive** — see
 *   {@link NTFY_MESSAGE_BOUNDARY_SLACK_BYTES}. Landing exactly on
 *   `limit-message-bytes` is not "a long message" to ntfy, it is an
 *   attachment, and with no attachment store (the default) that is
 *   `40014 attachments not allowed`. A notice cut to `4096 - marker` and put
 *   back together landed there exactly, which is how "attachments not allowed"
 *   came to be printed about a feature that never mentions an attachment.
 * - The whole JSON document is clamped to twice the message limit, because
 *   ntfy reads a JSON publish body with `MessageSizeLimit*2` bytes and
 *   escaping (`\n`, `\"`) inflates a message that already fitted on its own.
 *   The title is capped too, because the bead id has to survive it — the title
 *   is the field the notification is *found by* on a phone.
 *
 * And one thing that cannot be known in advance: ntfy does not report the
 * `limit-message-bytes` it is running. Declare yours with
 * `LOOP_NTFY_MAX_MESSAGE_BYTES`, or let the publisher learn it — a refusal
 * that means "too much message" halves the cap once, keeps it for the rest of
 * the run, and stops rather than grinding when sending less would not help.
 *
 * As everywhere else in this loop, delivery is **data**: every failure path —
 * refused topic, dropped socket, deadline, 5xx — returns a
 * {@link NtfyDelivery}. Nothing here throws at the caller except a
 * configuration error raised before any request is made.
 *
 * THE SEAM (workspace-k1o.3): this publisher **is** a `NoticePublisher`, and
 * `NtfyPublisher extends NoticePublisher` is the clause that makes TypeScript
 * check the claim rather than take it on faith. It takes a `Notice` — title,
 * body, opaque `hints` — and everything ntfy-only is interpreted on this side
 * of that line: {@link ntfyMessageFromNotice} turns `priority` / `tags` /
 * `click` back into ntfy fields, {@link ntfyPublishPayload} builds the JSON
 * envelope, and the clamping below owns `limit-message-bytes: 4096`,
 * {@link NTFY_MESSAGE_BOUNDARY_SLACK_BYTES} and the JSON 2×-headroom rule.
 * Those rules were never the notice's to know; before the seam they sat in
 * `NotifierOptions`, where the wording layer had to name them in order to pass
 * them along. `NTFY_MAX_MESSAGE_BYTES` and `NTFY_MIN_MESSAGE_BYTES` stay
 * exported as this transport's declared limits, now grouped as
 * {@link NTFY_MESSAGE_LIMITS} so the configuration layer can ask instead of
 * hard-coding a range.
 */
import { request as httpRequest } from "node:http";
import { request as httpsRequest } from "node:https";
import { Buffer } from "node:buffer";
import { hostname } from "node:os";

import type { Delivery, Notice, NoticePublisher } from "./notify.ts";

/**
 * The host this run is on, for the notice's "where from" line.
 *
 * `os.hostname()` comes back empty on some minimal images and in some containers,
 * and an empty host in a notice reads as a bug in the notice rather than a gap
 * in the environment, so it gets a fallback.
 */
export function localHostname(): string {
  const name = (hostname() ?? "").trim();
  return name === "" ? "localhost" : name;
}

/**
 * The notice, in this transport's own words.
 *
 * This is what a {@link Notice} becomes once ntfy has read its hints — the
 * transport's vocabulary (`priority`, `tags`, `click`) as opposed to the
 * notice layer's (`title`, `body`, `hints?`). {@link ntfyMessageFromNotice}
 * does the translation; nothing outside this module needs to name these fields.
 */
export interface NtfyMessage {
  readonly title: string;
  readonly body: string;
  /** ntfy priority: 1–5, or `min`/`low`/`default`/`high`/`urgent`. */
  readonly priority?: string;
  /** Emoji *names*, comma separated in the header, e.g. `+1,warning`. */
  readonly tags?: readonly string[];
  /** URL the notification opens. */
  readonly click?: string;
}

/**
 * What came back from a publish.
 *
 * An **alias**, not a second definition. The three kinds — delivered, skipped,
 * failed — were never ntfy's idea: they are the loop's report, and the shape now
 * lives in `src/notify.ts` next to the thing that reads it. Keeping the old name
 * here means the transport's own wording still reads as `NtfyDelivery` in a stack
 * trace, while making a second shape impossible: if the two ever drifted, that is
 * a compile error rather than a migration note.
 */
export type NtfyDelivery = Delivery;

/** ntfy's default `limit-message-bytes`. Body is trimmed to fit under it. */
export const NTFY_MAX_MESSAGE_BYTES = 4096;
/**
 * The boundary byte is not ours to spend, so ntfy's limit is treated as
 * **exclusive**.
 *
 * This is not caution, it is the arithmetic in ntfy itself:
 *
 * - `util.Peek(r.Body, limit)` reports `LimitReached: read == limit` — a
 *   message of exactly `limit` bytes counts as having hit the limit.
 * - `handlePublishBody` takes the *text message* path only when
 *   `!body.LimitReached`; otherwise it falls through to
 *   `handleBodyAsAttachment`.
 * - That handler answers `40014 attachments not allowed` when the server has
 *   no attachment store — which is ntfy's **default** — and silently turns the
 *   notice into a downloadable file when it does.
 *
 * So a notice truncated to exactly 4096 bytes is not "a long message", it is
 * an attachment request, and the previous code landed there whenever the text
 * had no line break in the last stretch of the window: cut to `4096 - marker`,
 * put the marker back, 4096 exactly. One byte shorter and it is a message.
 */
export const NTFY_MESSAGE_BOUNDARY_SLACK_BYTES = 1;
/**
 * ntfy's ceiling for a JSON publish body: `readJSONWithLimit(..., MessageSizeLimit*2)`
 * in `server.go`, "2x to account for JSON format overhead". A message that fit
 * 4096 bytes on its own does not fit once every newline is spelled `\n`, so the
 * envelope gets clamped too — otherwise the notice that survived the message
 * limit dies on the document limit instead.
 */
export const NTFY_MAX_JSON_BODY_BYTES = NTFY_MAX_MESSAGE_BYTES * 2;
/** ntfy's `limit-message-title-length`. */
export const NTFY_MAX_TITLE_LENGTH = 200;
/**
 * ntfy's own topic pattern (`topicRegex` in `server.go`): 1–64 of
 * `-_A-Za-z0-9`, and no `/`.
 *
 * Checked at startup because a topic outside it is a guaranteed `400` on every
 * bead, and a guaranteed failure is worth having once, in the terminal where the
 * typo was made.
 */
export const NTFY_TOPIC_PATTERN = /^[-_A-Za-z0-9]{1,64}$/u;
/** What an HTTP header value may contain: printable ASCII, no control characters. */
const HEADER_SAFE_PATTERN = /^[\t\x20-\x7E]*$/u;
const DEFAULT_TIMEOUT_MS = 10_000;
const MAX_RESPONSE_BYTES = 64 * 1024;
/** Appended wherever text was cut to fit a limit. */
const TRUNCATION_MARKER = "… (truncated)";
/** The only content type this module sends. */
export const NTFY_JSON_CONTENT_TYPE = "application/json";

/**
 * A `Notice`, read the way ntfy reads it.
 *
 * The whole of the transport's side of the seam: {@link Notice} arrives with an
 * opaque `hints` bag, and this is where `priority`, `tags` and `click` stop
 * being strings somebody passed through and become ntfy's fields. The notice
 * layer never learns any of this, which is the point — see the note on
 * `Notice.hints` for why they are hints rather than fields of a notice.
 *
 * Unrecognised keys are not an error: a hint written for another transport is
 * exactly what a hint is for. They are reported through `onUnknownHint` so a
 * typo (`pirority`) shows up in a log instead of quietly arriving unstyled.
 */
export function ntfyMessageFromNotice(
  notice: Notice,
  onUnknownHint?: (key: string) => void,
): NtfyMessage {
  const hints = notice.hints ?? {};
  const priority = hintString(hints.priority);
  const click = hintString(hints.click);
  const tags = hintList(hints.tags);
  if (onUnknownHint !== undefined) {
    for (const key of Object.keys(hints)) {
      if (!isNtfyHintKey(key)) onUnknownHint(key);
    }
  }
  return {
    title: notice.title,
    body: notice.body,
    ...(priority === undefined ? {} : { priority }),
    ...(tags.length === 0 ? {} : { tags }),
    ...(click === undefined ? {} : { click }),
  };
}

/**
 * The hint keys this transport reads. Anything else is somebody else's — and
 * is named in the log rather than dropped, so a typo costs a line, not a
 * silently unstyled notice.
 */
export const NTFY_HINT_KEYS: readonly string[] = ["priority", "tags", "click"];

export function isNtfyHintKey(key: string): boolean {
  return NTFY_HINT_KEYS.includes(key);
}

/** One-value hint. A list where a single value was wanted is not guessed at. */
function hintString(value: string | readonly string[] | undefined): string | undefined {
  if (typeof value !== "string") return undefined;
  return value.trim() === "" ? undefined : value;
}

/**
 * List-valued hint. A bare string is split on commas and spaces, because that
 * is the shape these values arrive in from the environment
 * (`LOOP_NTFY_TAGS="+1 beads"`), and the transport that reads a hint is the
 * place that knows what shape it takes.
 */
function hintList(value: string | readonly string[] | undefined): string[] {
  const parts = typeof value === "string" ? value.split(/[,\s]+/u) : value ?? [];
  return parts.map((part) => part.trim()).filter((part) => part !== "");
}

/** Where a notice goes: a server base, and one topic on it. */
export interface NtfyTarget {
  /** e.g. `http://192.168.1.20:8080` — no trailing slash. */
  readonly base: string;
  readonly topic: string;
  /** `base` + `/` + `topic`, as one string for logs and delivery reports. */
  readonly destination: string;
  /**
   * Where the publish request itself goes: the server root, or the reverse-proxy
   * prefix, with **no topic in the path** — the topic travels in the JSON body.
   *
   * ntfy routes a JSON publish on `r.URL.Path == "/"` alone, so
   * `POST /topic` with a JSON body is not "the same thing said differently":
   * the server takes the topic out of the path, never the body, and the entire
   * JSON document becomes the text of the notification. Hence a field of its
   * own rather than a string built at the call site.
   */
  readonly publishUrl: string;
}

/** How the message reaches the server. Injectable so tests can be a real server. */
export interface NtfyTransport {
  readonly name: string;
  publish(url: string, options: NtfyRequestOptions): Promise<NtfyRawResponse>;
}

export interface NtfyRequestOptions {
  readonly headers: Readonly<Record<string, string>>;
  readonly body: string;
  readonly timeoutMs: number;
}

export interface NtfyRawResponse {
  readonly statusCode: number;
  readonly body: string;
}

/**
 * Resolve the endpoint from configuration.
 *
 * `topic` may be a bare name (`my-loop-notices`) or a complete URL
 * (`https://ntfy.sh/my-loop-notices`), because that is how ntfy's own CLI takes
 * it and it is the form people paste from the web UI. A bare topic is joined onto
 * `base`, whose default is the hosted server.
 *
 * Refusals happen here, at startup, rather than as a per-bead failure later: a
 * typo in an endpoint is worth finding in the terminal it was typed in.
 */
export function resolveNtfyTarget(
  setting: { url?: string; topic?: string; defaultBase?: string },
  defaults: { defaultBase?: string } = {},
): NtfyTarget {
  const rawTopic = (setting.topic ?? "").trim();
  if (rawTopic === "") {
    throw new NtfyError("config", "no ntfy topic is configured");
  }

  if (looksLikeUrl(rawTopic)) {
    const parsed = parseHttpUrl(rawTopic, "the ntfy topic URL");
    // The topic is the last path segment; anything above it is a proxy prefix,
    // which is what a self-hosted ntfy behind nginx actually looks like.
    const segments = parsed.pathname.split("/").filter((part) => part !== "");
    const topic = segments[segments.length - 1] ?? "";
    if (topic === "") {
      throw new NtfyError("config", `"${rawTopic}" names no topic`);
    }
    assertTopicName(topic, "the ntfy topic");
    const origin = `${parsed.protocol}//${parsed.host}`;
    const prefixPath = segments.slice(0, -1).join("/");
    return {
      base: origin,
      topic,
      destination: parsed.toString().replace(/\/+$/gu, ""),
      publishUrl: `${origin}/${prefixPath}${prefixPath === "" ? "" : "/"}`,
    };
  }

  const fallback = setting.defaultBase ?? defaults.defaultBase ?? "https://ntfy.sh";
  const baseRaw = (setting.url ?? fallback).trim().replace(/\/+$/u, "");
  const baseParsed = parseHttpUrl(baseRaw, "the ntfy server URL");
  assertTopicName(rawTopic, "the ntfy topic");
  return {
    base: `${baseParsed.protocol}//${baseParsed.host}`,
    topic: rawTopic,
    // A base with a path prefix (`http://host/ntfy`) is kept: the publish goes
    // to the prefix, which is the root of the ntfy behind it, and the topic is
    // carried inside the JSON body rather than bolted onto the end of the URL.
    publishUrl: `${baseRaw}/`,
    // The human-readable form: what a subscription looks like, what a delivery
    // report names. It is not where the request goes.
    destination: `${baseRaw}/${encodeURIComponent(rawTopic)}`,
  };
}

/**
 * Refuse a topic ntfy would refuse, before the first bead is closed.
 *
 * The alternative is a `400 invalid request: topic invalid` per notice, which
 * after {@link NtfyPublisher} gives up is a run that quietly stopped telling
 * anybody anything.
 */
function assertTopicName(topic: string, what: string): void {
  if (NTFY_TOPIC_PATTERN.test(topic)) return;
  throw new NtfyError(
    "config",
    `${what} — "${topic}" is not a valid ntfy topic name ` +
      "(1-64 letters, digits, dashes or underscores; no slashes, spaces or accents)",
  );
}

function looksLikeUrl(value: string): boolean {
  return /^[a-z][a-z0-9+.-]*:\/\//iu.test(value);
}

/**
 * Parse one of the two URLs this feature accepts, rejecting the shapes that
 * would fail later in worse ways: a non-http scheme (there is no ftp:// for a
 * notification), and credentials hiding in the URL — ntfy authenticates with a
 * bearer header, and a user:pass in a URL is a secret that gets echoed into
 * logs and error messages by everything that touches it.
 */
function parseHttpUrl(value: string, what: string): URL {
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    throw new NtfyError("config", `${what} — "${value}" is not a valid URL`);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new NtfyError(
      "config",
      `${what} must be http:// or https://, not "${parsed.protocol}"`,
    );
  }
  if (parsed.username !== "" || parsed.password !== "") {
    throw new NtfyError(
      "config",
      `${what} carries credentials — set the access token instead of putting them in the URL`,
    );
  }
  return parsed;
}

/** A configuration problem, raised before any request goes out. */
export class NtfyError extends Error {
  readonly kind: string;

  constructor(kind: string, message: string) {
    super(message);
    this.name = "NtfyError";
    this.kind = kind;
  }

  static is(error: unknown): error is NtfyError {
    return error instanceof NtfyError;
  }
}

/**
 * Trim to a UTF-8 byte budget without cutting a character in half, and without
 * splitting a multi-line notice mid-line where it can be avoided.
 *
 * Truncation is marked (`… (truncated)`) rather than silent: a reader who finds
 * the notice shorter than the run produced should know that, not assume the run
 * produced less.
 */
export function truncateToBytes(text: string, maxBytes: number): string {
  const marker = TRUNCATION_MARKER;
  if (Buffer.byteLength(text, "utf8") <= maxBytes) return text;
  if (maxBytes <= Buffer.byteLength(marker, "utf8")) {
    return clipBytes(text, Math.max(0, maxBytes));
  }
  const budget = maxBytes - Buffer.byteLength(marker, "utf8");
  // Prefer the last whole line before the budget: a half-line of a notice reads
  // like a bug in the notice, a dropped line reads like a limit.
  const clipped = clipBytes(text, budget);
  const lastNewline = clipped.lastIndexOf("\n");
  const cut = lastNewline > budget / 2 ? clipped.slice(0, lastNewline) : clipped;
  return `${cut}${marker}`;
}

/** Byte-exact prefix that ends on a character boundary. */
function clipBytes(text: string, maxBytes: number): string {
  const buffer = Buffer.from(text, "utf8").subarray(0, Math.max(0, maxBytes));
  // `toString` on a buffer ending mid-character yields U+FFFD; step back until
  // the round trip is clean, which is the boundary.
  let end = buffer.length;
  while (end > 0) {
    const candidate = buffer.subarray(0, end).toString("utf8");
    if (Buffer.byteLength(candidate, "utf8") === end) return candidate;
    end -= 1;
  }
  return "";
}

/** Two ways a server can say "that is too much message", and one floor to stop at. */
const SHRINK_STEP_DIVISOR = 2;
const MAX_SHRINKS_PER_RUN = 3;
/** Below this a "notice" is a fragment, so the shrink stops rather than go on. */
export const NTFY_MIN_MESSAGE_BYTES = 256;

/**
 * The band a declared message cap may land in for this transport.
 *
 * Grouped and exported so the configuration layer can ask the transport what it
 * accepts instead of copy-pasting two numbers out of it. That is the same seam
 * as {@link Notice.hints}, one level further out: `src/main.ts` checks that a
 * byte count was *written* as one, and the layer that chose the transport —
 * `buildNotifier` in `src/app.ts` — checks that it is a byte count this
 * transport can carry.
 */
export const NTFY_MESSAGE_LIMITS: Readonly<{
  minBytes: number;
  maxBytes: number;
  defaultBytes: number;
}> = {
  minBytes: NTFY_MIN_MESSAGE_BYTES,
  maxBytes: NTFY_MAX_MESSAGE_BYTES,
  defaultBytes: NTFY_MAX_MESSAGE_BYTES,
};

/** Whether a declared cap is a whole number of bytes this transport will take. */
export function isNtfyMessageBytes(value: unknown): boolean {
  return (
    typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value >= NTFY_MESSAGE_LIMITS.minBytes &&
    value <= NTFY_MESSAGE_LIMITS.maxBytes
  );
}

/**
 * Which refusals are really "too much message, and I will not make an
 * attachment out of it".
 *
 * `40014` is the exact one. `413` and the "too large" wording are the same
 * shape from a proxy or a differently-configured server, and the same fix —
 * send less — applies, so they are treated alike rather than each getting
 * their own detection later.
 */
export function isMessageTooLargeRefusal(status: number, body: string): boolean {
  if (status === 413) return true;
  if (status !== 400) return false;
  const text = body.toLowerCase();
  return (/"code"\s*:\s*40014/u.test(text)
    ? true
    : text.includes("attachments not allowed") || text.includes("too large"));
}

/**
 * The next cap worth trying after one was refused, or `null` when going smaller
 * would stop being a notice.
 *
 * Halving rather than jumping to the floor: the floor of this loop is the
 * smallest server that could still be told something, and arriving there in one
 * step over-delivers the lesson. Three halvings from 4096 gets to 512, which
 * covers the tight configurations people actually run.
 */
export function shrinkMessageLimit(limit: number): number | null {
  const next = Math.floor(limit / SHRINK_STEP_DIVISOR);
  return next >= NTFY_MIN_MESSAGE_BYTES ? next : null;
}

/** One publish document, as ntfy's `publishMessage` struct reads it. */
export interface NtfyPublishEnvelope {
  readonly topic: string;
  readonly message: string;
  readonly title?: string;
  readonly priority?: number;
  readonly tags?: readonly string[];
  readonly click?: string;
}

/**
 * The JSON document ntfy reads for a publish.
 *
 * Field names and types are ntfy's (`publishMessage` in `server/types.go`), and
 * two of them are traps worth a comment each:
 *
 * - `priority` is an **int**. The names ntfy's own header API accepts
 *   (`high`, `urgent`, …) are translated here by {@link ntfyPriorityNumber},
 *   because a `"priority":"high"` in a JSON body does not unmarshal into an
 *   `int` and comes back as a `400`.
 * - `tags` is an **array of strings**, not the comma-joined string the header
 *   took.
 *
 * `title` is kept on one line. With a JSON body that is no longer a security
 * requirement — nothing in the body can become a header — but a notification
 * title with an embedded newline is still a broken-looking list entry, and the
 * summary that produced it is written by an agent.
 */
export function ntfyPublishPayload(
  message: NtfyMessage,
  topic: string,
  options: { maxMessageBytes?: number; maxJsonBytes?: number } = {},
): NtfyPublishEnvelope {
  /** The server's `limit-message-bytes`, as we understand it. */
  const messageLimitBytes = options.maxMessageBytes ?? NTFY_MAX_MESSAGE_BYTES;
  const maxJsonBytes = options.maxJsonBytes ?? NTFY_MAX_JSON_BODY_BYTES;
  // Exclusive, per NTFY_MESSAGE_BOUNDARY_SLACK_BYTES: exactly at the limit is
  // an attachment as far as ntfy is concerned, not a long message.
  const messageCeiling = Math.max(0, messageLimitBytes - NTFY_MESSAGE_BOUNDARY_SLACK_BYTES);
  const rest: {
    title?: string;
    priority?: number;
    tags?: readonly string[];
    click?: string;
  } = {};
  const title = singleLine(message.title);
  if (title !== "") rest.title = title;
  if (message.priority !== undefined && message.priority.trim() !== "") {
    rest.priority = ntfyPriorityNumber(message.priority);
  }
  const tags = (message.tags ?? []).map((tag) => tag.trim()).filter((tag) => tag !== "");
  if (tags.length > 0) rest.tags = tags;
  const click = (message.click ?? "").trim();
  if (click !== "") rest.click = click;

  // What the envelope costs with an empty message inside it; `- 2` is the pair
  // of quotes that empty message occupies, which is exactly the room `message`
  // itself may still use.
  const envelopeBytes =
    Buffer.byteLength(JSON.stringify({ topic, message: "", ...rest }), "utf8") - 2;
  // Also exclusive: ntfy's JSON reader has a ceiling of its own, and a document
  // sitting on a boundary is how the next boundary bug starts.
  const messageBudget = Math.max(
    0,
    maxJsonBytes - NTFY_MESSAGE_BOUNDARY_SLACK_BYTES - envelopeBytes,
  );
  return {
    topic,
    message: fitJsonBytes(truncateToBytes(message.body, messageCeiling), messageBudget),
    ...rest,
  };
}

/**
 * The same document, serialized — the request body of one publish.
 *
 * The message inside it is clamped twice on the way: to ntfy's message limit,
 * then to whatever the document limit leaves once the envelope and the escaping
 * are paid for. Both clamps say so, because a notice that reads shorter than
 * the run that produced it should tell the reader that rather than invite the
 * question.
 */
export function ntfyPublishBody(
  message: NtfyMessage,
  topic: string,
  options: { maxMessageBytes?: number; maxJsonBytes?: number } = {},
): string {
  return JSON.stringify(ntfyPublishPayload(message, topic, options));
}

/** ntfy's priority names, as the numbers the JSON body needs. */
const NTFY_PRIORITY_BY_NAME: Readonly<Record<string, number>> = {
  min: 1,
  low: 2,
  default: 3,
  high: 4,
  urgent: 5,
};

/**
 * `high` -> 4, `3` -> 3, anything else -> a config error.
 *
 * The number is what `priority` has to be; sending the name would trade one
 * undeliverable notice for a different undeliverable notice, and saying
 * "the priority is not a priority" here is more useful than Go's "cannot
 * unmarshal string into int" arriving later as somebody's 400.
 */
export function ntfyPriorityNumber(value: string): number {
  const raw = value.trim().toLowerCase();
  const named = NTFY_PRIORITY_BY_NAME[raw];
  if (named !== undefined) return named;
  if (/^[1-5]$/.test(raw)) return Number(raw);
  throw new NtfyError(
    "config",
    `"${value}" is not an ntfy priority (1-5, or min / low / default / high / urgent)`,
  );
}

/** Bytes the UTF-8 encoding of this string's JSON form occupies. */
function jsonBytes(text: string): number {
  return Buffer.byteLength(JSON.stringify(text), "utf8");
}

/**
 * Trim `text` so its JSON encoding fits `maxEncodedBytes`, on the same
 * "cut and say so" terms as {@link truncateToBytes}.
 *
 * Escaping is why this exists: a message of all newlines doubles in size once
 * every `\n` is spelled with a backslash, and a stray control character costs
 * six bytes per character. Every candidate is measured with `JSON.stringify`
 * rather than estimated, because estimating what needs escaping is the same bug
 * with more steps.
 */
export function fitJsonBytes(text: string, maxEncodedBytes: number): string {
  if (jsonBytes(text) <= maxEncodedBytes) return text;
  const base = text.endsWith(TRUNCATION_MARKER)
    ? text.slice(0, -TRUNCATION_MARKER.length)
    : text;
  const markerBytes = jsonBytes(TRUNCATION_MARKER);
  if (maxEncodedBytes <= markerBytes) return "";
  // Largest prefix that still fits with the marker on the end.
  let lo = 0;
  let hi = base.length;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    if (jsonBytes(base.slice(0, mid) + TRUNCATION_MARKER) <= maxEncodedBytes) lo = mid;
    else hi = mid - 1;
  }
  return `${base.slice(0, lo)}${TRUNCATION_MARKER}`;
}

/** Collapse a value to one line for a header or a notification title. */
function singleLine(value: string): string {
  return value.replace(/[\r\n]+/gu, " ").trim();
}

/**
 * The headers a publish carries — which is deliberately not the interesting
 * part of the request any more.
 *
 * Everything an agent wrote lives in the JSON body. What is left here is three
 * constants and one operator-supplied secret, all of which have to be a header
 * because that is where a bearer token goes. A value among them that cannot be
 * put on the wire is a configuration error, and it is named as one instead of
 * arriving as Node's `ERR_INVALID_CHAR` from the socket layer — the exact error
 * that killed this feature in its header-only form.
 */
export function transportHeaders(
  options: { token?: string; userAgent?: string } = {},
): Record<string, string> {
  const headers: Record<string, string> = {
    "Content-Type": NTFY_JSON_CONTENT_TYPE,
    "User-Agent": headerValue(options.userAgent ?? "pi-beads-loop", "the user agent"),
  };
  const token = (options.token ?? "").trim();
  if (token !== "") {
    headers.Authorization = `Bearer ${headerValue(token, "LOOP_NTFY_TOKEN")}`;
  }
  return headers;
}

/** Whether a string can be used as an HTTP header value at all. */
export function isHeaderSafe(value: string): boolean {
  return HEADER_SAFE_PATTERN.test(value);
}

/**
 * A header value, flattened and checked.
 *
 * The offending string is deliberately not quoted back: one of the two things
 * this can be is a bearer token, and a token that fails validation is still a
 * token. Naming the setting is enough to fix it.
 */
function headerValue(value: string, what: string): string {
  const flattened = value.replace(/[\r\n]+/gu, " ").trim();
  if (!isHeaderSafe(flattened)) {
    throw new NtfyError(
      "config",
      `${what} contains characters that cannot be sent in an HTTP header ` +
        "(only printable ASCII can be)",
    );
  }
  return flattened;
}

/**
 * Which responses are worth trying again, and which are a clean "no".
 *
 * `429` is ntfy's rate-limit answer and is explicitly retryable; the rest of
 * the 4xx family means the topic, the token or the message is wrong, and
 * retrying that just produces a pile of identical refusals. Anything 5xx or a
 * transport failure is "the server had a problem", which is exactly what a
 * retry is for.
 */
export function classifyResponse(status: number): { ok: boolean; retryable: boolean } {
  if (status >= 200 && status < 300) return { ok: true, retryable: false };
  if (status === 429) return { ok: false, retryable: true };
  if (status >= 400 && status < 500) return { ok: false, retryable: false };
  return { ok: false, retryable: true };
}

/** The real transport: one HTTP POST, read, closed. No keep-alive, no session. */
export function createHttpTransport(): NtfyTransport {
  return {
    name: "ntfy",
    publish(url: string, options: NtfyRequestOptions): Promise<NtfyRawResponse> {
      return new Promise<NtfyRawResponse>((resolvePromise, rejectPromise) => {
        let target: URL;
        try {
          target = new URL(url);
        } catch {
          rejectPromise(new NtfyError("config", `"${url}" is not a valid URL`));
          return;
        }
        const isHttps = target.protocol === "https:";
        const doRequest = isHttps ? httpsRequest : httpRequest;
        const body = Buffer.from(options.body, "utf8");
        const req = doRequest(
          {
            protocol: target.protocol,
            hostname: target.hostname,
            port: target.port !== "" ? Number(target.port) : isHttps ? 443 : 80,
            path: `${target.pathname}${target.search}`,
            method: "POST",
            headers: {
              ...options.headers,
              "Content-Length": String(body.length),
            },
          },
          (res) => {
            const chunks: Buffer[] = [];
            let received = 0;
            res.on("data", (chunk: Buffer) => {
              if (received >= MAX_RESPONSE_BYTES) {
                res.resume();
                return;
              }
              received += chunk.length;
              chunks.push(chunk);
            });
            res.on("end", () => {
              resolvePromise({
                statusCode: res.statusCode ?? 0,
                body: Buffer.concat(chunks).toString("utf8").slice(0, MAX_RESPONSE_BYTES),
              });
            });
            res.on("error", (error: Error) => {
              rejectPromise(new NtfyError("network", error.message));
            });
          },
        );
        req.setTimeout(options.timeoutMs, () => {
          req.destroy(new NtfyError("timeout", `no response after ${options.timeoutMs}ms`));
        });
        req.on("error", (error: unknown) => {
          if (NtfyError.is(error)) {
            rejectPromise(error);
            return;
          }
          rejectPromise(new NtfyError("network", error instanceof Error ? error.message : String(error)));
        });
        req.write(body);
        req.end();
      });
    },
  };
}

/**
 * A publisher bound to one topic.
 *
 * `publish` never throws: validation, the request and the response all sit in
 * the same catch, because the caller is a loop that has already done the work
 * the notice is about. The only exception is a malformed target, which is
 * refused at construction.
 */
export interface NtfyPublisherOptions {
  readonly target: NtfyTarget;
  readonly transport?: NtfyTransport;
  readonly token?: string;
  readonly timeoutMs?: number;
  /**
   * Our understanding of the server's `limit-message-bytes`. Default 4096,
   * which is ntfy's default. Declared exclusive — the message stays under it,
   * never on it — and lowered mid-run if the server says the notice is too big
   * to accept without turning it into an attachment.
   */
  readonly maxMessageBytes?: number;
  /** Ceiling for the whole JSON document, escaping included. */
  readonly maxJsonBytes?: number;
  readonly maxTitleLength?: number;
  readonly logger?: (line: string) => void;
  readonly userAgent?: string;
}

export interface NtfyPublisher extends NoticePublisher {
  /** Where the request goes: the server root/prefix, with the topic in the body. */
  readonly publishUrl: string;
  /**
   * The message cap in force right now, in bytes.
   *
   * A getter because it can move during a run: see {@link NtfyPublisherOptions
   * `maxMessageBytes`} and the shrink-on-refusal rule in the publisher. A
   * snapshot taken at construction would be a number the publisher no longer
   * uses.
   */
  readonly messageBytes: number;
  /** `enabled`, `destination` and `transport` come from {@link NoticePublisher}. */
  publish(notice: Notice): Promise<NtfyDelivery>;
}

export function createNtfyPublisher(options: NtfyPublisherOptions): NtfyPublisher {
  const transport = options.transport ?? createHttpTransport();
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const maxJson = options.maxJsonBytes ?? NTFY_MAX_JSON_BODY_BYTES;
  const maxTitle = options.maxTitleLength ?? NTFY_MAX_TITLE_LENGTH;
  const log = options.logger ?? (() => undefined);
  const destination = options.target.destination;
  const publishUrl = options.target.publishUrl;
  const topic = options.target.topic;

  /**
   * The message cap for the rest of this run. It only ever goes down.
   *
   * `options.maxMessageBytes` is our guess at the server's
   * `limit-message-bytes` — ntfy does not publish the number it is running,
   * so a self-hosted box on a tighter setting cannot be known in advance. When
   * a refusal says "too much message", the cap is halved and kept there
   * instead of being re-learned per bead: a limit worth discovering is worth
   * discovering once, and every later notice is cheaper for it.
   */
  let messageCap = options.maxMessageBytes ?? NTFY_MAX_MESSAGE_BYTES;
  let shrinksLeft = MAX_SHRINKS_PER_RUN;

  /**
   * One try at `limit` bytes. Never throws, and reports one thing a
   * {@link NtfyDelivery} cannot: whether this particular refusal is something
   * sending less would fix.
   */
  async function attempt(limit: number, payload: NtfyMessage): Promise<Attempt> {
    let response: NtfyRawResponse;
    let sentBytes = 0;
    try {
      // Built inside the try, because building either half can refuse
      // something: a priority that is not a priority, a token that is not a
      // header value. Both are operator errors, and both come back as a
      // delivery rather than an exception thrown into a loop that has already
      // done the work the notice is about.
      const envelope = ntfyPublishPayload(payload, topic, {
        maxMessageBytes: limit,
        maxJsonBytes: maxJson,
      });
      // What this attempt actually put on the wire, as opposed to the cap it was
      // built under: a short notice under a big cap sends the same bytes at any
      // cap, which is the fact the shrink decision needs.
      sentBytes = Buffer.byteLength(envelope.message, "utf8");
      const body = JSON.stringify(envelope);
      const headers = transportHeaders({
        ...(options.token === undefined ? {} : { token: options.token }),
        ...(options.userAgent === undefined ? {} : { userAgent: options.userAgent }),
      });
      response = await transport.publish(publishUrl, { headers, body, timeoutMs });
    } catch (error) {
      const reason = NtfyError.is(error)
        ? `${error.kind}: ${error.message}`
        : error instanceof Error
          ? error.message
          : String(error);
      // A config refusal (a bad priority, a token that is not a header value)
      // will not be fixed by asking again, so it is not marked retryable the
      // way a dropped socket is.
      const retryable = !(NtfyError.is(error) && error.kind === "config");
      // The topic identifies the channel, so it stays in the log; the token
      // never appears anywhere in this module, by construction — it only ever
      // exists inside the header value built above.
      log(`ntfy publish to ${destination} failed: ${reason}`);
      return { delivery: { kind: "failed", reason, destination, retryable }, tooBig: false, sentBytes };
    }

    const verdict = classifyResponse(response.statusCode);
    if (verdict.ok) {
      return {
        delivery: {
          kind: "delivered",
          messageId: parseMessageId(response.body),
          destination,
          transport: transport.name,
        },
        tooBig: false,
        sentBytes,
      };
    }
    const reason =
      `ntfy replied ${response.statusCode}` +
      (response.body.trim() === "" ? "" : `: ${oneLine(response.body, 180)}`);
    log(`ntfy publish to ${destination} refused: ${reason}`);
    return {
      delivery: { kind: "failed", reason, destination, retryable: verdict.retryable },
      tooBig: isMessageTooLargeRefusal(response.statusCode, response.body),
      sentBytes,
    };
  }

  /**
   * Unknown hint keys are reported once each rather than once per bead: the
   * typo is one typo, and noticing it on the first notice is the whole value
   * of reporting it.
   */
  const reportedHints = new Set<string>();

  return {
    enabled: options.target.topic.trim() !== "",
    destination,
    transport: transport.name,
    publishUrl,
    get messageBytes(): number {
      return messageCap;
    },
    async publish(notice: Notice): Promise<NtfyDelivery> {
      // The transport's side of the seam: the notice's hints become ntfy's
      // fields here, and nowhere else. Everything below this line speaks ntfy.
      const message = ntfyMessageFromNotice(notice, (key) => {
        if (reportedHints.has(key)) return;
        reportedHints.add(key);
        log(
          `ntfy ignored the notice hint "${key}" — this transport reads ` +
            `${NTFY_HINT_KEYS.join(", ")} and nothing else`,
        );
      });
      const title = clipTitle(message.title, maxTitle);
      if (message.body.trim() === "") {
        return { kind: "skipped", reason: "the notice had no body to publish" };
      }
      const payload = { ...message, title };
      // Shrinking is bounded three ways over, because it is the one place this
      // publisher could spend request after request on a refusal that will
      // never clear:
      //
      // - The floor of a useful notice (`shrinkMessageLimit`).
      // - The notice's own size, which is the floor that actually matters: a
      //   100-byte notice under a 4 KiB cap sends the same bytes under a 1 KiB
      //   cap, so once the next cap is not smaller than what is already being
      //   sent there is no "less" left to send and the refusal is about
      //   something else entirely.
      // - `shrinksLeft`, which belongs to the publisher and not to this call,
      //   so the whole run discovers a tighter server once instead of once per
      //   bead.
      for (;;) {
        const result = await attempt(messageCap, payload);
        if (!result.tooBig) return result.delivery;
        if (shrinksLeft === 0) {
          return {
            ...result.delivery,
            reason: `${result.delivery.reason} (this run's shrink budget is spent)`,
          };
        }
        const next = shrinkMessageLimit(messageCap);
        if (next === null) {
          return {
            ...result.delivery,
            reason:
              `${result.delivery.reason} ` +
              `(${messageCap} bytes is already as small as a notice usefully gets)`,
          };
        }
        if (next >= result.sentBytes) {
          return {
            ...result.delivery,
            reason:
              `${result.delivery.reason} (the notice is only ${result.sentBytes} bytes, ` +
              "so this refusal is not about how much we are sending)",
          };
        }
        shrinksLeft -= 1;
        log(
          `ntfy will not take a ${messageCap}-byte message without an attachment store; ` +
            `sending this notice at ${next} bytes and keeping that cap for the rest of the run`,
        );
        messageCap = next;
      }
    },
  };
}

/**
 * One attempt's outcome.
 *
 * `tooBig` only ever comes with a *failed* delivery — a delivered notice is not
 * sitting there waiting to be resized — so that invariant is in the type rather
 * than left for the reader to remember. `sentBytes` is what this attempt
 * actually put on the wire, which is not the cap it was built under.
 */
type Attempt =
  | {
      readonly delivery: NtfyDelivery;
      readonly tooBig: false;
      readonly sentBytes: number;
    }
  | {
      readonly delivery: Extract<NtfyDelivery, { kind: "failed" }>;
      readonly tooBig: true;
      readonly sentBytes: number;
    };

function clipTitle(title: string, maxChars: number): string {
  const trimmed = title.trim();
  if (trimmed.length <= maxChars) return trimmed;
  return `${trimmed.slice(0, Math.max(0, maxChars - 1))}…`;
}

/** ntfy answers a publish with JSON: `{"id":"…","time":…,"expires":…}`. */
function parseMessageId(body: string): string | null {
  try {
    const parsed = JSON.parse(body) as { id?: unknown };
    return typeof parsed.id === "string" && parsed.id !== "" ? parsed.id : null;
  } catch {
    // A 2xx with an unparsable body is still a 2xx. The id is a convenience.
    return null;
  }
}

function oneLine(value: string, max: number): string {
  const line = value.replace(/\s+/gu, " ").trim();
  return line.length > max ? `${line.slice(0, max)}…` : line;
}

/** A publisher that will never publish, carrying the reason it will not. */
export function createNullPublisher(reason: string): NtfyPublisher {
  return {
    enabled: false,
    destination: "none",
    publishUrl: "none",
    messageBytes: 0,
    transport: "none",
    async publish(): Promise<NtfyDelivery> {
      return { kind: "skipped", reason };
    },
  };
}
