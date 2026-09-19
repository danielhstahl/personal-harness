/**
 * Tests for `src/agent.ts` — the fresh-session runner and per-issue context
 * builder.
 *
 * Nothing here talks to a model. The session is faked, which is the point: every
 * claim under test is about the harness, and a harness property that only shows up
 * when a real model happens to cooperate is not a property worth having. The fake
 * runs the *real* tool definitions, so the verdict contract is exercised for real.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  asAgentPort,
  asSessionPort,
  AgentError,
  buildSplitPrompt,
  buildWorkContext,
  classifyRunEvidence,
  collectAssistantText,
  createAgentRunner,
  describeFailure,
  extractLastFencedJson,
  isDone,
  lastAssistantText,
  resolveModelForRun,
  toWorkEvent,
  validateSplitPayload,
  validateVerdict,
  WORK_CONTEXT_SECTIONS,
  WORK_OUTCOME_KINDS,
} from "../src/agent.js";
import type {
  AgentSessionLike,
  RunnerSessionKind,
  SessionFactory,
  SessionSpec,
  WorkContextSectionName,
  WorkOutcome,
} from "../src/agent.js";
import { BdError } from "../src/beads.js";
import { RepoError } from "../src/repo.js";
import type { RepoReaderLike } from "../src/agent.js";
import type { BdClient, Issue } from "../src/beads.js";
import { failureKeyFor, handoffKeyFor } from "../src/orchestrator.js";
import type { OrchestratorPorts } from "../src/orchestrator.js";
import type { RepoSnapshot } from "../src/repo.js";

const SRC_DIR = join(dirname(fileURLToPath(import.meta.url)), "..", "src");

// ── fake session ────────────────────────────────────────────────────────────

interface FakeToolCall {
  name: string;
  params: unknown;
}

/** The real element type of `session.messages`, borrowed so we do not re-declare it. */
type ChatMessage = AgentSessionLike["messages"][number];

interface FakeScript {
  /** Tool calls the fake model makes, in order, before it stops. */
  tools?: FakeToolCall[];
  /** Prose the fake assistant produces after the tool calls. */
  reply?: string;
  /** Never finish until {@link FakeSession.release} is called. */
  neverSettle?: boolean;
  /** Reject the prompt outright. */
  rejectWith?: string;
  /** Make `dispose()` blow up. */
  disposeThrows?: boolean;
  /** Milliseconds of fake elapsed time to burn during the prompt. */
  advanceOnPrompt?: number;
  /** Called from `abort()`. */
  onAbort?: () => void;
}

interface LooseTool {
  name: string;
  execute: (
    toolCallId: string,
    params: unknown,
    signal: AbortSignal,
    update: (partial: unknown) => void,
  ) => Promise<unknown>;
}

let sessionCounter = 0;

class FakeSession implements AgentSessionLike {
  readonly sessionId: string;
  readonly sessionFile: string | undefined;
  messages: ChatMessage[] = [];

  readonly spec: SessionSpec;
  readonly promptTexts: string[] = [];
  /** What `messages` held at the moment each prompt started — the amnesia probe. */
  readonly historyAtPrompt: ChatMessage[][] = [];
  readonly toolErrors: { name: string; message: string }[] = [];
  abortCalls = 0;
  disposeCalls = 0;
  subscribeCalls = 0;
  unsubscribeCalls = 0;

  private readonly script: FakeScript;
  private gate: Promise<void> | null = null;
  private releaseGate: (() => void) | null = null;

  constructor(spec: SessionSpec, script: FakeScript) {
    sessionCounter += 1;
    this.spec = spec;
    this.script = script;
    this.sessionId = `fake-session-${sessionCounter}`;
    // In-memory sessions have no file. A run that wrote one would be persisting
    // context across iterations, which is exactly the breach under test.
    this.sessionFile = undefined;
  }

  subscribe(listener: unknown): () => void {
    this.subscribeCalls += 1;
    return () => {
      this.unsubscribeCalls += 1;
      void listener;
    };
  }

  async prompt(text: string): Promise<void> {
    this.historyAtPrompt.push([...this.messages]);
    this.promptTexts.push(text);
    this.messages.push({ role: "user", content: text } as ChatMessage);

    if (this.script.advanceOnPrompt !== undefined) {
      clock.advance(this.script.advanceOnPrompt);
    }

    for (const [index, call] of (this.script.tools ?? []).entries()) {
      const tool = (this.spec.customTools as unknown as LooseTool[]).find(
        (candidate) => candidate.name === call.name,
      );
      if (tool === undefined) {
        this.toolErrors.push({ name: call.name, message: "tool not registered" });
        continue;
      }
      try {
        await tool.execute(`tc-${index}`, call.params, new AbortController().signal, () => {});
        this.messages.push({
          role: "toolResult",
          content: [{ type: "text", text: `${call.name} accepted` }],
        } as unknown as ChatMessage);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        this.toolErrors.push({ name: call.name, message });
        this.messages.push({ role: "toolResult", content: [{ type: "text", text: message }] } as unknown as ChatMessage);
      }
    }

    if (this.script.reply !== undefined) {
      this.messages.push({
        role: "assistant",
        content: [{ type: "text", text: this.script.reply }],
      } as unknown as ChatMessage);
    } else {
      this.messages.push({ role: "assistant", content: [{ type: "text", text: "" }] } as unknown as ChatMessage);
    }

    if (this.script.rejectWith !== undefined) {
      throw new Error(this.script.rejectWith);
    }
    if (this.script.neverSettle === true) {
      this.gate = new Promise<void>((resolve) => {
        this.releaseGate = resolve;
      });
      await this.gate;
    }
  }

  async abort(): Promise<void> {
    this.abortCalls += 1;
    this.script.onAbort?.();
  }

  dispose(): void {
    this.disposeCalls += 1;
    if (this.script.disposeThrows === true) throw new Error("dispose exploded");
  }

  /** Let a `neverSettle` prompt finish. */
  release(): void {
    this.releaseGate?.();
  }

