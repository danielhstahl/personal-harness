/**
 * Shared test doubles for the interpreter suite (`test/loop.test.ts`).
 *
 * Not a `*.test.ts` file on purpose: `node --test test/*.test.ts` will not run
 * it, so the fakes can be imported without becoming a suite of their own.
 *
 * What each double is *for*:
 *
 * - {@link createScriptBoard} — an in-memory `bd` with real-ish semantics:
 *   priority-then-id ordering, `ifStatus` guards that fail as `guard-mismatch`,
 *   append-only notes, keyed memories, and a `failNext` hook so a single write
 *   can be made to blow up exactly once (the crash-recovery tests need that).
 * - {@link makeRepo} — a throwaway git repo with **no identity anywhere**, plus
 *   a real `GitWriter` pointed at it. Commits here are real commits.
 * - {@link fakeSessionFactory} — the `.5` session seam: scripted answers, no
 *   model, no network. Records every prompt text, which is how the
 *   no-context-carry-over rule is checked.
 * - {@link scriptedIdle} — the `.6` surface reduced to a queue of outcomes.
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { AgentSessionLike, SessionFactory, SessionSpec } from "../src/agent.ts";
import { BdError } from "../src/beads.ts";
import type { BdClient, Issue, IssueStatus, NewIssueSpec } from "../src/beads.ts";
import type { IdleOutcome } from "../src/idle.ts";
import type { LoopIdlePort, LoopUi } from "../src/loop.ts";
import { createGitWriter } from "../src/vcs.ts";
import type { GitWriter } from "../src/vcs.ts";

// ── board ───────────────────────────────────────────────────────────────────

export interface ScriptBoard extends BdClient {
  readonly issues: Map<string, Issue>;
  /** Every method call, in order, as `method(args)` strings. */
  readonly calls: string[];
  readonly memories: Map<string, string>;
  /** Put an issue on the board directly, bypassing `createIssue`. */
  seed(partial: Partial<Issue> & { id: string; title: string }): Issue;
  /** Make the *next* call of `method` throw `error`. One shot. */
  failNext(method: keyof BdClient, error: Error): void;
  byStatus(status: IssueStatus): Issue[];
  statusOf(id: string): IssueStatus | null;
  notesOf(id: string): string;
}

