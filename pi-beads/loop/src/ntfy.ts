/**
 * `src/ntfy.ts` — publishing a notice to ntfy.
 *
 * ntfy's publish API is the whole feature: an HTTP `POST` to
 * `<server>/<topic>` whose body is the message, with the presentation carried
 * in ordinary headers (`Title`, `Priority`, `Tags`, `Click`). There is no
 * handshake, no session, no protocol to keep alive, and the topic model means
 * the loop never needs to know who is listening. That is why this replaced email
 * — see [ADR-007](../docs/ADR-007-ntfy-notices.md) — and it is why this module
 * is about eighty lines of transport instead of thirteen hundred: there is
 * nothing here that the previous transport needed and ntfy does not.
 *
 * Self-hosting is the same shape with a different base URL, which is the point of
 * `LOOP_NTFY_URL`: `http://192.168.1.20:8080` is as valid as
 * `https://ntfy.sh`, and nothing downstream knows the difference.
 *
 * Two limits worth designing around, both from ntfy's own defaults
 * (`limit-message-bytes: 4096`, `limit-message-title-length: 200`):
 *
 * - The body is truncated at a byte boundary rather than letting the server
 *   refuse it with `413`. A notice that arrives slightly short is a better
 *   outcome than a notice that arrives never, and the truncation says so.
 * - The title is capped the same way, because the bead id has to survive it —
 *   the title is the field the notification is *found by* on a phone.
 *
 * As everywhere else in this loop, delivery is **data**: every failure path —
 * refused topic, dropped socket, deadline, 5xx — returns a
 * {@link NtfyDelivery}. Nothing here throws at the caller except a
 * configuration error raised before any request is made.
 */
import { request as httpRequest } from "node:http";
import { request as httpsRequest } from "node:https";
import { Buffer } from "node:buffer";
import { hostname } from "node:os";

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

/** ntfy's default `limit-message-bytes`. Body is trimmed to fit under it. */
export const NTFY_MAX_MESSAGE_BYTES = 4096;
/** ntfy's `limit-message-title-length`. */
export const NTFY_MAX_TITLE_LENGTH = 200;
const DEFAULT_TIMEOUT_MS = 10_000;
const MAX_RESPONSE_BYTES = 64 * 1024;

/** Where a notice goes: a server base, and one topic on it. */
export interface NtfyTarget {
  /** e.g. `http://192.168.1.20:8080` — no trailing slash. */
  readonly base: string;
  readonly topic: string;
  /** `base` + `/` + `topic`, as one string for logs and delivery reports. */
  readonly destination: string;
}

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

export type NtfyDelivery =
  | {
      kind: "delivered";
      readonly messageId: string | null;
      readonly destination: string;
      readonly transport: string;
    }
  | { readonly kind: "skipped"; readonly reason: string }
  | {
      readonly kind: "failed";
      readonly reason: string;
      readonly destination: string;
      readonly retryable: boolean;
    };

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
    const topic = parsed.pathname.replace(/^\/+|\/+$/gu, "");
    if (topic === "") {
      throw new NtfyError("config", `"${rawTopic}" names no topic`);
    }
    return {
      base: `${parsed.protocol}//${parsed.host}`,
      topic,
      destination: parsed.toString().replace(/\/+$/u, ""),
    };
  }

  const fallback = setting.defaultBase ?? defaults.defaultBase ?? "https://ntfy.sh";
  const baseRaw = (setting.url ?? fallback).trim().replace(/\/+$/u, "");
  const baseParsed = parseHttpUrl(baseRaw, "the ntfy server URL");
  return {
    base: `${baseParsed.protocol}//${baseParsed.host}`,
    topic: rawTopic,
    // A base with a path prefix (`http://host/ntfy`) is kept: the topic goes
    // under it, which is what a reverse-proxied ntfy actually serves.
    destination: `${baseRaw}/${encodeURIComponent(rawTopic)}`,
  };
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
  const marker = "… (truncated)";
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