  get lastToolError(): string {
    return this.toolErrors.at(-1)?.message ?? "";
  }
}

// ── fixtures ────────────────────────────────────────────────────────────────

/** `bd show` shape: the other issue is inlined, there is no `depends_on_id`. */
function issueFixture(overrides: Partial<Issue> = {}): Issue {
  return {
    id: "loop-42",
    title: "Wire the fresh-session runner",
    status: "in_progress",
    priority: 1,
    issue_type: "task",
    description: "Build src/agent.ts so each iteration starts from nothing.",
    acceptance_criteria: "A second run in the same process must begin blank.",
    dependencies: [
      {
        id: "loop-7",
        title: "Toolchain is ready",
        dependency_type: "blocks",
      },
    ] as unknown as Issue["dependencies"],
    ...overrides,
  };
}

function repoFixture(): RepoSnapshot {
  // No cast here on purpose: this fixture must track the real shape or the build
  // tells us when it drifts.
  return {
    root: "/workspace/pi-beads/loop",
    branch: "main",
    detached: false,
    head: "0123456789abcdef0123456789abcdef01234567",
    recentCommits: [
      {
        sha: "bb953eb7",
        subject: "workspace-5yn.4: orchestrator — the pure reducer",
        author: "Dev",
        age: "2 hours ago",
      },
    ],
    dirtyFiles: [{ path: "src/agent.ts", status: " M" }],
    truncated: false,
    hasUncommittedChanges: true,
    emptyRepo: false,
  };
}

interface BeadsStub {
  readonly client: BdClient;
  readonly calls: string[];
}

/**
 * A bd client that records what was asked and blows up on any attempt to write
 * status or close — the runner must decide nothing about issue lifecycle.
 */
function beadsStub(issue: Issue | null, memories: Record<string, string> = {}): BeadsStub {
  const calls: string[] = [];
  const notImplemented = (name: string): never => {
    calls.push(name);
    throw new Error(`agent.ts must never call ${name} — the loop owns lifecycle`);
  };

  const client: BdClient = {
    async listReady() {
      return notImplemented("listReady");
    },
    async listInProgress() {
      return notImplemented("listInProgress");
    },
    async getIssue(id) {
      calls.push(`getIssue:${id}`);
      return issue;
    },
    async createIssue() {
      return notImplemented("createIssue");
    },
    async addDep() {
      return notImplemented("addDep");
    },
    async appendNote() {
      return notImplemented("appendNote");
    },
    async setStatus() {
      return notImplemented("setStatus");
    },
    async closeIssue() {
      return notImplemented("closeIssue");
    },
    async remember() {
      return notImplemented("remember");
    },
    async recall(key) {
      calls.push(`recall:${key}`);
      if (key in memories) return memories[key] as string;
      throw new BdError({ kind: "not-found", message: `no memory under ${key}` });
    },
  };
  return { client, calls };
}

/** Injectable clock: elapsed time is asserted, not slept. */
const clock = {
  value: 1_000_000,
  now(): number {
    return clock.value;
  },
  advance(ms: number): void {
    clock.value += ms;
  },
};

interface RunnerHarness {
  readonly runner: ReturnType<typeof createAgentRunner>;
  readonly sessions: FakeSession[];
  readonly specs: SessionSpec[];
  readonly calls: string[];
}

function runnerHarness(
  scripts: FakeScript[],
  options: {
    issue?: Issue | null;
    memories?: Record<string, string>;
    timeoutMs?: number;
    abortGraceMs?: number;
    noRepo?: boolean;
    repoError?: boolean;
  } = {},
): RunnerHarness {
  const sessions: FakeSession[] = [];
  const specs: SessionSpec[] = [];
  const issue = options.issue === undefined ? issueFixture() : options.issue;
  const stub = beadsStub(issue, options.memories ?? {});

  const repo: RepoReaderLike =
    options.repoError === true
      ? {
          async describe() {
            throw new RepoError({ kind: "exit", message: "git exited 128" });
          },
        }
      : options.noRepo === true
        ? { async describe() { return null; } }
        : { async describe() { return repoFixture(); } };

  const factory: SessionFactory = async (spec) => {
    specs.push(spec);
    const script = scripts[sessions.length] ?? {};
    const session = new FakeSession(spec, script);
    sessions.push(session);
    return session;
  };

  const runner = createAgentRunner({
    beads: stub.client,
    sessionFactory: factory,
    repo,
    timeoutMs: options.timeoutMs ?? 60_000,
    abortGraceMs: options.abortGraceMs ?? 50,
    now: clock.now,
  });

  return { runner, sessions, specs, calls: stub.calls };
}

const DONE_PARAMS = {
  done: true,
  summary: "Built the runner; disposed every session.",
  changed_files: ["src/agent.ts"],
  next_steps: ["wire it into the interpreter"],
};

// ── fresh session per run ───────────────────────────────────────────────────

test("a run creates exactly one session and disposes it exactly once", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_done", params: DONE_PARAMS }] }]);

  const outcome = await h.runner.run("loop-42");

  assert.equal(outcome.kind, "done");
  assert.equal(h.sessions.length, 1);
  assert.equal(h.sessions[0]?.disposeCalls, 1);
  assert.deepEqual(h.runner.stats(), { created: 1, disposed: 1, live: 0 });
});