/** Same order bd uses: lowest priority number first, then id by bytes. */
function boardOrder(a: Issue, b: Issue): number {
  if (a.priority !== b.priority) return a.priority - b.priority;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

/** Spread that skips keys whose value is `undefined`, so defaults survive. */
function definedOnly<T extends object>(value: T): Partial<T> {
  const out: Record<string, unknown> = {};
  for (const [key, entry] of Object.entries(value)) {
    if (entry !== undefined) out[key] = entry;
  }
  return out as Partial<T>;
}

export function createScriptBoard(): ScriptBoard {
  const issues = new Map<string, Issue>();
  const memories = new Map<string, string>();
  const calls: string[] = [];
  const failures = new Map<string, Error[]>();
  let seq = 0;

  function gate(method: string, args: readonly string[]): void {
    calls.push(`${method}(${args.join(", ")})`);
    const queued = failures.get(method);
    if (queued !== undefined && queued.length > 0) {
      const error = queued.shift() as Error;
      throw error;
    }
  }

  function mustExist(id: string): Issue {
    const issue = issues.get(id);
    if (issue === undefined) {
      throw new BdError({ kind: "not-found", message: `no issue ${id} on this board` });
    }
    return issue;
  }

  const board: ScriptBoard = {
    issues,
    calls,
    memories,

    seed(partial) {
      const base: Issue = {
        id: partial.id,
        title: partial.title,
        status: "open",
        priority: 2,
        issue_type: "task",
        description: "",
      };
      const issue: Issue = { ...base, ...definedOnly(partial) };
      issues.set(issue.id, issue);
      return issue;
    },

    failNext(method, error) {
      const queued = failures.get(String(method)) ?? [];
      queued.push(error);
      failures.set(String(method), queued);
    },

    byStatus(status) {
      return [...issues.values()].filter((issue) => issue.status === status);
    },

    statusOf(id) {
      return issues.get(id)?.status ?? null;
    },

    notesOf(id) {
      return issues.get(id)?.notes ?? "";
    },

    async listReady(options = {}) {
      gate("listReady", [`labels=${(options.labels ?? []).join("|")}`]);
      return [...issues.values()].filter((issue) => issue.status === "open").sort(boardOrder);
    },

    async listInProgress(options = {}) {
      gate("listInProgress", [`labels=${(options.labels ?? []).join("|")}`]);
      return [...issues.values()].filter((issue) => issue.status === "in_progress").sort(boardOrder);
    },

    async getIssue(id) {
      gate("getIssue", [id]);
      return issues.get(id) ?? null;
    },

    async createIssue(spec: NewIssueSpec) {
      gate("createIssue", [spec.title, `type=${spec.type ?? "task"}`, `deps=${(spec.deps ?? []).join("|")}`]);
      seq += 1;
      const id = `tst.${seq}`;
      const issue: Issue = {
        id,
        title: spec.title,
        status: "open",
        priority: spec.priority ?? 2,
        issue_type: spec.type ?? "task",
        description: spec.description ?? "",
        acceptance_criteria: spec.acceptance,
        labels: spec.labels ? [...spec.labels] : [],
        dependencies: (spec.deps ?? []).map((target) => ({
          type: "blocks",
          depends_on_id: target,
        })),
      };
      issues.set(id, issue);
      return issue;
    },

    async addDep(id, dependsOnId, type = "blocks") {
      gate("addDep", [id, dependsOnId, type]);
      const issue = mustExist(id);
      issue.dependencies = [...(issue.dependencies ?? []), { type, depends_on_id: dependsOnId }];
    },

    async appendNote(id, text) {
      gate("appendNote", [id]);
      const issue = mustExist(id);
      issue.notes = issue.notes === undefined ? text : `${issue.notes}\n${text}`;
      return issue;
    },

    async setStatus(id, status, options = {}) {
      gate("setStatus", [id, status, `ifStatus=${options.ifStatus ?? "-"}`]);
      const issue = mustExist(id);
      if (options.ifStatus !== undefined && issue.status !== options.ifStatus) {
        throw new BdError({
          kind: "guard-mismatch",
          message: `${id} is ${issue.status}, not ${options.ifStatus}; bd refused the write`,
          exitCode: 13,
        });
      }
      issue.status = status;
      return issue;
    },

    async closeIssue(id, reason) {
      gate("closeIssue", [id, reason ?? ""]);
      const issue = mustExist(id);
      issue.status = "closed";
      if (reason !== undefined) issue.notes = board.notesOf(id) + `\nClosed: ${reason}`;
      return issue;
    },

    async remember(text, key) {
      gate("remember", [key ?? "(anonymous)"]);
      memories.set(key ?? "(anonymous)", text);
    },

    async recall(key) {
      gate("recall", [key]);
      return memories.get(key) ?? null;
    },
  };

  return board;
}

// ── git ─────────────────────────────────────────────────────────────────────

export interface RepoSandbox {
  readonly dir: string;
  readonly home: string;
  readonly env: Readonly<Record<string, string>>;
  readonly writer: GitWriter;
  git(...args: string[]): string;
  commitCount(): number;
  head(): string;
  filesInHead(): string[];
  write(relativePath: string, content: string): void;
  dispose(): void;
}

const IDENTITY_VARS = [
  "GIT_AUTHOR_NAME",
  "GIT_AUTHOR_EMAIL",
  "GIT_COMMITTER_NAME",
  "GIT_COMMITTER_EMAIL",
  "GIT_AUTHOR_DATE",
  "GIT_COMMITTER_DATE",
  "EMAIL",
  "USERNAME",
];

/**
 * A scratch repo whose only possible identity is the one the writer supplies:
 * empty `HOME`, empty global and system config, identity env stripped.
 */
export function makeRepo(): RepoSandbox {
  const dir = mkdtempSync(join(tmpdir(), "loop-wire-repo-"));
  const home = mkdtempSync(join(tmpdir(), "loop-wire-home-"));
  const emptyConfig = join(home, "empty.gitconfig");
  writeFileSync(emptyConfig, "");

  const env: Record<string, string> = { HOME: home, PATH: process.env.PATH ?? "/usr/bin:/bin" };
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue;
    if (IDENTITY_VARS.includes(key)) continue;
    if (key.startsWith("GIT_AUTHOR_") || key.startsWith("GIT_COMMITTER_")) continue;
    env[key] = value;
  }
  env.GIT_CONFIG_GLOBAL = emptyConfig;
  env.GIT_CONFIG_SYSTEM = emptyConfig;
  env.GIT_CONFIG_NOSYSTEM = "1";

  const git = (...args: string[]): string =>
    execFileSync("git", ["-c", "user.name=setup", "-c", "user.email=setup@example.invalid", ...args], {
      cwd: dir,
      env: { ...env },
      encoding: "utf8",
    });

  git("init", "-q", ".");
  writeFileSync(join(dir, "BASE.md"), "base content\n");
  git("add", "--", "BASE.md");
  git("commit", "-q", "-m", "base commit");

  return {
    dir,
    home,
    env,
    writer: createGitWriter({
      cwd: dir,
      env,
      authorName: "pi-loop",
      authorEmail: "pi-loop@localhost",
    }),
    git,
    commitCount: () => Number(git("rev-list", "--count", "HEAD").trim()),
    head: () => git("rev-parse", "HEAD").trim(),
    filesInHead: () =>
      git("show", "--name-only", "--format=", "HEAD")
        .trim()
        .split("\n")
        .filter(Boolean),
    write(relativePath: string, content: string) {
      const target = join(dir, relativePath);
      mkdirSync(join(target, ".."), { recursive: true });
      writeFileSync(target, content);
    },
    dispose() {
      rmSync(dir, { recursive: true, force: true });
      rmSync(home, { recursive: true, force: true });
    },
  };
}

