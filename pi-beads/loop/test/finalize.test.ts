/**
 * Tests for `src/finalize.ts` (the ritual) and `src/vcs.ts` (the git writer).
 *
 * The interesting thing about a finalize step is not the happy path — it is
 * what the world looks like when it stops halfway. So the fakes here are built
 * around a single shared **order log**: every module that can change the outside
 * world writes into one array, which makes "closed before it was committed"
 * impossible to hide and "nothing was written after the failure" directly
 * observable.
 *
 * Where a claim can be checked against real git or real `bd`, it is: staging
 * discipline, hook rejection, empty-`HOME` identity, and the whole commit →
 * memory → close chain are run against real binaries in throwaway directories.
 * Numbered rules refer to the acceptance criteria on `workspace-5yn.8`.
 */

import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  BdError,
  createBdClient,
  type BdClient,
  type IssueStatus,
  type NewIssueSpec,
} from "../src/beads.js";
import {
  createInitialState,
  handoffKeyFor,
  step,
  type OrchestratorEvent,
  type StepResult,
  type OrchestratorState,
} from "../src/orchestrator.js";
import {
  COMMIT_TRAILER,
  createDefaultGitRunner,
  createGitWriter,
  VcsError,
  type CommitBlock,
  type CommitPlan,
  type GitWriter,
} from "../src/vcs.js";
import {
  createFinalizer,
  describeFinalizeFailure,
  isFinalized,
  renderCommitMessage,
  renderHandoff,
  toFinalizeEvents,
  UNRESOLVED_HASH,
  validateFinalizeRequest,
  type FinalizeRequest,
} from "../src/finalize.js";

// ── fixtures ────────────────────────────────────────────────────────────────

function request(overrides: Partial<FinalizeRequest> = {}): FinalizeRequest {
  return {
    issueId: "loop-1",
    title: "Teach the loop to close its own beads",
    summary: "Added the finalize ritual, its ordering rules and its tests.",
    changedFiles: ["src/finalize.ts", "test/finalize.test.ts"],
    nextSteps: ["wire it into the interpreter (.9)"],
    decisions: ["close last, because it is the claim of completion"],
    ...overrides,
  };
}

interface BoardCalls {
  readonly remembered: { text: string; key: string }[];
  readonly closed: { id: string; reason?: string }[];
}

interface BoardOptions {
  calls: BoardCalls;
  rememberError?: Error | null;
  closeError?: Error | null;
  closeStatus?: IssueStatus;
}

/**
 * A board that records what it was asked to do, in order, into the shared
 * `order` array. Everything the finalizer has no business doing throws, so a
 * green test cannot have quietly poked the tracker some other way.
 */
function fakeBoard(order: string[], options: BoardOptions): BdClient {
  const { calls } = options;
  return {
    async listReady(): Promise<never> {
      throw new Error("finalize must not read the ready list");
    },
    async listInProgress(): Promise<never> {
      throw new Error("finalize must not read the in-progress list");
    },
    async getIssue(): Promise<never> {
      throw new Error("finalize must not read issues; it is handed the verdict");
    },
    async createIssue(): Promise<never> {
      throw new Error("finalize must not create issues");
    },
    async addDep(): Promise<never> {
      throw new Error("finalize must not touch dependencies");
    },
    async appendNote(): Promise<never> {
      throw new Error("finalize writes one keyed handoff, not notes");
    },
    async setStatus(): Promise<never> {
      throw new Error("finalize closes with `bd close`, not by rewriting status");
    },
    async closeIssue(id: string, reason?: string) {
      order.push("close");
      if (options.closeError) throw options.closeError;
      calls.closed.push({ id, reason });
      return {
        id,
        title: "finalized",
        description: "",
        acceptance_criteria: "",
        status: options.closeStatus ?? "closed",
        priority: 1,
        issue_type: "task",
      };
    },
    async remember(text: string, key?: string): Promise<void> {
      order.push("remember");
      if (options.rememberError) throw options.rememberError;
      calls.remembered.push({ text, key: key ?? "no-key" });
    },
    async recall(): Promise<never> {
      throw new Error("finalize never reads memory back");
    },
  };
}

/**
 * Wrap a real bd client so a live test can see the order the real calls went
 * out in, without changing what those calls do.
 */
function recordBoard(order: string[], inner: BdClient): BdClient {
  return {
    listReady: (options) => inner.listReady(options),
    listInProgress: (options) => inner.listInProgress(options),
    getIssue: (id) => inner.getIssue(id),
    createIssue: (spec) => inner.createIssue(spec),
    addDep: (id, dependsOnId, type) => inner.addDep(id, dependsOnId, type),
    appendNote: (id, text) => inner.appendNote(id, text),
    setStatus: (id, status, options) => inner.setStatus(id, status, options),
    async closeIssue(id: string, reason?: string) {
      order.push("close");
      return inner.closeIssue(id, reason);
    },
    async remember(text: string, key?: string): Promise<void> {
      order.push("remember");
      await inner.remember(text, key);
    },
    recall: (key) => inner.recall(key),
  };
}

/** Same idea for git: a pass-through that notes when the write phase ran. */
function recordWriter(order: string[], inner: GitWriter): GitWriter {
  return {
    repoRoot: () => inner.repoRoot(),
    async planCommit(message: string, paths: readonly string[]) {
      order.push("plan");
      return inner.planCommit(message, paths);
    },
    async execute(plan: CommitPlan) {
      order.push("commit");
      return inner.execute(plan);
    },
    findCommitByTrailer: (value) => inner.findCommitByTrailer(value),
    commitPaths: (hash) => inner.commitPaths(hash),
    headHash: () => inner.headHash(),
  };
}

interface VcsCalls {
  readonly plans: CommitPlan[];
  readonly trailer: string[];
  readonly pathReads: string[];
}

interface VcsOptions {
  calls: VcsCalls;
  root?: string;
  stageable?: string[];
  skipped?: { path: string; reason: string }[];
  refused?: { path: string; reason: string }[];
  foreign?: string[];
  /** Explicit override; when omitted the block is derived like the real writer. */
  blocking?: CommitBlock | null;
  planError?: Error | null;
  commitError?: Error | null;
  hash?: string;
  committedPaths?: string[];
  trailerHit?: string | null;
  pathsByHash?: Record<string, readonly string[]>;
}

