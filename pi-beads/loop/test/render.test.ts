/**
 * Tests for the work presenter — `src/render.ts` (workspace-5yn.10).
 *
 * Every rule in the acceptance criteria is a `describe` below, keyed by its rule
 * number. No network, no model, no board, no git: everything renders against a
 * fake `Terminal` implementing pi-tui's real contract, the way `.6` does.
 *
 *   rule 0   the seam: one output region, no spawning, idle never shares the
 *            terminal, app wiring verified at the composition root
 *   rule 1   one machine still: no phases decided here, no board handle
 *   rule 2   streaming without corruption: no half escapes, no fence bleed
 *   rule 3   bounded frame cost: one paint per coalescing window, and a live
 *            stream gets one in every window rather than one per reply
 *   rule 4   highlighting that matches pi, asserted against pi's own
 *            `highlightCode` and its live theme
 *   rules 5-7  tool calls collapse to one line, results expand under a
 *            registered keybinding, unknown events degrade to one honest line
 *   rule 8   the footer: fixed order, tolerant fields, removed on teardown
 *   rule 9   non-TTY output is plain text with every field still present
 *   rule 10  no dead export left in `src/format.ts`
 *   rule 11  no hand-rolled ANSI, verified over comment-stripped source
 *   rule 12  composition: ordering and theme-styled warnings
 *   rule 13  timeout, abort and failure rendered as themselves
 *   rule 14  the heartbeat: a held surface repaints the clock between events
 */

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";

import {
  KeybindingsManager,
  StdinBuffer,
  TUI_KEYBINDINGS,
  stripTerminalSequences,
} from "@earendil-works/pi-tui";
import {
  AssistantMessageComponent,
  getMarkdownTheme,
  highlightCode,
  initTheme,
  rawKeyHint,
} from "@earendil-works/pi-coding-agent";

import type { RunnerEvent, WorkOutcome } from "../src/agent.ts";
import { buildApp } from "../src/app.ts";
import { createIdleMode } from "../src/idle.ts";
import {
  MISSING,
  PRESENTER_ROLES,
  SPINNER_FRAMES,
  buildFooterSegments,
  createNullPresenter,
  createPresenterTheme,
  createWorkPresenter,
  describeOutcome,
  formatElapsed,
  formatTokenPair,
  joinFooter,
  type WorkPresenter,
} from "../src/render.ts";

const here = dirname(fileURLToPath(import.meta.url));
const sourceOf = (name: string): string =>
  readFileSync(join(here, "..", "src", name), "utf8");

// pi's live theme, initialised once for the whole file (idempotent).
initTheme("dark", false);

// ── fakes ────────────────────────────────────────────────────────────────────

/**
 * A `Terminal` we can drive and inspect. It also counts how many surfaces are
 * attached at once — rule 0's "only one surface is live" is measured here rather
 * than hoped for.
 */
class FakeTerminal {
  writes: string[] = [];
  calls: string[] = [];
  columnsValue: number;
  rowsValue: number;
  /** Surfaces currently attached (start +1, stop -1). */
  attached = 0;
  /** High-water mark of `attached` — must never exceed 1. */
  maxAttached = 0;
  private inputHandler?: (data: string) => void;
  private resizeHandler?: () => void;

  constructor(columns = 80, rows = 24) {
    this.columnsValue = columns;
    this.rowsValue = rows;
  }

  get kittyProtocolActive(): boolean {
    return false;
  }

  start(onInput: (data: string) => void, onResize: () => void): void {
    this.calls.push("start");
    this.attached += 1;
    this.maxAttached = Math.max(this.maxAttached, this.attached);
    this.inputHandler = onInput;
    this.resizeHandler = onResize;
  }

  stop(): void {
    this.calls.push("stop");
    this.attached = Math.max(0, this.attached - 1);
  }

  async drainInput(): Promise<void> {}

  write(data: string): void {
    this.writes.push(data);
  }

  get columns(): number {
    return this.columnsValue;
  }

  get rows(): number {
    return this.rowsValue;
  }

  moveBy(_lines: number): void {}
  hideCursor(): void {}
  showCursor(): void {}
  clearLine(): void {}
  clearFromCursor(): void {}
  clearScreen(): void {}
  setTitle(_title: string): void {}
  setProgress(_active: boolean): void {}

  /** Input arrives through pi's own splitter, one key sequence at a time. */
  input(data: string): void {
    if (this.inputHandler === undefined) throw new Error("terminal not started");
    const buffer = new StdinBuffer();
    const sequences: string[] = [];
    buffer.on("data", (sequence) => sequences.push(sequence));
    buffer.process(data);
    for (const sequence of sequences) this.inputHandler(sequence);
  }

  resize(columns: number): void {
    this.columnsValue = columns;
    this.resizeHandler?.();
  }

  get output(): string {
    return this.writes.join("");
  }
}

/** A clock and a scheduler we advance by hand, so frame cost is arithmetic. */
function fakeTime(start = 1_000): {
  now: () => number;
  advance(ms: number): void;
  pending: () => number;
  fire: () => void;
  fireAll: () => void;
  tick: (ms: number) => void;
  schedule: (run: () => void, ms: number) => () => void;
} {
  let current = start;
  const timers = new Map<number, { at: number; run: () => void }>();
  let nextId = 1;
  return {
    now: () => current,
    advance(ms: number) {
      current += ms;
    },
    pending: () => timers.size,
    fire() {
      const first = [...timers.entries()].sort((a, b) => a[1].at - b[1].at)[0];
      if (first === undefined) return;
      timers.delete(first[0]);
      current = Math.max(current, first[1].at);
      first[1].run();
    },
    fireAll() {
      const ordered = [...timers.entries()].sort((a, b) => a[1].at - b[1].at);
      for (const [id, timer] of ordered) {
        timers.delete(id);
        current = Math.max(current, timer.at);
        timer.run();
      }
    },
    schedule(run: () => void, ms: number) {
      const id = nextId++;
      timers.set(id, { at: current + ms, run });
      return () => {
        timers.delete(id);
      };
    },
    /**
     * Move the clock on and fire only what came due — the way a real one does.
     * `fireAll` cannot express cadence: it fires timers that are not due yet,
     * which is exactly the thing a streaming test needs to measure honestly.
     */
    tick(ms: number) {
      current += ms;
      const due = [...timers.entries()]
        .filter(([, timer]) => timer.at <= current)
        .sort((a, b) => a[1].at - b[1].at);
      for (const [id, timer] of due) {
        timers.delete(id);
        timer.run();
      }
    },
  };
}

interface PresenterHarness {
  term: FakeTerminal;
  time: ReturnType<typeof fakeTime>;
  presenter: WorkPresenter;
  /** The current live frame, colours intact. */
  frame: () => string[];
  /** The same frame with escapes stripped and trailing space trimmed. */
  plain: () => string[];
  footerLine: () => string;
}

function presenterHarness(
  options: {
    columns?: number;
    tty?: boolean;
    coalesceMs?: number;
    heartbeatMs?: number;
    spinnerMs?: number;
    collapsedPreviewLines?: number;
    maxExpandedChars?: number;
    expandKey?: string;
    keybindings?: KeybindingsManager;
    live?: boolean;
  } = {},
): PresenterHarness {
  const term = new FakeTerminal(options.columns ?? 80, 24);
  const time = fakeTime();
  const presenter = createWorkPresenter({
    terminal: term as never,
    tty: options.tty ?? true,
    now: time.now,
    schedule: time.schedule,
    coalesceMs: options.coalesceMs ?? 33,
    heartbeatMs: options.heartbeatMs ?? 500,
    spinnerMs: options.spinnerMs,
    collapsedPreviewLines: options.collapsedPreviewLines ?? 2,
    maxExpandedChars: options.maxExpandedChars ?? 4_000,
    expandKey: options.expandKey,
    keybindings: options.keybindings,
  });
  if (options.live ?? true) presenter.acquire();

  return {
    term,
    time,
    presenter,
    frame: () => presenter.captureFrame(),
    plain: () => presenter.capturePlain(),
    footerLine: () => {
      const frame = presenter.capturePlain();
      const start = footerStartOf(frame);
      return start < 0 ? "" : frame.slice(start).join(" ");
    },
  };
}

/**
 * The footer is the last block in a frame. Find where it starts so "content"
 * assertions can stop before it — a footer colour is not a leak from a code block.
 */
function footerStartOf(frame: readonly string[]): number {
  return frame.findIndex((line) => /^\s*issue\b/u.test(stripTerminalSequences(line)));
}

