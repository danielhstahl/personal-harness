/**
 * Tests for `src/split.ts` — SPLIT.
 *
 * Two rules shape everything here:
 *
 * 1. **Nothing reaches the board unless it was validated first.** The fakes below
 *    are built to *refuse* bad writes the way real `bd` does — a dependency on an
 *    unknown id throws, an unknown parent throws — so a test that passes cannot
 *    have slipped a dangling edge through.
 * 2. **No model, no network.** Every "model" is a canned value in a port. The
 *    live-`bd` test at the end still uses a canned answer; only the board is real.
 */

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
  BdError,
  createBdClient,
  dependsOn,
  normaliseDependencies,
  type BdClient,
  type Issue,
  type NewIssueSpec,
} from "../src/beads.js";
import { buildSplitPrompt, validateSplitPayload } from "../src/agent.js";
import { createInitialState, step } from "../src/orchestrator.js";
import {
  buildRetryRequest,
  createSplitLedger,
  createSplitter,
  createdIds,
  DEFAULT_MAX_ITEMS,
  describeSplitFailure,
  epicTitleFor,
  formatSplitProblem,
  indexForSpec,
  isSplitSuccess,
  planSplitOrder,
  portFromAgentRunner,
  recordHumanRequest,
  SplitError,
  toNewIssueSpec,
  toNewIssueSpecs,
  toSplitEvent,
  unavailableSplitPort,
  validateSplitBatch,
  verbatimRecord,
  VERBATIM_MARKER,
  type SplitAgentPort,
  type SplitItem,
  type SplitOutcome,
  type SplitProblem,
  type SplitValidationConfig,
} from "../src/split.js";

// ── fakes ───────────────────────────────────────────────────────────────

interface FakeAgent {
  readonly port: SplitAgentPort;
  readonly requests: string[];
  calls(): number;
}

/**
 * A model that says exactly what it is told to say, in order. The last entry
 * repeats if asked more times than there are entries — which is itself a fact
 * worth failing on, so `calls()` is always available.
 */
function fakeAgent(replies: readonly unknown[]): FakeAgent {
  const requests: string[] = [];
  const port: SplitAgentPort = {
    async propose(request: string): Promise<unknown> {
      requests.push(request);
      const index = Math.min(requests.length - 1, replies.length - 1);
      const reply = replies[index];
      if (typeof reply === "function") return await (reply as () => unknown)();
      return reply;
    },
  };
  return { port, requests, calls: () => requests.length };
}

interface FakeBoard {
  readonly client: BdClient;
  /** createIssue specs, in the order they arrived. */
  readonly specs: NewIssueSpec[];
  readonly calls: string[];
  issue(id: string): Issue | undefined;
  noteWrites(): number;
  /** Child (non-epic) creates only. */
  childCreates(): NewIssueSpec[];
  /** Make the next create whose spec matches throw. */
  injectFailure(match: (spec: NewIssueSpec) => Error | null): void;
  createAttemptsFor(title: string): number;
}

/**
 * An in-memory board that refuses bad writes the way `bd` does.
 *
 * The refusal matters more than the storage: if the splitter ever tried to hang a
 * dependency off an id that was never created, this fake throws instead of
 * quietly recording a fiction.
 */
function fakeBoard(prefix = "fake"): FakeBoard {
  const issues = new Map<string, Issue>();
  const specs: NewIssueSpec[] = [];
  const calls: string[] = [];
  const noteTexts: string[] = [];
  const attempts = new Map<string, number>();
  let counter = 0;
  let failure: ((spec: NewIssueSpec) => Error | null) | null = null;

  const client: BdClient = {
    async listReady(): Promise<Issue[]> {
      throw new Error("the splitter must not read the ready list");
    },
    async listInProgress(): Promise<Issue[]> {
      throw new Error("the splitter must not read the in-progress list");
    },
    async getIssue(id: string): Promise<Issue | null> {
      return issues.get(id) ?? null;
    },
    async createIssue(spec: NewIssueSpec): Promise<Issue> {
      calls.push(`createIssue:${spec.title}`);
      attempts.set(spec.title, (attempts.get(spec.title) ?? 0) + 1);

      const injected = failure === null ? null : failure(spec);
      if (injected !== null) throw injected;

      if (spec.parent !== undefined && !issues.has(spec.parent)) {
        throw new BdError({ kind: "exit-1", message: `bd: parent issue ${spec.parent} not found` });
      }
      for (const dep of spec.deps ?? []) {
        if (!issues.has(dep)) {
          throw new BdError({
            kind: "exit-1",
            message: `bd: dependency ${dep} does not exist (dangling edge for "${spec.title}")`,
          });
        }
      }

      counter += 1;
      const id = `${prefix}-${counter}`;
      const issue: Issue = {
        id,
        title: spec.title,
        description: spec.description,
        acceptance_criteria: spec.acceptance,
        status: "open",
        priority: spec.priority ?? 2,
        issue_type: spec.type ?? "task",
        dependencies: (spec.deps ?? []).map((dep) => ({ id: dep, dependency_type: "blocks" })),
      };
      issues.set(id, issue);
      specs.push(spec);
      return issue;
    },
    async addDep(): Promise<void> {
      throw new Error("the splitter binds deps at create time, not afterwards");
    },
    async appendNote(id: string, text: string): Promise<Issue> {
      calls.push(`appendNote:${id}`);
      const issue = issues.get(id);
      if (issue === undefined) {
        throw new BdError({ kind: "exit-1", message: `bd: resolving ${id}: no issue found` });
      }
      noteTexts.push(text);
      const notes = [issue.notes ?? "", text].filter((entry) => entry !== "").join("\n");
      const updated: Issue = { ...issue, notes };
      issues.set(id, updated);
      return updated;
    },
    async setStatus(): Promise<Issue> {
      throw new Error("the splitter must not change issue status");
    },
    async closeIssue(): Promise<Issue> {
      throw new Error("the splitter must not close issues");
    },
    async remember(): Promise<void> {
      throw new Error("the splitter must not write memory — finalize owns that");
    },
    async recall(): Promise<string | null> {
      throw new Error("the splitter must not read memory");
    },
  };

  return {
    client,
    specs,
    calls,
    issue: (id) => issues.get(id),
    noteWrites: () => noteTexts.length,
    childCreates: () => specs.filter((spec) => spec.type !== "epic"),
    injectFailure: (match) => {
      failure = match;
    },
    createAttemptsFor: (title) => attempts.get(title) ?? 0,
  };
}