function derivedBlock(input: {
  refused: readonly { path: string; reason: string }[];
  stageable: readonly string[];
  foreign: readonly string[];
}): CommitBlock | null {
  if (input.refused.length > 0) {
    return { kind: "unsafe-path", message: "refused", paths: input.refused.map((r) => r.path) };
  }
  if (input.stageable.length === 0) {
    return { kind: "nothing-to-commit", message: "nothing to stage" };
  }
  if (input.foreign.length > 0) {
    return { kind: "unrelated-staged", message: "foreign staged", paths: input.foreign };
  }
  return null;
}

/** A git writer that records into `order` and obeys scripted outcomes. */
function fakeVcs(order: string[], options: VcsOptions): GitWriter {
  const { calls } = options;
  const root = options.root ?? "/repo";
  const stageable = options.stageable ?? ["src/a.ts"];
  const refused = options.refused ?? [];
  const foreign = options.foreign ?? [];
  const blocking =
    options.blocking !== undefined
      ? options.blocking
      : derivedBlock({ refused, stageable, foreign });
  const writes =
    blocking === null
      ? [
          ...stageable.map((path) => ({
            phase: "write" as const,
            argv: ["add", "--", path],
            note: `stage ${path}`,
          })),
          {
            phase: "write" as const,
            argv: [
              "-c",
              "user.name=pi-loop",
              "-c",
              "user.email=pi-loop@localhost",
              "commit",
              "-m",
              "<commit-message>",
            ],
            note: "commit with an explicit identity",
          },
        ]
      : [];

  return {
    async repoRoot(): Promise<string> {
      return root;
    },
    async planCommit(message: string, paths: readonly string[]): Promise<CommitPlan> {
      order.push("plan");
      if (options.planError) throw options.planError;
      assert.ok(Array.isArray(paths), "the plan is built from the reported paths");
      const plan: CommitPlan = {
        commands: [
          { phase: "read", argv: ["rev-parse", "--show-toplevel"], note: "root" },
          ...writes,
        ],
        stageable,
        skipped: options.skipped ?? [],
        refused,
        preexistingStaged: foreign,
        blocking,
        strictness: "strict",
        repoRoot: root,
        commitMessage: message,
      };
      calls.plans.push(plan);
      return plan;
    },
    async execute(plan: CommitPlan) {
      order.push("commit");
      if (options.commitError) throw options.commitError;
      return {
        hash: options.hash ?? "abc123def4567890",
        paths: options.committedPaths ?? [...plan.stageable],
        exact: true,
      };
    },
    async findCommitByTrailer(value: string): Promise<string | null> {
      order.push("trailer");
      calls.trailer.push(value);
      return options.trailerHit ?? null;
    },
    async commitPaths(hash: string): Promise<string[]> {
      order.push("commitPaths");
      calls.pathReads.push(hash);
      return [...(options.pathsByHash?.[hash] ?? [])];
    },
    async headHash(): Promise<string | null> {
      return options.hash ?? "abc123def4567890";
    },
  };
}

// ── real git ────────────────────────────────────────────────────────────────

interface Sandbox {
  readonly dir: string;
  readonly home: string;
  readonly env: Readonly<Record<string, string>>;
  git(...args: string[]): string;
  committedPaths(): string[];
  stagedPaths(): string[];
  commitCount(): number;
  write(relativePath: string, content: string): void;
  dispose(): void;
}

/**
 * A throwaway git repo with **no identity available from anywhere**: an empty
 * `HOME`, an empty global and system config, and any identity environment
 * variables stripped out. A commit landing here can only be because the writer
 * passed its own `-c user.name/user.email`.
 */
function makeSandbox(): Sandbox {
  const dir = mkdtempSync(join(tmpdir(), "loop-finalize-repo-"));
  const home = mkdtempSync(join(tmpdir(), "loop-finalize-home-"));
  const emptyConfig = join(home, "empty.gitconfig");
  writeFileSync(emptyConfig, "");

  const env: Record<string, string> = {
    ...stripIdentity(process.env),
    HOME: home,
    GIT_CONFIG_GLOBAL: emptyConfig,
    GIT_CONFIG_SYSTEM: emptyConfig,
    GIT_CONFIG_NOSYSTEM: "1",
  };

  const git = (...args: string[]): string =>
    execFileSync(
      "git",
      ["-c", "user.name=setup", "-c", "user.email=setup@example.invalid", ...args],
      { cwd: dir, env, encoding: "utf8" },
    );

  git("init", "-q", ".");
  writeFileSync(join(dir, "BASE.md"), "base content\n");
  git("add", "--", "BASE.md");
  git("commit", "-q", "-m", "base commit");

  return {
    dir,
    home,
    env,
    git,
    committedPaths: () =>
      git("show", "--name-only", "--format=", "HEAD")
        .trim()
        .split("\n")
        .filter(Boolean),
    stagedPaths: () =>
      git("diff", "--cached", "--name-only")
        .trim()
        .split("\n")
        .filter(Boolean),
    commitCount: () => Number(git("rev-list", "--count", "HEAD").trim()),
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

/** Drop anything that could hand git an identity from the environment. */
function stripIdentity(base: NodeJS.ProcessEnv): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(base)) {
    if (value === undefined) continue;
    if (
      key === "GIT_AUTHOR_NAME" ||
      key === "GIT_AUTHOR_EMAIL" ||
      key === "GIT_COMMITTER_NAME" ||
      key === "GIT_COMMITTER_EMAIL" ||
      key === "EMAIL"
    ) {
      continue;
    }
    out[key] = value;
  }
  return out;
}

function realWriter(
  sandbox: Sandbox,
  extra: { strictness?: "strict" | "lenient" } = {},
): GitWriter {
  return createGitWriter({ cwd: sandbox.dir, env: sandbox.env, ...extra });
}

function emptyCalls(): BoardCalls {
  return { remembered: [], closed: [] };
}

function emptyVcsCalls(): VcsCalls {
  return { plans: [], trailer: [], pathReads: [] };
}

function countIn(order: string[], entry: string): number {
  return order.filter((value) => value === entry).length;
}

// ── rules 1 & 2: the order is the whole point ───────────────────────────────