// ── sessions ────────────────────────────────────────────────────────────────

export interface FakeToolCall {
  readonly name: string;
  readonly params: unknown;
}

export interface FakeScript {
  tools?: FakeToolCall[];
  reply?: string;
  /** Never settle until `abort()` releases the gate (the interrupt test). */
  neverSettle?: boolean;
  rejectWith?: string;
}

type ChatMessage = AgentSessionLike["messages"][number];

interface LooseTool {
  name: string;
  execute: (
    toolCallId: string,
    params: unknown,
    signal: AbortSignal,
    update: (partial: unknown) => void,
  ) => Promise<unknown>;
}

let fakeSessionSeq = 0;

export class FakeSession implements AgentSessionLike {
  readonly spec: SessionSpec;
  readonly sessionId: string;
  readonly sessionFile: string | undefined = undefined;
  messages: ChatMessage[] = [];
  readonly promptTexts: string[] = [];
  readonly kinds: string[] = [];
  abortCalls = 0;
  disposeCalls = 0;

  private readonly script: FakeScript;
  private releaseGate: (() => void) | null = null;

  constructor(spec: SessionSpec, script: FakeScript) {
    this.spec = spec;
    this.script = script;
    fakeSessionSeq += 1;
    this.sessionId = `fake-session-${fakeSessionSeq}`;
    this.kinds.push(spec.kind);
  }

  subscribe(): () => void {
    return () => {};
  }

  async prompt(text: string): Promise<void> {
    this.promptTexts.push(text);
    this.messages.push({ role: "user", content: text } as ChatMessage);

    for (const [index, call] of (this.script.tools ?? []).entries()) {
      const tool = (this.spec.customTools as unknown as LooseTool[]).find(
        (candidate) => candidate.name === call.name,
      );
      if (tool === undefined) {
        this.messages.push({
          role: "toolResult",
          content: [{ type: "text", text: `${call.name} is not registered` }],
        } as unknown as ChatMessage);
        continue;
      }
      await tool.execute(`tc-${index}`, call.params, new AbortController().signal, () => {});
      this.messages.push({
        role: "toolResult",
        content: [{ type: "text", text: `${call.name} accepted` }],
      } as unknown as ChatMessage);
    }

    this.messages.push({
      role: "assistant",
      content: [{ type: "text", text: this.script.reply ?? "" }],
    } as unknown as ChatMessage);

    if (this.script.rejectWith !== undefined) {
      throw new Error(this.script.rejectWith);
    }
    if (this.script.neverSettle === true) {
      await new Promise<void>((resolve) => {
        this.releaseGate = resolve;
      });
    }
  }