test("the second iteration in the same process starts with an empty history", async () => {
  const firstSentinel = "SENTINEL-FIRST-ITERATION-zulu";
  const secondSentinel = "SENTINEL-SECOND-ITERATION-kilo";

  const sessions: FakeSession[] = [];
  const scripts: FakeScript[] = [
    { reply: `first run said ${firstSentinel}`, tools: [{ name: "report_done", params: DONE_PARAMS }] },
    { tools: [{ name: "report_done", params: DONE_PARAMS }] },
  ];

  const runner = createAgentRunner({
    beads: {
      async listReady() {
        throw new Error("unused");
      },
      async listInProgress() {
        throw new Error("unused");
      },
      async getIssue(id) {
        return id === "first-1"
          ? issueFixture({ id: "first-1", title: `first ${firstSentinel}` })
          : issueFixture({ id: "second-1", title: `second ${secondSentinel}` });
      },
      async createIssue() {
        throw new Error("must not be called");
      },
      async addDep() {
        throw new Error("must not be called");
      },
      async appendNote() {
        throw new Error("must not be called");
      },
      async setStatus() {
        throw new Error("must not be called");
      },
      async closeIssue() {
        throw new Error("must not be called");
      },
      async remember() {
        throw new Error("must not be called");
      },
      async recall() {
        return null;
      },
    },
    sessionFactory: async (spec) => {
      const session = new FakeSession(spec, scripts[sessions.length] ?? {});
      sessions.push(session);
      return session;
    },
    repo: {
      async describe() {
        return repoFixture();
      },
    },
    now: clock.now,
  });

  const first = await runner.run("first-1");
  const second = await runner.run("second-1");

  assert.equal(first.kind, "done");
  assert.equal(second.kind, "done");
  assert.equal(sessions.length, 2);

  const [sessionOne, sessionTwo] = sessions as [FakeSession, FakeSession];

  // Distinct objects: not reset, not reused.
  assert.notEqual(sessionOne, sessionTwo);
  assert.notEqual(sessionOne.sessionId, sessionTwo.sessionId);

  // The amnesia proof: before the second prompt was sent, the second session held
  // nothing. Not "compacted to a summary" — nothing.
  assert.deepEqual(
    sessionTwo.historyAtPrompt[0],
    [],
    "iteration 2 must start with empty history",
  );

  const secondTranscript = JSON.stringify(sessionTwo.messages);
  assert.equal(
    secondTranscript.includes(firstSentinel),
    false,
    "iteration 1 must not be reachable from iteration 2",
  );
  assert.equal(sessionOne.promptTexts[0]?.includes(firstSentinel), true);
  assert.equal(sessionTwo.promptTexts[0]?.includes(secondSentinel), true);
  assert.equal(sessionTwo.disposeCalls, 1);
});

test("sessions are in-memory: no session file is ever produced", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_done", params: DONE_PARAMS }] }]);
  const outcome = await h.runner.run("loop-42");
  assert.equal(outcome.sessionFile, null, "a persisted session file means shared context");
});

test("the default factory is in-memory and never compacts (source guard)", () => {
  const source = stripComments(readFileSync(join(SRC_DIR, "agent.ts"), "utf8"));

  assert.match(source, /SessionManager\.inMemory/u, "each run must build an in-memory session");
  assert.doesNotMatch(source, /SessionManager\.(create|open|resume)/u, "no persistent session reuse");
  assert.doesNotMatch(source, /\.compact\s*\(/u, "compact() retains summarised turns: never a reset");
  assert.doesNotMatch(source, /navigateTree\s*\(/u, "navigateTree() is a rewind, not an amnesia");
  assert.doesNotMatch(source, /--mode\s+rpc|RpcClient/u, "ADR-001: no RPC subprocess");
  assert.doesNotMatch(source, /child_process|execFile|spawnSync|\bspawn\s*\(/u, "no process spawning here");
});

// ── disposal on every path ──────────────────────────────────────────────────

test("disposal happens exactly once on every exit path", async () => {
  const paths: { name: string; script: FakeScript }[] = [
    { name: "success", script: { tools: [{ name: "report_done", params: DONE_PARAMS }] } },
    { name: "prose-only", script: { reply: "I am done, trust me" } },
    { name: "refused-verdict", script: { tools: [{ name: "report_done", params: { done: true } }] } },
    { name: "prompt-error", script: { rejectWith: "model exploded" } },
    { name: "timeout", script: { neverSettle: true } },
  ];

  for (const path of paths) {
    const h = runnerHarness([path.script], { timeoutMs: 30, abortGraceMs: 20 });
    const outcome = await h.runner.run("loop-42");
    assert.ok(outcome, `${path.name}: no outcome`);
    // Tidy up any never-settling prompt now that the outcome is in.
    h.sessions[0]?.release();

    assert.equal(
      h.sessions[0]?.disposeCalls,
      1,
      `${path.name}: expected exactly one dispose, got ${h.sessions[0]?.disposeCalls}`,
    );
    assert.equal(h.runner.stats().live, 0, `${path.name}: a session leaked`);
  }
});

test("a throwing dispose is reported, not swallowed, and never retried", async () => {
  const h = runnerHarness([
    { tools: [{ name: "report_done", params: DONE_PARAMS }], disposeThrows: true },
  ]);

  const outcome = await h.runner.run("loop-42");

  assert.equal(outcome.kind, "error");
  assert.ok(outcome.kind === "error");
  assert.match(outcome.message, /dispose exploded/u);
  assert.equal(h.sessions[0]?.disposeCalls, 1, "a failed dispose must not be retried");

  // And the runner does not try to dispose it a second time later.
  const extra = await h.runner.dispose();
  assert.equal(extra, 0);
  assert.equal(h.sessions[0]?.disposeCalls, 1);
});

test("runner.dispose() disposes a session that is still live mid-run", async () => {
  const h = runnerHarness([{ neverSettle: true }], { timeoutMs: 100_000, abortGraceMs: 10 });

  const running = h.runner.run("loop-42");
  await waitFor(() => h.runner.stats().live === 1);

  await asSessionPort(h.runner).dispose();
  assert.equal(h.sessions[0]?.disposeCalls, 1);

  // The port returns void by contract; the count comes from the runner itself.
  assert.equal(await h.runner.dispose(), 0, "nothing is left to dispose twice");

  // Let the prompt finish; the run's own finally must not dispose twice.
  h.sessions[0]?.release();
  const outcome = await running;
  assert.equal(
    outcome.kind,
    "unstructured-verdict",
    "a run whose session was disposed externally still reports an explicit outcome",
  );
  assert.equal(h.sessions[0]?.disposeCalls, 1, "external dispose must not be doubled by the run");
  assert.equal(h.runner.stats().live, 0);
});

test("asAgentPort and asSessionPort satisfy OrchestratorPorts by type, not by hope", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_done", params: DONE_PARAMS }] }]);
  const agent: OrchestratorPorts["agent"] = asAgentPort(h.runner);
  const session: OrchestratorPorts["session"] = asSessionPort(h.runner);

  const result = await agent.run("loop-42");
  assert.equal((result as WorkOutcome).kind, "done");
  assert.equal(typeof agent.split, "function");

  await session.dispose();
  assert.equal(h.runner.stats().live, 0);
});

// ── the verdict contract ────────────────────────────────────────────────────

test("report_done with a valid verdict is the only path to done", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_done", params: DONE_PARAMS }] }]);
  const outcome = await h.runner.run("loop-42");

  assert.ok(isDone(outcome));
  assert.equal(outcome.kind, "done");
  assert.equal(outcome.verdictSource, "report_done_tool");
  assert.equal(outcome.verdict.summary, DONE_PARAMS.summary);
  assert.deepEqual(outcome.verdict.changedFiles, ["src/agent.ts"]);
  assert.deepEqual(outcome.verdict.nextSteps, ["wire it into the interpreter"]);

  const event = toWorkEvent(outcome);
  assert.equal(event.type, "work_succeeded");
});

test("prose that merely says done is typed unstructured-verdict, never done", async () => {
  const h = runnerHarness([{ reply: "All finished! I'm done. Shipped it." }]);
  const outcome = await h.runner.run("loop-42");

  assert.equal(outcome.kind, "unstructured-verdict");
  assert.equal(isDone(outcome), false);
  assert.equal(outcome.verdictSource, "none");
  assert.match(outcome.rawText, /All finished/u);

  const event = toWorkEvent(outcome);
  assert.equal(event.type, "work_failed");
  assert.ok(event.type === "work_failed");
  assert.match(event.reason, /no structured verdict/u);
});

test("a malformed fenced block is distinct from a legitimate done:false", async () => {
  const broken = [
    "Here is my verdict:",
    "```json",
    "{ 'done': true, 'summary': }",
    "```",
  ].join("\n");
  const h = runnerHarness([{ reply: broken }]);
  const outcome = await h.runner.run("loop-42");

  assert.equal(outcome.kind, "malformed-verdict");
  assert.equal(outcome.verdictSource, "fenced_json");
  assert.equal(outcome.problems.length > 0, true);
  assert.match(outcome.rawBlock, /'summary'/u, "the raw block is preserved for debugging");

  // And a *valid* `done: false` is incomplete, a different kind entirely.
  const honest = [
    "```json",
    '{"done": false, "reason": "blocked on the API", "summary": "half done", "changed_files": []}',
    "```",
  ].join("\n");
  const h2 = runnerHarness([{ reply: honest }]);
  const outcome2 = await h2.runner.run("loop-42");
  assert.equal(outcome2.kind, "incomplete");
  assert.equal(outcome2.verdict.done, false);
  assert.equal(outcome2.verdict.reason, "blocked on the API");
});

test("report_done with done:false and no reason is refused, then reported as malformed", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_done", params: { done: false, summary: "meh" } }] }]);
  const outcome = await h.runner.run("loop-42");

  assert.match(h.sessions[0]?.lastToolError ?? "", /`reason` is required/u);
  assert.equal(outcome.kind, "malformed-verdict");
  assert.equal(outcome.verdictSource, "report_done_tool");
  assert.ok(outcome.problems.some((problem) => problem.includes("`reason` is required")));
});