/** A well-formed raw issue as the wire would carry it. */
function rawIssue(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    title: "Wire the splitter",
    description: "Give SPLIT its module, built on the bd adapter.",
    acceptance_criteria: "typecheck clean; tests green",
    priority: 2,
    depends_on: [],
    ...overrides,
  };
}

function rawIssueWithout(field: string, overrides: Record<string, unknown> = {}): Record<string, unknown> {
  const base = rawIssue(overrides);
  delete base[field];
  return base;
}

function problemsFor(raw: unknown, config?: SplitValidationConfig): readonly SplitProblem[] {
  const result = validateSplitBatch(raw, config);
  assert.equal(result.ok, false, `expected a rejection, got: ${JSON.stringify(result)}`);
  return (result as { ok: false; problems: readonly SplitProblem[] }).problems;
}

function problemOn(
  raw: unknown,
  field: string,
  index = 0,
  config?: SplitValidationConfig,
): SplitProblem {
  const problems = problemsFor(raw, config);
  const found = problems.find((problem) => problem.field === field && problem.index === index);
  assert.ok(
    found !== undefined,
    `expected a problem on ${index === -1 ? "the batch" : `issue #${index}`} field ${field}; ` +
      `got: ${problems.map(formatSplitProblem).join(" | ")}`,
  );
  return found;
}

// ── the wire format ─────────────────────────────────────────────────────

test("a bare array of well-formed issues validates, with defaults reported", () => {
  const result = validateSplitBatch([rawIssue(), rawIssue({ title: "Second", depends_on: [0] })]);
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.equal(result.items.length, 2);
  assert.deepEqual(result.items[1]?.dependsOn, [0]);
  assert.deepEqual(result.items[0]?.filledFromDefaults, []);
});

test("a {issues: [...]} wrapper is accepted because models reach for it", () => {
  const result = validateSplitBatch({ issues: [rawIssue()] });
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.equal(result.items.length, 1);
});

test("field aliases are accepted: name, acceptance, acceptanceCriteria, deps, dependsOn", () => {
  const result = validateSplitBatch([
    rawIssue({ title: "anchor" }),
    rawIssueWithout("acceptance_criteria", { title: "alias 1", acceptance: "via alias" }),
    rawIssueWithout("acceptance_criteria", { title: "alias 2", acceptanceCriteria: "via camel alias" }),
    rawIssueWithout("depends_on", { title: "alias 3", deps: [0] }),
    rawIssueWithout("depends_on", { title: "alias 4", dependsOn: [0] }),
    rawIssueWithout("title", { name: "titled by the alias" }),
  ]);
  assert.equal(result.ok, true, JSON.stringify(result));
  if (!result.ok) return;
  assert.equal(result.items[1]?.acceptanceCriteria, "via alias");
  assert.equal(result.items[2]?.acceptanceCriteria, "via camel alias");
  assert.deepEqual(result.items[3]?.dependsOn, [0]);
  assert.deepEqual(result.items[4]?.dependsOn, [0]);
  assert.equal(result.items[5]?.title, "titled by the alias");
});

test("a batch with nothing readable in it is rejected as the batch, not as an empty success", () => {
  for (const raw of [{}, "hello", null, undefined, 42, [], { issues: {} }]) {
    const problems = problemsFor(raw);
    assert.equal(problems.length, 1);
    const problem = problems[0]!;
    assert.equal(problem.index, -1);
    assert.equal(problem.field, "batch");
    assert.match(problem.message, /^(is empty|expected an array of issues)/);
  }
});

// ── validation: every field, by name and by index ───────────────────────

test("an empty title is rejected by index and field", () => {
  const problem = problemOn([rawIssue({ title: "   " })], "title", 0);
  assert.match(problem.message, /must be a non-empty string/);
  assert.equal(problemOn([rawIssueWithout("title")], "title", 0).index, 0);
});

test("an empty acceptance criterion is rejected — a ticket that can never be checked off", () => {
  const problem = problemOn([rawIssue({ acceptance_criteria: "" })], "acceptance_criteria", 0);
  assert.match(problem.message, /must be a non-empty string/);
  assert.match(
    problemOn([rawIssueWithout("acceptance_criteria")], "acceptance_criteria", 0).message,
    /defaultAcceptance/,
  );
});

test("an empty description is rejected by index and field", () => {
  assert.equal(problemOn([rawIssue({ description: "  " })], "description", 0).index, 0);
  assert.equal(problemOn([rawIssueWithout("description")], "description", 0).index, 0);
});