test("rule 1: the ritual runs commit → remember → close, in that order, in one pass", async () => {
  const order: string[] = [];
  const calls = emptyCalls();
  const events: OrchestratorEvent[] = [];
  const finalizer = createFinalizer(
    {
      vcs: fakeVcs(order, { calls: emptyVcsCalls() }),
      beads: fakeBoard(order, { calls }),
    },
    { onEvent: (event) => events.push(event) },
  );

  const outcome = await finalizer.finalize(request());

  assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
  assert.deepEqual(
    order.filter((entry) => entry !== "plan"),
    ["commit", "remember", "close"],
    "the writes must land in ritual order",
  );
  assert.equal(outcome.stageReached, "close");
  assert.equal(isFinalized(outcome), true);

  // The memory carries the hash the commit actually produced — proof it was
  // written after, not merely alongside.
  assert.equal(calls.remembered.length, 1);
  assert.equal(calls.remembered[0]?.key, handoffKeyFor("loop-1"));
  assert.ok(calls.remembered[0]!.text.includes("abc123def4567890"));
  assert.equal(calls.closed.length, 1);
  assert.equal(calls.closed[0]?.id, "loop-1");

  assert.deepEqual(events.map((event) => event.type), ["committed", "remembered", "closed"]);
});

test("rule 2: a failed commit writes no memory and no close", async () => {
  const order: string[] = [];
  const calls = emptyCalls();
  const finalizer = createFinalizer({
    vcs: fakeVcs(order, {
      calls: emptyVcsCalls(),
      commitError: new VcsError({ kind: "exit", message: "pre-commit hook said no", stderr: "no" }),
    }),
    beads: fakeBoard(order, { calls }),
  });

  const outcome = await finalizer.finalize(request());

  assert.equal(outcome.kind, "commit-failed");
  assert.deepEqual(order.filter((entry) => entry !== "plan"), ["commit"]);
  assert.equal(calls.remembered.length, 0, "no memory after a failed commit");
  assert.equal(calls.closed.length, 0, "no close after a failed commit");
  assert.equal(outcome.stageReached, null);
  if (outcome.kind === "commit-failed") {
    assert.equal(outcome.errorKind, "exit");
    assert.match(outcome.message, /hook said no/);
  }
  assert.deepEqual(toFinalizeEvents(outcome), [
    { type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) },
  ]);
});

test("rule 7: a failed handoff keeps the commit and refuses to close", async () => {
  const order: string[] = [];
  const calls = emptyCalls();
  const finalizer = createFinalizer({
    vcs: fakeVcs(order, {
      calls: emptyVcsCalls(),
      hash: "cafe1234deadbeef",
    }),
    beads: fakeBoard(order, {
      calls,
      rememberError: new BdError({ kind: "guard-mismatch", message: "bd refused the write" }),
    }),
  });

  const outcome = await finalizer.finalize(request());

  assert.equal(outcome.kind, "handoff-failed");
  assert.deepEqual(order.filter((entry) => entry !== "plan"), ["commit", "remember"]);
  assert.equal(calls.closed.length, 0, "nothing closes without its handoff note");
  if (outcome.kind === "handoff-failed") {
    assert.equal(outcome.commitHash, "cafe1234deadbeef", "the hash that DOES exist is reported");
    assert.equal(outcome.errorKind, "guard-mismatch", "bd's own kind survives the mapping");
    assert.equal(outcome.stageReached, "commit");
  }
  assert.deepEqual(toFinalizeEvents(outcome).map((event) => event.type), [
    "committed",
    "finalize_failed",
  ]);
});

test("rule 8: a failed close reports the commit and the note that already exist", async () => {
  const order: string[] = [];
  const calls = emptyCalls();
  const finalizer = createFinalizer({
    vcs: fakeVcs(order, { calls: emptyVcsCalls(), hash: "beef0001cafe2024" }),
    beads: fakeBoard(order, { calls, closeError: new BdError({ kind: "exit-1", message: "boom" }) }),
  });

  const outcome = await finalizer.finalize(request());

  assert.equal(outcome.kind, "close-failed");
  assert.deepEqual(order.filter((entry) => entry !== "plan"), ["commit", "remember", "close"]);
  if (outcome.kind === "close-failed") {
    assert.equal(outcome.commitHash, "beef0001cafe2024");
    assert.equal(outcome.stageReached, "handoff");
    assert.match(describeFinalizeFailure(outcome), /could not be closed/);
    assert.match(describeFinalizeFailure(outcome), /loop:handoff:loop-1/);
  }
});

test("a close that does not come back as closed is a failure, not a trust fall", async () => {
  const order: string[] = [];
  const finalizer = createFinalizer({
    vcs: fakeVcs(order, { calls: emptyVcsCalls() }),
    beads: fakeBoard(order, { calls: emptyCalls(), closeStatus: "open" }),
  });

  const outcome = await finalizer.finalize(request());
  assert.equal(outcome.kind, "close-failed");
  if (outcome.kind === "close-failed") {
    assert.equal(outcome.errorKind, "unexpected-status");
    assert.match(outcome.message, /"open"/);
  }
  assert.equal(isFinalized(outcome), false);
});

test("rule 9: a guard mismatch is never retried — one remember, then stop", async () => {
  const order: string[] = [];
  const finalizer = createFinalizer({
    vcs: fakeVcs(order, { calls: emptyVcsCalls() }),
    beads: fakeBoard(order, {
      calls: emptyCalls(),
      rememberError: new BdError({ kind: "guard-mismatch", message: "stale --if-status" }),
    }),
  });

  await finalizer.finalize(request());
  assert.equal(countIn(order, "remember"), 1, "a guard mismatch must not be blindly retried");
  assert.equal(countIn(order, "commit"), 1);
  assert.equal(countIn(order, "close"), 0);
});

// ── rules 10 & 11: the artifacts carry their own story ──────────────────────

test("rule 11: the commit message is `id: title`, summary, paths, then the trailer", () => {
  const req = request();
  const key = handoffKeyFor(req.issueId);

  assert.equal(
    renderCommitMessage(req, key),
    [
      "loop-1: Teach the loop to close its own beads",
      "",
      "Added the finalize ritual, its ordering rules and its tests.",
      "",
      "Changed:",
      "- src/finalize.ts",
      "- test/finalize.test.ts",
      "",
      `${COMMIT_TRAILER}: loop:handoff:loop-1`,
    ].join("\n"),
  );
});

test("rule 10: the handoff note is self-sufficient", () => {
  const req = request();
  const key = handoffKeyFor(req.issueId);
  const note = renderHandoff(req, "1234567890abcdef", ["src/a.ts", "docs/b.md"], key);

  for (const fragment of [
    "Finalized loop-1",
    "1234567890abcdef",
    "src/a.ts",
    "docs/b.md",
    req.summary,
    "close last, because it is the claim of completion",
    "wire it into the interpreter (.9)",
    "loop:handoff:loop-1",
  ]) {
    assert.ok(note.includes(fragment), `the note must carry ${fragment}`);
  }
});

