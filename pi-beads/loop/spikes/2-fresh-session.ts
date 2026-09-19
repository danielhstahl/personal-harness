/**
 * ADR-001 spike (2 of 2) — the context boundary, proven against a live model.
 *
 * Question it answers: with the in-process SDK, does "start a brand-new session
 * per iteration" really give us a clean context slate — and does the streamed
 * reply render through the same highlighted path as spike 1?
 *
 * Run:  cd spikes && FORCE_COLOR=3 npx tsx 2-fresh-session.ts
 *
 * Requires the local model endpoint from ~/.pi/agent/models.json to be reachable.
 * Exits non-zero if any assertion fails.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
	AssistantMessageComponent,
	createAgentSession,
	createExtensionRuntime,
	getMarkdownTheme,
	initTheme,
	type ResourceLoader,
	SettingsManager,
	SessionManager,
} from "@earendil-works/pi-coding-agent";
import { ModelRuntime } from "@earendil-works/pi-coding-agent";

initTheme("dark");

const OUT = join(import.meta.dirname, "out");
mkdirSync(OUT, { recursive: true });

const CODE_FIXTURE_PROMPT =
	"You are a terse code fixture used by an automated loop. Answer with exactly one markdown code block and no prose.";

const PROBE_PROMPT =
	"You are a stateless fixture. Answer in one short sentence. Do not guess.";

/** No extensions, no skills, no AGENTS.md, no themes discovery.
 *  A loop iteration must be reproducible from its arguments alone; inheriting
 *  the developer's ambient extension set (pi-workgraph et al.) would make the
 *  proof — and the app — non-reproducible. */
function bareResourceLoader(systemPrompt: string): ResourceLoader {
	return {
		getExtensions: () => ({ extensions: [], errors: [], runtime: createExtensionRuntime() }),
		getSkills: () => ({ skills: [], diagnostics: [] }),
		getPrompts: () => ({ prompts: [], diagnostics: [] }),
		getThemes: () => ({ themes: [], diagnostics: [] }),
		getAgentsFiles: () => ({ agentsFiles: [] }),
		getSystemPrompt: () => systemPrompt,
		getSystemPromptSource: () => undefined,
		getAppendSystemPrompt: () => [],
		getAppendSystemPromptSources: () => [],
		extendResources: () => {},
		reload: async () => {},
	};
}

type AnyMessage = {
	role: string;
	content?: Array<{ type: string; text?: string; thinking?: string }>;
	usage?: Record<string, number>;
	stopReason?: string;
};

type Iteration = {
	n: number;
	prompt: string;
	sessionId: string;
	sessionFile: string | undefined;
	messagesBefore: number;
	messagesAfter: number;
	deltas: number;
	inputTokens: number;
	cacheRead: number;
	cacheWrite: number;
	totalContextTokens: number;
	sessionCreateMs: number;
	firstDeltaMs: number;
	totalMs: number;
	text: string;
	rendered: string[];
};

async function runIteration(
	modelRuntime: ModelRuntime,
	model: never,
	n: number,
	prompt: string,
	systemPrompt: string,
): Promise<Iteration> {
	const tStart = Date.now();
	const { session } = await createAgentSession({
		cwd: process.cwd(),
		modelRuntime,
		model,
		thinkingLevel: "low",
		sessionManager: SessionManager.inMemory(),
		resourceLoader: bareResourceLoader(systemPrompt),
		noTools: "all",
		settingsManager: SettingsManager.inMemory({ compaction: { enabled: false } }),
	});

	const sessionId = session.sessionId;
	const sessionFile = session.sessionFile;
	const messagesBefore = session.messages.length;
	const sessionCreateMs = Date.now() - tStart;

	let deltas = 0;
	let firstDeltaMs = 0;
	let streamed = "";
	const unsubscribe = session.subscribe((event) => {
		if (event.type === "message_update") {
			const am = (event as { assistantMessageEvent?: { type: string; delta?: string } })
				.assistantMessageEvent;
			if (am?.type === "text_delta") {
				deltas++;
				if (!firstDeltaMs) firstDeltaMs = Date.now() - tStart;
				streamed += am.delta ?? "";
			}
		}
	});

	try {
		await session.prompt(prompt);
	} finally {
		unsubscribe();
		session.dispose();
	}

	const messages = session.messages as unknown as AnyMessage[];
	const lastAssistant = [...messages].reverse().find((m) => m.role === "assistant");
	const text =
		(lastAssistant?.content ?? [])
			.filter((c) => c.type === "text")
			.map((c) => c.text ?? "")
			.join("") || streamed;

	const component = new AssistantMessageComponent(
		{ role: "assistant", content: lastAssistant?.content ?? [{ type: "text", text }] } as never,
		true,
		getMarkdownTheme(),
		"Thinking...",
		1,
		[],
	);

	return {
		n,
		prompt,
		sessionId,
		sessionFile,
		messagesBefore,
		messagesAfter: messages.length,
		deltas,
		inputTokens: lastAssistant?.usage?.input ?? 0,
		cacheRead: lastAssistant?.usage?.cacheRead ?? 0,
		cacheWrite: lastAssistant?.usage?.cacheWrite ?? 0,
		totalContextTokens:
			(lastAssistant?.usage?.input ?? 0) +
			(lastAssistant?.usage?.cacheRead ?? 0) +
			(lastAssistant?.usage?.cacheWrite ?? 0),
		sessionCreateMs,
		firstDeltaMs,
		totalMs: Date.now() - tStart,
		text,
		rendered: component.render(88),
	};
}