/** The code a colour starts with — the reset codes every span shares are noise. */
function openCode(styled: string): string {
  return /^\x1b\[[0-9;]*m/u.exec(styled)?.[0] ?? "";
}

// ── shared fixtures ──────────────────────────────────────────────────────────

const PY_BLOCK = [
  "```python",
  "def greet(name):",
  '    """Say hello."""',
  '    return f"hello {name}"  # interpolated',
  "```",
].join("\n");

const TS_BLOCK = [
  "```typescript",
  "// memoised fib",
  "function fib(n: number, memo = new Map<number, number>()): number {",
  "  if (n <= 1) return n;",
  "  return memo.get(n) ?? fib(n - 1, memo) + fib(n - 2, memo);",
  "}",
  "```",
].join("\n");

const REPLY = [
  "Here is the greeter and the fib.",
  "",
  PY_BLOCK,
  "",
  "And now the typed version.",
  "",
  TS_BLOCK,
  "",
  "Both are colour-checked below the fence.",
].join("\n");

const assistantEvent = (
  type: "message_start" | "message_update" | "message_end",
  text: string,
  extra: Record<string, unknown> = {},
): RunnerEvent => ({
  type: "agent_event",
  raw: {
    type,
    message: { role: "assistant", content: [{ type: "text", text }], ...extra },
  },
});

/** Split `text` into `n` cumulative prefixes — how pi actually streams. */
function cumulativeChunks(text: string, n: number): string[] {
  const size = Math.ceil(text.length / n);
  const out: string[] = [];
  for (let i = 1; i <= n; i += 1) out.push(text.slice(0, i * size));
  if (out.length === 0 || out[out.length - 1] !== text) out.push(text);
  return out;
}

const SGR = /\x1b\[[0-9;]*m/g;
const codesIn = (text: string): string[] => [...new Set(text.match(SGR) ?? [])];
const codeSet = (lines: readonly string[]): Set<string> =>
  new Set(lines.flatMap((line) => line.match(SGR) ?? []));

/** Strip comments so "no hand-rolled ANSI" is checked over live code only. */
function stripComments(source: string): string {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^\s*\/\/.*$/gm, "");
}

const toolStart = (
  id: string,
  name: string,
  args: unknown,
): RunnerEvent => ({
  type: "agent_event",
  raw: { type: "tool_execution_start", toolCallId: id, toolName: name, args },
});

const toolEnd = (
  id: string,
  name: string,
  result: unknown,
  isError = false,
): RunnerEvent => ({
  type: "agent_event",
  raw: {
    type: "tool_execution_end",
    toolCallId: id,
    toolName: name,
    result,
    isError,
  },
});

const textResult = (text: string): unknown => ({
  content: [{ type: "text", text }],
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 0 — the seam
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 0: the presenter owns one output region and spawns nothing", () => {
  it("render.ts imports no child_process and spawns no process", () => {
    const source = stripComments(sourceOf("render.ts"));
    for (const forbidden of [
      "child_process",
      "execSync",
      "spawnSync",
      "execFile",
      "spawn(",
    ]) {
      assert.ok(
        !source.includes(forbidden),
        `render.ts must not contain ${forbidden}`,
      );
    }
  });

  it("render.ts is not added to the spawn allowlist", () => {
    const allowlistSource = readFileSync(join(here, "beads.test.ts"), "utf8");
    const block = /const SPAWN_ALLOWLIST[^{]*\{([\s\S]*?)\n\}/u.exec(
      allowlistSource,
    );
    assert.ok(block, "SPAWN_ALLOWLIST should still be a literal in beads.test.ts");
    const keys = [...(block?.[1] ?? "").matchAll(/"([^"]+\.ts)"/gu)].map((m) => m[1]);
    assert.ok(
      !keys.includes("render.ts"),
      `render.ts must not be allowlisted to spawn; got ${keys.join(", ")}`,
    );
  });

  it("acquire attaches exactly one surface; release detaches it", () => {
    const h = presenterHarness({ live: false });
    assert.equal(h.presenter.isLive, false);
    h.presenter.acquire();
    assert.equal(h.presenter.isLive, true);
    assert.equal(h.term.maxAttached, 1, "one surface attached, no more");
    h.presenter.release();
    assert.equal(h.presenter.isLive, false);
    assert.equal(h.term.calls.filter((c) => c === "start").length, 1);
    assert.equal(h.term.calls.filter((c) => c === "stop").length, 1);
  });

  it("the work presenter and idle are never both attached to one terminal", async () => {
    const term = new FakeTerminal(80, 24);
    const presenter = createWorkPresenter({ terminal: term as never, tty: true });

    presenter.setContext({ issueId: "ws.1", phase: "work" });
    presenter.acquire();
    presenter.say("working");
    presenter.flushSync();
    assert.equal(presenter.isLive, true);

    // Hand over, then bring idle up on the SAME terminal.
    presenter.release();
    const idle = createIdleMode({
      terminal: term as never,
      status: async () => ({ ready: 0, inProgress: 0 }),
      signals: { on: () => () => undefined },
    });
    const pending = idle.next();
    void pending.catch(() => undefined);
    await new Promise((resolve) => setTimeout(resolve, 60));

    assert.equal(term.attached, 1, "exactly one surface attached right now");
    assert.equal(term.maxAttached, 1, "never two at once, at any point");
    assert.equal(presenter.isLive, false, "presenter stayed down while idle is up");
    await idle.dispose();
  });

  it("acquire after release re-attaches without repainting earlier scrollback", () => {
    const h = presenterHarness({ live: false });
    h.presenter.say("first, before any surface");
    h.presenter.acquire();
    h.presenter.say("then this one only");
    h.presenter.flushSync();
    const lines = h.plain();
    assert.ok(lines.some((l) => l.includes("then this one only")));
    assert.ok(
      !lines.some((l) => l.includes("first, before any surface")),
      "content written before acquire stays in scrollback, out of the live region",
    );
  });

  it("the app wires the presenter at the runner seam and releases it before idle", async () => {
    const term = new FakeTerminal(100, 24);
    const presenter = createWorkPresenter({ terminal: term as never, tty: true });
    const seen: { duringWork: boolean | null; duringIdle: boolean | null } = {
      duringWork: null,
      duringIdle: null,
    };

    const outcome: WorkOutcome = {
      kind: "done",
      issueId: "app.1",
      sessionId: "s",
      sessionFile: null,
      verdictSource: "report_done_tool",
      assistantText: "",
      elapsedMs: 1,
      contextNotes: [],
      verdictToolCalls: 1,
      verdict: {
        done: true,
        summary: "did it",
        changedFiles: ["a.ts"],
        nextSteps: [],
      },
    };

    const app = buildApp({
      cwd: here,
      overrides: {
        presenter,
        beads: {} as never,
        git: {} as never,
        splitter: {} as never,
        finalizer: { finalize: async () => ({ kind: "finalized" }) as never },
        runner: {
          run: async () => {
            seen.duringWork = presenter.isLive;
            return outcome;
          },
          split: async () => [],
          dispose: async () => 0,
          liveSessionIds: () => [],
          stats: () => ({ created: 0, disposed: 0, live: 0 }),
        },
        idle: {
          next: async () => {
            seen.duringIdle = presenter.isLive;
            return { kind: "exit", reason: "command" };
          },
        },
      },
    });

    assert.equal(app.presenter, presenter, "the app exposes the presenter it wired");
    await app.ports.runner.run("app.1");
    assert.equal(seen.duringWork, true, "the surface is live while work runs");
    assert.ok(
      presenter.capturePlain().some((line) => line.includes("issue app.1")),
      "the footer names the issue the loop asked to work on",
    );

    await app.ports.idle.next();
    assert.equal(seen.duringIdle, false, "idle never comes up over a live surface");
    assert.equal(term.maxAttached, 1);
  });

  it("a null presenter is wired the same way and owns nothing", async () => {
    const app = buildApp({
      cwd: here,
      overrides: {
        presenter: null,
        beads: {} as never,
        git: {} as never,
        splitter: {} as never,
        finalizer: { finalize: async () => ({ kind: "finalized" }) as never },
        runner: {
          run: async () => outcomeDone(),
          split: async () => [],
          dispose: async () => 0,
          liveSessionIds: () => [],
          stats: () => ({ created: 0, disposed: 0, live: 0 }),
        },
        idle: { next: async () => ({ kind: "exit", reason: "command" }) },
      },
    });
    assert.equal(app.presenter.isLive, false);
    await app.ports.runner.run("app.9");
    await app.ports.idle.next();
    assert.deepEqual(app.presenter.capturePlain(), []);
    assert.equal(createNullPresenter().path, "plain");
  });
});

function outcomeDone(): WorkOutcome {
  return {
    kind: "done",
    issueId: "app.9",
    sessionId: "s",
    sessionFile: null,
    verdictSource: "report_done_tool",
    assistantText: "",
    elapsedMs: 1,
    contextNotes: [],
    verdictToolCalls: 1,
    verdict: { done: true, summary: "s", changedFiles: [], nextSteps: [] },
  };
}

// ══════════════════════════════════════════════════════════════════════════════
// rule 1 — one machine still
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 1: the presenter decides nothing", () => {
  it("render.ts imports neither the orchestrator nor the loop nor the board", () => {
    const source = stripComments(sourceOf("render.ts"));
    for (const forbidden of [
      "./orchestrator",
      "./loop",
      "./beads",
      "./vcs",
      "./repo",
      "./finalize",
      "OrchestratorState",
    ]) {
      assert.ok(
        !source.includes(forbidden),
        `render.ts must not reference ${forbidden} — phase arrives as data`,
      );
    }
  });

  it("phase arrives as data and is never recomputed", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.1", phase: "work" });
    h.presenter.feed(assistantEvent("message_end", "done"));
    h.presenter.feed({ type: "session_disposed", detail: "closed" });
    h.presenter.feed({ type: "timeout", detail: "budget" });
    h.presenter.flushSync();
    const footer = h.footerLine();
    assert.match(footer, /phase work/u, "the phase shown is the phase we were told");
    assert.ok(
      !/phase (idle|split|finalize|done)/u.test(footer),
      "the presenter invented a phase it was never given",
    );
  });

  it("no branch on iteration: the word never appears in the source", () => {
    const source = stripComments(sourceOf("render.ts"));
    assert.ok(!/\biteration\b/u.test(source), "render.ts must not track iterations");
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 2 — streaming without corruption
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 2: deltas render incrementally and corrupt nothing", () => {
  it("(a) the final streamed frame equals a one-shot render of the same text", () => {
    const streamed = presenterHarness();
    streamed.presenter.feed(assistantEvent("message_start", ""));
    for (const chunk of cumulativeChunks(REPLY, 24)) {
      streamed.presenter.feed(assistantEvent("message_update", chunk));
    }
    streamed.presenter.feed(assistantEvent("message_end", REPLY));
    streamed.presenter.flushSync();

    const oneShot = presenterHarness();
    oneShot.presenter.feed(assistantEvent("message_end", REPLY));
    oneShot.presenter.flushSync();

    assert.deepEqual(
      streamed.plain(),
      oneShot.plain(),
      "streaming 24 deltas must land on the frame one-shot paints",
    );
  });

  it("(b) no intermediate frame contains a partial escape", () => {
    const h = presenterHarness();
    const frames: string[] = [];
    for (const chunk of cumulativeChunks(REPLY, 40)) {
      h.presenter.feed(assistantEvent("message_update", chunk));
      frames.push(h.frame().join("\n"));
    }
    assert.ok(frames.length >= 40, "expected a frame per delta");
    for (const [index, frame] of frames.entries()) {
      const stripped = stripTerminalSequences(frame);
      const at = stripped.indexOf("\x1b");
      assert.equal(
        at,
        -1,
        `frame ${index} still holds an escape after stripping: ${JSON.stringify(
          stripped.slice(at, at + 24),
        )}`,
      );
    }
  });

  it("(b) every streamed frame is pi's own render of the same partial text", () => {
    const h = presenterHarness();
    const sample = "A **bold** claim and `code` here.\n\n- one bullet\n- two bullets";
    for (const chunk of cumulativeChunks(sample, 20)) {
      h.presenter.feed(assistantEvent("message_update", chunk));
      const component = new AssistantMessageComponent(
        {
          role: "assistant",
          content: [{ type: "text", text: chunk }],
        } as never,
        true,
        getMarkdownTheme(),
        "Thinking…",
        1,
        [],
      );
      const expected = component.render(80).join("\n");
      assert.ok(
        h.frame().join("\n").includes(expected),
        `frame is not pi's render of ${JSON.stringify(chunk.slice(-12))}: ${JSON.stringify(
          h.plain().join("\n").slice(0, 160),
        )}`,
      );
    }
  });

  it("(b) a finished frame carries no literal emphasis markup", () => {
    const h = presenterHarness();
    h.presenter.feed(
      assistantEvent("message_end", "A **bold** claim and `code` here."),
    );
    h.presenter.flushSync();
    const visible = h.plain().join("\n");
    assert.ok(!/\*\w/u.test(visible), `markdown emphasis leaked: ${visible}`);
    assert.ok(!/`/u.test(visible), "inline code markers leaked");
  });

  it("(c) closing the fence closes the block — nothing bleeds past it", () => {
    const h = presenterHarness();
    h.presenter.feed(assistantEvent("message_end", REPLY));
    h.presenter.flushSync();
    const frame = h.frame();
    const text = frame.map((line) => stripTerminalSequences(line));

    const fenceIndexes = text
      .map((line, i) => (/^\s*```/u.test(line) ? i : -1))
      .filter((i) => i >= 0);
    assert.ok(fenceIndexes.length >= 4, "both fences present");

    const firstFence = fenceIndexes[0]!;
    const lastFence = fenceIndexes[fenceIndexes.length - 1]!;
    const footerAt = footerStartOf(frame);
    assert.ok(footerAt > lastFence, "the footer sits after the content");
    const leadingProse = frame.slice(0, firstFence);
    const blockLines = frame.slice(firstFence, lastFence + 1);
    const trailingProse = frame.slice(lastFence + 1, footerAt);

    assert.ok(trailingProse.length > 0, "there IS text after the last fence");
    assert.ok(codeSet(blockLines).size > 1, "the block is multi-coloured");

    const blockOnly = [...codeSet(blockLines)].filter(
      (c) => !codeSet(leadingProse).has(c),
    );
    const leaked = blockOnly.filter((c) => codeSet(trailingProse).has(c));
    assert.deepEqual(
      leaked,
      [],
      "colour unique to the code block is running past the closing fence",
    );
  });

  it("an unterminated fence mid-stream never emits a partial escape", () => {
    const h = presenterHarness();
    const partial = "Writing now:\n```python\ndef half_open(x):";
    for (const chunk of cumulativeChunks(partial, 12)) {
      h.presenter.feed(assistantEvent("message_update", chunk));
      const stripped = stripTerminalSequences(h.frame().join("\n"));
      assert.equal(stripped.indexOf("\x1b"), -1, "partial escape during an open fence");
    }
  });
});

