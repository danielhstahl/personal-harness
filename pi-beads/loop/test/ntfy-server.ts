/**
 * A fake ntfy server on loopback, shared by `test/ntfy.test.ts` (where the
 * publisher under test is this repo's own) and `test/loop.test.ts` (where the
 * whole chain from closed bead to a published message is).
 *
 * Not a `*.test.ts` file on purpose: `node --test test/*.test.ts` will not run
 * it, so importing it does not execute anybody's suite a second time.
 *
 * It answers the way ntfy answers — `200 {"id":…,"time":…,"expires":…}` on a
 * good publish — and can be told to fail in the four ways that matter to the
 * client's promises: `403` (bad topic or token), `404` (no such topic), `429`
 * (rate limited), `500` (server broke), or never answering at all so the
 * client's deadline is what replies.
 *
 * Every request is recorded: method, path, headers, body. The tests assert the
 * conversation rather than inferring it from a delivery result.
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
}

export interface FakeNtfyRequest {
  readonly method: string;
  readonly path: string;
  readonly headers: Readonly<Record<string, string | string[] | undefined>>;
  readonly body: string;
}

export interface FakeNtfy {
  readonly port: number;
  /** e.g. `http://127.0.0.1:54321` */
  readonly base: string;
  readonly requests: FakeNtfyRequest[];
  close(): Promise<void>;
}

export async function startFakeNtfy(behaviour: FakeNtfyBehaviour = {}): Promise<FakeNtfy> {
  const requests: FakeNtfyRequest[] = [];
  const sockets = new Set<import("node:net").Socket>();

  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      const record: FakeNtfyRequest = {
        method: req.method ?? "",
        path: req.url ?? "",
        headers: { ...req.headers },
        body,
      };
      requests.push(record);

      if (behaviour.hang === true) {
        // Deliberately no response: the client's own deadline has to answer.
        return;
      }
      if (behaviour.requireToken !== undefined) {
        const got = String(req.headers.authorization ?? "");
        if (got !== `Bearer ${behaviour.requireToken}`) {
          res.writeHead(401, { "Content-Type": "application/json" });
          res.end('{"code":401,"message":"unauthorized"}');
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
          topic: (req.url ?? "").replace(/^\//u, ""),
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