test("a note with no next steps says so instead of trailing off", () => {
  const req = request({ nextSteps: [], decisions: [] });
  const note = renderHandoff(req, "abc", ["src/a.ts"], handoffKeyFor("loop-1"));
  assert.match(note, /Next: \(none recorded\)/);
});

// ── rule 12: the dry run prints the truth ───────────────────────────────────

test("rule 12: a dry run prints every command and mutates nothing", async () => {
  const order: string[] = [];
  const calls = emptyCalls();
  const printed: string[] = [];
  const finalizer = createFinalizer(
    {
      vcs: fakeVcs(order, {
        calls: emptyVcsCalls(),
        stageable: ["src/a.ts", "src/b.ts"],
      }),
      beads: fakeBoard(order, { calls }),
    },
    { dryRun: true, onPlan: (line) => printed.push(line) },
  );

  const outcome = await finalizer.finalize(request({ changedFiles: ["src/a.ts", "src/b.ts"] }));

  assert.equal(outcome.kind, "planned");
  for (const mutation of ["commit", "remember", "close"]) {
    assert.equal(order.includes(mutation), false, `a dry run must not ${mutation}`);
  }
  assert.equal(calls.remembered.length, 0);
  assert.equal(calls.closed.length, 0);

  assert.ok(printed.some((line) => line === "WRITE [commit] git add -- src/a.ts"));
  assert.ok(
    printed.some((line) =>
      line.startsWith(
        "WRITE [commit] git -c user.name=pi-loop -c user.email=pi-loop@localhost commit -m",
      ),
    ),
    `expected an explicit-identity commit command, got:\n${printed.join("\n")}`,
  );
  assert.equal(linesContaining(printed, "WRITE [handoff] bd remember").length, 1);
  assert.ok(printed.some((line) => line.startsWith("WRITE [close] bd close loop-1")));
  assert.ok(
    printed.some((line) => line.includes(UNRESOLVED_HASH)),
    "the hash is unknowable before the commit; the plan must say so, not invent one",
  );
  assert.deepEqual(toFinalizeEvents(outcome), [], "a dry run drives no state");
});

function linesContaining(lines: readonly string[], needle: string): string[] {
  return lines.filter((line) => line.includes(needle));
}

test("rule 12b: the printed plan is the live run's argv, byte for byte", async () => {
  const sandbox = makeSandbox();
  const recordFile = join(sandbox.home, "bd-record.jsonl");
  const fakeBd = fileURLToPath(new URL("./fake-bin/bd", import.meta.url));
  try {
    sandbox.write("src/a.ts", "export const a = 1;\n");
    sandbox.write("src/b.ts", "export const b = 2;\n");

    const real = createDefaultGitRunner(30_000);
    const recordedGit: string[][] = [];
    const recordingWriter = createGitWriter({
      cwd: sandbox.dir,
      env: sandbox.env,
      run: (bin, args, cwd, env) => {
        recordedGit.push([...args]);
        return real(bin, args, cwd, env);
      },
    });
    const beads = createBdClient({
      cwd: sandbox.dir,
      bin: fakeBd,
      env: { FAKE_BD_RECORD: recordFile, FAKE_BD_SCENARIO: "finalize" },
    });

    const req = request({ changedFiles: ["src/a.ts", "src/b.ts"] });

    const dryPrinted: string[] = [];
    const dry = await createFinalizer(
      { vcs: recordingWriter, beads },
      { dryRun: true, onPlan: (line) => dryPrinted.push(line) },
    ).finalize(req);
    assert.equal(dry.kind, "planned");
    assert.equal(sandbox.commitCount(), 1, "the dry run committed nothing");

    const live = await createFinalizer({ vcs: recordingWriter, beads }).finalize(req);
    assert.equal(live.kind, "finalized", describeFinalizeFailure(live));

    const dryWrites = dry.planned
      .filter((command) => command.phase === "write")
      .map((command) => `[${command.stage}] ${command.tool} ${command.argv.join(" ")}`)
      .map((line) =>
        line.split(UNRESOLVED_HASH).join(live.kind === "finalized" ? live.commitHash : ""),
      );

    const readSubcommands = new Set([
      "rev-parse",
      "show",
      "diff",
      "ls-files",
      "status",
      "log",
    ]);
    const liveWrites: string[] = [];
    for (const argv of recordedGit) {
      let index = 0;
      while (argv[index] === "-c") index += 2;
      if (readSubcommands.has(argv[index] ?? "")) continue;
      liveWrites.push(`[commit] git ${argv.join(" ")}`);
    }
    const bdRecords = readFileSync(recordFile, "utf8")
      .trim()
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line) as { argv: string[] });
    for (const record of bdRecords) {
      liveWrites.push(
        `[${record.argv[0] === "remember" ? "handoff" : "close"}] bd ${record.argv.join(" ")}`,
      );
    }

    assert.deepEqual(dryWrites, liveWrites, "the dry plan must be the live commands");
  } finally {
    sandbox.dispose();
  }
});

// ── rules 3, 4, 5: staging discipline, against real git ─────────────────────

test("rule 3: only the reported paths are committed", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  const calls = emptyCalls();
  try {
    sandbox.write("src/a.ts", "export const a = 1;\n");
    sandbox.write("src/b.ts", "export const b = 2;\n");
    sandbox.write("scratch/notes.txt", "someone else's mess\n");

    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls }),
    });
    const outcome = await finalizer.finalize(request({ changedFiles: ["src/a.ts", "src/b.ts"] }));

    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
    assert.deepEqual([...sandbox.committedPaths()].sort(), ["src/a.ts", "src/b.ts"]);
    assert.equal(sandbox.commitCount(), 2, "exactly one commit on top of base");
    assert.ok(
      sandbox
        .git("status", "--porcelain", "--untracked-files=all")
        .includes("scratch/notes.txt"),
      "unreported files must not be swept in",
    );
    assert.equal(sandbox.stagedPaths().length, 0);
    assert.equal(calls.closed.length, 1);
  } finally {
    sandbox.dispose();
  }
});

