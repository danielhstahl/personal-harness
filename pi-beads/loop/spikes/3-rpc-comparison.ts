/**
 * ADR-001 spike (3 of 3) — the rejected alternative, measured not asserted.
 *
 * Spawns `pi --mode rpc` over pipes and times the round trip, then replays the
 * exact spawn call the current scaffold uses to demonstrate that it does not
 * work at all.
 *
 * Run:  cd spikes && npx tsx 3-rpc-comparison.ts
 */
import { spawn, spawnSync } from "node:child_process";
import { StringDecoder } from "node:string_decoder";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

/** LF-only JSONL reader (same shape as the scaffold's src/utils.ts helper). */
function jsonl(stream: NodeJS.ReadableStream, onLine: (l: string) => void) {
	const decoder = new StringDecoder("utf8");
	let buf = "";
	stream.on("data", (chunk: Buffer) => {
		buf += decoder.write(chunk);
		let i: number;
		while ((i = buf.indexOf("\n")) !== -1) {
			onLine(buf.slice(0, i));
			buf = buf.slice(i + 1);
		}
	});
}

// ---------------------------------------------------------------- A. correct RPC spawn
const t0 = Date.now();
const pi = spawn(
	"pi",
	["--mode", "rpc", "--no-session", "--no-extensions", "--no-tools", "--offline"],
	{
		cwd: process.cwd(),
		stdio: ["pipe", "pipe", "pipe"],
	},
);

let firstDeltaAt = 0;
let events = 0;
let deltas = 0;
let sawAgentEnd = false;
const eventTypes = new Map<string, number>();

jsonl(pi.stdout!, (line) => {
	if (!line.trim()) return;
	let data: { type?: string; assistantMessageEvent?: { type?: string } };
	try {
		data = JSON.parse(line);
	} catch {
		return;
	}
	events++;
	const t = data.type ?? "?";
	eventTypes.set(t, (eventTypes.get(t) ?? 0) + 1);
	if (t === "message_update") {
		if (data.assistantMessageEvent?.type === "text_delta") {
			if (!firstDeltaAt) firstDeltaAt = Date.now() - t0;
			deltas++;
		}
	}
	if (t === "agent_end" || t === "agent_settled") sawAgentEnd = true;
});

const exited = new Promise<number>((resolve) => {
	pi.on("exit", (code) => resolve(code ?? -1));
});

pi.stdin?.write(
	JSON.stringify({
		id: "spike-1",
		type: "prompt",
		message: "Reply with exactly one python code block containing print('hello').",
	}) + "\n",
);

const deadline = Date.now() + 180_000;
while (!sawAgentEnd && Date.now() < deadline) {
	await new Promise((r) => setTimeout(r, 50));
}
const totalMs = Date.now() - t0;
pi.kill("SIGTERM");
await Promise.race([exited, new Promise((r) => setTimeout(r, 3000))]);

const rpc = {
	mode: "pi --mode rpc (subprocess over pipes, correctly spelled args)",
	started: firstDeltaAt > 0,
	time_to_first_text_delta_ms: firstDeltaAt,
	total_ms: totalMs,
	events_received: events,
	text_deltas: deltas,
	event_types: Object.fromEntries(eventTypes),
};
console.log("A. RPC round trip:", JSON.stringify(rpc, null, 2));

// ---------------------------------------------------------------- B. the scaffold's spawn, verbatim
const brokenArgs = [" --mode", "rpc", "--no-session"];
const broken = spawnSync("pi", brokenArgs, { encoding: "utf8", timeout: 20_000, input: "" });
const brokenNote = {
	mode: 'scaffold form: spawn("pi", [" --mode", "rpc", ...])',
	exit_status: broken.status,
	stdout_bytes: broken.stdout?.length ?? 0,
	stderr_excerpt: (broken.stderr || broken.stdout || "")
		.trim()
		.split("\n")
		.slice(0, 3)
		.join(" | ")
		.slice(0, 300),
};
console.log("\nB. scaffold's exact argv, replayed:", JSON.stringify(brokenNote, null, 2));

// ---------------------------------------------------------------- verdict input
const summary = {
	rpc: rpc,
	scaffold_argv_replay: brokenNote,
	note:
		"A working RPC round trip requires a hand-maintained JSONL framing layer plus a " +
		"subprocess lifecycle to supervise, and still hands the caller raw JSON events " +
		"that must be re-mapped onto pi's components by hand. The in-process SDK skips " +
		"both layers and exposes those components directly.",
};
writeFileSync(join(import.meta.dirname, "out", "rpc-comparison.json"), JSON.stringify(summary, null, 2));
console.log("\nevidence -> spikes/out/rpc-comparison.json");
