/**
 * A fake ntfy server on loopback, shared by `test/ntfy.test.ts` (where the
 * publisher under test is this repo's own) and `test/loop.test.ts` (where the
 * whole chain from closed bead to a published message is).
 *
 * Not a `*.test.ts` file on purpose: `node --test test/*.test.ts` will not run
 * it, so importing it does not execute anybody's suite a second time.
 *
 * It answers the way ntfy answers — `200 {"id":…,"time":…,"expires":…}` on a
 * good publish — and it *checks* the way ntfy checks, because a fake that only
 * echoes what the client believes proves nothing. The rules below are ntfy's,
 * read out of `server/server.go` / `server/types.go`:
 *
 * - A JSON publish is routed on the path being exactly `/`. The topic comes from
 *   the body. `POST /mytopic` with a JSON body is not the same request said
 *   differently — the real server takes the topic out of the *path* there and
 *   makes the entire JSON document the text of the notification, so this fake
 *   refuses it rather than quietly accepting something ntfy would turn into a
 *   nonsense notice.
 * - The topic must match `^[-_A-Za-z0-9]{1,64}$`, else `400` / code 40009.
 * - `priority` unmarshals into an `int`: a `"high"` in the JSON is a `400`, even
 *   though the header API accepts that name.
 * - `tags` unmarshals into `[]string`.
 *
 * It can be told to fail in the four ways that matter to the client's promises:
 * `403` (bad topic or token), `404` (no such topic), `429` (rate limited),
 * `500` (server broke), or never answering at all so the client's deadline is
 * what replies.
 *
 * Every request is recorded: method, path, headers, raw body, and the parsed
 * JSON envelope when there was one. The tests assert the conversation rather
 * than inferring it from a delivery result.
 */
import http from "node:http";
import assert from "node:assert/strict";
import type { AddressInfo } from "node:net";

export interface FakeNtfyBehaviour {
  /** Status code to answer with. Default 200. */
  readonly status?: number;
  /** Body to answer with. Default a well-formed ntfy JSON ack. */
  readonly responseBody?: string;
  /** Never answer — the client's timeout is what ends the request. */
  readonly hang?: boolean;
  /** Require this bearer token, else 401. */
  readonly requireToken?: string;
  /**
   * Accept a JSON publish aimed at a topic URL (`POST /topic`) instead of
   * refusing it the way ntfy does. Only a test that wants the refusal itself
   * should care; nothing in this repo publishes that way.
   */
  readonly allowTopicUrlJson?: boolean;
}

export interface FakeNtfyRequest {
  readonly method: string;
  readonly path: string;
  readonly headers: Readonly<Record<string, string | string[] | undefined>>;
  readonly body: string;
  /** The parsed JSON envelope, for a JSON publish; `null` for anything else. */
  readonly json: Readonly<Record<string, unknown>> | null;
  /** The topic the server took this message for, from the body or the path. */
  readonly topic: string;
}

export interface FakeNtfy {
  readonly port: number;
  /** e.g. `http://127.0.0.1:54321` */
  readonly base: string;
  readonly requests: FakeNtfyRequest[];
  close(): Promise<void>;
}

/** ntfy's own `topicRegex`. No `/`, 1-64 of `-_A-Za-z0-9`. */
const TOPIC_PATTERN = /^[-_A-Za-z0-9]{1,64}$/u;

interface Refusal {
  readonly status: number;
  readonly code: number;
  readonly message: string;
}

interface JsonPublishInspection {
  readonly json: Record<string, unknown> | null;
  readonly topic: string;
  readonly refusal: Refusal | null;
}

/**
 * Read a JSON publish the way ntfy reads one, and refuse it for the same reasons.
 *
 * Extracted from the request handler so the rules above are one readable list
 * rather than an if-chain inside a closure.
 */
