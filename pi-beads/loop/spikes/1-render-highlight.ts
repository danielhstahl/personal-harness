/**
 * ADR-001 spike (1 of 2) — rendering proof. No model call, fully offline.
 *
 * Question it answers: can we get pi.dev's syntax highlighting without running
 * pi's InteractiveMode, by reusing pi's own theme + markdown renderer?
 *
 * Run:  cd spikes && FORCE_COLOR=3 npx tsx 1-render-highlight.ts
 *
 * Exits non-zero if any assertion fails.
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
	AssistantMessageComponent,
	getMarkdownTheme,
	highlightCode,
	initTheme,
} from "@earendil-works/pi-coding-agent";
import { Markdown } from "@earendil-works/pi-tui";

initTheme("dark");

const TS_CODE = `// fib with memoisation
function fib(n: number, memo = new Map<number, number>()): number {
	if (n <= 1) return n;
	if (memo.has(n)) return memo.get(n)!;
	const value = fib(n - 1, memo) + fib(n - 2, memo);
	memo.set(n, value);
	return value;
}`;

const PY_CODE = `def greet(name):
    """Say hello."""
    return f"hello {name}"  # interpolated`;

const DOC = `## Loop iteration 4

Working **workspace-5yn.3** on the beads adapter.

Key rule: never trust \`bd\` exit codes blindly.

${"```"}typescript
${TS_CODE}
${"```"}

- commit first
- remember second
- close last`;

const OUT = join(import.meta.dirname, "out");
mkdirSync(OUT, { recursive: true });

const failures: string[] = [];
function check(name: string, ok: boolean, detail = "") {
	console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
	if (!ok) failures.push(name);
}

const SGR = /\x1b\[[0-9;]*m/g;
const codesIn = (s: string): string[] => [...new Set(s.match(SGR) ?? [])];
const strip = (s: string): string => s.replace(SGR, "");

// ---------------------------------------------------------------- 1. highlightCode
console.log("\n=== 1. pi's own highlightCode() ===");
const tsLines = highlightCode(TS_CODE, "typescript");
const pyLines = highlightCode(PY_CODE, "python");
console.log(tsLines.join("\n"));
console.log(pyLines.join("\n"));

const tsCodes = codesIn(tsLines.join("\n"));
check("TS snippet emits ANSI SGR codes", tsCodes.length > 0, `${tsCodes.length} distinct`);
check(
	"TS snippet is syntax-coloured, not uniformly painted",
	tsCodes.length > 3,
	`${tsCodes.length} distinct codes`,
);

// A keyword must be painted differently from a string literal: this is what
// "syntax highlighting" means, as opposed to "this block is cyan".
const kw = codesIn(highlightCode("function", "typescript").join("\n"));
const str = codesIn(highlightCode('"a string"', "typescript").join("\n"));
const cmt = codesIn(highlightCode("// comment", "typescript").join("\n"));
check("keyword / string / comment get three different colours",
	kw.length > 0 && str.length > 0 && cmt.length > 0 &&
		!(kw.join() === str.join() && str.join() === cmt.join()),
	`kw=[${kw.join(" ")}] str=[${str.join(" ")}] cmt=[${cmt.join(" ")}]`,
);
check("highlighting survives re-render (no cache corruption)",
	highlightCode(TS_CODE, "typescript").join() === tsLines.join());

// ---------------------------------------------------------------- 2. Markdown component
console.log("\n=== 2. pi-tui Markdown + pi markdown theme ===");
const md = new Markdown(DOC, 0, 0, getMarkdownTheme());
const mdLines = md.render(78);
console.log(mdLines.join("\n"));
check("markdown block renders", mdLines.length > 8, `${mdLines.length} lines`);
check(
	"inside the fenced block, code is still syntax-coloured (not plain)",
	codesIn(mdLines.join("\n")).length > 4,
	`${codesIn(mdLines.join("\n")).length} distinct codes`,
);
check(
	"code block body text is present and wrapped",
	strip(mdLines.join("\n")).includes("memo.set(n, value)"),
);

// ---------------------------------------------------------------- 3. AssistantMessageComponent
console.log("\n=== 3. AssistantMessageComponent driven outside InteractiveMode ===");
const assistantLike = {
	role: "assistant",
	content: [
		{ type: "thinking", thinking: "The user wants a fenced code block." },
		{ type: "text", text: DOC },
	],
} as never;
const component = new AssistantMessageComponent(assistantLike, false, getMarkdownTheme(), "Thinking...", 1, []);
const rendered = component.render(78);
console.log(rendered.join("\n"));
check("component renders a non-trivial tree", rendered.length > 8, `${rendered.length} lines`);
check(
	"component output carries theme colours",
	codesIn(rendered.join("\n")).length > 4,
	`${codesIn(rendered.join("\n")).length} distinct codes`,
);
check(
	"component is re-renderable (live streaming updates work the same way)",
	component.render(78).length === rendered.length,
);

// ---------------------------------------------------------------- 4. status quo comparison
console.log("\n=== 4. what the current scaffold could offer instead ===");
// src/format.ts — the hand-rolled ANSI file in the scaffold.
// It declares no exports at all, so nothing outside the file can even call it.
const formatPath = join(OUT, "..", "..", "src", "format.ts");
if (existsSync(formatPath)) {
	const formatSrc = readFileSync(formatPath, "utf8");
	const exportCount = (formatSrc.match(/^export\b/gm) ?? []).length;
	check(
		"src/format.ts is currently un-importable (0 exports) — dead code as a renderer",
		exportCount === 0,
		`${exportCount} export statement(s) found in ${formatPath}`,
	);
} else {
	console.log(`(skipped: ${formatPath} not found)`);
}

// ---------------------------------------------------------------- evidence
writeFileSync(join(OUT, "render-highlight.raw.txt"), [
	"--- highlightCode(typescript) ---",
	tsLines.join("\n"),
	"--- highlightCode(python) ---",
	pyLines.join("\n"),
	"--- Markdown(doc) @ 78 cols ---",
	mdLines.join("\n"),
	"--- AssistantMessageComponent @ 78 cols ---",
	rendered.join("\n"),
	"",
].join("\n"));
writeFileSync(
	join(OUT, "render-highlight.plain.txt"),
	strip(
		[tsLines.join("\n"), mdLines.join("\n"), rendered.join("\n")].join("\n\n"),
	),
);
console.log(`\n${codesIn([tsLines.join("\n"), mdLines.join("\n")].join("\n")).length} distinct SGR codes captured`);
console.log(`evidence -> spikes/out/render-highlight.raw.txt (ANSI) + .plain.txt`);

console.log(
	`\n${failures.length === 0 ? "ALL RENDER ASSERTIONS PASSED" : `FAILED: ${failures.join(", ")}`}`,
);
process.exit(failures.length === 0 ? 0 : 1);