test("a refused report_done then a valid one still resolves to done", async () => {
  const h = runnerHarness([
    {
      tools: [
        { name: "report_done", params: { done: true } },
        { name: "report_done", params: DONE_PARAMS },
      ],
    },
  ]);
  const outcome = await h.runner.run("loop-42");
  assert.equal(outcome.kind, "done");
  assert.equal(outcome.verdictToolCalls, 1);
  assert.equal(h.sessions[0]?.toolErrors.length, 1);
});

test("an unknown tool call is recorded, never silently ignored", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_finished", params: DONE_PARAMS }] }]);
  const outcome = await h.runner.run("loop-42");
  assert.equal(h.sessions[0]?.toolErrors[0]?.message, "tool not registered");
  assert.equal(outcome.kind, "unstructured-verdict", "a typo'd tool call is not a verdict");
  assert.equal(outcome.verdictToolCalls, 0);
});

test("elapsed time comes from the injected clock, not the wall", async () => {
  const h = runnerHarness([
    { tools: [{ name: "report_done", params: DONE_PARAMS }], advanceOnPrompt: 4_500 },
  ]);
  const outcome = await h.runner.run("loop-42");
  assert.equal(outcome.elapsedMs, 4_500);
});

// ── timeout ─────────────────────────────────────────────────────────────────

test("a timeout aborts the session and is its own outcome kind", async () => {
  const h = runnerHarness([{ neverSettle: true }], { timeoutMs: 30, abortGraceMs: 20 });

  const outcome = await h.runner.run("loop-42");
  h.sessions[0]?.release();

  assert.equal(outcome.kind, "timeout");
  assert.equal(outcome.budgetMs, 30, "the outcome names the budget that ran out");
  assert.equal(h.sessions[0]?.abortCalls, 1, "abort() must be called on timeout");
  assert.equal(outcome.settledAfterAbort, false);
  assert.match(describeFailure(outcome), /timed out after 30ms/u);
  assert.equal(toWorkEvent(outcome).type, "work_failed");
});

test("a session that settles after abort says so", async () => {
  let release: (() => void) | null = null;
  const h = runnerHarness([
    {
      neverSettle: true,
      onAbort: () => {
        // The agent winds down promptly once told.
        setTimeout(() => release?.(), 0);
      },
    },
  ], { timeoutMs: 25, abortGraceMs: 200 });

  const running = h.runner.run("loop-42");
  await waitFor(() => h.sessions.length === 1);
  release = () => h.sessions[0]?.release();
  const outcome = await running;

  assert.equal(outcome.kind, "timeout");
  assert.equal(outcome.settledAfterAbort, true);
  assert.equal(h.sessions[0]?.disposeCalls, 1);
});