function inspectJsonPublish(
  body: string,
  url: string,
  allowTopicUrlJson: boolean | undefined,
): JsonPublishInspection {
  let parsed: unknown = null;
  try {
    parsed = JSON.parse(body);
  } catch {
    parsed = null;
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return refused(null, "", 400, 40017, "invalid request: request body must be message JSON");
  }
  const json = parsed as Record<string, unknown>;
  if (allowTopicUrlJson !== true && url !== "/") {
    return refused(
      json,
      "",
      400,
      40017,
      "a JSON publish must go to the server root, not the topic URL " +
        "(ntfy would take the topic from the path and the JSON would arrive as the message text)",
    );
  }
  const topic = json.topic;
  if (typeof topic !== "string" || topic === "") {
    return refused(json, "", 400, 40009, 'invalid request: "topic" is missing');
  }
  if (!TOPIC_PATTERN.test(topic)) {
    return refused(json, "", 400, 40009, "invalid request: topic invalid");
  }
  if (json.priority !== undefined && !(typeof json.priority === "number" && Number.isInteger(json.priority))) {
    return refused(
      json,
      topic,
      400,
      40000,
      "json: cannot unmarshal non-int into Go struct field publishMessage.priority of type int",
    );
  }
  if (
    json.tags !== undefined &&
    !(Array.isArray(json.tags) && json.tags.every((tag) => typeof tag === "string"))
  ) {
    return refused(
      json,
      topic,
      400,
      40000,
      "json: cannot unmarshal non-array into Go struct field publishMessage.tags of type []string",
    );
  }
  return { json, topic, refusal: null };
}

function refused(
  json: Record<string, unknown> | null,
  topic: string,
  status: number,
  code: number,
  message: string,
): JsonPublishInspection {
  return { json, topic, refusal: { status, code, message } };
}

export async function startFakeNtfy(behaviour: FakeNtfyBehaviour = {}): Promise<FakeNtfy> {
  const requests: FakeNtfyRequest[] = [];
  const sockets = new Set<import("node:net").Socket>();

  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      const contentType = String(req.headers["content-type"] ?? "");
      const isJson = contentType.toLowerCase().startsWith("application/json");
      const url = req.url ?? "/";
      const pathTopic = url.replace(/^\//u, "").replace(/\/+$/u, "");

      // The rules this fake enforces are ntfy's, read out of `server.go`:
      // see the module comment above. Every request is recorded, including the
      // ones refused, so a test can see what was sent rather than infer it from
      // what came back.
      const inspected: JsonPublishInspection = isJson
        ? inspectJsonPublish(body, url, behaviour.allowTopicUrlJson)
        : { json: null, topic: pathTopic, refusal: null };
      const refusal = inspected.refusal;

      requests.push({
        method: req.method ?? "",
        path: url,
        headers: { ...req.headers },
        body,
        json: inspected.json,
        topic: inspected.topic,
      });

      if (behaviour.hang === true) {
        // Deliberately no response: the client's own deadline has to answer.
        return;
      }
      if (refusal !== null) {
        rejectResponse(res, refusal.status, refusal.code, refusal.message);
        return;
      }
      if (behaviour.requireToken !== undefined) {
        const got = String(req.headers.authorization ?? "");
        if (got !== `Bearer ${behaviour.requireToken}`) {
          rejectResponse(res, 401, 401, "unauthorized");
          return;
        }
      }

      const status = behaviour.status ?? 200;
      const responseBody =
        behaviour.responseBody ??
        JSON.stringify({
          id: "abc123XYZ",
          time: Math.floor(Date.now() / 1000),
          expires: Math.floor(Date.now() / 1000) + 3600,
          topic: inspected.topic,
        });
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(responseBody);
    });
  });

  const socketsOf = server as unknown as { on(event: string, cb: (s: import("node:net").Socket) => void): void };
  socketsOf.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => sockets.delete(socket));
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address() as AddressInfo | null;
  assert.ok(address !== null && typeof address === "object");

  return {
    port: address.port,
    base: `http://127.0.0.1:${address.port}`,
    requests,
    async close(): Promise<void> {
      for (const socket of sockets) socket.destroy();
      sockets.clear();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

/** Answer the way ntfy answers an error: its code, its message. */
function rejectResponse(
  res: http.ServerResponse,
  httpStatus: number,
  code: number,
  message: string,
): void {
  res.writeHead(httpStatus, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ code, http: httpStatus, httpMessage: message }));
}

/** The JSON envelope of request `n` (default: the only/last one), or fail the test. */
export function publishedJson(
  request: FakeNtfyRequest | undefined,
): Readonly<Record<string, unknown>> {
  assert.ok(request !== undefined, "expected a request to have arrived");
  assert.ok(request.json !== null, `expected a JSON publish body, got: ${request.body}`);
  return request.json;
}