test("rule 3b: a reported deletion is staged as a deletion, not skipped", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    rmSync(join(sandbox.dir, "BASE.md"));
    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls: emptyCalls() }),
    });

    const outcome = await finalizer.finalize(request({ changedFiles: ["BASE.md"] }));
    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
    assert.deepEqual(sandbox.committedPaths(), ["BASE.md"]);
    assert.equal(existsSync(join(sandbox.dir, "BASE.md")), false);
  } finally {
    sandbox.dispose();
  }
});

test("rule 4: escapes are refused and nothing is staged to compensate", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    const outside = join(sandbox.dir, "..", "outside.txt");
    writeFileSync(outside, "never yours\n");

    const escapes = [
      "../outside.txt",
      join(sandbox.home, "secret.txt"),
      ":(glob)**/*.ts",
      ".git/config",
    ];
    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls: emptyCalls() }),
    });

    const outcome = await finalizer.finalize(request({ changedFiles: escapes }));

    assert.equal(outcome.kind, "unsafe-path", describeFinalizeFailure(outcome));
    if (outcome.kind === "unsafe-path") {
      assert.equal(outcome.refused.length, escapes.length);
      for (const refusal of outcome.refused) {
        assert.ok(refusal.reason.length > 0, `${refusal.path} must say why`);
      }
    }
    assert.equal(sandbox.commitCount(), 1, "no commit at all");
    assert.equal(sandbox.stagedPaths().length, 0, "no partial staging left behind");
    assert.deepEqual(order.filter((entry) => entry !== "plan"), [], "no board writes");
    rmSync(outside, { force: true });
  } finally {
    sandbox.dispose();
  }
});

test("rule 4b: a reported path that is not there is skipped, never guessed at", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    sandbox.write("src/real.ts", "export const real = 1;\n");
    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls: emptyCalls() }),
    });

    const outcome = await finalizer.finalize(
      request({ changedFiles: ["src/real.ts", "src/ghost.ts"] }),
    );
    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
    assert.deepEqual(sandbox.committedPaths(), ["src/real.ts"]);

    const adds = outcome.planned.filter(
      (command) => command.phase === "write" && command.argv[0] === "add",
    );
    assert.deepEqual(
      adds.map((command) => command.argv[2]),
      ["src/real.ts"],
      "the ghost path never reached an `add`",
    );
  } finally {
    sandbox.dispose();
  }
});

test("rule 5: nothing reported means no commit, no memory, no close", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  const calls = emptyCalls();
  try {
    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls }),
    });

    const outcome = await finalizer.finalize(request({ changedFiles: [] }));

    assert.equal(outcome.kind, "nothing-to-commit");
    assert.equal(sandbox.commitCount(), 1, "still just the base commit");
    assert.deepEqual(order.filter((entry) => entry !== "plan"), []);
    assert.equal(calls.remembered.length, 0);
    assert.equal(calls.closed.length, 0);
    assert.match(describeFinalizeFailure(outcome), /no commit, no memory, no close/);
  } finally {
    sandbox.dispose();
  }
});

test("rule 3 strict: an unrelated staged path blocks the commit outright", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    sandbox.write("src/mine.ts", "export const mine = 1;\n");
    sandbox.write("src/theirs.ts", "export const theirs = 1;\n");
    sandbox.git("add", "--", "src/theirs.ts");

    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls: emptyCalls() }),
    });
    const outcome = await finalizer.finalize(request({ changedFiles: ["src/mine.ts"] }));

    assert.equal(outcome.kind, "unrelated-staged", describeFinalizeFailure(outcome));
    if (outcome.kind === "unrelated-staged") {
      assert.deepEqual([...outcome.foreign], ["src/theirs.ts"]);
    }
    assert.equal(sandbox.commitCount(), 1, "the stranger's work was not folded in");
    assert.deepEqual(order.filter((entry) => entry !== "plan"), []);
  } finally {
    sandbox.dispose();
  }
});

test("rule 3 lenient: the commit holds only what was reported; the stranger stays staged", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    sandbox.write("src/mine.ts", "export const mine = 1;\n");
    sandbox.write("src/theirs.ts", "export const theirs = 1;\n");
    sandbox.git("add", "--", "src/theirs.ts");

    const finalizer = createFinalizer({
      vcs: realWriter(sandbox, { strictness: "lenient" }),
      beads: fakeBoard(order, { calls: emptyCalls() }),
    });
    const outcome = await finalizer.finalize(request({ changedFiles: ["src/mine.ts"] }));

    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
    assert.deepEqual(sandbox.committedPaths(), ["src/mine.ts"]);
    assert.deepEqual(sandbox.stagedPaths(), ["src/theirs.ts"], "left exactly where it was found");
  } finally {
    sandbox.dispose();
  }
});

// ── rule 6: a failed commit leaves nothing behind ────────────────────────────

test("rule 6: a real hook rejection fails the commit, unstages, writes no memory", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  const calls = emptyCalls();
  try {
    const hook = join(sandbox.dir, ".git", "hooks", "pre-commit");
    writeFileSync(hook, "#!/bin/sh\necho 'no hooks for you' >&2\nexit 1\n");
    chmodSync(hook, 0o755);
    sandbox.write("src/a.ts", "export const a = 1;\n");

    const finalizer = createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls }),
    });
    const outcome = await finalizer.finalize(request({ changedFiles: ["src/a.ts"] }));

    assert.equal(outcome.kind, "commit-failed");
    if (outcome.kind === "commit-failed") {
      assert.equal(outcome.errorKind, "exit");
      assert.match(outcome.message, /no hooks for you/, "the hook's own complaint is preserved");
    }
    assert.equal(sandbox.commitCount(), 1, "no commit landed");
    assert.equal(sandbox.stagedPaths().length, 0, "no half-staged index left behind");
    assert.equal(existsSync(join(sandbox.dir, "src", "a.ts")), true, "the work itself survives");
    assert.deepEqual(order.filter((entry) => entry !== "plan"), [], "no memory, no close");
  } finally {
    sandbox.dispose();
  }
});

// ── rule 7b: recovery, with real git ────────────────────────────────────────

