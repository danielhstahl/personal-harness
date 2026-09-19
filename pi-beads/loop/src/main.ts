/**
 * pi-beads loop — entry point.
 *
 * Scope: workspace-5yn.2 (toolchain). This proves the ESM + tsx + pi-SDK
 * toolchain end to end: it boots a session through `@earendil-works/pi-coding-agent`
 * and streams one assistant reply through the rendering path chosen in
 * [ADR-001](../docs/ADR-001-transport-and-rendering.md).
 *
 * It is deliberately NOT the loop yet. The real iteration body — check the board,
 * idle, split, work, finalize — lands in workspace-5yn.4 / .6 / .7 / .8 / .9.
 */
import {
	AssistantMessageComponent,
	createAgentSession,
	getMarkdownTheme,
	initTheme,
	ModelRuntime,
	SessionManager,
	SettingsManager,
} from "@earendil-works/pi-coding-agent";

/** Type of what the component consumes; derived from the component API itself. */
type CapturedAssistant = Parameters<AssistantMessageComponent["updateContent"]>[0];

const DEFAULT_WIDTH = 88;

function resolveWidth(): number {
	const raw = Number(process.env.LOOP_WIDTH ?? DEFAULT_WIDTH);
	if (!Number.isFinite(raw)) return DEFAULT_WIDTH;
	return Math.min(Math.max(Math.trunc(raw), 40), 240);
}

/** Pick the model from PI_PROVIDER/PI_MODEL, else the first configured model. */
async function resolveModel(runtime: ModelRuntime): Promise<NonNullable<ReturnType<ModelRuntime["getModel"]>>> {
	const provider = process.env.PI_PROVIDER;
	const modelId = process.env.PI_MODEL;
	if (provider && modelId) {
		const exact = runtime.getModel(provider, modelId);
		if (exact) return exact;
		console.warn(
			`warning: ${provider}/${modelId} is not in the ModelRuntime catalog; ` +
				`falling back to the first available model`,
		);
	}
	const first = runtime.getModels()[0];
	if (!first) {
		throw new Error(
			"ModelRuntime has no models configured — check ~/.pi/agent/models.json",
		);
	}
	return first;
}

async function smokeRun(): Promise<number> {
	initTheme(process.env.PI_THEME ?? undefined, false);

	const modelRuntime = await ModelRuntime.create();
	const model = await resolveModel(modelRuntime);

	const { session } = await createAgentSession({
		cwd: process.cwd(),
		modelRuntime,
		model,
		thinkingLevel: "low",
		noTools: "all",
		// One fresh, unpersisted session: nothing here may carry context forward.
		sessionManager: SessionManager.inMemory(),
		settingsManager: SettingsManager.inMemory({
			compaction: { enabled: false },
		}),
	});

	console.log(
		"\npi-beads loop — toolchain smoke run\n" +
			`  model      ${model.provider}/${model.id}\n` +
			`  session    ${session.sessionId} (in-memory, persisted: ${session.sessionFile ? "yes" : "no"})\n` +
			`  messages   ${session.messages.length} at start\n`,
	);

	const view = new AssistantMessageComponent(
		undefined,
		true,
		getMarkdownTheme(),
		"Thinking...",
		0,
		[],
	);

	let streamedText = "";
	let lastAssistant: CapturedAssistant | undefined;

	const unsubscribe = session.subscribe((event) => {
		switch (event.type) {
			case "message_update": {
				const message = event.message;
				if (message.role !== "assistant") break;
				const part = event.assistantMessageEvent;
				if (part.type === "text_delta") streamedText += part.delta;
				// Live repaint (incremental redraw, tool components, footer) is
				// workspace-5yn.10's job. This proves the streamed message is
				// renderable through pi's own component.
				lastAssistant = message;
				view.updateContent(message, true);
				break;
			}
			case "message_end": {
				const message = event.message;
				if (message.role !== "assistant") break;
				lastAssistant = message;
				view.updateContent(message, false);
				break;
			}
			default:
				break;
		}
	});

	const startedAt = Date.now();
	try {
		await session.prompt(
			"Reply with exactly one short sentence, then one small TypeScript code " +
				"block (three to five lines). No other prose.",
		);
	} finally {
		unsubscribe();
		session.dispose();
	}

	console.log(view.render(resolveWidth()).join("\n"));

	const usage = lastAssistant?.usage;
	if (usage) {
		console.log(
			`\n  tokens     in ${usage.input} · out ${usage.output} · ` +
				`cache read ${usage.cacheRead} · cache write ${usage.cacheWrite}`,
		);
	}
	console.log(
		`  elapsed    ${Date.now() - startedAt}ms (model-bound)\n` +
			`  streamed   ${streamedText.length} chars\n`,
	);

	if (streamedText.trim().length === 0) {
		console.error("smoke run produced no streamed text");
		return 1;
	}
	console.log("toolchain OK: ESM + tsx + pi SDK streamed through ADR-001 render path");
	return 0;
}

smokeRun()
	.then((code) => process.exit(code))
	.catch((error: unknown) => {
		console.error("smoke run failed:", error instanceof Error ? error.message : error);
		process.exit(1);
	});