const SGR = /\x1b\[[0-9;]*m/g;
const codesIn = (s: string): string[] => [...new Set(s.match(SGR) ?? [])];
const failures: string[] = [];
function check(name: string, ok: boolean, detail = "") {
	console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
	if (!ok) failures.push(name);
}

const modelRuntime = await ModelRuntime.create();
const providerId = process.env.SPIKE_PROVIDER ?? process.env.PI_PROVIDER ?? "llamacpp";
const modelId = process.env.SPIKE_MODEL ?? process.env.PI_MODEL ?? "halogen-qwen3.8-flash-next";
const model =
	modelRuntime.getModel(providerId, modelId) ??
	(modelRuntime.getModels()[0] as never);
if (!model) {
	console.error("No model resolvable from ModelRuntime; cannot run spike 2.");
	process.exit(1);
}
console.log(`model: ${(model as { provider?: string; id?: string }).provider}/${(model as { id?: string }).id}`);

const runs: Iteration[] = [];
const scenarios = [
	{
		system: CODE_FIXTURE_PROMPT,
		text: "Write a Python function `fib(n)` that returns the nth Fibonacci number.",
	},
	{
		system: CODE_FIXTURE_PROMPT,
		text: "Write a TypeScript function `reverse(s: string): string` that returns the string reversed.",
	},
	{
		system: PROBE_PROMPT,
		text:
			"Earlier in this session you were asked to write some code. In one short " +
			"sentence, state exactly what code you were asked for. If you have no " +
			"record of any such request, reply with exactly: UNKNOWN",
	},
];

for (const [i, scenario] of scenarios.entries()) {
	console.log(`\n=== iteration ${i + 1} ===\nprompt: ${scenario.text}`);
	const run = await runIteration(modelRuntime, model as never, i + 1, scenario.text, scenario.system);
	runs.push(run);
	console.log(
		`session=${run.sessionId.slice(0, 8)} file=${run.sessionFile ?? "<none: in-memory>"} ` +
			`msgs ${run.messagesBefore}->${run.messagesAfter} deltas=${run.deltas} ` +
			`input_tokens=${run.inputTokens} cache_read=${run.cacheRead} cache_write=${run.cacheWrite} ` +
			`context=${run.totalContextTokens} ` +
			`session_create=${run.sessionCreateMs}ms first_delta=${run.firstDeltaMs}ms total=${run.totalMs}ms`,
	);
	console.log("--- rendered through AssistantMessageComponent ---");
	console.log(run.rendered.join("\n"));
}

// ---------------------------------------------------------------- assertions
console.log("\n=== context-boundary assertions ===");

check(
	"every iteration started from an empty message list",
	runs.every((r) => r.messagesBefore === 0),
	runs.map((r) => `#${r.n}:${r.messagesBefore}`).join(" "),
);
check(
	"every iteration is a distinct session",
	new Set(runs.map((r) => r.sessionId)).size === runs.length,
	`${new Set(runs.map((r) => r.sessionId)).size} distinct ids for ${runs.length} runs`,
);
check(
	"in-memory sessions wrote no session file",
	runs.every((r) => r.sessionFile === undefined),
	runs.map((r) => String(r.sessionFile)).join(" "),
);
check(
	"iteration 1 produced a fenced code block from the live stream",
	/```/.test(runs[0].text),
	`${runs[0].deltas} text deltas`,
);
check(
	"the live-streamed code block renders with syntax colour (not plain text)",
	codesIn(runs[0].rendered.join("\n")).length > 3,
	`${codesIn(runs[0].rendered.join("\n")).length} distinct SGR codes`,
);

const [a, b, c] = runs;
const cumulative = a.totalContextTokens + b.totalContextTokens;
check(
	"iteration 3 context is NOT cumulative (fresh context each iteration)",
	c.totalContextTokens <= Math.max(1, cumulative) * 0.8,
	`p3=${c.totalContextTokens} vs p1=${a.totalContextTokens} p2=${b.totalContextTokens} (sum ${cumulative})`,
);
check(
	"iteration 3 context is comparable to a cold first prompt",
	c.totalContextTokens <= a.totalContextTokens * 1.6,
	`p3=${c.totalContextTokens} vs p1=${a.totalContextTokens}`,
);
const probe = c.text.trim();
check(
	"iteration 3 has no record of iteration 1 or 2 (true amnesia, not truncation)",
	/UNKNOWN/i.test(probe) && !/fib|fibonacci|reverse/i.test(probe),
	`answer: ${probe.slice(0, 200)}`,
);

check(
	"a fresh in-process session is cheap to construct (no process spawn)",
	Math.max(...runs.map((r) => r.sessionCreateMs)) < 1500,
	`max create ${Math.max(...runs.map((r) => r.sessionCreateMs))}ms`,
);

// ---------------------------------------------------------------- evidence
writeFileSync(
	join(OUT, "fresh-session.json"),
	JSON.stringify(
		runs.map(({ rendered, ...r }) => ({
			...r,
			rendered_plain: rendered.map((l) => l.replace(SGR, "")),
		})),
		null,
		2,
	),
);
console.log(`evidence -> spikes/out/fresh-session.json`);
console.log(
	`\n${failures.length === 0 ? "ALL CONTEXT ASSERTIONS PASSED" : `FAILED: ${failures.join(", ")}`}`,
);
process.exit(failures.length === 0 ? 0 : 1);
