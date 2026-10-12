/**
 * workspace-eqn.5 — typed `bd` tools as a pi extension: the no-engine-switch control arm.
 *
 * ADR-0010 prices two separate claims that the proposal bundles together: "the
 * child process is overhead" and "we want typed tool calls instead of the worker
 * shelling `bd` out of its bash tool". The second one is the claim that survives
 * contact with the numbers, and this file is the evidence that it does **not**
 * need an engine swap to have: pi's own extension API registers schema-validated
 * tools, and the worker's `--tools` allowlist gates them exactly like the
 * built-ins.
 *
 * What each tool is, and why it is shaped like that:
 *
 *   bd_ready  — the read the loop's own picker does. Optional `limit`, because
 *               `bd ready` on a big board is a lot of text for a screen.
 *   bd_show   — one ticket, whole. This is the call the worker makes first on
 *               every pass, today as `bash: bd show <id>`.
 *   bd_claim  — `bd update <id> --claim`. Refusals are data: "someone else holds
 *               it" is a different outcome from "bd did not answer", and the
 *               loop's recovery paths branch on exactly that difference.
 *   bd_close  — `bd close <id>`.
 *   bd_remember — the note-taking verb the loop uses for its own findings.
 *
 * Every one of them shells out to the same `bd` binary the bash tool would have
 * shelled out to — `bd` is a Go program with a SQLite file, not a library, so
 * there is no in-process bd for anyone to link, engine swap or no engine swap.
 * That is the finding, so it is said here in the file where someone would
 * otherwise discover it later: the typed tool buys the *schema*, the refusal
 * semantics and the `--tools` gating; it does not buy a process.
 *
 * Load it the way the spike driver loads it:
 *
 *   pi --mode rpc -e spikes/bd_tools/bd_tools.ts --tools read,bash,bd_ready,bd_show
 *
 * `LOOPRS_SPIKE_BD` overrides the binary, the same knob `spikes/board_poll_cost.py`
 * uses, so the arm can be measured against a fake `bd` without touching a board.
 */

import { spawnSync } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const BD = process.env.LOOPRS_SPIKE_BD ?? "bd";

const ID = Type.String({ description: "Bead id, e.g. workspace-eqn.1" });

const SCHEMAS: Record<string, { label: string; description: string; parameters: any; args: (p: any) => string[] }> = {
	bd_ready: {
		label: "bd ready",
		description:
			"List tickets the beads loop can pick up right now (unassigned, unblocked, open). Read-only.",
		parameters: Type.Object({
			limit: Type.Optional(Type.Number({ description: "Maximum tickets to return" })),
		}),
		args: (p) => ["ready", ...(p.limit ? ["--limit", String(p.limit)] : [])],
	},
	bd_show: {
		label: "bd show",
		description: "Show one ticket in full: description, notes, children and status. Read-only.",
		parameters: Type.Object({ id: ID }),
		args: (p) => ["show", p.id],
	},
	bd_claim: {
		label: "bd claim",
		description: "Claim a ticket so no other worker takes it. Fails if another worker holds the lease.",
		parameters: Type.Object({ id: ID }),
		args: (p) => ["update", p.id, "--claim"],
	},
	bd_close: {
		label: "bd close",
		description: "Close a ticket as done. The loop checks the board, not intentions.",
		parameters: Type.Object({ id: ID }),
		args: (p) => ["close", p.id],
	},
	bd_remember: {
		label: "bd remember",
		description: "Save a durable note for a future session working on this project.",
		parameters: Type.Object({
			text: Type.String({ description: "The insight, in one or two sentences" }),
			key: Type.Optional(Type.String({ description: "Explicit memory key" })),
		}),
		args: (p) => ["remember", p.text, ...(p.key ? ["--key", p.key] : [])],
	},
};

export default function bdTools(pi: ExtensionAPI) {
	for (const [name, spec] of Object.entries(SCHEMAS)) {
		pi.registerTool({
			name,
			label: spec.label,
			description: spec.description,
			parameters: spec.parameters,
			promptSnippet: `${spec.label}: ${spec.description.split(".")[0]}`,
			exposure: "model",
			async execute(_toolCallId, params: any) {
				const argv = spec.args(params ?? {});
				const t0 = Date.now();
				// shell: false — the typed path's whole argument list is argv, with no
				// shell in front of it. That is the one layer this removes, and it is
				// measured rather than asserted (spikes/bd_tools_cost.py).
				const r = spawnSync(BD, argv, { encoding: "utf-8", shell: false, maxBuffer: 8 << 20 });
				const ms = Date.now() - t0;
				const ok = r.status === 0;
				const body = ok ? (r.stdout ?? "") : `${r.stdout ?? ""}${r.stderr ?? ""}`;
				return {
					content: [
						{
							type: "text",
							text:
								body.trim() ||
								(r.error ? `bd did not run: ${r.error.message}` : `bd ${argv[0]} produced no output`),
						},
					],
					// `details` is tool state that follows the transcript branch; the
					// refusal-vs-crash distinction is what the harness reads.
					details: { tool: name, argv, exit: r.status, ms, refused: ok === false && r.status === 1 },
				};
			},
		});
	}
}