// ── context builder ─────────────────────────────────────────────────────────

test("the work context has the sections in a fixed order", () => {
  const context = buildWorkContext({
    issue: issueFixture(),
    priorFailure: "ran out of budget",
    handoff: "left the tests half-written",
    repo: repoFixture(),
    memories: [{ key: "loop-design", text: "context never carries" }],
  });

  const names = context.sections.map((section) => section.name);
  const expected: WorkContextSectionName[] = [
    "task",
    "description",
    "acceptance_criteria",
    "dependencies",
    "prior_attempt",
    "repo",
    "memories",
    "instructions",
  ];
  assert.deepEqual(names, expected);
  assert.deepEqual(new Set(names).size, names.length, "no section name repeats");
  assert.deepEqual(WORK_CONTEXT_SECTIONS, [...expected]);
});

test("the work prompt carries the issue, its deps, notes and the repo state", () => {
  const prompt = buildWorkContext({
    issue: issueFixture(),
    repo: repoFixture(),
  }).prompt;

  assert.match(prompt, /loop-42/u);
  assert.match(prompt, /Wire the fresh-session runner/u);
  assert.match(prompt, /Build src\/agent\.ts/u);
  assert.match(prompt, /A second run in the same process/u);
  assert.match(prompt, /loop-7/u, "dependency ids must be shown");
  assert.match(prompt, /Toolchain is ready/u);
  assert.match(prompt, /Branch: main/u);
  assert.match(prompt, /bb953eb/u);
  assert.match(prompt, /report_done/u, "the reporting contract is part of the context");
});

test("a prior failure and a handoff both land in one prior_attempt section", () => {
  const context = buildWorkContext({
    issue: issueFixture(),
    priorFailure: "timed out after 30ms",
    handoff: "changed src/agent.ts, tests pending",
  });
  const prior = context.sections.filter((section) => section.name === "prior_attempt");
  assert.equal(prior.length, 1);
  assert.match(prior[0]?.body ?? "", /timed out after 30ms/u);
  assert.match(prior[0]?.body ?? "", /changed src\/agent\.ts/u);
});

test("the prompt never renders the word undefined or null", () => {
  const sparse = buildWorkContext({ issue: issueFixture({ description: undefined, acceptance_criteria: undefined, dependencies: undefined }) });
  assert.equal(sparse.prompt.includes("undefined"), false);
  assert.equal(sparse.prompt.includes("null"), false);

  const full = buildWorkContext({
    issue: issueFixture(),
    priorFailure: "",
    handoff: "   ",
    repo: null,
    memories: [{ key: "empty", text: "  " }],
  });
  assert.equal(full.prompt.includes("undefined"), false);
  assert.equal(full.prompt.includes("null"), false);
  assert.equal(
    full.sections.some((section) => section.name === "memories"),
    false,
    "a blank memory must not create a section",
  );
});

test("buildWorkContext is pure: deterministic, non-mutating, transcript-blind", () => {
  const input = {
    issue: issueFixture(),
    priorFailure: "boom",
    repo: repoFixture(),
    memories: [{ key: "k", text: "v" }],
  };
  const before = JSON.stringify(input);

  const first = buildWorkContext(input);
  const second = buildWorkContext(input);

  assert.equal(first.prompt, second.prompt);
  assert.equal(JSON.stringify(input), before, "the input must not be mutated");

  // A caller smuggled an extra field that looks like a transcript. The builder
  // reads only what its input type names, so the output cannot differ.
  const smuggled = {
    ...input,
    transcript: "the previous conversation, verbatim",
    scratchNotes: "half-formed idea from the last run",
  };
  const third = buildWorkContext(smuggled);
  assert.equal(third.prompt, first.prompt, "anything outside the input shape cannot reach the prompt");
  assert.equal(third.prompt.includes("verbatim"), false);
});

test("oversized fields are truncated rather than crowding out the contract", () => {
  const huge = issueFixture({ description: "x".repeat(5_000) });
  const context = buildWorkContext({ issue: huge, maxFieldChars: 1_000 });
  assert.match(context.prompt, /truncated 4000 chars/u);
  assert.ok(context.prompt.length < 5_000);
});

test("dependencies are read through the normalised shape, not depends_on_id", () => {
  // `bd show` inlines the other issue: reading `depends_on_id` here would yield
  // undefined and the loop would report "not blocked".
  const shown = issueFixture({
    dependencies: [
      { id: "loop-3", title: "Adapter", dependency_type: "blocks" },
    ] as unknown as Issue["dependencies"],
  });
  const prompt = buildWorkContext({ issue: shown }).prompt;
  assert.match(prompt, /loop-3/u);
  assert.match(prompt, /blocks/u);
});

test("the split prompt quotes the human request verbatim", () => {
  const prompt = buildSplitPrompt("  make the loop stop forgetting  ");
  assert.match(prompt, /make the loop stop forgetting/u);
  assert.match(prompt, /report_split/u);
});

test("a missing git tree degrades the context instead of killing the run", async () => {
  const h = runnerHarness(
    [{ tools: [{ name: "report_done", params: DONE_PARAMS }] }],
    { noRepo: true },
  );
  const outcome = await h.runner.run("loop-42");
  assert.equal(outcome.kind, "done");
  assert.equal(outcome.contextNotes.length, 1);
  assert.match(outcome.contextNotes[0] ?? '', /not inside a git working tree/u);
  assert.equal(
    h.specs[0] === undefined,
    false,
    "the run must still have created a session",
  );
});

test("a git that errors is noted, not fatal", async () => {
  const h = runnerHarness(
    [{ tools: [{ name: "report_done", params: DONE_PARAMS }] }],
    { repoError: true },
  );
  const outcome = await h.runner.run("loop-42");
  assert.equal(outcome.kind, "done");
  assert.match(outcome.contextNotes.join("\n"), /repo snapshot unavailable \(exit\)/u);
});

