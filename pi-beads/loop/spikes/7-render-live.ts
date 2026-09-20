/**
 * Spike 7: the work presenter on a real terminal, with the evidence on disk.
 *
 * What the suite proves with a fake `Terminal`, this proves with a pty: that the
 * presenter really paints, that pi's highlighter puts real colour on a real
 * screen, that a physical ctrl+o expands a collapsed result, that streaming
 * arrives as a growing frame rather than a scramble, and that a piped run shows
 * not one escape byte.
 *
 * It bootstraps itself: `node spikes/7-render-live.ts` is the driver; the driver
 * re-invokes this same file under `script(1)` as the child that owns the
 * terminal.
 *
 *   child stream   deltas, a tool call with a long result, a closing say
 *   child expand   the same, plus a real ctrl+o arrives mid-flight
 *   child failure  a timeout, an aborted reply, a failed outcome
 *   child plain    forced off-TTY: the same content, zero escapes
 *
 * Evidence: `spikes/out/7-render-live.raw.txt` (bytes as the terminal got them)
 * and `spikes/out/7-render-live.plain.txt` (the same, escapes stripped).
 *
 * Run: `node spikes/7-render-live.ts`
 */

import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import type { RunnerEvent } from "../src/agent.ts";
import { createWorkPresenter } from "../src/render.ts";

const here = dirname(fileURLToPath(import.meta.url));
const self = join(here, "7-render-live.ts");
const outDir = process.env.LOOP_SPIKE_OUT_DIR ?? join(here, "out");
const rawPath = join(outDir, "7-render-live.raw.txt");
const plainPath = join(outDir, "7-render-live.plain.txt");

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

const PY = [
  "```python",
  "def greet(name):",
  '    """Say hello."""',
  '    return f"hello {name}"  # interpolated',
  "```",
].join("\n");

const TS = [
  "```typescript",
  "// memoised fib",
  "function fib(n: number, memo = new Map<number, number>()): number {",
  "  if (n <= 1) return n;",
  "  return memo.get(n) ?? fib(n - 1, memo) + fib(n - 2, memo);",
  "}",
  "```",
].join("\n");

const REPLY = [
  "Two blocks, streamed in order.",
  "",
  PY,
  "",
  "The fenced section is closed; this prose is plain.",
  "",
  TS,
].join("\n");

const TEST_OUTPUT = Array.from(
  { length: 12 },
  (_, i) => `test ${String(i + 1).padStart(2, "0")} ok — ${"assertion".repeat(3)}`,
).join("\n");

const agentEvent = (raw: unknown): RunnerEvent =>
  ({ type: "agent_event", raw }) as RunnerEvent;

// ── the child: owns the terminal, gets fed scripted events ────────────────────

async function streamScript(presenter: ReturnType<typeof createWorkPresenter>): Promise<void> {
  presenter.setContext({
    issueId: "workspace-5yn.10",
    phase: "work",
    model: "llamacpp/qwen3-coder",
    thinkingLevel: "medium",
  });
  presenter.acquire();
  presenter.notice("info", "picked up workspace-5yn.10");

  presenter.feed(
    agentEvent({ type: "message_start", message: { role: "assistant", content: [] } }),
  );
  const step = Math.ceil(REPLY.length / 28);
  for (let i = step; i <= REPLY.length; i += step) {
    presenter.feed(
      agentEvent({
        type: "message_update",
        message: {
          role: "assistant",
          content: [{ type: "text", text: REPLY.slice(0, i) }],
        },
      }),
    );
    await sleep(18);
  }
  presenter.feed(
    agentEvent({
      type: "message_end",
      message: {
        role: "assistant",
        content: [{ type: "text", text: REPLY }],
        usage: { input: 1_428, output: 512 },
      },
    }),
  );

  presenter.feed(
    agentEvent({
      type: "tool_execution_start",
      toolCallId: "t1",
      toolName: "bash",
      args: { command: "npm test" },
    }),
  );
  await sleep(250);
  presenter.feed(
    agentEvent({
      type: "tool_execution_end",
      toolCallId: "t1",
      toolName: "bash",
      result: { content: [{ type: "text", text: TEST_OUTPUT }] },
      isError: false,
    }),
  );
}