test("rule 7b: a re-run after a failed handoff finishes without a second commit", async () => {
  const sandbox = makeSandbox();
  try {
    sandbox.write("src/a.ts", "export const a = 1;\n");

    const brokenBoardOrder: string[] = [];
    const first = await createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(brokenBoardOrder, {
        calls: emptyCalls(),
        rememberError: new BdError({ kind: "exit-1", message: "db busy" }),
      }),
    }).finalize(request({ changedFiles: ["src/a.ts"] }));

    assert.equal(first.kind, "handoff-failed");
    assert.equal(sandbox.commitCount(), 2, "the commit from run one is real");
    const firstHash = first.kind === "handoff-failed" ? first.commitHash : "";
    assert.match(firstHash, /^[0-9a-f]{7,}$/);

    const secondOrder: string[] = [];
    const secondCalls = emptyCalls();
    const second = await createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(secondOrder, { calls: secondCalls }),
    }).finalize(request({ changedFiles: ["src/a.ts"] }));

    assert.equal(second.kind, "finalized", describeFinalizeFailure(second));
    assert.equal(sandbox.commitCount(), 2, "still one commit — the run reused it");
    if (second.kind === "finalized") {
      assert.equal(second.reusedCommit, true);
      assert.equal(second.commitHash, firstHash);
      assert.deepEqual([...second.committedPaths], ["src/a.ts"]);
    }
    assert.equal(secondCalls.remembered.length, 1);
    assert.ok(secondCalls.remembered[0]!.text.includes(firstHash));
    assert.ok(secondCalls.remembered[0]!.text.includes("reused from an earlier run"));
    assert.equal(secondOrder.includes("commit"), false, "no git write in the recovery run");
  } finally {
    sandbox.dispose();
  }
});

test("reuse can be turned off: then a clean re-run is nothing-to-commit, not a silent reuse", async () => {
  const sandbox = makeSandbox();
  try {
    sandbox.write("src/a.ts", "export const a = 1;\n");
    const first = await createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard([], { calls: emptyCalls() }),
    }).finalize(request({ changedFiles: ["src/a.ts"] }));
    assert.equal(first.kind, "finalized");

    const order: string[] = [];
    const second = await createFinalizer(
      { vcs: realWriter(sandbox), beads: fakeBoard(order, { calls: emptyCalls() }) },
      { reuseExistingCommit: false },
    ).finalize(request({ changedFiles: ["src/a.ts"] }));

    assert.equal(second.kind, "nothing-to-commit");
    assert.equal(order.includes("trailer"), false, "with reuse off, history is not consulted");
    assert.equal(sandbox.commitCount(), 2);
  } finally {
    sandbox.dispose();
  }
});

// ── rules 13, 14, 15: git mechanics ─────────────────────────────────────────

test("rules 13 & 15: identity comes from the writer, the hash comes back from git", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    sandbox.write("src/a.ts", "export const a = 1;\n");
    const writer = realWriter(sandbox);
    const finalizer = createFinalizer({ vcs: writer, beads: fakeBoard(order, { calls: emptyCalls() }) });

    const outcome = await finalizer.finalize(request({ changedFiles: ["src/a.ts"] }));
    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
    if (outcome.kind !== "finalized") return;

    const head = sandbox.git("rev-parse", "HEAD").trim();
    assert.equal(outcome.commitHash, head, "the reported hash is git's, not a guess");
    assert.equal(await writer.headHash(), head);
    assert.deepEqual(
      [...(await writer.commitPaths(head))],
      sandbox.committedPaths(),
      "committed paths are read back from git",
    );
    // No HOME, no global config, no system config, no identity env vars: the
    // only identity available is the one the writer put on the command line.
    assert.equal(sandbox.git("log", "-1", "--pretty=%an").trim(), "pi-loop");
    assert.equal(sandbox.git("log", "-1", "--pretty=%ae").trim(), "pi-loop@localhost");
    assert.ok(sandbox.git("log", "-1", "--pretty=%B").includes(`${COMMIT_TRAILER}: loop:handoff:loop-1`));
  } finally {
    sandbox.dispose();
  }
});

test("a writer with no git at all says so as a typed error", async () => {
  const missing = createGitWriter({
    cwd: "/nonexistent-loop-dir",
    bin: "git-that-does-not-exist",
  });
  await assert.rejects(
    () => missing.planCommit("msg", ["a.txt"]),
    (error: unknown) =>
      error instanceof VcsError &&
      ["missing-binary", "not-a-repo", "exit"].includes(error.kind),
  );
});

test("a commit whose message was rewritten is not claimed as ours", async () => {
  const sandbox = makeSandbox();
  const order: string[] = [];
  try {
    const hook = join(sandbox.dir, ".git", "hooks", "commit-msg");
    // Wholesale rewrite: the hook throws away everything we wrote.
    writeFileSync(hook, "#!/bin/sh\nprintf 'unrelated rewrite\\n' > \"$1\"\nexit 0\n");
    chmodSync(hook, 0o755);
    sandbox.write("src/a.ts", "export const a = 1;\n");

    const outcome = await createFinalizer({
      vcs: realWriter(sandbox),
      beads: fakeBoard(order, { calls: emptyCalls() }),
    }).finalize(request({ changedFiles: ["src/a.ts"] }));

    assert.equal(outcome.kind, "commit-failed");
    if (outcome.kind === "commit-failed") {
      assert.equal(outcome.errorKind, "verification");
      assert.match(outcome.message, /not the commit this plan made/);
      // The commit still exists; the error says so instead of hiding it.
      assert.equal(sandbox.commitCount(), 2);
    }
    assert.deepEqual(order.filter((entry) => entry !== "plan"), [], "no memory, no close");
  } finally {
    sandbox.dispose();
  }
});

// ── rule 16: bad requests are refused before anything is touched ──────────────

test("rule 16: an invalid request never reaches git or bd", async () => {
  const order: string[] = [];
  const finalizer = createFinalizer({
    vcs: fakeVcs(order, { calls: emptyVcsCalls() }),
    beads: fakeBoard(order, { calls: emptyCalls() }),
  });

  const bad = {
    issueId: "loop-1",
    title: "",
    summary: "   ",
    changedFiles: ["src/a.ts"],
  } as unknown as FinalizeRequest;

  const outcome = await finalizer.finalize(bad);
  assert.equal(outcome.kind, "invalid-request");
  if (outcome.kind === "invalid-request") {
    assert.ok(outcome.problems.some((problem) => problem.includes("title")));
    assert.ok(outcome.problems.some((problem) => problem.includes("summary")));
  }
  assert.deepEqual(order, [], "not even a plan was built");
});

test("validateFinalizeRequest names every problem instead of the first", () => {
  const problems = validateFinalizeRequest({
    issueId: " ",
    title: "",
    summary: "",
    changedFiles: "src/a.ts",
  } as unknown as FinalizeRequest);
  assert.ok(problems.length >= 4, problems.join("; "));
  assert.equal(validateFinalizeRequest(request()).length, 0);
});