test("an unexpected repo failure still surfaces rather than being swallowed", async () => {
  const runner = createAgentRunner({
    beads: beadsStub(issueFixture()).client,
    sessionFactory: async (spec) => new FakeSession(spec, {}),
    repo: {
      async describe() {
        throw new TypeError("bug in the reader");
      },
    },
    now: clock.now,
  });
  await assert.rejects(() => runner.run("loop-42"), TypeError);
});

test("a hard recall failure is noted but a missing memory is not", async () => {
  const withMemory = runnerHarness(
    [{ tools: [{ name: "report_done", params: DONE_PARAMS }] }],
    { memories: { [failureKeyFor("loop-42")]: "last time: timed out" } },
  );
  const done = await withMemory.runner.run("loop-42");
  assert.equal(done.kind, "done");
  assert.deepEqual(done.contextNotes, [], "a present memory is not a failure");
  assert.ok(withMemory.calls.includes(`recall:${failureKeyFor("loop-42")}`));
  assert.ok(withMemory.calls.includes(`recall:${handoffKeyFor("loop-42")}`));

  const exploding = runnerHarness(
    [{ tools: [{ name: "report_done", params: DONE_PARAMS }] }],
    { memories: {} },
  );
  const outcome = await exploding.runner.run("loop-42");
  assert.equal(outcome.kind, "done");
  // not-found is a normal answer: no note, no fuss.
  assert.deepEqual(outcome.contextNotes, []);
});

test("a recalled prior failure reaches the prompt", async () => {
  const priorText = "PRIOR-ATTEMPT-NOTE: the tests were left red";
  let capturedPrompt = "";
  const runner = createAgentRunner({
    beads: beadsStub(issueFixture(), { [failureKeyFor("loop-42")]: priorText }).client,
    sessionFactory: async (spec) => {
      const session = new FakeSession(spec, { tools: [{ name: "report_done", params: DONE_PARAMS }] });
      capturedPrompt = "";
      const original = session.prompt.bind(session);
      session.prompt = async (text: string) => {
        capturedPrompt = text;
        return original(text);
      };
      return session;
    },
    repo: { async describe() { return repoFixture(); } },
    now: clock.now,
  });

  const outcome = await runner.run("loop-42");
  assert.equal(outcome.kind, "done");
  assert.match(capturedPrompt, /PRIOR-ATTEMPT-NOTE/u);
});

// ── no lifecycle writes from the runner ─────────────────────────────────────

test("agent.ts never writes issue status, claims, or assigns", () => {
  const source = stripComments(readFileSync(join(SRC_DIR, "agent.ts"), "utf8"));

  for (const forbidden of ["setStatus", "closeIssue", "--status", "--assignee", "--claim"]) {
    assert.equal(
      source.includes(forbidden),
      false,
      `agent.ts must not contain ${forbidden}; lifecycle belongs to the loop/workgraph`,
    );
  }
});

test("a full run touches beads only through getIssue and recall", async () => {
  const h = runnerHarness([{ tools: [{ name: "report_done", params: DONE_PARAMS }] }]);
  await h.runner.run("loop-42");

  const verbs = h.calls.map((call) => call.split(":")[0] as string);
  assert.deepEqual(
    [...new Set(verbs)].sort(),
    ["getIssue", "recall"],
    `unexpected bd verbs: ${verbs.join(", ")}`,
  );
});

// ── split ───────────────────────────────────────────────────────────────────

test("a valid report_split yields specs and creates nothing itself", async () => {
  const h = runnerHarness([
    {
      tools: [
        {
          name: "report_split",
          params: {
            issues: [
              { title: "Do A", description: "A is needed", acceptance: "A exists", priority: 1, type: "task" },
              { title: "Do B", priority: 3 },
            ],
          },
        },
      ],
    },
  ]);

  const specs = await h.runner.split("two things please");
  assert.equal(specs.length, 2);
  assert.equal(specs[0]?.title, "Do A");
  assert.equal(specs[0]?.acceptance, "A exists");
  assert.equal(specs[0]?.priority, 1);
  assert.equal(specs[1]?.priority, 3);
  assert.equal(h.calls.length, 0, "split must not touch bd at all");
});

test("the split session runs with no built-in tools; work does not", async () => {
  const splitRun = runnerHarness([
    { tools: [{ name: "report_split", params: { issues: [{ title: "X" }] } }] },
  ]);
  await splitRun.runner.split("plan please");
  assert.equal(splitRun.specs[0]?.kind, "split");
  assert.equal(splitRun.specs[0]?.noBuiltinTools, true, "planning must not be editing");

  const workRun = runnerHarness([{ tools: [{ name: "report_done", params: DONE_PARAMS }] }]);
  await workRun.runner.run("loop-42");
  assert.equal(workRun.specs[0]?.kind, "work");
  assert.notEqual(workRun.specs[0]?.noBuiltinTools, true, "work needs its tools");
});

test("split without a structured proposal is a typed error", async () => {
  const h = runnerHarness([{ reply: "I would split this into a couple of tasks, probably" }]);
  await assert.rejects(
    () => h.runner.split("vibes"),
    (error: unknown) =>
      AgentError.is(error) &&
      error.kind === "unstructured-verdict" &&
      /report_split was never called/u.test(error.message),
  );
  assert.equal(h.sessions[0]?.disposeCalls, 1);
});

test("split with an invalid proposal is malformed, with the raw payload kept", async () => {
  const h = runnerHarness([
    { tools: [{ name: "report_split", params: { issues: [{ description: "no title" }] } }] },
  ]);
  await assert.rejects(
    () => h.runner.split("bad plan"),
    (error: unknown) =>
      AgentError.is(error) && error.kind === "malformed-verdict" && error.detail.includes("title"),
  );
});

test("split timeout is a typed timeout error", async () => {
  const h = runnerHarness([{ neverSettle: true }], { timeoutMs: 25, abortGraceMs: 15 });
  await assert.rejects(
    () => h.runner.split("slow planner"),
    (error: unknown) => AgentError.is(error) && error.kind === "timeout",
  );
  h.sessions[0]?.release();
  assert.equal(h.sessions[0]?.abortCalls, 1);
  assert.equal(h.sessions[0]?.disposeCalls, 1);
});