test("priority must be present and an integer in 0..4", () => {
  assert.equal(problemOn([rawIssueWithout("priority")], "priority", 0).index, 0);
  assert.match(problemOn([rawIssue({ priority: 7 })], "priority", 0).message, /must be within 0\.\.4, got 7/);
  assert.match(problemOn([rawIssue({ priority: -1 })], "priority", 0).message, /must be within 0\.\.4, got -1/);
  assert.match(problemOn([rawIssue({ priority: 2.5 })], "priority", 0).message, /non-integer/);
  assert.match(problemOn([rawIssue({ priority: "high" })], "priority", 0).message, /must be the integer/);
  for (const priority of [0, 1, 2, 3, 4]) {
    const result = validateSplitBatch([rawIssue({ priority })]);
    assert.equal(result.ok, true, `priority ${priority} must be accepted`);
  }
});

test("depends_on must be an array of in-range indexes, never a self-edge", () => {
  assert.equal(problemOn([rawIssue({ depends_on: "0" })], "depends_on", 0).index, 0);
  assert.match(problemOn([rawIssue({ depends_on: [5] })], "depends_on", 0).message, /points at #5/);
  assert.match(problemOn([rawIssue({ depends_on: [1] })], "depends_on", 0).message, /points at #1/);
  assert.match(problemOn([rawIssue({ depends_on: [-1] })], "depends_on", 0).message, /not a batch index/);
  assert.match(
    problemOn([rawIssue({ depends_on: [0] })], "depends_on", 0).message,
    /lists itself/,
  );
  assert.match(
    problemOn([rawIssue(), rawIssue({ title: "b", depends_on: ["urgent"] })], "depends_on", 1).message,
    /not a batch index/,
  );
});

test("depends_on accepts integer, numeric-string and #token forms, and dedupes them", () => {
  const result = validateSplitBatch([
    rawIssue(),
    rawIssue({ title: "b", depends_on: ["0", 0, "#0"] }),
  ]);
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.deepEqual(result.items[1]?.dependsOn, [0]);
});

test("an unknown issue type is rejected rather than silently coerced", () => {
  assert.match(problemOn([rawIssue({ type: "epick" })], "type", 0).message, /is not a bd issue type/);
  const result = validateSplitBatch([rawIssue({ type: "feature" })]);
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.equal(result.items[0]?.type, "feature");
});

test("each issue is reported separately: three bad issues name three indexes", () => {
  const problems = problemsFor([
    rawIssue({ title: "" }),
    rawIssue({ priority: 9 }),
    rawIssue({ acceptance_criteria: "  " }),
  ]);
  assert.deepEqual(
    problems.map((problem) => `${problem.index}:${problem.field}`).sort(),
    ["0:title", "1:priority", "2:acceptance_criteria"],
  );
});

test("an array element that is not an object is reported as the issue itself", () => {
  const problems = problemsFor([rawIssue(), "just a sentence"]);
  const problem = problems.find((entry) => entry.field === "issue" && entry.index === 1);
  assert.ok(problem !== undefined, problems.map(formatSplitProblem).join(" | "));
});

test("cyclic dependencies are rejected with the cycle spelled out", () => {
  const problems = problemsFor([
    rawIssue({ title: "a", depends_on: [1] }),
    rawIssue({ title: "b", depends_on: [0] }),
  ]);
  const cycle = problems.find((problem) => problem.index === -1 && problem.field === "depends_on");
  assert.ok(cycle !== undefined, problems.map(formatSplitProblem).join(" | "));
  assert.match(cycle.message, /has a cycle: #0 → #1 → #0/);

  // A longer cycle is caught too.
  assert.equal(
    planSplitOrder([itemAt(0, [1]), itemAt(1, [2]), itemAt(2, [0])]).ok,
    false,
  );
});

/** A validated-looking SplitItem, built directly, for ledger-only tests. */
function itemAt(index: number, dependsOn: readonly number[] = []): SplitItem {
  return {
    title: `item ${index}`,
    description: `what item ${index} is for`,
    acceptanceCriteria: `how item ${index} is checked`,
    priority: 2,
    dependsOn,
    type: "task",
    filledFromDefaults: [],
  };
}

test("the over-split guard is a typed rejection with a number in it", () => {
  const three = [rawIssue(), rawIssue({ title: "b" }), rawIssue({ title: "c" })];
  const problem = problemOn(three, "batch", -1, { maxItems: 2 });
  assert.match(problem.message, /proposes 3 issue\(s\) for one request, over the limit of 2/);
  // The default is a guard rail, not a wall a normal request runs into.
  assert.equal(DEFAULT_MAX_ITEMS, 12);
  assert.equal(validateSplitBatch(three).ok, true);
});

test("defaults fill only what they are configured to fill", () => {
  const filled = validateSplitBatch([rawIssueWithout("priority"), rawIssueWithout("acceptance_criteria")], {
    defaultPriority: 1,
    defaultAcceptance: "Configured acceptance criterion",
  });
  assert.equal(filled.ok, true);
  if (!filled.ok) return;
  assert.equal(filled.items[0]?.priority, 1);
  assert.equal(filled.items[1]?.acceptanceCriteria, "Configured acceptance criterion");
  assert.deepEqual(filled.items[0]?.filledFromDefaults, ["priority"]);
  assert.deepEqual(filled.items[1]?.filledFromDefaults, ["acceptance_criteria"]);

  // An omitted priority with no configured default is still a rejection.
  assert.equal(validateSplitBatch([rawIssueWithout("priority")]).ok, false);
});

test("every accepted issue has a non-empty title, description and acceptance criterion", () => {
  const result = validateSplitBatch([rawIssue(), rawIssue({ title: "b", depends_on: [0] })]);
  assert.equal(result.ok, true);
  if (!result.ok) return;
  for (const item of result.items) {
    assert.ok(item.title.trim().length > 0);
    assert.ok(item.description.trim().length > 0);
    assert.ok(item.acceptanceCriteria.trim().length > 0);
    assert.ok(Number.isInteger(item.priority));
    assert.ok(item.priority >= 0 && item.priority <= 4);
  }
});

// ── empty input never reaches anything ────────────────────────────────

test("empty or whitespace-only input never reaches the agent and writes nothing", async () => {
  for (const text of ["", "   ", "\n\t  \n  "]) {
    const agent = fakeAgent([[rawIssue()]]);
    const board = fakeBoard("empty");
    const splitter = createSplitter({ agent: agent.port, beads: board.client });

    const outcome = await splitter.split(text);

    assert.equal(outcome.kind, "empty-input");
    assert.equal(agent.calls(), 0, "the model must not be consulted for nothing");
    assert.equal(board.calls.length, 0, "nothing may be written for nothing");
    assert.equal(isSplitSuccess(outcome), false);
    assert.deepEqual(createdIds(outcome), []);
    assert.match(describeSplitFailure(outcome), /nothing to split/);

    const proposal = await splitter.propose(text);
    assert.equal(proposal.ok, false);
    assert.equal(proposal.attempts, 0);
    assert.equal(agent.calls(), 0);
  }
});

// ── one small request, one issue ──────────────────────────────────────

test("a single small request yields exactly one issue, not a spray", async () => {
  const agent = fakeAgent([[rawIssue({ title: "Fix the typo in the README" })]]);
  const board = fakeBoard("single");
  const splitter = createSplitter({ agent: agent.port, beads: board.client });

  const outcome = await splitter.split("fix the typo in the README");

  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
  assert.equal(outcome.created.length, 1);
  assert.equal(board.childCreates().length, 1);
  assert.equal(agent.calls(), 1);
  assert.equal(outcome.created[0]?.title, "Fix the typo in the README");
  // The only other create is the epic that carries the request verbatim.
  assert.equal(board.specs.length, 2);
  assert.equal(board.specs[0]?.type, "epic");
});

test("over-splitting one small request is a typed rejection with no board writes", async () => {
  const spray = Array.from({ length: 13 }, (_unused, index) =>
    rawIssue({ title: `Piece ${index}` }),
  );
  const agent = fakeAgent([spray]);
  const board = fakeBoard("spray");

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split(
    "fix the typo in the README",
  );

  assert.equal(outcome.kind, "invalid-batch");
  assert.equal(agent.calls(), 2, "one retry is allowed, then it stops");
  assert.equal(board.calls.length, 0, "a rejected split writes nothing — not even the epic");
  assert.match(describeSplitFailure(outcome), /over the limit of 12/);
});

// ── the retry: exactly once, with the evidence ────────────────────────

const BAD_JSON = '{ "issues": [ { "title": "unterminated';

test("unreadable JSON is retried exactly once, quoting the parse error and the request", async () => {
  const input = "add a retry for flaky tests";
  const agent = fakeAgent([BAD_JSON, BAD_JSON]);
  const board = fakeBoard("retry");

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split(input);

  assert.equal(outcome.kind, "invalid-batch");
  assert.equal(agent.calls(), 2, "exactly one retry, no more");

  const second = agent.requests[1]!;
  const first = agent.requests[0]!;
  assert.equal(first, input, "the first attempt is the human's text, unmodified");
  assert.match(second, /could not be read as JSON/);
  assert.ok(second.includes(input), "the retry carries the original request unchanged");
  assert.ok(second.includes(BAD_JSON), "the retry quotes what came back last time");

  // Both raw answers are carried out, so the failure is diagnosable.
  assert.deepEqual(outcome.rawOutputs, [BAD_JSON, BAD_JSON]);
  assert.equal(board.calls.length, 0);
});

test("buildRetryRequest carries the problems, the previous output and the request", () => {
  const problems = problemsFor([rawIssue({ title: "", priority: 9 })]);
  const request = buildRetryRequest("fix the typo in the README", problems, BAD_JSON);

  assert.ok(request.includes("fix the typo in the README"), "the human's text is carried through");
  assert.match(request, /`title`: must be a non-empty string/);
  assert.match(request, /`priority`: must be within 0\.\.4, got 9/);
  assert.ok(request.includes(BAD_JSON), "the rejected output is quoted back");

  // Without a previous output the text still stands on its own.
  const bare = buildRetryRequest("go", problems);
  assert.ok(bare.includes("go"));
  assert.equal(bare.includes(BAD_JSON), false);
});

test("a valid second attempt is taken", async () => {
  const agent = fakeAgent([BAD_JSON, [rawIssue({ title: "Retry landed" })]]);
  const board = fakeBoard("retryok");

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split("go");

  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
  assert.equal(agent.calls(), 2);
  assert.equal(outcome.created.length, 1);
});

test("the retry budget is the config it claims to be", async () => {
  const twoTries = await createSplitter(
    { agent: fakeAgent([BAD_JSON]).port, beads: fakeBoard("b1").client },
    { retryBudget: 2 },
  ).split("go");
  assert.equal(twoTries.kind, "invalid-batch");
  assert.equal(twoTries.attempts, 3);

  const noTries = await createSplitter(
    { agent: fakeAgent([BAD_JSON]).port, beads: fakeBoard("b0").client },
    { retryBudget: 0 },
  ).split("go");
  assert.equal(noTries.attempts, 1);
});

test("a broken channel is not retried as if it were a bad answer", async () => {
  const agent = fakeAgent([
    () => {
      throw new Error("connection reset by peer");
    },
  ]);
  const board = fakeBoard("channel");

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split("go");

  assert.equal(outcome.kind, "model-error");
  assert.equal(agent.calls(), 1, "a dead channel is repeated into a stall, not a fix");
  assert.equal(board.calls.length, 0);
  assert.match(outcome.message, /connection reset/);
});

test("a verdict-shaped failure from the runner is read as a bad answer, and retried", async () => {
  // `.5` reports a rejected `report_split` payload this way, with the raw block
  // attached. That is the model being wrong, which is retryable once.
  const verdictError = () => {
    const error = new Error("report_split was called with a payload that is not a valid batch");
    error.name = "AgentError";
    Object.assign(error, { kind: "malformed-verdict", detail: BAD_JSON });
    throw error;
  };
  const agent = fakeAgent([verdictError, [rawIssue({ title: "Recovered" })]]);
  const board = fakeBoard("verdict");

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split("go");

  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
  assert.equal(agent.calls(), 2);
  assert.match(agent.requests[1]!, /could not be read as JSON/);
  assert.ok(agent.requests[1]!.includes(BAD_JSON));
});

test("an unwired port fails as a model error rather than an empty success", async () => {
  const board = fakeBoard("unwired");
  const outcome = await createSplitter({ agent: unavailableSplitPort(), beads: board.client }).split(
    "go",
  );
  assert.equal(outcome.kind, "model-error");
  assert.match(outcome.message, /no split agent is wired/);
  assert.equal(board.calls.length, 0);
});

// ── local indexes become real ids ───────────────────────────────────

test("batch-local depends_on become real bd ids, read back through the adapter", async () => {
  const agent = fakeAgent([
    [
      rawIssue({ title: "A" }),
      rawIssue({ title: "B", depends_on: [0] }),
      rawIssue({ title: "C", depends_on: [0, 1] }),
    ],
  ]);
  const board = fakeBoard("idx");

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split(
    "A, then B, then C",
  );

  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
  const [a, b, c] = outcome.created;
  assert.ok(a && b && c);

  // No index token survives into a created spec: every dep is a real id.
  for (const spec of board.childCreates()) {
    for (const dep of spec.deps ?? []) {
      assert.equal(dep.includes("#"), false, `index token leaked into ${spec.title}: ${dep}`);
      assert.match(dep, /^idx-\d+$/, `dep ${dep} is not a created id`);
    }
  }

  // Read back the way the rest of the app reads dependencies.
  const issueB = board.issue(b.id);
  const issueC = board.issue(c.id);
  assert.ok(issueB && issueC);
  assert.equal(dependsOn(issueB, a.id), true);
  assert.equal(dependsOn(issueC, a.id), true);
  assert.equal(dependsOn(issueC, b.id), true);
  assert.deepEqual(normaliseDependencies(board.issue(a.id)!), []);

  // And dependencies were created before their dependents.
  const order = board.calls.filter((call) => call.startsWith("createIssue:"));
  assert.ok(
    order.indexOf(`createIssue:${a.title}`) < order.indexOf(`createIssue:${b.title}`),
    order.join(" | "),
  );
  assert.ok(
    order.indexOf(`createIssue:${b.title}`) < order.indexOf(`createIssue:${c.title}`),
    order.join(" | "),
  );
});

test("a dependent asked for first drags its dependency in ahead of it", async () => {
  const items = [itemAt(0, [1]), itemAt(1)];
  const board = fakeBoard("lazy");
  const ledger = createSplitLedger({ items, beads: board.client });

  // Ask for the dependent before the dependency.
  const first = await ledger.ensure(0);
  assert.ok(first);
  assert.equal(board.childCreates()[0]?.title, "item 1", "the dependency goes first");
  assert.equal(dependsOn(board.issue(first.id)!, ledger.idOf(1)!), true);
  assert.equal(board.childCreates().length, 2, "the dependency was not created twice");

  // A second ask is a no-op.
  const again = await ledger.ensure(1);
  assert.equal(again?.id, ledger.idOf(1));
  assert.equal(board.childCreates().length, 2);
});

test("a spec can be handed back to the ledger by identity, and only its own kind is", async () => {
  const items = [itemAt(0), itemAt(1, [0])];
  const specs = toNewIssueSpecs(items, null);
  assert.equal(indexForSpec(specs[1]!), 1);
  assert.equal(indexForSpec({ title: "stranger" }), null);

  const board = fakeBoard("spec");
  const ledger = createSplitLedger({ items, beads: board.client });

  const created = await ledger.createForSpec(specs[1]!);
  assert.ok(created);
  assert.equal(created.index, 1);
  assert.equal(dependsOn(board.issue(created.id)!, ledger.idOf(0)!), true);

  await assert.rejects(
    () => ledger.createForSpec({ title: "stranger" }),
    (error: unknown) => error instanceof SplitError && error.kind === "unknown-spec",
    "a spec the ledger did not mint must be refused, not guessed at",
  );
  // Only the two real items were created — the stranger never got near bd.
  assert.equal(board.childCreates().length, 2);
});

test("toNewIssueSpec resolves #tokens to the ids the caller supplies", () => {
  const spec = toNewIssueSpec(itemAt(1, [0, 2]), "epic-9", ["real-0", "real-2"]);
  assert.equal(spec.parent, "epic-9");
  assert.deepEqual(spec.deps, ["real-0", "real-2"]);
  assert.equal(spec.acceptance, "how item 1 is checked");
  assert.equal(spec.priority, 2);
  assert.equal(toNewIssueSpec(itemAt(0), null, []).parent, undefined);
});

// ── partial creation ─────────────────────────────────────────────────

test("a mid-batch failure reports what exists and never touches what cannot", async () => {
  const board = fakeBoard("partial");
  board.injectFailure((spec) =>
    spec.title === "item 1"
      ? new BdError({ kind: "exit-1", message: "bd: database is locked" })
      : null,
  );
  const agent = fakeAgent([[rawIssue({ title: "item 0" }), rawIssue({ title: "item 1" }), rawIssue({ title: "item 2", depends_on: [1] })]]);

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split("three steps");

  assert.equal(outcome.kind, "partial");
  assert.equal(isSplitSuccess(outcome), false, "a partial is not a success");
  assert.deepEqual(outcome.created.map((entry) => entry.title), ["item 0"]);
  assert.deepEqual(outcome.failed.map((entry) => entry.title), ["item 1"]);
  assert.equal(outcome.failed[0]?.errorKind, "exit-1", "the bd failure kind is preserved");
  assert.deepEqual(outcome.skipped.map((entry) => `${entry.title}:${entry.missing.join(",")}`), [
    "item 2:1",
  ]);
  assert.deepEqual(createdIds(outcome), outcome.created.map((entry) => entry.id));
  assert.match(describeSplitFailure(outcome), /1 created, 1 failed, 1 not attempted/);

  assert.equal(board.createAttemptsFor("item 1"), 1, "a failed create is not blindly retried");
  assert.equal(board.createAttemptsFor("item 2"), 0, "its dependent is not attempted at all");
});

test("a guard mismatch is reported, never retried", async () => {
  const board = fakeBoard("guard");
  board.injectFailure((spec) =>
    spec.title === "item 0"
      ? new BdError({ kind: "guard-mismatch", message: "stale --if-status: precondition failed" })
      : null,
  );
  const agent = fakeAgent([[rawIssue({ title: "item 0" }), rawIssue({ title: "item 1", depends_on: [0] })]]);

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split("two steps");

  assert.equal(outcome.kind, "partial");
  assert.equal(outcome.failed[0]?.errorKind, "guard-mismatch");
  assert.equal(board.createAttemptsFor("item 0"), 1, "a guard mismatch means the world moved; hammering it is worse");
  assert.equal(board.createAttemptsFor("item 1"), 0);
});

test("every create failing is still a structured partial, not a bare throw", async () => {
  const board = fakeBoard("allfail");
  board.injectFailure((spec) =>
    spec.type === "epic" ? null : new BdError({ kind: "exit-1", message: "no writes permitted" }),
  );
  const agent = fakeAgent([[rawIssue({ title: "item 0" }), rawIssue({ title: "item 1" })]]);

  const outcome = await createSplitter({ agent: agent.port, beads: board.client }).split("two steps");

  assert.equal(outcome.kind, "partial");
  assert.deepEqual(outcome.created, []);
  assert.equal(outcome.failed.length, 2);
  assert.deepEqual(createdIds(outcome), []);
});

// ── the epic, and the request on it ──────────────────────────────────

test("a configured epic gets the request appended verbatim, and owns the children", async () => {
  const board = fakeBoard("epic");
  const epic = await board.client.createIssue({ title: "Loop epic", type: "epic" });
  const input = "Add a retry for flaky tests, then document it in the README.";

  const outcome = await createSplitter(
    {
      agent: fakeAgent([[rawIssue({ title: "Implement retry" })]]).port,
      beads: board.client,
    },
    { epicId: epic.id },
  ).split(input);

  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
  assert.equal(outcome.epic.epicId, epic.id);
  assert.equal(outcome.epic.mode, "existing");
  assert.equal(outcome.epic.record, verbatimRecord(input));

  const notes = board.issue(epic.id)?.notes ?? "";
  assert.ok(notes.includes(VERBATIM_MARKER), notes);
  assert.ok(notes.endsWith(input), "the note ends with the request, byte for byte");
  assert.equal(board.noteWrites(), 1);

  // No second epic; the children point at the one that was given.
  assert.equal(board.specs.filter((spec) => spec.type === "epic").length, 1);
  assert.equal(board.childCreates()[0]?.parent, epic.id);
});

test("with no epic configured, one is created carrying the request verbatim", async () => {
  const board = fakeBoard("newepic");
  const input = "Ship the splitter."
  const outcome = await createSplitter({
    agent: fakeAgent([[rawIssue({ title: "Implement it" })]]).port,
    beads: board.client,
  }).split(input);

  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
  assert.equal(outcome.epic.mode, "created");
  assert.equal(outcome.epic.record, verbatimRecord(input));
  assert.equal(outcome.epic.landedIn, "description");

  const epicSpec = board.specs.find((spec) => spec.type === "epic");
  assert.ok(epicSpec, "an epic should have been created");
  assert.equal(epicSpec.description, verbatimRecord(input));
  assert.ok(epicSpec.title && epicSpec.title.length <= 72, `epic title too long: ${epicSpec.title}`);
  assert.equal(board.childCreates()[0]?.parent, outcome.epic.epicId);
});

test("verbatim means verbatim: unicode, backticks, quotes, newlines, trailing spaces", () => {
  const input = " _split café \"üñí\" `code` first line\nsecond line   \n";
  const record = verbatimRecord(input);
  assert.equal(record, `${VERBATIM_MARKER}\n${input}`);
  assert.ok(record.endsWith(input), "nothing is trimmed off the request");
  assert.equal(record.split("\n").length, 4);

  assert.equal(epicTitleFor(input).length <= 72, true);
  assert.match(epicTitleFor("   \t\n  "), /^Human request/);
});

test("if the request cannot be recorded, no children are created", async () => {
  const board = fakeBoard("epicfail");
  const outcome = await createSplitter({
    agent: fakeAgent([[rawIssue({ title: "Anything" })]]).port,
    beads: board.client,
  }, { epicId: "does-not-exist" }).split("record me please");

  assert.equal(outcome.kind, "epic-failed");
  assert.equal(outcome.errorKind, "exit-1");
  assert.ok(
    board.calls.includes("appendNote:does-not-exist"),
    `the append should have been attempted; calls: ${board.calls.join(" | ")}`,
  );
  assert.equal(board.childCreates().length, 0, "an unrecorded request must not spawn children");
  assert.match(describeSplitFailure(outcome), /No child issues were created/);
});

test("recordHumanRequest is usable on its own, with the same rules", async () => {
  const board = fakeBoard("standalone");
  const epic = await board.client.createIssue({ title: "E", type: "epic" });

  const reused = await recordHumanRequest(board.client, "second request", { epicId: epic.id });
  assert.equal(reused.epicId, epic.id);
  assert.equal(reused.mode, "existing");
  assert.equal(reused.landedIn, "notes");

  const made = await recordHumanRequest(board.client, "brand new request");
  assert.notEqual(made.epicId, epic.id);
  assert.equal(board.issue(made.epicId)?.description, verbatimRecord("brand new request"));

  await assert.rejects(
    () => recordHumanRequest(board.client, "  "),
    (error: unknown) => error instanceof SplitError && error.kind === "empty-input",
  );
});

// ── events, and the state machine that consumes them ────────────────

test("toSplitEvent maps only a clean create onto split_created", () => {
  const created = { kind: "created", created: [{ index: 0, id: "x-1", title: "A" }] } as unknown as SplitOutcome;
  assert.deepEqual(toSplitEvent(created), { type: "split_created", createdIds: ["x-1"] });

  const partial = {
    kind: "partial",
    created: [{ index: 0, id: "x-1", title: "A" }],
    failed: [{ index: 1, title: "B", message: "boom", errorKind: "exit-1" }],
    skipped: [],
  } as unknown as SplitOutcome;
  const failure = toSplitEvent(partial);
  assert.equal(failure.type, "split_failed");
  assert.deepEqual((failure as { createdIds?: string[] }).createdIds, ["x-1"]);
  assert.match((failure as { reason: string }).reason, /1 created, 1 failed/);

  const empty = { kind: "empty-input" } as unknown as SplitOutcome;
  assert.deepEqual(toSplitEvent(empty), {
    type: "split_failed",
    reason: "nothing to split: the request was empty",
    createdIds: [],
  });
});

test("the orchestrator accepts the splitter's events where the machine expects them", async () => {
  const input = "add a retry for flaky tests";
  const agent = fakeAgent([[rawIssue({ title: "Retry flaky tests" }), rawIssue({ title: "Document it", depends_on: [0] })]]);
  const board = fakeBoard("machine");
  const splitter = createSplitter({ agent: agent.port, beads: board.client });

  let state = step(createInitialState(), { type: "start" }).state;
  state = step(state, { type: "board_observed", inProgress: [], ready: [] }).state;
  assert.equal(state.name, "idle");

  const asked = step(state, { type: "human_input", text: input });
  assert.equal(asked.applied, true);
  assert.equal(asked.state.splitStage, "propose");

  const proposal = await splitter.propose(input);
  assert.equal(proposal.ok, true);
  const proposed = step(asked.state, { type: "split_proposed", specs: proposal.specs });
  assert.equal(proposed.applied, true);
  assert.equal(proposed.state.splitStage, "create");

  const outcome = await splitter.split(input);
  assert.equal(outcome.kind, "created", describeSplitFailure(outcome));

  const landed = step(proposed.state, toSplitEvent(outcome));
  assert.equal(landed.applied, true, JSON.stringify((landed as { rejection?: unknown }).rejection));
  assert.equal(createdIds(outcome).length, 2);
  // The split crosses the cold boundary, which clears what it carried — the count
  // is what survives, in the message the human sees.
  assert.equal(landed.state.name, "check_work");
  assert.equal(landed.state.iteration, asked.state.iteration + 1);
  assert.ok(
    landed.effects.some(
      (effect) => effect.kind === "ui.say" && /into 2 issue\(s\)/.test(effect.text ?? ""),
    ),
    JSON.stringify(landed.effects),
  );
});

// ── source guards ──────────────────────────────────────────────────

/** Comments out, so a guard cannot be satisfied by prose about the thing it bans. */
function codeOnly(source: string): string {
  const withoutBlock = source.replace(/\/\*[\s\S]*?\*\//g, "");
  return withoutBlock
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
}

test("the splitter has no claim path, no memory writes, and no process or model access", () => {
  const source = codeOnly(readFileSync(new URL("../src/split.ts", import.meta.url), "utf8"));

  for (const banned of [
    "--assignee",
    "--claim",
    "assignee:",
    "bd remember",
    ".remember(",
    "child_process",
    "execFile",
    "spawn",
    "createAgentSession",
    "@mariozechner/pi-ai",
    "openai",
    "anthropic",
    "fetch(",
  ]) {
    assert.equal(source.includes(banned), false, `src/split.ts must not contain ${banned}`);
  }

  // It reaches bd only through the adapter, and the machine only through its types.
  assert.match(source, /from "\.\/beads\.js"/);
  assert.match(source, /from "\.\/orchestrator\.js"/);
});

test("portFromAgentRunner wires a .5-shaped runner without importing it", async () => {
  const seen: string[] = [];
  const runner = {
    async split(text: string) {
      seen.push(text);
      return [{ title: "from the runner", description: "d", acceptance: "a", priority: 1 }] as const;
    },
  };
  const port = portFromAgentRunner(runner);
  const value = (await port.propose("go")) as NewIssueSpec[];
  assert.deepEqual(seen, ["go"]);
  const validated = validateSplitBatch(value);
  assert.equal(validated.ok, true, JSON.stringify(validated));
  assert.equal(validated.items[0]?.title, "from the runner");
});

test("a .5 report_split payload feeds .7's validator without losing the ordering", () => {
  const payload = {
    issues: [
      { title: "Add the retry", description: "d", acceptance: "a", priority: 1, depends_on: [] },
      { title: "Document it", description: "d2", acceptance: "a2", priority: 2, depends_on: [0] },
      { title: "Depend on a string", description: "d3", acceptance: "a3", priority: 3, depends_on: ["1"] },
    ],
  };

  const halfFive = validateSplitPayload(payload);
  assert.equal(halfFive.ok, true, JSON.stringify(halfFive));
  if (!halfFive.ok) return;
  assert.deepEqual(halfFive.specs[1]?.deps, ["0"]);
  assert.deepEqual(halfFive.specs[2]?.deps, ["1"]);

  const validated = validateSplitBatch(halfFive.specs);
  assert.equal(validated.ok, true, JSON.stringify(validated));
  if (!validated.ok) return;
  assert.deepEqual(
    validated.items.map((item) => item.dependsOn),
    [[], [0], [1]],
  );
});

test("a nonsense dependency is caught by split's validator, not lost in the runner", () => {
  // `.5` keeps the payload honest but does not interpret it; `src/split.ts` is
  // the authority on what a split means. A string that is not a position gets
  // through the runner and is refused here, by field and index — never quietly
  // turned into "no ordering required".
  const passedThrough = validateSplitPayload({
    issues: [
      { title: "First", description: "d", acceptance: "a", priority: 1 },
      { title: "Second", description: "d", acceptance: "a", priority: 2, depends_on: ["first"] },
    ],
  });
  assert.equal(passedThrough.ok, true, JSON.stringify(passedThrough));

  const problem = problemOn(passedThrough.specs, "depends_on", 1);
  assert.match(problem.message, /not a batch index/);

  // A structurally impossible entry is refused earlier, by the runner.
  assert.equal(
    validateSplitPayload({ issues: [{ title: "x", depends_on: [{ nope: true }] }] }).ok,
    false,
  );
});

test("the split prompt teaches the model the index contract", () => {
  const prompt = buildSplitPrompt("add a retry for flaky tests, then document it");
  assert.match(prompt, /depends_on/);
  assert.match(prompt, /0-based/);
  assert.ok(prompt.includes("add a retry for flaky tests, then document it"));
});

// ── a real bd, in a scratch database ────────────────────────────────
//
// The fakes prove the contract; this proves the contract against the real binary.
// It runs in a throwaway BEADS_DIR, so the project board is untouched — and if
// `bd` is absent the test skips rather than pretending it ran.

function bdAvailable(): boolean {
  const result = spawnSync("bd", ["version"], { encoding: "utf8" });
  return !result.error && result.status === 0;
}

test("against real bd: indexes become real ids, the epic holds the request verbatim", async (t) => {
  if (!bdAvailable()) {
    t.skip("bd is not installed");
    return;
  }

  const root = mkdtempSync(join(tmpdir(), "loop-split-live-"));
  const beadsDir = join(root, ".beads");
  const env = { BEADS_DIR: beadsDir };
  const init = spawnSync("bd", ["init", "--prefix", "splittest", "--non-interactive"], {
    cwd: root,
    env: { ...process.env, ...env },
    encoding: "utf8",
  });
  assert.equal(init.status, 0, `bd init failed: ${init.stderr}`);

  try {
    const client = createBdClient({ cwd: root, env });
    const input = "add a retry for flaky tests, then document it";
    const splitter = createSplitter(
      {
        agent: fakeAgent([
          [
            rawIssue({ title: "Implement retry" }),
            rawIssue({ title: "Document the retry", depends_on: [0] }),
          ],
        ]).port,
        beads: client,
      },
    );

    const outcome = await splitter.split(input);
    assert.equal(outcome.kind, "created", describeSplitFailure(outcome));
    const ids = createdIds(outcome);
    assert.equal(ids.length, 2);

    const epic = await client.getIssue(outcome.kind === "created" ? outcome.epic.epicId : "");
    assert.ok(epic, "the epic must exist in the real database");
    assert.ok(
      (epic.description ?? "").endsWith(input),
      `epic description must end with the request verbatim: ${JSON.stringify(epic.description)}`,
    );

    const [first, second] = [
      await client.getIssue(ids[0]!),
      await client.getIssue(ids[1]!),
    ];
    assert.ok(first && second, "both children must exist");

    // Every created issue: non-empty acceptance, sane priority, parented.
    for (const issue of [first, second]) {
      assert.ok((issue!.acceptance_criteria ?? "").trim().length > 0, "acceptance must be stored");
      assert.ok(issue!.priority >= 0 && issue!.priority <= 4);
    }

    // The intra-batch dependency survived the round trip, read the way the app
    // reads it — not by inspecting raw JSON.
    assert.equal(dependsOn(second, first.id), true);

    // Real `bd` records the parent link as a dependency of the child, so the
    // only other thing allowed in here is the epic: no invented edges.
    const epicId = outcome.kind === "created" ? outcome.epic.epicId : "";
    const extraEdges = (issue: Issue) =>
      normaliseDependencies(issue).map((dep) => dep.id).filter((id) => id !== epicId);
    assert.deepEqual(extraEdges(first), []);
    assert.deepEqual(extraEdges(second), [first.id]);

    for (const dep of normaliseDependencies(second)) {
      assert.equal(dep.id.includes("#"), false);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});