// ── rule 17: bd discipline through the real argv ────────────────────────────

test("rule 17: every bd call carries the last-touched guard and --json, with explicit ids", async () => {
  const sandbox = makeSandbox();
  const recordFile = join(sandbox.home, "bd-record.jsonl");
  const fakeBd = fileURLToPath(new URL("./fake-bin/bd", import.meta.url));
  try {
    sandbox.write("src/a.ts", "export const a = 1;\n");
    const beads = createBdClient({
      cwd: sandbox.dir,
      bin: fakeBd,
      env: { FAKE_BD_RECORD: recordFile, FAKE_BD_SCENARIO: "finalize" },
    });

    const outcome = await createFinalizer({
      vcs: realWriter(sandbox),
      beads,
    }).finalize(request({ changedFiles: ["src/a.ts"] }));
    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));

    const records = readFileSync(recordFile, "utf8")
      .trim()
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line) as { argv: string[]; bdLastTouchedFallback?: string });

    assert.equal(records.length, 2, "one remember, one close");
    assert.deepEqual(records.map((record) => record.argv[0]), ["remember", "close"]);
    for (const record of records) {
      assert.equal(record.bdLastTouchedFallback, "0", "BD_LAST_TOUCHED_FALLBACK must be 0");
      assert.ok(record.argv.includes("--json"), "every bd call is machine-readable");
    }
    assert.equal(records[1]!.argv[1], "loop-1", "the close names the issue explicitly");
  } finally {
    sandbox.dispose();
  }
});

// ── rule 19: the orchestrator seam ──────────────────────────────────────────

function issueWith(id: string, status: IssueStatus) {
  return {
    id,
    title: "the one",
    description: "",
    acceptance_criteria: "",
    status,
    priority: 1,
    issue_type: "task",
  };
}

function why(result: StepResult): string {
  return result.applied ? "applied" : `${result.rejection.code}: ${result.rejection.message}`;
}

function rejectionOf(result: StepResult): { code: string; message: string } {
  if (result.applied) assert.fail("expected the event to be rejected, but it applied");
  return result.rejection;
}

/** The machine as `.4` leaves it: `w-1` worked, finalize pending at `commit`. */
function finalizingMachine(): OrchestratorState {
  let state = createInitialState();
  for (const event of [
    { type: "start" } as OrchestratorEvent,
    {
      type: "board_observed",
      inProgress: [],
      ready: [issueWith("w-1", "open")],
    } as OrchestratorEvent,
    { type: "issue_claimed", id: "w-1" } as OrchestratorEvent,
    {
      type: "work_succeeded",
      summary: "did the thing",
      changedFiles: ["src/a.ts"],
    } as OrchestratorEvent,
  ]) {
    const result = step(state, event);
    assert.equal(result.applied, true, `${event.type} should apply: ${why(result)}`);
    state = result.state;
  }
  return state;
}

test("rule 19: the finalizer's events walk the orchestrator through the three stages", () => {
  let state = finalizingMachine();
  assert.equal(state.name, "finalize");
  assert.equal(state.finalizeStage, "commit");

  const outcome = {
    ...baseFor("w-1"),
    kind: "finalized" as const,
    commitHash: "deadbeefcafe",
    committedPaths: ["src/a.ts"],
    reusedCommit: false,
    closedStatus: "closed" as IssueStatus,
  };

  for (const event of toFinalizeEvents(outcome)) {
    const result = step(state, event);
    assert.equal(result.applied, true, `${event.type}: ${why(result)}`);
    state = result.state;
  }
  assert.equal(state.finalizeStage, null);
});

function baseFor(issueId: string) {
  const req = request({ issueId });
  return {
    issueId: req.issueId,
    request: req,
    dryRun: false,
    handoffKey: handoffKeyFor(issueId),
    commitMessage: renderCommitMessage(req, handoffKeyFor(issueId)),
    planned: [],
    planText: [],
    stageReached: "close" as const,
    handoffText: renderHandoff(req, "deadbeefcafe", ["src/a.ts"], handoffKeyFor(issueId)),
  };
}

test("rule 19b: out-of-order reports are rejected, not absorbed", () => {
  const machine = finalizingMachine();

  // Closing before committing.
  const early = rejectionOf(step(machine, { type: "closed", id: "w-1" }));
  assert.equal(early.code, "stage-mismatch");

  // Remembering before committing.
  assert.equal(
    rejectionOf(step(machine, { type: "remembered", key: handoffKeyFor("w-1") })).code,
    "stage-mismatch",
  );

  // Reporting a handoff failure while the commit stage is in flight.
  assert.equal(
    rejectionOf(
      step(machine, { type: "finalize_failed", stage: "handoff", reason: "late report" }),
    ).code,
    "stage-mismatch",
  );

  // The order the finalizer actually emits is accepted, all the way through.
  let state = machine;
  for (const event of toFinalizeEvents({
    ...baseFor("w-1"),
    kind: "finalized" as const,
    commitHash: "abc123",
    committedPaths: ["src/a.ts"],
    reusedCommit: false,
    closedStatus: "closed" as IssueStatus,
  })) {
    const result = step(state, event);
    assert.equal(result.applied, true, `${event.type}: ${why(result)}`);
    state = result.state;
  }
  assert.equal(state.finalizeStage, null);
});

test("the finalizer's handoff key is the key the orchestrator expects", async () => {
  const order: string[] = [];
  const calls = emptyCalls();
  const outcome = await createFinalizer({
    vcs: fakeVcs(order, { calls: emptyVcsCalls() }),
    beads: fakeBoard(order, { calls }),
  }).finalize(request({ issueId: "w-1" }));
  assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
  assert.equal(calls.remembered.length, 1);

  // The orchestrator fixes the key when the commit lands, before the handoff is
  // reported — so the two sides must agree on it or the write is rejected.
  let state = finalizingMachine();
  assert.equal(state.handoffKey, null, "the key is fixed by the commit, not before it");
  const afterCommit = step(state, { type: "committed", hash: "abc123" });
  assert.equal(afterCommit.applied, true, why(afterCommit));
  state = afterCommit.state;
  assert.equal(state.handoffKey, handoffKeyFor("w-1"));
  assert.equal(calls.remembered[0]?.key, state.handoffKey);

  const afterRemember = step(state, { type: "remembered", key: calls.remembered[0]!.key });
  assert.equal(afterRemember.applied, true, why(afterRemember));

  // A key that does not match is refused, not quietly accepted.
  const mismatch = rejectionOf(step(state, { type: "remembered", key: "something:else" }));
  assert.equal(mismatch.code, "handoff-key-mismatch");
});

