/**
 * The typed `bd` tools looprs hands its beads worker.
 *
 * Shipped by workspace-01z out of `spikes/bd_tools_cost.py`'s working arm, on
 * ADR-0010 decision 2: the engine stays a `pi --mode rpc` child, and the one real
 * win that decision kept on the pi side is a schema-validated `bd` surface instead
 * of the worker shelling `bd` out of its bash tool.
 *
 * **What this is, and what it is not.** Every tool here runs the same `bd` binary
 * the bash tool would have run. `bd` is a Go program over a SQLite file, so there
 * is no `bd` library for anyone to link; the typed path removes the *shell* from
 * the call path and nothing else, and the shell measures 1.1 ms against a 170 ms
 * `bd ready` — the CALL leg of
 * [`spikes/results/bd-tools-cost-shipped.log`](../spikes/results/bd-tools-cost-shipped.log),
 * where "through a shell" and "fork+exec bd" are inside a few ms of each other. So
 * this file buys three things and sells none:
 *
 *   1. **Schema-validated argv.** No shell quoting between the model and a ticket
 *      id, and no way to write `bd close $ID` with `$ID` empty.
 *   2. **Refusals as data.** `bd`'s exit code travels in the result's `details`
 *      next to `bd`'s own words, so "somebody else holds that lease" is a
 *      distinguishable outcome from "there is no `bd` on PATH".
 *   3. **`--tools` gating.** A worker can be handed `bd_show`/`bd_claim`/`bd_close`
 *      and no `bash` at all, which a bash-invocation convention can never do.
 *
 * **The price, stated where someone would otherwise discover it.** A tool schema
 * is prompt, and prompt rides on *every* model request of a pass, not on the calls.
 * Read out of a real run's first system message, for the allowlists looprs actually
 * passes (the PROMPT leg of the capture named above):
 *
 *   allowlist                          tool section   vs built-ins   per request
 *   read,bash,edit,write                     339 B            —            —
 *   + bd_show, bd_close, bd_remember         562 B        +223 B     ≈55 tokens
 *   all five verbs                           729 B        +390 B     ≈97 tokens
 *   bd_show, bd_claim, bd_close, no bash     481 B        +142 B     ≈35 tokens
 *
 * A median pass of this project's own corpus is **114** assistant steps
 * (ADR-0010), and the schema is in every one of those requests — so the shipped
 * default costs about **6.3k input tokens per pass**, the full five about **11.1k**,
 * and the shell-less list about **4.0k**. That is the denominator to quote, not the
 * per-call one. If a context budget cannot pay it, the fix is fewer names in
 * `LOOPRS_BEADS_WORKER_TOOLS`; `off` costs nothing at all.
 *
 * **What is deliberately not here (ADR-0010 "What this forbids" 4).** No derived
 * statuses, no re-interpreted board states, no looprs-side vocabulary: each schema
 * mirrors one `bd` command line, and the result body is `bd`'s own stdout/stderr,
 * byte for byte. `bd`'s read stays the one honest read (ADR-0007). If `bd`'s CLI
 * changes, the mirror below changes with it — that is the whole maintenance
 * contract of this file, and `services::bd_tools` in the Rust tree is the half
 * that keeps the verb list and the knob grammar from drifting away from it.
 *
 * Loaded by the loop as:
 *
 *   pi --mode rpc -e <this file, materialised by looprs at startup> \
 *      --tools read,bash,edit,write,bd_show,bd_close,bd_remember
 *
 * `LOOPRS_BD_BIN` picks the `bd` binary, which is the same variable the loop itself
 * reads and the reason "two binaries in one run" cannot happen here: the child
 * inherits the parent's environment, so the worker's `bd_show` and the loop's
 * board poll read one board (ADR-0007 §5). `LOOPRS_SPIKE_BD` is the older name,
 * kept because `spikes/bd_tools_cost.py` still sets it.
 */

import { spawnSync } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const BD = process.env.LOOPRS_BD_BIN ?? process.env.LOOPRS_SPIKE_BD ?? "bd";

const ID = Type.String({ description: "Bead id, e.g. workspace-01z" });