  async abort(): Promise<void> {
    this.abortCalls += 1;
    // An abort that does not unblock the prompt would make the interrupt test hang.
    this.releaseGate?.();
  }

  dispose(): void {
    this.disposeCalls += 1;
  }
}

export interface FakeSessionFactory {
  readonly factory: SessionFactory;
  readonly sessions: FakeSession[];
  /** Concatenation of every prompt sent in every session — the transcript probe. */
  allPrompts(): string;
}

export function fakeSessionFactory(scripts: readonly FakeScript[] = []): FakeSessionFactory {
  const sessions: FakeSession[] = [];
  const factory: SessionFactory = async (spec) => {
    const session = new FakeSession(spec, scripts[sessions.length] ?? {});
    sessions.push(session);
    return session;
  };
  return {
    factory,
    sessions,
    allPrompts: () => sessions.map((session) => session.promptTexts.join("\n")).join("\n"),
  };
}

/** The `report_done` shape that makes a run classify as `done`. */
export function doneParams(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    done: true,
    summary: "Made the change and checked it.",
    changed_files: ["src/thing.ts"],
    next_steps: [],
    ...overrides,
  };
}

// ── idle / ui / signals / log ───────────────────────────────────────────────

export interface ScriptedIdle extends LoopIdlePort {
  readonly outcomes: IdleOutcome[];
  refreshCalls: number;
  disposeCalls: number;
}

/** Feed `texts` as submits, then exit. Records refresh and dispose calls. */
export function scriptedIdle(texts: readonly string[] = []): ScriptedIdle {
  const queue: IdleOutcome[] = [
    ...texts.map((text): IdleOutcome => ({ kind: "input", text })),
    { kind: "exit", reason: "command" },
  ];
  const idle: ScriptedIdle = {
    outcomes: queue,
    refreshCalls: 0,
    disposeCalls: 0,
    async next() {
      const outcome = queue.shift();
      return outcome ?? { kind: "exit", reason: "command" };
    },
    refresh() {
      idle.refreshCalls += 1;
    },
    async dispose() {
      idle.disposeCalls += 1;
    },
  };
  return idle;
}

export interface RecordingUi extends LoopUi {
  readonly said: string[];
  readonly warned: string[];
}

export function recordingUi(): RecordingUi {
  const said: string[] = [];
  const warned: string[] = [];
  return {
    said,
    warned,
    say(text: string) {
      said.push(text);
    },
    warn(text: string) {
      warned.push(text);
    },
  };
}

export interface ScriptedSignals {
  readonly adapter: { on(signal: string, handler: () => void): () => void };
  fire(signal: string): void;
  readonly attached: string[];
  readonly detached: string[];
}

export function scriptedSignals(): ScriptedSignals {
  const handlers = new Map<string, Set<() => void>>();
  const attached: string[] = [];
  const detached: string[] = [];
  return {
    attached,
    detached,
    adapter: {
      on(signal, handler) {
        attached.push(signal);
        const set = handlers.get(signal) ?? new Set();
        set.add(handler);
        handlers.set(signal, set);
        return () => {
          detached.push(signal);
          set.delete(handler);
        };
      },
    },
    fire(signal) {
      for (const handler of handlers.get(signal) ?? []) handler();
    },
  };
}

// ── split wire payloads ─────────────────────────────────────────────────────

/** One item in the shape the split validator accepts from a model. */
export function splitWireItem(
  title: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    title,
    description: `${title}: what to do`,
    acceptance_criteria: `${title}: how to tell it worked`,
    priority: 2,
    depends_on: [],
    type: "task",
    ...extra,
  };
}

export interface FakeSplitPort {
  propose(request: string): Promise<unknown>;
  readonly requests: string[];
  attempts(): number;
}

/**
 * A `SplitAgentPort` that answers from a script. Each entry is the raw value a
 * model would have returned; `Error` entries model the port itself failing.
 */
export function fakeSplitPort(answers: readonly unknown[]): FakeSplitPort {
  let index = 0;
  const requests: string[] = [];
  return {
    requests,
    attempts: () => index,
    async propose(request: string) {
      requests.push(request);
      const answer = answers[index];
      index += 1;
      if (answer instanceof Error) throw answer;
      return answer;
    },
  };
}