// ── rule 21: real git + real bd, end to end ─────────────────────────────────

function bdAvailable(): boolean {
  const result = spawnSync("bd", ["version"], { encoding: "utf8" });
  return !result.error && result.status === 0;
}

test("live: real git, real bd — commit, then memory, then close, verified by read-back", async (t) => {
  if (!bdAvailable()) {
    t.skip("bd is not installed — this test must be skipped loudly, not silently passed");
    return;
  }
  const sandbox = makeSandbox();
  const beadsDir = join(sandbox.home, "beads");
  const init = spawnSync("bd", ["init", "--prefix", "fin", "--non-interactive"], {
    cwd: sandbox.dir,
    env: { ...process.env, BEADS_DIR: beadsDir },
    encoding: "utf8",
  });
  assert.equal(init.status, 0, `bd init failed: ${init.stderr}`);

  try {
    const beads = createBdClient({ cwd: sandbox.dir, env: { BEADS_DIR: beadsDir } });
    const spec: NewIssueSpec = {
      title: "Close the loop's own bead",
      description: "Finalize the iteration in the mandated order.",
      acceptance: "A commit exists before the memory, and the memory before the close.",
      priority: 1,
      type: "task",
    };
    const created = await beads.createIssue(spec);
    const issueId = created.id;
    const key = handoffKeyFor(issueId);

    sandbox.write("src/result.ts", "export const result = 42;\n");
    sandbox.write("docs/why.md", "# why\n");
    sandbox.write("scratch/ignore-me.txt", "not reported, not committed\n");

    const order: string[] = [];
    const finalizer = createFinalizer({
      vcs: recordWriter(order, realWriter(sandbox)),
      beads: recordBoard(order, beads),
    });
    const outcome = await finalizer.finalize(
      request({
        issueId,
        changedFiles: ["src/result.ts", "docs/why.md"],
        nextSteps: ["run the loop against the real board"],
      }),
    );

    assert.equal(outcome.kind, "finalized", describeFinalizeFailure(outcome));
    if (outcome.kind !== "finalized") return;

    // (1) The commit: exactly the reported paths, and the id verbatim in it.
    assert.deepEqual([...sandbox.committedPaths()].sort(), ["docs/why.md", "src/result.ts"]);
    const message = sandbox.git("log", "-1", "--pretty=%B");
    assert.ok(message.includes(`${issueId}: `), `subject must name ${issueId}: ${message}`);
    assert.ok(message.includes(`${COMMIT_TRAILER}: ${key}`));
    assert.equal(sandbox.git("log", "-1", "--pretty=%an").trim(), "pi-loop");
    assert.equal(outcome.commitHash, sandbox.git("rev-parse", "HEAD").trim());

    // (2) The memory: written under the handoff key, carrying that same hash.
    const recalled = await beads.recall(key);
    assert.ok(recalled !== null, "the handoff must be recallable");
    assert.ok(recalled!.includes(issueId), "the note names the issue");
    assert.ok(recalled!.includes(outcome.commitHash), "the note names the commit that exists");
    assert.ok(recalled!.includes("src/result.ts") && recalled!.includes("docs/why.md"));
    assert.ok(recalled!.includes("run the loop against the real board"));
    assert.ok(
      sandbox
        .git("status", "--porcelain", "--untracked-files=all")
        .includes("scratch/ignore-me.txt"),
      "the unreported file was never touched",
    );

    // (3) The close: last, and only after the two above were verified.
    const shown = await beads.getIssue(issueId);
    assert.ok(shown !== null, "the bead must still exist");
    assert.equal(shown.status, "closed");
    assert.deepEqual(order.filter((entry) => entry !== "plan"), ["commit", "remember", "close"]);
  } finally {
    sandbox.dispose();
  }
});

// ── rule 18 & 20: boundaries, asserted against the source itself ─────────────

function codeOnly(source: string): string {
  const withoutBlock = source.replace(/\/\*[\s\S]*?\*\//g, "");
  return withoutBlock
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
}

test("rule 18: src/finalize.ts spawns nothing and calls no model", () => {
  const source = codeOnly(readFileSync(new URL("../src/finalize.ts", import.meta.url), "utf8"));

  for (const banned of [
    "child_process",
    "execFile",
    "spawnSync",
    "spawn(",
    "execSync",
    "createAgentSession",
    "@mariozechner/pi-ai",
    "openai",
    "anthropic",
    "fetch(",
    "--assignee",
    "--claim",
    "assignee:",
    "setStatus",
  ]) {
    assert.equal(source.includes(banned), false, `src/finalize.ts must not contain ${banned}`);
  }

  // It reaches the outside world only through the two adapters and the machine's
  // own vocabulary.
  assert.match(source, /from "\.\/beads\.js"/);
  assert.match(source, /from "\.\/vcs\.js"/);
  assert.match(source, /from "\.\/orchestrator\.js"/);
});

test("rule 20: the git writer never bypasses hooks, forces, sweeps, or pushes", () => {
  const source = codeOnly(readFileSync(new URL("../src/vcs.ts", import.meta.url), "utf8"));

  for (const banned of [
    "--no-verify",
    "--hard",
    "--force",
    "-A",
    '"push"',
    "'push'",
    "commit -a",
    '"-a"',
    "--amend",
  ]) {
    assert.equal(source.includes(banned), false, `src/vcs.ts must not contain ${banned}`);
  }

  // Identity is explicit, every add is an explicit path, and the trailer is real.
  assert.match(source, /-c/);
  assert.match(source, /user\.name/);
  assert.match(source, /"add", "--"/);
  assert.match(source, new RegExp(`export const COMMIT_TRAILER = "${COMMIT_TRAILER}"`));
});

test("the git writer's write path has exactly one caller in src", () => {
  // Staging and committing are dangerous enough to have one home. If a second
  // file starts calling the writer's write path, this test says so.
  const directory = fileURLToPath(new URL("../src", import.meta.url));
  const callers = readdirSync(directory)
    .filter((name) => name.endsWith(".ts") && name !== "vcs.ts")
    .filter((name) =>
      codeOnly(readFileSync(join(directory, name), "utf8")).includes(".execute("),
    );
  assert.deepEqual(callers, ["finalize.ts"]);
});