/**
 * One entry per `bd` command line, and nothing in an entry that `bd` does not
 * already have. `args` is the argv `bd` is run with; `read` marks the verbs that
 * cannot move the board, which is `bd`'s own distinction, not ours.
 */
const TOOLS: Record<
	string,
	{
		label: string;
		description: string;
		parameters: any;
		read: boolean;
		args: (p: any) => string[];
	}
> = {
	bd_ready: {
		label: "bd ready",
		description:
			"List tickets the beads loop can pick up right now (open, unassigned, unblocked). Read-only.",
		parameters: Type.Object({
			limit: Type.Optional(
				Type.Number({ description: "Maximum tickets to return; `bd ready` defaults to 100" }),
			),
		}),
		read: true,
		args: (p) => ["ready", ...(p.limit ? ["--limit", String(p.limit)] : [])],
	},
	bd_show: {
		label: "bd show",
		description: "Show one ticket in full: description, notes, children and status. Read-only.",
		parameters: Type.Object({ id: ID }),
		read: true,
		args: (p) => ["show", p.id],
	},
	bd_claim: {
		label: "bd claim",
		description: "Claim a ticket so no other worker takes it. Fails if another worker holds the lease.",
		parameters: Type.Object({ id: ID }),
		read: false,
		args: (p) => ["update", p.id, "--claim"],
	},
	bd_close: {
		label: "bd close",
		description: "Close a ticket as done. The loop checks the board, not intentions.",
		parameters: Type.Object({
			id: ID,
			reason: Type.Optional(Type.String({ description: "Why it is being closed (`bd close --reason`)" })),
		}),
		read: false,
		args: (p) => ["close", p.id, ...(p.reason ? ["--reason", p.reason] : [])],
	},
	bd_remember: {
		label: "bd remember",
		description: "Save a durable note for a future session working on this project.",
		parameters: Type.Object({
			text: Type.String({ description: "The insight, in one or two sentences" }),
			key: Type.Optional(
				Type.String({ description: "Explicit memory key; a matching memory is updated in place" }),
			),
		}),
		read: false,
		args: (p) => ["remember", p.text, ...(p.key ? ["--key", p.key] : [])],
	},
};

export default function bdTools(pi: ExtensionAPI) {
	for (const [name, spec] of Object.entries(TOOLS)) {
		pi.registerTool({
			name,
			label: spec.label,
			description: spec.description,
			parameters: spec.parameters,
			promptSnippet: `${spec.label}: ${spec.description.split(".")[0]}`,
			// `direct` is the default exposure: declared to the model while the tool
			// is active, and `--tools` is what decides whether it is active.
			exposure: "direct",
			annotations: { readOnlyHint: spec.read, destructiveHint: !spec.read },
			async execute(_toolCallId, params: any) {
				const argv = spec.args(params ?? {});
				const t0 = Date.now();
				// `shell: false` — the whole argument list is argv, with no shell in
				// front of it. That is the one layer this file removes, and it is
				// measured rather than asserted (spikes/bd_tools_cost.py).
				const r = spawnSync(BD, argv, { encoding: "utf-8", shell: false, maxBuffer: 8 << 20 });
				const ms = Date.now() - t0;
				// Three outcomes, and they must not collapse into each other:
				// `bd` ran and said yes, `bd` ran and said no (a refusal is a result,
				// not a crash — looprs' own claim path branches on exactly that), and
				// there was no `bd` to run at all. Only the last one is `isError`.
				if (r.error) {
					return {
						content: [{ type: "text", text: `\`${BD}\` could not be run: ${r.error.message}` }],
						isError: true,
						details: { tool: name, argv, bin: BD, ms, ran: false },
					};
				}
				const body = (r.status === 0 ? (r.stdout ?? "") : `${r.stdout ?? ""}${r.stderr ?? ""}`).trim();
				const text =
					r.status === 0
						? body || `bd ${argv[0]} produced no output`
						: `bd ${argv[0]} exited ${r.status}${body ? `:\n${body}` : ""}`;
				return {
					content: [{ type: "text", text }],
					// `details` is tool state that follows the transcript branch. The
					// exit code is `bd`'s own, copied, not a status this file invented.
					details: { tool: name, argv, bin: BD, exit: r.status, ms, ran: true },
				};
			},
		});
	}
}