/** The ntfy header set for one message, with unset fields simply absent. */
export function ntfyHeaders(
  message: NtfyMessage,
  options: { token?: string; userAgent?: string } = {},
): Record<string, string> {
  const headers: Record<string, string> = {
    "Content-Type": "text/plain; charset=utf-8",
    "User-Agent": options.userAgent ?? "pi-beads-loop",
  };
  const title = message.title.trim();
  if (title !== "") headers["Title"] = flattenHeader(title);
  if (message.priority !== undefined && message.priority !== "") {
    headers["Priority"] = flattenHeader(message.priority);
  }
  const tags = (message.tags ?? []).map((tag) => tag.trim()).filter((tag) => tag !== "");
  if (tags.length > 0) headers["Tags"] = flattenHeader(tags.join(","));
  if (message.click !== undefined && message.click !== "") {
    headers["Click"] = flattenHeader(message.click);
  }
  if (options.token !== undefined && options.token !== "") {
    headers.Authorization = `Bearer ${options.token.trim()}`;
  }
  return headers;
}

/** No CR, no LF, no folding tricks in a header value. */
function flattenHeader(value: string): string {
  return value.replace(/[\r\n]+/gu, " ").trim();
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
  readonly maxMessageBytes?: number;
  readonly maxTitleLength?: number;
  readonly logger?: (line: string) => void;
  readonly userAgent?: string;
}

export interface NtfyPublisher {
  readonly enabled: boolean;
  readonly destination: string;
  readonly transport: string;
  publish(message: NtfyMessage): Promise<NtfyDelivery>;
}

export function createNtfyPublisher(options: NtfyPublisherOptions): NtfyPublisher {
  const transport = options.transport ?? createHttpTransport();
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const maxBytes = options.maxMessageBytes ?? NTFY_MAX_MESSAGE_BYTES;
  const maxTitle = options.maxTitleLength ?? NTFY_MAX_TITLE_LENGTH;
  const log = options.logger ?? (() => undefined);
  const destination = options.target.destination;

  return {
    enabled: options.target.topic.trim() !== "",
    destination,
    transport: transport.name,
    async publish(message: NtfyMessage): Promise<NtfyDelivery> {
      const body = truncateToBytes(message.body, maxBytes);
      const title = clipTitle(message.title, maxTitle);
      if (body.trim() === "") {
        return { kind: "skipped", reason: "the notice had no body to publish" };
      }
      const headers = ntfyHeaders(
        { ...message, title, body },
        {
          ...(options.token === undefined ? {} : { token: options.token }),
          ...(options.userAgent === undefined ? {} : { userAgent: options.userAgent }),
        },
      );

      let response: NtfyRawResponse;
      try {
        response = await transport.publish(destination, {
          headers,
          body,
          timeoutMs,
        });
      } catch (error) {
        const reason = NtfyError.is(error)
          ? `${error.kind}: ${error.message}`
          : error instanceof Error
            ? error.message
            : String(error);
        // The topic identifies the channel, so it stays in the log; the token
        // never appears anywhere in this module, by construction — it only ever
        // exists inside the header value built above.
        log(`ntfy publish to ${destination} failed: ${reason}`);
        return { kind: "failed", reason, destination, retryable: true };
      }

      const verdict = classifyResponse(response.statusCode);
      if (verdict.ok) {
        return {
          kind: "delivered",
          messageId: parseMessageId(response.body),
          destination,
          transport: transport.name,
        };
      }
      const reason =
        `ntfy replied ${response.statusCode}` +
        (response.body.trim() === "" ? "" : `: ${oneLine(response.body, 180)}`);
      log(`ntfy publish to ${destination} refused: ${reason}`);
      return { kind: "failed", reason, destination, retryable: verdict.retryable };
    },
  };
}

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
    transport: "none",
    async publish(): Promise<NtfyDelivery> {
      return { kind: "skipped", reason };
    },
  };
}
