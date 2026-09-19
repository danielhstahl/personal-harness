/**
 * Idle-mode app used by the pty spike (`spikes/4-idle-pty.ts`).
 *
 * It is the real `createIdleMode`, with a fixed status snapshot — no model, no
 * beads, no orchestrator. It prints machine-readable markers on stdout so a
 * driver can synchronise on them:
 *
 *   READY                       — booted, first frame painted
 *   OUTCOME {"kind":...}        — the idle surface produced its outcome
 *   RAWMODE yes|no              — is the tty still in raw mode after teardown
 *   GOODBYE written             — our goodbye sink fired
 *
 * argv: `submit` (drive a submission through) or `ctrlc` (drive the double
 * Ctrl+C exit). Keystrokes come from the driver over the pty.
 */

import { spawnSync } from "node:child_process";
import { createIdleMode } from "../src/idle.js";

const mode = process.argv[2] ?? "submit";

const status = {
  ready: 3,
  inProgress: 1,
  model: { provider: "llamacpp", id: "halogen-qwen3.8-flash-next" },
  thinkingLevel: "low",
};

let goodbyeWritten = false;
const handle = createIdleMode({
  status,
  goodbye: () => {
    goodbyeWritten = true;
  },
});

// The first frame lands on a render timer, so announce readiness after a beat
// rather than assuming the paint already happened.
handle.refresh();
setTimeout(() => {
  process.stdout.write("READY\n");
}, 300);

const outcome = await handle.next();
process.stdout.write(`OUTCOME ${JSON.stringify(outcome)}\n`);
handle.dispose();

// Ask the tty itself, not our own bookkeeping: after a correct teardown stdin
// should be cooked again (icanon on, no -icanon).
const stty = spawnSync("stty", ["-a"], { encoding: "utf8" });
const sttyText = `${stty.stdout ?? ""}${stty.stderr ?? ""}`;
const rawStillOn = /-\s*icanon/.test(sttyText);
process.stdout.write(`RAWMODE ${rawStillOn ? "yes" : "no"}\n`);
process.stdout.write(`GOODBYE ${goodbyeWritten ? "written" : "missing"}\n`);
process.exitCode = 0;