async function runChild(caseName: string): Promise<void> {
  const presenter = createWorkPresenter({
    tty: caseName === "plain" ? false : undefined,
    coalesceMs: 33,
  });

  if (caseName === "plain") {
    // The off-TTY path: same events, no surface, no escapes.
    presenter.setContext({
      issueId: "workspace-5yn.10",
      phase: "work",
      model: "llamacpp/qwen3-coder",
      thinkingLevel: "medium",
    });
    presenter.notice("info", "picked up workspace-5yn.10");
    presenter.feed(
      agentEvent({
        type: "tool_execution_start",
        toolCallId: "t1",
        toolName: "bash",
        args: { command: "npm test" },
      }),
    );
    presenter.feed(
      agentEvent({
        type: "tool_execution_end",
        toolCallId: "t1",
        toolName: "bash",
        result: { content: [{ type: "text", text: TEST_OUTPUT }] },
        isError: false,
      }),
    );
    presenter.warn("board read failed, retrying");
    presenter.release();
    presenter.dispose();
    process.stdout.write("SPIKE-DONE plain\n");
    return;
  }

  if (caseName === "failure") {
    presenter.setContext({
      issueId: "workspace-5yn.10",
      phase: "work",
      model: "llamacpp/qwen3-coder",
      thinkingLevel: "medium",
    });
    presenter.acquire();
    presenter.feed(
      agentEvent({
        type: "message_update",
        message: {
          role: "assistant",
          content: [{ type: "text", text: "I was midway through the " }],
        },
      }),
    );
    await sleep(150);
    presenter.feed(
      agentEvent({
        type: "message_end",
        message: {
          role: "assistant",
          content: [{ type: "text", text: "I was midway through the " }],
          stopReason: "aborted",
        },
      }),
    );
    await sleep(150);
    presenter.feed(agentEvent({ type: "timeout", detail: "budget 01:00" }));
    presenter.notice(
      "error",
      "work on workspace-5yn.10 timed out after 01:00 — left behind: two modified files, no commit",
    );
    await sleep(400);
    presenter.release();
    presenter.dispose();
    process.stdout.write("SPIKE-DONE failure\n");
    return;
  }

  if (caseName === "quiet") {
    // The heartbeat's own case, and the one no unit test can prove: a surface
    // held while nothing happens. One real tool call in flight, then three
    // seconds in which no event of any kind is fed.
    presenter.setContext({
      issueId: "workspace-cog",
      phase: "work",
      model: "llamacpp/qwen3-coder",
      thinkingLevel: "medium",
    });
    presenter.acquire();
    presenter.feed(
      agentEvent({
        type: "tool_execution_start",
        toolCallId: "t1",
        toolName: "bash",
        args: { command: "npm test" },
      }),
    );
    await sleep(100);
    process.stdout.write(`SPIKE-QUIET before=${presenter.stats().paints}\n`);
    await sleep(3_000);
    process.stdout.write(`SPIKE-QUIET after=${presenter.stats().paints}\n`);
    presenter.release();
    presenter.dispose();
    process.stdout.write("SPIKE-DONE quiet\n");
    return;
  }

  // "stream" and "expand" share the script; expand just waits for the keypress.
  await streamScript(presenter);
  if (caseName === "expand") {
    for (let i = 0; i < 60 && !presenter.stats().expanded; i += 1) await sleep(50);
  }
  presenter.say("committed 2 files");
  await sleep(300);
  const stats = presenter.stats();
  presenter.release();
  presenter.dispose();
  process.stdout.write(
    `SPIKE-DONE ${caseName} expanded=${String(stats.expanded)} paints=${String(stats.paints)}\n`,
  );
}

// ── the driver: spawns children under script(1), collects the evidence ────────

interface CaseResult {
  name: string;
  raw: string;
  rc: number | null;
  timedOut: boolean;
}

