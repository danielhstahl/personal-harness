/**
 * A fake SMTP relay on loopback, shared by `test/mail.test.ts` (where the client
 * under test is this repo's own) and `test/loop.test.ts` (where the whole chain
 * from closed bead to a message on the wire is).
 *
 * Not a `*.test.ts` file on purpose: `node --test test/*.test.ts` will not run
 * it, so importing it does not execute anybody's suite a second time.
 *
 * The behaviour switches are the ones the client's promises need checking, and
 * nothing else: refuse until authenticated, refuse one recipient, stop answering,
 * hang up. Every command the relay received is recorded so a test can assert the
 * conversation rather than infer it from a delivery result.
 *
 * TLS is not implemented, and cannot be here: the relay is an IP address, which is
 * not a legal TLS server name. Tests that exercise the STARTTLS *handover* inject
 * the transport's `upgrade` seam and get the plaintext socket back, "as if" the
 * handshake succeeded — which is what that code path actually is from this
 * module's point of view.
 */
import net from "node:net";
import assert from "node:assert/strict";

export interface FakeSmtpBehaviour {
  readonly greeting?: string;
  /** Capability lines advertised in reply to EHLO, after the greeting line. */
  readonly caps?: readonly string[];
  /** Refuse `MAIL FROM` until `AUTH` has landed. */
  readonly requireAuth?: boolean;
  readonly rejectRecipient?: {
    readonly address: string;
    readonly code: number;
    readonly message: string;
  };
  /** Say nothing in reply to `DATA`, so the client's deadline is what answers. */
  readonly hangOnData?: boolean;
  /** Hang up right after the greeting. */
  readonly dropAfterGreeting?: boolean;
}

export interface FakeSmtp {
  readonly port: number;
  /** Every command line the relay received, in order. */
  readonly received: string[];
  /** Every `DATA` payload, in order. */
  readonly messages: string[];
  close(): Promise<void>;
}

export const DEFAULT_CAPS = ["HELP", "STARTTLS", "AUTH=PLAIN LOGIN", "8BITMIME", "SIZE 35882568"];

export async function startFakeSmtp(behaviour: FakeSmtpBehaviour = {}): Promise<FakeSmtp> {
  const received: string[] = [];
  const messages: string[] = [];
  const sockets = new Set<net.Socket>();
  let authenticated = false;

  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => sockets.delete(socket));
    socket.write(`${behaviour.greeting ?? "220 fake.test ESMTP rehearsing"}\r\n`);
    if (behaviour.dropAfterGreeting === true) {
      socket.end();
      return;
    }

    let buffer = "";
    let inData = false;
    let dataLines: string[] = [];
    const caps = behaviour.caps ?? DEFAULT_CAPS;

    socket.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      for (;;) {
        const newline = buffer.indexOf("\n");
        if (newline < 0) return;
        const line = buffer.slice(0, newline).replace(/\r$/u, "");
        buffer = buffer.slice(newline + 1);

        if (inData) {
          if (line === ".") {
            inData = false;
            messages.push(dataLines.join("\r\n"));
            dataLines = [];
            socket.write("250 2.0.0 <q7f3a@fake.test> Queued mail received\r\n");
          } else {
            dataLines.push(line);
          }
          continue;
        }

        received.push(line);
        const verb = (line.split(" ")[0] ?? "").toUpperCase();
        const rest = line.slice(verb.length).trim();

        if (verb === "EHLO" || verb === "HELO") {
          const lines = [`250-fake.test hello ${rest}`]
            .concat(caps.map((cap) => `250-${cap}`))
            .concat(["250 END"])
            .join("\r\n");
          socket.write(`${lines}\r\n`);
          return;
        }
        if (verb === "STARTTLS") {
          if (caps.some((cap) => cap.toUpperCase() === "STARTTLS")) {
            socket.write("220 2.0.0 Ready to start TLS\r\n");
          } else {
            socket.write("504 5.5.2 Unrecognized command\r\n");
          }
          return;
        }
        if (verb === "AUTH") {
          authenticated = true;
          socket.write("235 2.7.0 Authentication successful\r\n");
          return;
        }
        if (verb === "MAIL") {
          if (behaviour.requireAuth === true && !authenticated) {
            socket.write("530 5.7.0 Must issue an AUTH first\r\n");
          } else {
            socket.write("250 2.1.0 Ok\r\n");
          }
          return;
        }
        if (verb === "RCPT") {
          const target = /<([^>]*)>/u.exec(rest)?.[1] ?? rest;
          const reject = behaviour.rejectRecipient;
          if (reject !== undefined && reject.address === target) {
            socket.write(`${reject.code} ${reject.message}\r\n`);
          } else {
            socket.write("250 2.1.5 Ok\r\n");
          }
          return;
        }
        if (verb === "DATA") {
          if (behaviour.hangOnData === true) return;
          socket.write("354 End data with <CR><LF>.<CR><LF>\r\n");
          inData = true;
          return;
        }
        if (verb === "QUIT") {
          socket.write("221 2.0.0 Bye\r\n");
          socket.end();
          return;
        }
        socket.write("500 5.5.2 Unrecognized command\r\n");
      }
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  assert.ok(address !== null && typeof address === "object");
  const port = address.port;

  return {
    port,
    received,
    messages,
    async close(): Promise<void> {
      for (const socket of sockets) socket.destroy();
      sockets.clear();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