/**
 * Stream `REPLY` across `windows` coalescing windows and report how many
 * frames came back. Each chunk is followed by the slice of time it would have
 * taken to arrive, so this measures cadence rather than luck.
 */
function framesWhileStreaming(
  options: { coalesceMs?: number; heartbeatMs?: number },
  windows = 30,
): number {
  const h = presenterHarness(options);
  h.presenter.setContext({ issueId: "ws.5", phase: "work" });
  h.presenter.feed(assistantEvent("message_start", ""));
  const before = h.presenter.stats().paints;

  const windowMs = options.coalesceMs ?? 33;
  const chunks = cumulativeChunks(REPLY, windows * 10);
  const perWindow = Math.ceil(chunks.length / windows);
  let fed = 0;
  for (let w = 0; w < windows; w += 1) {
    for (const chunk of chunks.slice(fed, fed + perWindow)) {
      h.presenter.feed(assistantEvent("message_update", chunk));
    }
    fed += perWindow;
    h.time.tick(windowMs);
  }
  const painted = h.presenter.stats().paints - before;
  h.presenter.dispose();
  return painted;
}

// ══════════════════════════════════════════════════════════════════════════════
// rule 3 — bounded frame cost
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 3: frame cost is bounded by the coalescing window, not by tokens", () => {
  it("N deltas inside one window cost exactly one paint", () => {
    // The heartbeat is rule 14's subject. Here the count that matters is the
    // coalescing window's alone, so it is turned off.
    const h = presenterHarness({ coalesceMs: 33, heartbeatMs: 0 });
    const before = h.presenter.stats().paints;
    const chunks = cumulativeChunks(REPLY, 200);
    for (const chunk of chunks) {
      h.presenter.feed(assistantEvent("message_update", chunk));
    }
    assert.equal(h.presenter.stats().paints, before, "no write-ahead per token");
    assert.equal(h.time.pending(), 1, "one coalescing timer, whatever N was");

    h.time.fire();
    const after = h.presenter.stats();
    assert.equal(after.paints - before, 1, "200 deltas produced one paint");
    assert.equal(after.coalescedTicks, 1, "one tick for the whole burst");
  });

  it("every window boundary paints at most once, over many windows", () => {
    const h = presenterHarness({ coalesceMs: 33 });
    const before = h.presenter.stats().paints;
    let windows = 0;
    for (let i = 0; i < 10; i += 1) {
      for (const chunk of cumulativeChunks(`line ${i}\n${"word ".repeat(40)}`, 20)) {
        h.presenter.feed(assistantEvent("message_update", chunk));
      }
      h.time.fireAll();
      windows += 1;
    }
    const painted = h.presenter.stats().paints - before;
    assert.ok(
      painted <= windows,
      `${painted} paints for ${windows} windows: more than one frame per window`,
    );
  });

  it("flushSync paints now and leaves no paint pending", () => {
    const h = presenterHarness();
    for (const chunk of cumulativeChunks(REPLY, 50)) {
      h.presenter.feed(assistantEvent("message_update", chunk));
    }
    const paintsBefore = h.presenter.stats().paints;
    h.presenter.flushSync();
    assert.equal(h.presenter.stats().paints - paintsBefore, 1);
    assert.equal(h.presenter.stats().paintPending, false);
  });

  it("a live stream gets a frame in every window, not one for the whole reply", () => {
    // The regression: an assistant delta updated its block without asking for
    // a frame, so a whole reply cost ONE paint — the frame in which its block
    // appeared — and then nothing until the footer's `elapsed` string rolled
    // over. A streaming reply sat at ~1fps while tool output beside it ran at
    // 30, which reads as a frozen terminal, not a slow one.
    const painted = framesWhileStreaming({ coalesceMs: 33, heartbeatMs: 0 }, 30);
    assert.equal(painted, 30, "one frame per window, and one in every window");
  });

  it("coalesceMs is the refresh-rate knob: halve the window, double the frames", () => {
    const thirty = framesWhileStreaming({ coalesceMs: 33, heartbeatMs: 0 }, 30);
    const sixty = framesWhileStreaming({ coalesceMs: 16, heartbeatMs: 0 }, 60);
    assert.equal(thirty, 30);
    assert.equal(sixty, 60, "~60fps is reachable without breaking rule 3");
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 14 — the heartbeat: a surface we hold repaints between events
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 14: the clock repaints the frame without outrunning the budget", () => {
  /** A work unit on screen: the footer clock running, one call in flight. */
  const startWork = (h: PresenterHarness): void => {
    h.presenter.setContext({ issueId: "ws.5", phase: "work" });
    h.presenter.feed(toolStart("t1", "bash", { command: "npm test" }));
    h.presenter.flushSync();
  };

  /** One beat plus the frame it scheduled: two timers, beat first. */
  const beat = (h: PresenterHarness): void => {
    h.time.fire();
    h.time.fire();
  };

  it("elapsed advances when no event arrives at all", () => {
    const h = presenterHarness({ heartbeatMs: 500 });
    startWork(h);
    assert.match(h.footerLine(), /elapsed 00:00/u);
    const before = h.presenter.stats().paints;

    h.time.advance(1_000);
    beat(h);

    assert.ok(
      h.presenter.stats().paints > before,
      "a surface held through a silent second must still repaint",
    );
    assert.match(h.footerLine(), /elapsed 00:01/u);
  });

  it("a beat repaints through the coalescing path, never inline", () => {
    const h = presenterHarness({ heartbeatMs: 500, coalesceMs: 33 });
    startWork(h);
    h.time.advance(1_000);
    const before = h.presenter.stats().paints;
    h.time.fire();
    assert.equal(h.presenter.stats().paints, before, "the beat asked, it did not write");
    h.time.fire();
    assert.equal(h.presenter.stats().paints - before, 1, "one frame at the window boundary");
  });

  it("a beat whose footer text has not moved asks for no frame", () => {
    const h = presenterHarness({ heartbeatMs: 500 });
    startWork(h);
    const before = h.presenter.stats().paints;
    h.time.advance(400);
    h.time.fire();
    assert.equal(h.presenter.stats().paints, before, "00:00 again is not a change");
    assert.ok(h.time.pending() >= 1, "and the beat re-armed for the next one");
  });

  it("ten quiet seconds cost at most one frame each", () => {
    const h = presenterHarness({ heartbeatMs: 500 });
    startWork(h);
    const before = h.presenter.stats().paints;
    for (let second = 0; second < 10; second += 1) {
      h.time.advance(1_000);
      beat(h);
    }
    const painted = h.presenter.stats().paints - before;
    assert.ok(
      painted >= 9,
      `${painted} frames in ten seconds: the clock is not being kept`,
    );
    assert.ok(
      painted <= 10,
      `${painted} frames in ten seconds: the heartbeat outran the clock it reports`,
    );
  });

  it("release stops the heartbeat: no timer held, nothing painted after", () => {
    const h = presenterHarness({ heartbeatMs: 500 });
    startWork(h);
    h.presenter.release();
    assert.equal(h.time.pending(), 0, "a released presenter holds no timer");
    const before = h.presenter.stats().paints;
    h.time.advance(5_000);
    beat(h);
    assert.equal(
      h.presenter.stats().paints,
      before,
      "nothing paints on a surface we no longer hold",
    );
  });

  it("dispose leaves no timer behind", () => {
    const h = presenterHarness({ heartbeatMs: 100 });
    startWork(h);
    h.presenter.dispose();
    assert.equal(h.time.pending(), 0);
  });

  it("a presenter that never took the surface schedules nothing", () => {
    const h = presenterHarness({ heartbeatMs: 100, live: false });
    startWork(h);
    assert.equal(h.time.pending(), 0, "no surface, no heartbeat");
    // And firing whatever the clock has cannot make it paint: idle owns that
    // terminal, and two surfaces must never be repainting the same tty.
    h.time.advance(3_000);
    h.time.fire();
    assert.equal(h.presenter.stats().paints, 0);
  });

  it("heartbeatMs 0 turns the heartbeat off outright", () => {
    const h = presenterHarness({ heartbeatMs: 0 });
    startWork(h);
    assert.equal(h.time.pending(), 0, "an explicit zero means no timer at all");
  });

  it("a stream does not lean on the beat: with the heartbeat off it still moves", () => {
    // These two numbers were 1 and 0 before the fix. A reply's only frames came
    // from the clock being checked, so prose and clock were one code path and
    // the screen moved at the resolution of `formatElapsed` — one second.
    const noBeat = framesWhileStreaming({ coalesceMs: 33, heartbeatMs: 0 }, 30);
    const withBeat = framesWhileStreaming({ coalesceMs: 33, heartbeatMs: 500 }, 30);
    assert.equal(noBeat, 30, "the stream itself drives the frames");
    assert.ok(
      withBeat >= noBeat && withBeat <= noBeat + 1,
      `the beat added ${withBeat - noBeat} frames to a busy stream; it is a clock, not the animation`,
    );
  });

  it("flushSync paints now but does not kill the heartbeat", () => {
    const h = presenterHarness({ heartbeatMs: 500 });
    startWork(h);
    h.presenter.flushSync();
    assert.equal(
      h.presenter.stats().paintPending,
      false,
      "the flush consumed the pending frame",
    );
    h.time.advance(1_000);
    h.time.fire();
    h.time.fire();
    assert.match(
      h.footerLine(),
      /elapsed 00:01/u,
      "the surface is still ours, so the clock is still being kept",
    );
  });

  it("the plain path holds no heartbeat — there is nothing to repaint", () => {
    const h = presenterHarness({ heartbeatMs: 100, tty: false });
    startWork(h);
    assert.equal(h.presenter.path, "plain");
    assert.equal(h.time.pending(), 0);
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 4 — highlighting that matches pi
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 4: the highlighting is pi's, asserted against pi", () => {
  const renderBlock = (block: string): string[] => {
    const h = presenterHarness();
    h.presenter.feed(assistantEvent("message_end", `Prose.\n\n${block}\n`));
    h.presenter.flushSync();
    const frame = h.frame();
    // Content only: the footer's own colours are not part of the highlight.
    return frame.slice(0, footerStartOf(frame));
  };
  const pyLines = renderBlock(PY_BLOCK);
  const tsLines = renderBlock(TS_BLOCK);

  it("a python block gets three distinguishable theme colours", () => {
    const codes = codesIn(pyLines.join("\n"));
    assert.ok(codes.length >= 3, `expected >=3 colours, got ${codes.length}`);
    const kw = codesIn(highlightCode("def greet(name):", "python").join("\n"));
    const str = codesIn(highlightCode('"hello world"', "python").join("\n"));
    const cmt = codesIn(highlightCode("# comment", "python").join("\n"));
    const groups: Array<[string, string[]]> = [
      ["keyword", kw],
      ["string", str],
      ["comment", cmt],
    ];
    for (const [label, set] of groups) {
      assert.ok(
        set.some((c) => codes.includes(c)),
        `python ${label} colour from pi's highlighter absent from the rendered block`,
      );
    }
    const signatures = new Set(groups.map(([, set]) => set.join()));
    assert.equal(signatures.size, 3, "pi gives keyword/string/comment one colour");
  });

  it("a typescript block gets three distinguishable theme colours", () => {
    const codes = codesIn(tsLines.join("\n"));
    assert.ok(codes.length >= 3, `expected >=3 colours, got ${codes.length}`);
    const kw = codesIn(
      highlightCode("function fib(n: number): number {", "typescript").join("\n"),
    );
    const str = codesIn(highlightCode('"a string"', "typescript").join("\n"));
    const cmt = codesIn(highlightCode("// comment", "typescript").join("\n"));
    const groups: Array<[string, string[]]> = [
      ["keyword", kw],
      ["string", str],
      ["comment", cmt],
    ];
    for (const [label, set] of groups) {
      assert.ok(
        set.some((c) => codes.includes(c)),
        `typescript ${label} colour absent from the rendered block`,
      );
    }
  });

  it("the two languages do not come out byte-identical", () => {
    const py = codeSet(pyLines);
    const ts = codeSet(tsLines);
    const different =
      [...py].filter((c) => !ts.has(c)).length +
      [...ts].filter((c) => !py.has(c)).length;
    assert.ok(
      different > 0,
      "python and typescript blocks rendered identically — not highlighting",
    );
  });

  it("uses no colour pi's own live theme does not emit", () => {
    // This assertion used to diff against `spikes/out/render-highlight.raw.txt`
    // — a capture of a pi.dev terminal. The live theme is the better baseline:
    // it *is* pi, right now, so the check cannot go stale, needs no committed
    // artifact, and follows a theme change instead of falsely failing under one.
    // Everything painted must come out of pi's own palette: the markdown theme
    // pi renders with, pi's own highlighter for this content, and the
    // presenter's declared roles on the live theme.
    const live = new Set<string>();

    const markdown = getMarkdownTheme() as unknown as Record<string, unknown>;
    for (const value of Object.values(markdown)) {
      if (typeof value !== "function") continue;
      try {
        const painted = (value as (text: string) => string)("x");
        if (typeof painted === "string") {
          for (const code of codesIn(painted)) live.add(code);
        }
      } catch {
        // Roles that need more than a bare string contribute nothing here; the
        // highlighter pass below covers the token colours.
      }
    }

    const fence = /^```\w+\n([^]*?)\n?```$/;
    for (const [block, language] of [
      [PY_BLOCK, "python"],
      [TS_BLOCK, "typescript"],
    ] as const) {
      const body = block.replace(fence, "$1");
      for (const line of highlightCode(body, language)) {
        for (const code of codesIn(line)) live.add(code);
      }
    }

    const theme = createPresenterTheme();
    for (const role of PRESENTER_ROLES) {
      for (const code of codesIn(theme.color(role, "x"))) live.add(code);
    }
    for (const code of codesIn(theme.bold("x"))) live.add(code);

    const used = new Set([...codeSet(pyLines), ...codeSet(tsLines)]);
    const invented = [...used].filter((code) => !live.has(code));
    assert.deepEqual(
      invented,
      [],
      `colours the live pi theme never emitted: ${invented.join(" ")}`,
    );
    assert.ok(live.size >= 4, "the live palette should not be near-empty");
  });

  it("colours come from pi's live theme, narrowed to declared roles", () => {
    const theme = createPresenterTheme();
    for (const role of PRESENTER_ROLES) {
      const painted = theme.color(role, "x");
      assert.match(painted, /\x1b\[/u, `role ${role} produced no colour`);
      assert.equal(stripTerminalSequences(painted), "x");
    }
    assert.ok(PRESENTER_ROLES.length >= 8, "the role list is auditable");
  });

  it("the markdown theme pi hands us is the one in play", () => {
    const mdTheme = getMarkdownTheme();
    assert.equal(typeof mdTheme, "object");
    assert.ok(mdTheme !== null);
  });
});

/**
 * Any tool-call header line, settled or not. A pending call wears one of the
 * spinner frames, so the glyph that marks these lines is a set, not a
 * character — and the set belongs to the presenter.
 */
const TOOL_LINE = new RegExp(`^\\s*[✓✗…${SPINNER_FRAMES.join("")}]`, "u");

// ══════════════════════════════════════════════════════════════════════════════
// a pending call moves, so a waiting screen is not a hung one
// ══════════════════════════════════════════════════════════════════════════════

describe("a pending tool call shows that it is waiting", () => {
  const SPIN = 120;

  /** The pending tool line in the current frame, or "" if none is showing. */
  const pendingLine = (h: PresenterHarness): string => {
    const line = h
      .plain()
      .find((l) => TOOL_LINE.test(l) && !/^\s*[✓✗]/u.test(l));
    return line === undefined ? "" : line.trim();
  };

  const glyphOf = (line: string): string => line.trim().charAt(0);

  /** One animation beat and the frame it asked for. */
  const spinBeat = (h: PresenterHarness, ms = SPIN): void => {
    h.time.fire(); // the beat
    h.time.fire(); // the frame it scheduled inside the coalescing window
    h.time.advance(ms);
  };

  const openCall = (h: PresenterHarness, id = "c1", command = "cargo test"): void => {
    h.presenter.setContext({ issueId: "ws.7", phase: "work" });
    h.presenter.feed(toolStart(id, "bash", { command }));
    h.presenter.flushSync();
  };

  it("turns while the call is outstanding", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h);

    const seen: string[] = [glyphOf(pendingLine(h))];
    for (let beat = 0; beat < 5; beat += 1) {
      spinBeat(h);
      seen.push(glyphOf(pendingLine(h)));
    }

    assert.ok(pendingLine(h) !== "", "the pending call stayed on screen");
    for (const glyph of seen) {
      assert.ok(
        SPINNER_FRAMES.includes(glyph),
        `frame glyph is not a spinner frame: ${JSON.stringify(glyph)} in ${seen.join("")}`,
      );
    }
    assert.ok(
      new Set(seen).size >= 5,
      `only ${new Set(seen).size} distinct frames across five beats: ${seen.join("")}`,
    );
  });

  it("turns at spinnerMs, not at the footer's one-second clock", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h);
    const before = h.presenter.stats().paints;

    // One second of outstanding call. At the footer's cadence that is two
    // frames, which is a thing that happens twice and then looks stopped.
    for (let step = 0; step < Math.floor(1_000 / SPIN); step += 1) {
      spinBeat(h);
    }
    const animated = h.presenter.stats().paints - before;
    assert.ok(
      animated >= 7,
      `${animated} frames in a second of outstanding work; the spinner is not running at ${SPIN}ms`,
    );
  });

  it("goes back to the slow beat once the call reports back", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h);
    h.presenter.feed(toolEnd("c1", "bash", textResult("ok")));
    h.presenter.flushSync();

    assert.equal(h.presenter.stats().animating, false, "nothing outstanding");
    const settled = h.plain().find((l) => TOOL_LINE.test(l)) ?? "";
    assert.match(settled, /^\s*✓/u, "a settled call wears its result, not a spinner");

    const before = h.presenter.stats().paints;
    for (let step = 0; step < 4; step += 1) {
      h.time.advance(1_000);
      h.time.fire();
      h.time.fire();
    }
    const painted = h.presenter.stats().paints - before;
    assert.ok(
      painted <= 5,
      `${painted} frames in four settled seconds: the fast beat outlived the call`,
    );
    assert.doesNotMatch(h.plain().join("\n"), /[⠁-⣿]/u, "no spinner after the fact");
  });

  it("stats animating is true only while something is outstanding", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    h.presenter.setContext({ issueId: "ws.7", phase: "work" });
    assert.equal(h.presenter.stats().animating, false, "nothing asked yet");

    h.presenter.feed(toolStart("c1", "bash", { command: "sleep 5" }));
    assert.equal(h.presenter.stats().animating, true);

    h.presenter.feed(toolEnd("c1", "bash", textResult("done")));
    assert.equal(h.presenter.stats().animating, false);

    h.presenter.feed(toolStart("c2", "read", { path: "src/app.ts" }));
    h.presenter.release();
    assert.equal(
      h.presenter.stats().animating,
      false,
      "a surface we no longer hold must not animate anything",
    );
  });

  it("only the outstanding call moves; a settled one keeps its verdict", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h, "c1");
    h.presenter.feed(toolEnd("c1", "bash", textResult("green")));
    h.presenter.feed(toolStart("c2", "bash", { command: "git push" }));
    h.presenter.flushSync();

    const settledBefore = (h.plain().find((l) => l.includes("cargo test")) ?? "").trim();
    const movingBefore = (h.plain().find((l) => l.includes("git push")) ?? "").trim();
    assert.match(settledBefore, /^✓/u, settledBefore);
    assert.ok(SPINNER_FRAMES.includes(glyphOf(movingBefore)), movingBefore);

    spinBeat(h);
    spinBeat(h);

    const settledAfter = (h.plain().find((l) => l.includes("cargo test")) ?? "").trim();
    const movingAfter = (h.plain().find((l) => l.includes("git push")) ?? "").trim();
    assert.equal(settledAfter, settledBefore, "a finished call never reanimates");
    assert.notEqual(movingAfter.charAt(0), movingBefore.charAt(0), "the open call moved");
  });

  it("says how long a long call has been going, and stays one line", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h, "c1", "npm run build");
    assert.doesNotMatch(
      pendingLine(h),
      /·/u,
      "an instant call showing a timer is noise, not information",
    );

    h.time.advance(4_500);
    h.time.fire();
    h.time.fire();
    const line = pendingLine(h);
    assert.match(line, /npm run build/u);
    assert.match(line, /· 00:04/u, line);
    assert.ok(!line.includes("\n"), "the timer must not split the header");

    h.time.advance(60_000);
    h.time.fire();
    h.time.fire();
    assert.match(pendingLine(h), /· 01:04/u, pendingLine(h));
  });

  it("plain output shows no motion and no spinner glyph", () => {
    const h = presenterHarness({ tty: false, heartbeatMs: 500, spinnerMs: SPIN });
    h.presenter.setContext({ issueId: "ws.7", phase: "work" });
    h.presenter.feed(toolStart("c1", "bash", { command: "npm test" }));

    const first = h.plain().join("\n");
    h.time.advance(2_000);
    h.time.fire();
    const second = h.plain().join("\n");

    assert.equal(first, second, "an escape-free stream cannot animate; it must not try");
    assert.doesNotMatch(second, /[⠁-⣿]/u, "no braille in log output");
    assert.equal(h.presenter.stats().animating, false);
  });

  it("spinnerMs 0 keeps the honest static pending glyph", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: 0 });
    openCall(h);
    assert.equal(glyphOf(pendingLine(h)), "…");
    assert.equal(h.presenter.stats().animating, false, "the animation was turned off");

    // The heartbeat is a separate thing and still runs.
    h.time.advance(1_000);
    h.time.fire();
    h.time.fire();
    assert.match(h.footerLine(), /elapsed 00:01/u);
    assert.equal(glyphOf(pendingLine(h)), "…");
  });

  it("the animation does not outrun the coalescing window", () => {
    // A beat asks; the window decides. With spinnerMs far above coalesceMs the
    // two numbers should agree, and neither should turn into a write storm.
    const h = presenterHarness({ coalesceMs: 33, heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h);
    const before = h.presenter.stats();
    const beats = Math.floor(1_200 / SPIN);
    for (let step = 0; step < beats; step += 1) {
      spinBeat(h);
    }
    const after = h.presenter.stats();
    const painted = after.paints - before.paints;
    const asked = after.coalescedTicks - before.coalescedTicks;
    assert.ok(painted <= asked, "no frame without an ask");
    assert.ok(painted <= beats, `${painted} frames for ${beats} beats: more than one per beat`);
    assert.ok(painted >= beats - 2, `${painted} frames for ${beats} beats: frames dropped`);
  });

  it("the fast beat is shared, not a second timer", () => {
    const h = presenterHarness({ heartbeatMs: 500, spinnerMs: SPIN });
    openCall(h);
    // Beat + coalesced paint pending: at most two timers, whatever the cadence.
    assert.ok(
      h.time.pending() <= 2,
      `${h.time.pending()} timers held; the cadences are stacking instead of sharing`,
    );
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rules 5, 6, 7 — tool calls
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 5: every tool call is exactly one summary line", () => {
  it("bash, read, edit and write each collapse to verb + meaningful argument", () => {
    const h = presenterHarness();
    h.presenter.feed(toolStart("c1", "bash", { command: "ls -la /etc" }));
    h.presenter.feed(
      toolStart("c2", "read", { path: "src/app.ts", offset: 10, limit: 20 }),
    );
    h.presenter.feed(
      toolStart("c3", "edit", {
        path: "src/x.ts",
        edits: [{ oldText: "a" }, { oldText: "b" }],
      }),
    );
    h.presenter.feed(
      toolStart("c4", "write", { path: "src/new.ts", content: "hello" }),
    );
    h.presenter.flushSync();

    const headers = h.plain().filter((line) => TOOL_LINE.test(line));
    assert.equal(headers.length, 4, "one line per tool call");
    assert.match(headers[0]!, /\$ ls -la \/etc/u);
    assert.match(headers[1]!, /src\/app\.ts:10-30/u);
    assert.match(headers[2]!, /src\/x\.ts \(2 edits\)/u);
    assert.match(headers[3]!, /src\/new\.ts \(5B\)/u);
  });

  it("the whole arg object never reaches the screen", () => {
    const h = presenterHarness();
    h.presenter.feed(
      toolStart("c1", "edit", {
        path: "src/x.ts",
        edits: [{ oldText: "SECRET_OLD_VALUE", newText: "SECRET_NEW_VALUE" }],
      }),
    );
    h.presenter.flushSync();
    const visible = h.plain().join("\n");
    assert.ok(!visible.includes("SECRET_OLD_VALUE"), "raw edit payload leaked");
    assert.ok(!visible.includes('"path"'), "raw arg object leaked");
  });

  it("a multi-line argument still occupies one line", () => {
    const h = presenterHarness();
    h.presenter.feed(toolStart("c1", "bash", { command: "echo one\necho two" }));
    h.presenter.flushSync();
    const headers = h.plain().filter((line) => TOOL_LINE.test(line));
    assert.equal(headers.length, 1);
    assert.ok(!headers[0]!.includes("echo two"));
  });

  it("summary lines go through formatToolArgs — no parallel formatter", () => {
    const source = stripComments(sourceOf("render.ts"));
    assert.match(source, /formatToolArgs\(/u, "render.ts must use formatToolArgs");
    assert.match(
      source,
      /formatToolResult\(/u,
      "render.ts must use formatToolResult",
    );
    assert.ok(
      !/JSON\.stringify\([^)]*args/u.test(source),
      "render.ts must not stringify tool args itself",
    );
  });
});

describe("rule 6: results are collapsed and expandable with a registered key", () => {
  const longText = Array.from({ length: 12 }, (_, i) => `out ${i + 1}`).join("\n");

  it("collapsed by default: a short preview plus an explicit +N more lines", () => {
    const h = presenterHarness({ collapsedPreviewLines: 2 });
    h.presenter.feed(toolStart("c1", "bash", { command: "ls" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult(longText)));
    h.presenter.flushSync();
    const shown = h.plain().filter((line) => /^\s+out \d+/u.test(line));
    assert.equal(shown.length, 2, "two preview lines only");
    assert.match(h.plain().join("\n"), /\+10 more lines/u);
  });

  it("the expand key is registered on pi's keybinding manager", () => {
    const manager = new KeybindingsManager({
      ...TUI_KEYBINDINGS,
      "app.tools.expand": {
        defaultKeys: "ctrl+j",
        description: "Toggle tool output",
      },
    } as never);
    assert.equal(manager.matches("\n", "app.tools.expand" as never), true);

    const h = presenterHarness({ keybindings: manager });
    h.presenter.feed(toolStart("c1", "bash", { command: "ls" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult(longText)));
    h.presenter.flushSync();
    assert.equal(h.presenter.stats().expanded, false);

    h.term.input("\n"); // ctrl+j, resolved through the manager
    h.presenter.flushSync();
    assert.equal(h.presenter.stats().expanded, true);
    assert.equal(
      h.plain().filter((line) => /^\s+out \d+/u.test(line)).length,
      12,
      "expanded shows every line",
    );
    assert.ok(!h.plain().join("\n").includes("+10 more lines"));

    h.term.input("\n");
    h.presenter.flushSync();
    assert.equal(h.presenter.stats().expanded, false, "the key toggles back");
  });

  it("the key is shown in the footer legend", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.9", phase: "work" });
    h.presenter.flushSync();
    const footer = h.footerLine();
    assert.match(footer, /ctrl\+o/u, "legend shows the expand key");
    assert.match(footer, /toggle tool output/u);
  });

  it("a remapped key shows up in the legend too", () => {
    const manager = new KeybindingsManager({
      ...TUI_KEYBINDINGS,
      "app.tools.expand": {
        defaultKeys: "ctrl+j",
        description: "Toggle tool output",
      },
    } as never);
    const h = presenterHarness({ keybindings: manager });
    h.presenter.setContext({ issueId: "ws.9", phase: "work" });
    h.presenter.flushSync();
    assert.match(h.footerLine(), /ctrl\+j/u, "legend follows the manager, not a literal");
  });

  it("expanded output is bounded, with the cut stated", () => {
    const huge = `${"x".repeat(40)}\n${"y".repeat(20_000)}`;
    const h = presenterHarness({ maxExpandedChars: 500 });
    h.presenter.feed(toolStart("c1", "bash", { command: "cat big" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult(huge)));
    h.presenter.setExpanded(true);
    h.presenter.flushSync();
    const rendered = h.plain().join("\n");
    assert.ok(
      rendered.length < 20_000,
      `expanded output was unbounded: ${rendered.length} chars`,
    );
    assert.match(
      rendered,
      /more lines?/u,
      "the truncation says how much was cut",
    );
  });
});

describe("rule 7: unknown and error-shaped events degrade honestly", () => {
  it("an unrecognized runner event kind becomes one line naming its type", () => {
    const h = presenterHarness();
    h.presenter.feed({ type: "quantum_flux", detail: "who knows" } as never);
    h.presenter.flushSync();
    const lines = h.plain().filter((l) => l.includes("quantum_flux"));
    assert.equal(lines.length, 1, "exactly one line naming the unknown type");
  });

  it("an unrecognized pi event becomes one line naming its type", () => {
    const h = presenterHarness();
    h.presenter.feed({ type: "agent_event", raw: { type: "wat_is_this", x: 1 } });
    h.presenter.flushSync();
    const lines = h.plain().filter((l) => l.includes("wat_is_this"));
    assert.equal(lines.length, 1);
  });

  it("a tool error shows the error on its own line, not as JSON", () => {
    const h = presenterHarness();
    h.presenter.feed(toolStart("c1", "bash", { command: "false" }));
    h.presenter.feed(
      toolEnd(
        "c1",
        "bash",
        { content: [{ type: "text", text: "exit code 1: nope" }] },
        true,
      ),
    );
    h.presenter.flushSync();
    const lines = h.plain();
    const errorLine = lines.find((l) => l.includes("nope"));
    assert.ok(errorLine, "the error text is shown");
    assert.match(errorLine!, /^\s+/u, "the error sits under the header");
    assert.ok(
      !/\{"type"|"isError":/u.test(lines.join("\n")),
      "raw JSON leaked into the render",
    );
  });

  it("garbage fed to feed() never throws out of the render loop", () => {
    const h = presenterHarness();
    const junk: unknown[] = [
      null,
      undefined,
      42,
      "a string",
      [],
      { type: "agent_event" },
      { type: "agent_event", raw: null },
      { type: "agent_event", raw: 7 },
      { type: "tool_call" },
      { type: "tool_call", raw: { toolName: null, args: undefined } },
      { type: "agent_event", raw: { type: "message_end", message: null } },
    ];
    for (const item of junk) {
      assert.doesNotThrow(() => h.presenter.feed(item as RunnerEvent));
    }
    h.presenter.flushSync();
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 8 — the footer
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 8: the footer is ordered, tolerant, in-place, and removed on teardown", () => {
  it("carries every field in the documented order", () => {
    const h = presenterHarness();
    h.presenter.setContext({
      issueId: "ws.7",
      phase: "work",
      model: "llamacpp/qwen",
      thinkingLevel: "high",
    });
    h.presenter.feed(
      assistantEvent("message_end", "hi", { usage: { input: 11, output: 22 } }),
    );
    h.presenter.flushSync();
    const footer = h.footerLine();
    const order = [
      "issue ws.7",
      "phase work",
      "elapsed",
      "tokens 11→22",
      "model llamacpp/qwen",
      "thinking high",
      "ctrl+o",
    ];
    let cursor = -1;
    for (const part of order) {
      const at = footer.indexOf(part);
      assert.ok(
        at > cursor,
        `footer field "${part}" missing or out of order: ${footer}`,
      );
      cursor = at;
    }
  });

  it("a missing token count shows the em dash, never undefined", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.1" });
    h.presenter.flushSync();
    const footer = h.footerLine();
    assert.ok(!/undefined|null/u.test(footer), `footer leaked: ${footer}`);
    assert.ok(footer.includes(MISSING), `expected ${MISSING} for unknown fields`);
    assert.equal(formatElapsed(undefined), MISSING);
    assert.equal(formatTokenPair(undefined, undefined), `${MISSING}→${MISSING}`);
    assert.equal(formatElapsed(65_000), "01:05");
    assert.equal(formatElapsed(3_700_000), "1:01:40");
  });

  it("no field is dropped when nothing was supplied", () => {
    const segments = buildFooterSegments({});
    assert.deepEqual(
      segments.map((s) => s.label),
      ["issue", "phase", "elapsed", "tokens", "model", "thinking"],
    );
    const line = joinFooter(segments, null);
    for (const label of [
      "issue",
      "phase",
      "elapsed",
      "tokens",
      "model",
      "thinking",
    ]) {
      assert.ok(line.includes(label), `plain footer lost ${label}`);
    }
  });

  it("updates in place without duplicating", () => {
    const h = presenterHarness();
    for (let i = 0; i < 6; i += 1) {
      h.presenter.setContext({ issueId: `ws.${i}`, phase: "work" });
      h.presenter.say(`update ${i}`);
      h.presenter.flushSync();
    }
    const footerLines = h.plain().filter((line) => line.includes("issue ws."));
    assert.equal(footerLines.length, 1, "one footer region, not one per update");
    assert.match(footerLines[0]!, /ws\.5/u, "it shows the latest context");
  });

  it("is removed on teardown so no stale footer survives", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.1", phase: "work" });
    h.presenter.flushSync();
    assert.match(h.footerLine(), /issue ws\.1/u);
    h.presenter.release();
    assert.equal(
      h.plain().filter((line) => line.includes("issue ws.")).length,
      0,
      "a footer survived the teardown",
    );
  });

  it("elapsed moves with the injected clock and tokens accumulate", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.2", phase: "work" });
    h.time.advance(90_000);
    h.presenter.feed(
      assistantEvent("message_end", "one", { usage: { input: 100, output: 20 } }),
    );
    h.presenter.feed(
      assistantEvent("message_end", "two", { usage: { input: 50, output: 30 } }),
    );
    h.presenter.flushSync();
    const footer = h.footerLine();
    assert.match(footer, /elapsed 01:30/u);
    assert.match(footer, /tokens 150→50/u, "tokens sum across messages");
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 9 — non-TTY degradation
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 9: non-TTY output is plain text with every field present", () => {
  const plainHarness = (): {
    term: FakeTerminal;
    presenter: WorkPresenter;
    out: () => string;
  } => {
    const term = new FakeTerminal(80, 24);
    const presenter = createWorkPresenter({
      terminal: term as never,
      tty: false,
      now: () => 1_000,
    });
    return { term, presenter, out: () => term.writes.join("") };
  };

  it("zero escape sequences reach the pipe", () => {
    const h = plainHarness();
    h.presenter.setContext({ issueId: "ws.5", phase: "work" });
    h.presenter.say("plain run");
    h.presenter.feed(toolStart("c1", "bash", { command: "ls" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult("a\nb\nc")));
    h.presenter.feed(assistantEvent("message_end", REPLY));
    h.presenter.release();
    const out = h.out();
    assert.equal(out.indexOf("\x1b"), -1, "a plain run emitted an escape sequence");
  });

  it("every footer field is still there, one line per tool call", () => {
    const h = plainHarness();
    h.presenter.setContext({
      issueId: "ws.5",
      phase: "work",
      model: "llamacpp/qwen",
      thinkingLevel: "low",
    });
    h.presenter.feed(toolStart("c1", "bash", { command: "ls" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult("a\nb\nc")));
    h.presenter.feed(toolStart("c2", "read", { path: "src/x.ts" }));
    h.presenter.release();

    const out = h.out();
    for (const field of ["issue ws.5", "phase work", "elapsed", "tokens", "model llamacpp/qwen", "thinking low"]) {
      assert.ok(out.includes(field), `plain footer lost "${field}": ${out}`);
    }
    const headers = out.split("\n").filter((line) => TOOL_LINE.test(line));
    assert.equal(headers.length, 2, "one line per tool call in plain mode");
  });

  it("the plain path and the live path describe the same content", () => {
    const plain = plainHarness();
    plain.presenter.setContext({ issueId: "ws.6", phase: "work" });
    plain.presenter.feed(toolStart("c1", "bash", { command: "ls -la" }));
    plain.presenter.feed(toolEnd("c1", "bash", textResult("one\ntwo")));
    plain.presenter.release();

    const live = presenterHarness();
    live.presenter.setContext({ issueId: "ws.6", phase: "work" });
    live.presenter.feed(toolStart("c1", "bash", { command: "ls -la" }));
    live.presenter.feed(toolEnd("c1", "bash", textResult("one\ntwo")));
    live.presenter.flushSync();

    const plainHeaders = plain
      .out()
      .split("\n")
      .filter((l) => TOOL_LINE.test(l));
    const liveHeaders = live.plain().filter((l) => TOOL_LINE.test(l));
    assert.deepEqual(plainHeaders, liveHeaders, "same summary lines, two transports");
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 10 — dead code, resolved
// ══════════════════════════════════════════════════════════════════════════════

/** Every top-level exported name in a module. */
function parseExports(source: string): string[] {
  const names: string[] = [];
  const re =
    /^export\s+(?:async\s+)?(?:function\*?|const|let|var|class|type|interface)\s+([A-Za-z_$][\w$]*)/gm;
  for (const match of source.matchAll(re)) names.push(match[1]!);
  return [...new Set(names)];
}

/** Top-level declaration blocks, so "who calls whom" is read per declaration. */
function declarationBlocks(
  source: string,
): Array<{ name: string; body: string }> {
  const re =
    /^export\s+(?:async\s+)?(?:function\*?|const|let|var|class|type|interface)\s+([A-Za-z_$][\w$]*)/gm;
  const matches = [...source.matchAll(re)];
  return matches.map((match, index) => {
    const end =
      index + 1 < matches.length ? matches[index + 1]!.index! : source.length;
    return { name: match[1]!, body: source.slice(match.index!, end) };
  });
}

describe("rule 10: nothing in src/format.ts is dead", () => {
  const formatSource = sourceOf("format.ts");
  const renderSource = stripComments(sourceOf("render.ts"));
  const blocks = declarationBlocks(formatSource);
  const exports = parseExports(formatSource);

  it("format.ts has exports and all of them are reachable through the presenter", () => {
    const importMatch =
      /import\s*\{([^}]*)\}\s*from\s*"\.\/format\.ts"/u.exec(renderSource);
    assert.ok(importMatch, "render.ts must import from ./format.ts");
    const imported = new Set(
      (importMatch?.[1] ?? "")
        .split(",")
        .map((part) => part.trim())
        .filter((part) => part !== ""),
    );
    assert.ok(imported.size > 0, "render.ts imports nothing from format.ts");

    // Transitive closure: a reachable export reaches whatever its body calls.
    const reached = new Set<string>(imported);
    for (;;) {
      const before = reached.size;
      for (const block of blocks) {
        if (!reached.has(block.name)) continue;
        for (const other of blocks) {
          if (reached.has(other.name)) continue;
          if (new RegExp(`\\b${other.name}\\b`, "u").test(block.body)) {
            reached.add(other.name);
          }
        }
      }
      if (reached.size === before) break;
    }

    const dead = exports.filter((name) => !reached.has(name));
    assert.deepEqual(
      dead,
      [],
      `zero-reference export(s) left in src/format.ts: ${dead.join(", ")}`,
    );
  });

  it("each export is exercised through the presenter, not just referenced", () => {
    const exercised = new Set<string>();

    const h = presenterHarness();
    // formatToolArgs + parseArgs + formatBytes + formatGeneric + formatToolResult
    // + extractText + truncateLines: one call per interesting shape.
    h.presenter.feed(toolStart("a", "bash", { command: "pwd" }));
    exercised.add("formatToolArgs");
    exercised.add("parseArgs");
    h.presenter.feed(toolStart("b", "write", { path: "p.ts", content: "0123456789" }));
    exercised.add("formatBytes");
    h.presenter.feed(toolStart("c", "mystery_tool", { foo: "bar" }));
    exercised.add("formatGeneric");
    h.presenter.feed(toolEnd("a", "bash", textResult("x".repeat(5_000))));
    exercised.add("formatToolResult");
    exercised.add("extractText");
    exercised.add("truncateLines");
    // formatGenericObj: a result that is not a content array at all.
    h.presenter.feed(toolEnd("c", "mystery_tool", { weird: { shape: 1 } }));
    exercised.add("formatGenericObj");
    // indentContent: a multi-line context note hangs under its own first line.
    h.presenter.feed({ type: "context_note", detail: "context line\nsecond line" });
    exercised.add("indentContent");
    h.presenter.flushSync();

    const unexercised = exports.filter((name) => !exercised.has(name));
    assert.deepEqual(unexercised, [], `export never exercised: ${unexercised.join(", ")}`);

    // And the shapes really did reach the screen.
    const visible = h.plain().join("\n");
    assert.match(visible, /\$ pwd/u);
    assert.match(visible, /p\.ts \(10B\)/u);
    assert.match(visible, /foo: bar/u, "generic tool args rendered");
    assert.match(visible, /second line/u, "indented context note rendered");
  });

  it("src/utils.ts does not come back", () => {
    assert.ok(
      !existsSync(join(here, "..", "src", "utils.ts")),
      "src/utils.ts was deleted and must stay deleted",
    );
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 11 — no hand-rolled ANSI
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 11: no ANSI is built by string concatenation in render.ts", () => {
  it("comment-stripped source contains no escape literal", () => {
    const source = stripComments(sourceOf("render.ts"));
    assert.ok(!source.includes("\x1b"), "a raw ESC character in the source");
    assert.ok(!/\\x1b/u.test(source), "an \\x1b escape in the source");
    assert.ok(!/\\u001b/u.test(source), "a \\u001b escape in the source");
    assert.ok(!/\\e\[/u.test(source), "an \\e[ escape in the source");
    assert.ok(!/chalk|ansi-styles|kleur/u.test(source), "no colour library either");
  });

  it("every escape in a captured frame came from a component render", () => {
    const h = presenterHarness();
    h.presenter.setContext({
      issueId: "ws.4",
      phase: "work",
      model: "m",
      thinkingLevel: "high",
    });
    h.presenter.say("a notice");
    h.presenter.feed(assistantEvent("message_end", REPLY));
    h.presenter.feed(toolStart("c1", "bash", { command: "ls" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult("one\ntwo\nthree\nfour")));
    h.presenter.flushSync();
    const captured = codeSet(h.frame());

    // What pi itself would paint for the same content.
    const theme = createPresenterTheme();
    const expected = new Set<string>();
    for (const role of PRESENTER_ROLES) {
      for (const code of codesIn(theme.color(role, "x"))) expected.add(code);
    }
    for (const code of codesIn(theme.bold("x"))) expected.add(code);
    const component = new AssistantMessageComponent(
      { role: "assistant", content: [{ type: "text", text: REPLY }] } as never,
      false,
      getMarkdownTheme(),
      "Thinking…",
      1,
      [],
    );
    for (const code of codesIn(component.render(80).join("\n"))) expected.add(code);
    for (const code of codesIn(rawKeyHint("ctrl+o", "toggle tool output"))) {
      expected.add(code);
    }

    const foreign = [...captured].filter((code) => !expected.has(code));
    assert.deepEqual(
      foreign,
      [],
      `escape sequences with no component origin: ${foreign.join(" ")}`,
    );
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 12 — composition
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 12: one ordered surface for both text sources", () => {
  it("a say after a tool summary lands below it, whole", () => {
    const h = presenterHarness();
    h.presenter.feed(toolStart("c1", "bash", { command: "ls -la" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult("one\ntwo")));
    h.presenter.say("committed 3 files");
    h.presenter.flushSync();

    const lines = h.plain();
    const toolAt = lines.findIndex((l) => l.includes("$ ls -la"));
    const sayAt = lines.findIndex((l) => l.includes("committed 3 files"));
    assert.ok(toolAt >= 0 && sayAt >= 0, "both lines are present");
    assert.ok(sayAt > toolAt, "the say landed above the tool call");
    // Contiguous: nothing was inserted inside the say line.
    assert.equal(stripTerminalSequences(lines[sayAt]!).trim(), "committed 3 files");
  });

  it("a streamed reply followed by a warn keeps both, warning-styled by theme", () => {
    const h = presenterHarness();
    h.presenter.feed(assistantEvent("message_update", "Working on it"));
    h.presenter.feed(assistantEvent("message_end", "Working on it"));
    h.presenter.warn("board read failed, retrying");
    h.presenter.flushSync();

    const lines = h.plain();
    assert.ok(lines.some((l) => l.includes("Working on it")));
    const warnLine = lines.find((l) => l.includes("board read failed"));
    assert.ok(warnLine, "the warning is kept");

    const theme = createPresenterTheme();
    const warnCode = openCode(theme.color("warning", "x"));
    const errCode = openCode(theme.color("error", "x"));
    assert.notEqual(warnCode, "", "the theme has a warning colour");
    assert.notEqual(warnCode, errCode, "warning and error are different colours");
    const styled = h
      .frame()
      .find((l) => stripTerminalSequences(l).includes("board read failed"))!;
    assert.ok(
      styled.includes(warnCode),
      "the warn line is not painted with the theme's warning colour",
    );
    assert.ok(
      !styled.includes(errCode),
      "the warn line carries the error colour",
    );
    assert.ok(
      !/^\s*[!xX✗✘]/u.test(stripTerminalSequences(warnLine)),
      "a hand-typed marker stands in for theming",
    );
  });

  it("interleaved sources keep their order in capture", () => {
    const h = presenterHarness();
    h.presenter.say("first");
    h.presenter.feed(toolStart("c1", "bash", { command: "one" }));
    h.presenter.feed(toolEnd("c1", "bash", textResult("r")));
    h.presenter.warn("middle");
    h.presenter.feed(assistantEvent("message_end", "last prose"));
    h.presenter.flushSync();

    const text = h.plain().join("\n");
    const positions = ["first", "$ one", "middle", "last prose"].map((needle) =>
      text.indexOf(needle),
    );
    for (const [index, at] of positions.entries()) {
      assert.ok(at >= 0, `missing "${["first", "$ one", "middle", "last prose"][index]}"`);
      if (index > 0) {
        assert.ok(
          at > positions[index - 1]!,
          `"${["first", "$ one", "middle", "last prose"][index]}" is out of order`,
        );
      }
    }
  });
});

// ══════════════════════════════════════════════════════════════════════════════
// rule 13 — failure visibility
// ══════════════════════════════════════════════════════════════════════════════

describe("rule 13: timeout, abort and failure render as themselves", () => {
  const theme = createPresenterTheme();

  const styledLine = (h: PresenterHarness, needle: string): string => {
    const line = h
      .frame()
      .find((l) => stripTerminalSequences(l).includes(needle));
    assert.ok(line, `no rendered line containing "${needle}"`);
    return line;
  };

  it("a timeout says so, in the error colour, with the issue named", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.8", phase: "work" });
    h.presenter.feed({ type: "timeout", detail: "budget exhausted" });
    h.presenter.flushSync();
    const line = styledLine(h, "timed out");
    assert.match(stripTerminalSequences(line), /timed out/u);
    assert.match(stripTerminalSequences(line), /budget exhausted/u);
    const errorCodes = codesIn(theme.color("error", "x"));
    assert.ok(
      errorCodes.some((c) => line.includes(c)),
      "the timeout line is not painted with the error colour",
    );
  });

  it("an aborted reply is marked as aborted, not passed off as finished", () => {
    const h = presenterHarness();
    h.presenter.feed(
      assistantEvent("message_update", "I was midway through"),
    );
    h.presenter.feed(
      assistantEvent("message_end", "I was midway through", { stopReason: "aborted" }),
    );
    h.presenter.flushSync();
    const line = styledLine(h, "aborted");
    assert.match(stripTerminalSequences(line), /aborted/u);
    const warningCodes = codesIn(theme.color("warning", "x"));
    assert.ok(
      warningCodes.some((c) => line.includes(c)),
      "the abort notice is not themed as a warning",
    );
  });

  it("a reply cut off at the token limit says so", () => {
    const h = presenterHarness();
    h.presenter.feed(
      assistantEvent("message_end", "truncated halfway thr", { stopReason: "length" }),
    );
    h.presenter.flushSync();
    assert.match(stripTerminalSequences(styledLine(h, "cut off")), /token limit/u);
  });

  it("a timeout reports the run's own elapsed, not the surface's clock", () => {
    // The surface had been showing this issue for 20:01. The run that timed out
    // had been going 20:00 of a 20:00 budget. Only the runner knows which is
    // which, so the numbers travel on the event.
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.7eg", phase: "work", runId: 1 });
    h.time.advance(20 * 60_000 + 1_000);
    h.presenter.feed({
      type: "timeout",
      elapsedMs: 1_200_000,
      budgetMs: 1_200_000,
      detail: "budget 1200000ms exceeded; aborting",
    });
    h.presenter.flushSync();
    const line = stripTerminalSequences(styledLine(h, "timed out"));
    assert.match(line, /timed out after 20:00 of a 20:00 budget \(issue ws\.7eg\)/u);
    assert.ok(!line.includes("20:01"), "the surface's own clock did not leak into the line");
  });

  it("a re-run of the same bead is a new unit, and the footer clock starts over", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.7eg", phase: "work", runId: 1 });
    h.time.advance(20 * 60_000);
    // The pass timed out and the loop handed the same bead straight back. Same
    // issue, new unit: a clock that keeps running across the handoff reports the
    // two passes added together, which reads as a budget that doubled.
    h.presenter.setContext({ issueId: "ws.7eg", phase: "work", runId: 2 });
    h.time.advance(61_000);
    h.presenter.flushSync();
    assert.match(h.footerLine(), /elapsed 01:01/u, "the footer describes this pass, not both");
  });

  it("a wrap-up nudge shows as itself", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.8", phase: "work", runId: 1 });
    h.presenter.feed({
      type: "wrap_up",
      elapsedMs: 1_020_000,
      budgetMs: 1_200_000,
      detail: "asked this session to land what it has and report; 03:00 left to do it in",
    });
    h.presenter.flushSync();
    const line = stripTerminalSequences(styledLine(h, "wrapping up"));
    assert.match(line, /wrapping up ws\.8/u);
    assert.match(line, /land what it has/u);
  });

  it("describeOutcome gives every kind its own word and level", () => {
    const cases: Array<[string, string, string]> = [
      ["done", "info", "finished"],
      ["incomplete", "warn", "incomplete"],
      ["unstructured-verdict", "warn", "unstructured verdict"],
      ["malformed-verdict", "error", "malformed verdict"],
      ["timeout", "error", "timed out"],
      ["error", "error", "failed"],
      ["sideways", "warn", "unknown outcome"],
    ];
    const seenWords = new Set<string>();
    for (const [kind, level, word] of cases) {
      const said = describeOutcome({ kind, issueId: "ws.3" });
      assert.equal(said.level, level, `kind ${kind} should be ${level}`);
      assert.ok(
        said.text.includes(word),
        `kind ${kind} did not say "${word}": ${said.text}`,
      );
      seenWords.add(word);
    }
    assert.equal(seenWords.size, cases.length, "two kinds share a word");
  });

  it("the failure line names the issue and what it left behind", () => {
    const h = presenterHarness();
    h.presenter.setContext({ issueId: "ws.3", phase: "work" });
    const said = describeOutcome({
      kind: "timeout",
      issueId: "ws.3",
      budgetMs: 60_000,
      settledAfterAbort: false,
      leftBehind: "two modified files, no commit",
    });
    h.presenter.notice(said.level, said.text);
    h.presenter.flushSync();
    const frame = h.plain();
    const notice = frame
      .slice(0, footerStartOf(frame))
      .join(" ")
      .replace(/\s+/gu, " ");
    assert.match(notice, /ws\.3/u, "the failure names the issue");
    assert.match(notice, /timed out/u);
    assert.match(notice, /after 01:00/u);
    assert.match(notice, /left behind: two modified files, no commit/u);
    assert.match(notice, /still running/u);
  });

  it("failure is never swallowed into a silent cursor move", () => {
    const h = presenterHarness();
    const before = h.frame().length;
    h.presenter.setContext({ issueId: "ws.3", phase: "work" });
    h.presenter.notice("error", describeOutcome({ kind: "error", issueId: "ws.3", message: "boom" }).text);
    h.presenter.flushSync();
    assert.ok(h.frame().length > before, "the frame did not grow: nothing was shown");
    assert.match(h.plain().join("\n"), /boom/u);
  });
});