function shq(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

function runPtyCase(name: string, captureFile: string): Promise<CaseResult> {
  const command = `${shq(process.execPath)} ${shq(self)} child ${shq(name)}`;
  return new Promise((resolve) => {
    const child = spawn("script", ["-q", "-f", "-c", command, captureFile], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    let live = "";
    let keySent = false;
    const timeout = setTimeout(() => child.kill("SIGKILL"), 45_000);

    const finish = (timedOut: boolean): void => {
      clearTimeout(timeout);
      let raw = live;
      try {
        raw = readFileSync(captureFile, "utf8") || live;
      } catch {
        /* keep the live buffer */
      }
      resolve({ name, raw, rc: timedOut ? null : (child.exitCode ?? null), timedOut });
    };

    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      live += chunk;
      // ctrl+o once the collapsed result has had time to reach the screen.
      if (name !== "expand" || keySent || !live.includes("more lines")) return;
      keySent = true;
      setTimeout(() => child.stdin.write("\x0f"), 200);
    });
    child.stderr.resume();
    child.on("error", () => finish(true));
    child.on("close", () => finish(false));
  });
}

function runPlainCase(): Promise<CaseResult> {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [self, "child", "plain"], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let raw = "";
    const timeout = setTimeout(() => child.kill("SIGKILL"), 20_000);
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      raw += chunk;
    });
    child.stderr.resume();
    child.on("error", () => {
      clearTimeout(timeout);
      resolve({ name: "plain", raw, rc: null, timedOut: true });
    });
    child.on("close", (code) => {
      clearTimeout(timeout);
      resolve({ name: "plain", raw, rc: code, timedOut: false });
    });
  });
}

function printable(text: string): string {
  return text
    .replace(/\x1b\[[0-9;?]*[a-zA-Z]/gu, "")
    .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/gu, "")
    .replace(/\r\n/gu, "\n")
    .replace(/\r/gu, "\n");
}