test("blank input is refused before a session is created", async () => {
  const h = runnerHarness([]);
  await assert.rejects(() => h.runner.split("   "), (error: unknown) => AgentError.is(error) && error.kind === "invalid-arguments");
  await assert.rejects(() => h.runner.run(""), (error: unknown) => AgentError.is(error) && error.kind === "invalid-arguments");
  assert.equal(h.sessions.length, 0, "no session may be created for blank input");
});

test("a missing issue is a typed error with no session", async () => {
  const h = runnerHarness([], { issue: null });
  await assert.rejects(
    () => h.runner.run("nope-1"),
    (error: unknown) => AgentError.is(error) && error.kind === "issue-not-found",
  );
  assert.equal(h.sessions.length, 0);
});

test("a session that cannot be created is an error outcome, not a hang", async () => {
  const runner = createAgentRunner({
    beads: beadsStub(issueFixture()).client,
    sessionFactory: async () => {
      throw new Error("pi is on fire");
    },
    repo: { async describe() { return repoFixture(); } },
    now: clock.now,
  });
  const outcome = await runner.run("loop-42");
  assert.equal(outcome.kind, "error");
  assert.ok(outcome.kind === "error");
  assert.equal(outcome.phase, "session");
  assert.match(outcome.message, /pi is on fire/u);
  assert.equal(outcome.sessionId, null);
});

// ── pure helpers, directly ──────────────────────────────────────────────────

test("validateVerdict accepts both field spellings and rejects garbage", () => {
  assert.equal(validateVerdict(DONE_PARAMS).ok, true);
  assert.equal(
    validateVerdict({
      done: true,
      summary: "camel works",
      changedFiles: ["a.ts"],
      nextSteps: [],
    }).ok,
    true,
  );
  assert.equal(validateVerdict(null).ok, false);
  assert.equal(validateVerdict("done").ok, false);
  assert.equal(validateVerdict({ done: "yes", summary: "x", changed_files: [] }).ok, false);
  assert.equal(validateVerdict({ done: true, summary: " ", changed_files: [] }).ok, false);
  assert.equal(validateVerdict({ done: true, summary: "x", changed_files: [1] }).ok, false);

  const missing = validateVerdict({});
  assert.equal(missing.ok, false);
  if (!missing.ok) {
    assert.ok(missing.problems.length >= 3, "every missing field should be named");
  }
});

test("validateSplitPayload is strict about titles and priorities", () => {
  assert.deepEqual(validateSplitPayload([]), { ok: false, problems: ["no issues were proposed"] });
  assert.equal(validateSplitPayload({ issues: [{ title: "ok" }] }).ok, true);
  assert.equal(validateSplitPayload([{ title: "bare array is fine" }]).ok, true);
  assert.equal(validateSplitPayload({ issues: [{ title: " " }] }).ok, false);
  assert.equal(validateSplitPayload({ issues: [{ title: "x", priority: 9 }] }).ok, false);
  assert.equal(validateSplitPayload({ issues: [{ title: "x", priority: "2" }] }).ok, true);
  const withDeps = validateSplitPayload({ issues: [{ title: "x", deps: ["a", "b"] }] });
  assert.equal(withDeps.ok, true);
  if (withDeps.ok) assert.deepEqual(withDeps.specs[0]?.deps, ["a", "b"]);
});

test("extractLastFencedJson takes the last block and reports parse errors", () => {
  assert.deepEqual(extractLastFencedJson("nothing here"), { found: false });

  const one = extractLastFencedJson('text\n```json\n{"a": 1}\n```\ntail');
  assert.equal(one.found, true);
  if (one.found) assert.deepEqual(one.json, { a: 1 });

  const two = extractLastFencedJson('```\n{"a": 1}\n```\nmore\n```json\n{"b": 2}\n```');
  assert.equal(two.found, true);
  if (two.found) assert.deepEqual(two.json, { b: 2 }, "the last block wins — later edits supersede");

  const broken = extractLastFencedJson("```json\n{ nope }\n```");
  assert.equal(broken.found, true);
  if (broken.found) {
    assert.equal(broken.parseError !== undefined, true);
    assert.equal(broken.raw, "{ nope }");
  }
});

test("classifyRunEvidence orders the evidence: tool, rejection, fence, nothing", () => {
  const verdict = { done: true, summary: "s", changed_files: ["a"] };

  assert.equal(
    classifyRunEvidence({ toolVerdicts: [verdict], toolRejections: [], assistantText: "" }).kind,
    "done",
  );
  assert.equal(
    classifyRunEvidence({
      toolVerdicts: [],
      toolRejections: [{ raw: "{}", problems: ["nope"] }],
      assistantText: "```json\n" + JSON.stringify(verdict) + "\n```",
    }).kind,
    "malformed-verdict",
  );
  assert.equal(
    classifyRunEvidence({
      toolVerdicts: [],
      toolRejections: [],
      assistantText: "```json\n" + JSON.stringify(verdict) + "\n```",
    }).kind,
    "done",
  );
  assert.equal(
    classifyRunEvidence({ toolVerdicts: [], toolRejections: [], assistantText: "done!" }).kind,
    "unstructured-verdict",
  );
});

test("every outcome kind maps to exactly one work event", () => {
  const events = WORK_OUTCOME_KINDS.map((kind) => {
    const outcome = outcomeFixtureFor(kind);
    const event = toWorkEvent(outcome);
    assert.ok(
      event.type === "work_succeeded" || event.type === "work_failed",
      `${kind} mapped to ${event.type}`,
    );
    return [kind, event.type] as const;
  });

  assert.deepEqual(
    Object.fromEntries(events),
    {
      done: "work_succeeded",
      incomplete: "work_failed",
      "unstructured-verdict": "work_failed",
      "malformed-verdict": "work_failed",
      timeout: "work_failed",
      error: "work_failed",
    },
  );
});

test("failure text names what actually went wrong for every failing kind", () => {
  assert.match(describeFailure(outcomeFixtureFor("incomplete")), /agent reported incomplete/u);
  assert.match(describeFailure(outcomeFixtureFor("unstructured-verdict")), /no structured verdict/u);
  assert.match(describeFailure(outcomeFixtureFor("malformed-verdict")), /malformed verdict/u);
  assert.match(describeFailure(outcomeFixtureFor("timeout")), /timed out after 1234ms/u);
  assert.match(describeFailure(outcomeFixtureFor("error")), /run error: kaboom/u);
});

test("text helpers read the message shapes bd-style content can have", () => {
  const messages = [
    { role: "user", content: "hello" },
    { role: "assistant", content: [{ type: "thinking", text: "hmm" }, { type: "text", text: "hi" }] },
    { role: "assistant", content: [{ type: "toolCall", name: "x" }] },
    { role: "assistant", content: [{ type: "text", text: "second" }] },
  ];
  assert.equal(collectAssistantText(messages), "hi\nsecond");
  assert.equal(lastAssistantText(messages), "second");
  assert.equal(lastAssistantText([]), "");
  assert.equal(messageTextish({ role: "assistant", content: 42 }), "");
});

/** Local shim so the test does not import an internal helper. */
function messageTextish(message: unknown): string {
  if (typeof message !== "object" || message === null) return "";
  const content = (message as { content?: unknown }).content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter((part) => typeof part === "object" && part !== null)
    .map((part) => (part as { type?: string; text?: string }).type === "text" ? (part as { text?: string }).text ?? "" : "")
    .join("");
}

function outcomeFixtureFor(kind: (typeof WORK_OUTCOME_KINDS)[number]): WorkOutcome {
  const base = {
    issueId: "loop-42",
    sessionId: "fake",
    sessionFile: null,
    verdictSource: "none" as const,
    assistantText: "",
    elapsedMs: 1,
    contextNotes: [],
    verdictToolCalls: 0,
  };
  switch (kind) {
    case "done":
      return {
        ...base,
        kind: "done",
        verdictSource: "report_done_tool",
        verdict: { done: true, summary: "s", changedFiles: ["a"], nextSteps: [] },
      };
    case "incomplete":
      return {
        ...base,
        kind: "incomplete",
        verdictSource: "report_done_tool",
        verdict: { done: false, reason: "blocked", summary: "s", changedFiles: [], nextSteps: [] },
      };
    case "unstructured-verdict":
      return { ...base, kind: "unstructured-verdict", rawText: "done-ish" };
    case "malformed-verdict":
      return {
        ...base,
        kind: "malformed-verdict",
        verdictSource: "fenced_json",
        rawBlock: "{bad}",
        problems: ["bad json"],
      };
    case "timeout":
      return { ...base, kind: "timeout", budgetMs: 1234, settledAfterAbort: false };
    case "error":
      return { ...base, kind: "error", message: "kaboom", phase: "run" };
  }
}

// ── model resolution: configured, never ambient ─────────────────────────────

type ResolverArgs = Parameters<typeof resolveModelForRun>;

const CHOSEN_MODEL = { id: "chosen" };

function runtimeStub(rows: unknown[]): ResolverArgs[0] {
  return {
    getModel: (_provider: string, id: string) =>
      rows.find((row) => (row as { id?: string }).id === id),
    getModels: () => rows,
  } as unknown as ResolverArgs[0];
}

function settingsStub(values: { provider?: string; model?: string }): ResolverArgs[1] {
  return {
    getDefaultProvider: () => values.provider,
    getDefaultModel: () => values.model,
  };
}

test("an explicit modelRef wins, and an unavailable one is a typed error", () => {
  assert.equal(
    resolveModelForRun(runtimeStub([CHOSEN_MODEL]), settingsStub({}), { provider: "p", id: "chosen" }),
    CHOSEN_MODEL,
  );

  assert.throws(
    () => resolveModelForRun(runtimeStub([]), settingsStub({ provider: "p", model: "real" }), { provider: "p", id: "ghost" }),
    (error: unknown) =>
      AgentError.is(error) &&
      error.kind === "session-failed" &&
      error.message.includes("p/ghost"),
    "the error must name the ref that was asked for",
  );
});

test("with no ref, pi's configured default is used — and a dead default is loud", () => {
  assert.equal(
    resolveModelForRun(runtimeStub([CHOSEN_MODEL]), settingsStub({ provider: "p", model: "chosen" })),
    CHOSEN_MODEL,
  );

  assert.throws(
    () => resolveModelForRun(runtimeStub([]), settingsStub({ provider: "p", model: "stale" })),
    (error: unknown) =>
      AgentError.is(error) &&
      error.message.includes("p/stale") &&
      /not available/u.test(error.message),
    "a stale default must fail loudly, not silently run something else",
  );
});

test("no configured default falls through to pi's own choice; zero models is an error", () => {
  assert.equal(
    resolveModelForRun(runtimeStub([CHOSEN_MODEL]), settingsStub({})),
    undefined,
    "no default named: pi picks, we do not guess",
  );

  assert.throws(
    () => resolveModelForRun(runtimeStub([]), settingsStub({})),
    (error: unknown) => AgentError.is(error) && /no models are configured/u.test(error.message),
  );
});

test("agent.ts reads no environment variables at all", () => {
  const source = stripComments(readFileSync(join(SRC_DIR, "agent.ts"), "utf8"));
  assert.equal(
    source.includes("process.env"),
    false,
    "model/tool config must be passed in, not absorbed from whatever the parent shell exported",
  );
});

// ── test utilities ──────────────────────────────────────────────────────────

/** Strip comments so prose in the source cannot satisfy a "does not appear" test. */
function stripComments(source: string): string {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .split("\n")
    .filter((line) => !line.trimStart().startsWith("//"))
    .join("\n");
}

async function waitFor(predicate: () => boolean, timeoutMs = 2_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("waitFor timed out");
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

export type { RunnerSessionKind };