async function drive(): Promise<void> {
  mkdirSync(outDir, { recursive: true });
  const cases = ["stream", "expand", "failure", "quiet"];
  const results: CaseResult[] = [];
  for (const name of cases) {
    // eslint-disable-next-line no-await-in-loop -- one pty at a time, by design
    // The pty capture is scratch; the evidence is 7-render-live.raw.txt.
    const captureDir = mkdtempSync(join(tmpdir(), "loop-spike7-"));
    results.push(await runPtyCase(name, join(captureDir, `${name}.capture`)));
  }
  results.push(await runPlainCase());

  const byName = new Map(results.map((result) => [result.name, result]));
  const stripped = (name: string): string => printable(byName.get(name)?.raw ?? "");
  const raw = (name: string): string => byName.get(name)?.raw ?? "";
  /** Wrapping collapsed: a footer split over two lines is still one footer. */
  const flat = (name: string): string => stripped(name).replace(/\s+/gu, " ");

  const checks: Array<[string, boolean]> = [
    [
      "stream: the child exited cleanly",
      byName.get("stream")?.timedOut === false && byName.get("stream")?.rc === 0,
    ],
    [
      "stream: the presenter painted colour on a real terminal (pi keyword colour 38;5;74)",
      raw("stream").includes("\x1b[38;5;74m"),
    ],
    [
      "stream: pi's comment colour reached the screen (38;5;65)",
      raw("stream").includes("\x1b[38;5;65m"),
    ],
    [
      "stream: the footer shows issue, phase, model and thinking",
      [
        "issue workspace-5yn.10",
        "phase work",
        "model llamacpp/qwen3-coder",
        "thinking medium",
      ].every((part) => flat("stream").includes(part)),
    ],
    [
      "stream: the whole reply is on screen, fence closed, prose after it",
      stripped("stream").includes("def greet(name):") &&
        stripped("stream").includes("The fenced section is closed") &&
        stripped("stream").includes("function fib(n: number"),
    ],
    [
      "stream: the settled tool line reads ✓ and the result collapsed with a count",
      stripped("stream").includes("✓ bash $ npm test") &&
        stripped("stream").includes("+10 more lines"),
    ],
    [
      "expand: a real ctrl+o revealed the hidden lines",
      raw("expand").includes("SPIKE-DONE expand expanded=true") &&
        stripped("expand").includes("test 11 ok") &&
        stripped("expand").includes("test 12 ok"),
    ],
    [
      "failure: the abort and the timeout are both on screen",
      stripped("failure").includes("aborted") &&
        stripped("failure").includes("timed out"),
    ],
    [
      "failure: the failure is painted in a theme colour, not plain text",
      raw("failure").includes("\x1b[38;5;") || raw("failure").includes("\x1b[31m"),
    ],
    [
      "quiet: the surface repainted with no event fed at all",
      (() => {
        const text = stripped("quiet");
        const before = Number(/SPIKE-QUIET before=(\d+)/u.exec(text)?.[1] ?? NaN);
        const after = Number(/SPIKE-QUIET after=(\d+)/u.exec(text)?.[1] ?? NaN);
        return Number.isFinite(before) && Number.isFinite(after) && after - before >= 2;
      })(),
    ],
    [
      "quiet: the footer's elapsed field visibly advanced through the silence",
      flat("quiet").includes("elapsed 00:02") && flat("quiet").includes("elapsed 00:03"),
    ],
    [
      "quiet: those were real erase-and-redraw frames after the marker, not a dump",
      (() => {
        const stream = raw("quiet");
        const marker = stream.indexOf("SPIKE-QUIET before=");
        return (
          marker >= 0 &&
          (stream.slice(marker).match(/\x1b\[2K/gu) ?? []).length > 1
        );
      })(),
    ],
    [
      "teardown: no footer line survives after the child's DONE marker",
      !(stripped("stream").split("SPIKE-DONE")[1] ?? "").includes(
        "issue workspace-5yn.10",
      ),
    ],
    [
      "teardown: the cursor is left visible (last cursor sequence is ?25h)",
      (() => {
        const stream = raw("stream");
        const hidden = stream.lastIndexOf("\x1b[?25l");
        const shown = stream.lastIndexOf("\x1b[?25h");
        return shown !== -1 && shown > hidden;
      })(),
    ],
    [
      "stream: the paint used real repaint sequences, not a single dump",
      (raw("stream").match(/\x1b\[2K/gu) ?? []).length > 1 &&
        (raw("stream").match(/\x1b\[\?2026[hl]/gu) ?? []).length > 1,
    ],
    [
      "plain: a piped run emits zero escape bytes",
      raw("plain").indexOf("\x1b") === -1 && raw("plain").length > 0,
    ],
    [
      "plain: every footer field is still there",
      [
        "issue workspace-5yn.10",
        "phase work",
        "elapsed",
        "tokens",
        "model llamacpp/qwen3-coder",
        "thinking medium",
      ].every((part) => raw("plain").includes(part)),
    ],
    [
      "plain: the settled tool line reads ✓, not …",
      raw("plain").includes("✓ bash $ npm test"),
    ],
  ];

  const lines: string[] = [];
  lines.push("# Work presenter: live pty evidence (spikes/7-render-live.ts)");
  lines.push("");
  lines.push(
    'generated by: script -q -f -c "node spikes/7-render-live.ts child <case>" <capture>',
  );
  lines.push("");
  lines.push("## checks");
  for (const [label, ok] of checks) lines.push(`${ok ? "PASS" : "FAIL"}  ${label}`);
  lines.push("");
  lines.push("## frames (escapes stripped, last 26 non-blank lines per case)");
  for (const name of [...cases, "plain"]) {
    lines.push(`### ${name}`);
    lines.push(
      stripped(name)
        .split("\n")
        .filter((line) => line.trim().length > 0)
        .slice(-26)
        .join("\n"),
    );
    lines.push("");
  }

  const passed = checks.every(([, ok]) => ok);
  lines.push(passed ? "spike passed" : "SPIKE FAILED");
  const text = `${lines.join("\n")}\n`;

  writeFileSync(plainPath, text, "utf8");
  writeFileSync(
    rawPath,
    [
      "# Raw terminal bytes as delivered (spikes/7-render-live.ts)",
      "# The live repaint stream; `cat -v` it to see the paint.",
      "",
      ...[...cases, "plain"].map(
        (name) => `=== case ${name} ===\n${byName.get(name)?.raw ?? ""}\n`,
      ),
    ].join("\n"),
    "utf8",
  );

  process.stdout.write(text);
  process.exitCode = passed ? 0 : 1;
}

// Dispatch last: top-level evaluation is ordered, so by the time the branch runs
// every const above is initialised. Child first, driver otherwise.
if (process.argv[2] === "child") {
  await runChild(process.argv[3] ?? "stream");
} else {
  await drive();
}
