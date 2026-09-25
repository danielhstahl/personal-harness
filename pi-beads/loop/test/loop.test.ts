/**
 * Tests for `src/loop.ts` — the interpreter that makes the parts one program
 * (workspace-5yn.9).
 *
 * Every test has the same shape: run the real machine under the real interpreter
 * against doubles that behave like the real world in the one way the assertion
 * cares about, then read back what actually happened — the transition log, the
 * board, the git repo, the session prompts.
 *
 * Real here: the orchestrator's transitions, the splitter's ledger (real epic
 * record, real `#index` → id binding), the finalizer's commit/remember/close
 * ritual, real `git` in a throwaway repo with no identity reachable from
 * anywhere, and the `.5` runner's prompt assembly behind a fake session.
 * Faked: `bd` (in-memory), the model (scripted), the terminal (a queue),
 * signals (a queue).
 *
 * Rule map: 1/4/5 → "the whole walk" and "the effect order"; 2 → coverage plus
 * the unknown-effect and unit-drift tests; 6/7 → the probe tests; 8 → split
 * failure; 9 → the unfinished verdict; 10 → the refused claim; 11 → crash
 * recovery; 12/13 → fatal and interrupted exits; 0/15/16 → source guards.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { buildApp, idleStatusFrom, perTurnIdle } from "../src/app.ts";
import { readEnv, runFromEnv } from "../src/main.ts";
import { createAgentRunner } from "../src/agent.ts";
import { BdError } from "../src/beads.ts";
import { createFinalizer } from "../src/finalize.ts";
import { normaliseDependencies } from "../src/beads.ts";
import { HANDLED_EFFECT_KINDS, runLoop } from "../src/loop.ts";
import type { LoopLogEntry, LoopPorts } from "../src/loop.ts";
import { EFFECT_KINDS, failureKeyFor, handoffKeyFor, step } from "../src/orchestrator.ts";
import type { OrchestratorEvent, OrchestratorState, StepResult } from "../src/orchestrator.ts";
import { createSplitter } from "../src/split.ts";
import { createIdleMode, IdleError } from "../src/idle.ts";
import type { IdleHandle, IdleOutcome } from "../src/idle.ts";
import { createNullPresenter } from "../src/render.ts";
import type { WorkPresenter } from "../src/render.ts";
import { createGitWriter } from "../src/vcs.ts";
import {
  createScriptBoard,
  doneParams,
  fakeSessionFactory,
  fakeSplitPort,
  makeRepo,
  recordingUi,
  scriptedIdle,
  scriptedSignals,
  splitWireItem,
} from "./loop-support.ts";
import type { FakeScript, ScriptBoard } from "./loop-support.ts";
import { FakeSignals, FakeTerminal, settle } from "./idle-fakes.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const readSource = (name: string): string =>
  readFileSync(join(HERE, "..", "src", name), "utf8");

const SPLIT_REQUEST = "Add a colour mode to the parser, then write the docs that explain it";

/** A good split: docs depend on the parser change (`#0`). */
const SPLIT_BATCH = [
  splitWireItem("Add colour mode to the parser", { priority: 1 }),
  splitWireItem("Write the colour-mode docs", { priority: 2, depends_on: [0] }),
];

/** What the two worked children report, in order. */
const WORK_SCRIPTS: FakeScript[] = [
  {
    tools: [
      {
        name: "report_done",
        params: doneParams({
          summary: "Added the colour mode to the parser.",
          changed_files: ["src/colour.ts"],
          next_steps: ["docs are still missing"],
        }),
      },
    ],
  },
  {
    tools: [
      {
        name: "report_done",
        params: doneParams({
          summary: "Wrote the colour-mode docs.",
          changed_files: ["docs/colour.md"],
        }),
      },
    ],
  },
];

type Repo = ReturnType<typeof makeRepo>;

interface HarnessOptions {
  board?: ScriptBoard;
  repo?: Repo;
  scripts?: FakeScript[];
  splitAnswers?: readonly unknown[];
  idleTexts?: readonly string[];
  dryRun?: boolean;
  maxIterations?: number;
  maxConsecutiveFailures?: number;
  timeoutMs?: number;
  abortGraceMs?: number;
  wrapUpMs?: number;
  retryUnfitWork?: boolean;
  machine?: (state: OrchestratorState, event: OrchestratorEvent) => StepResult;
}

interface Harness {
  readonly board: ScriptBoard;
  readonly repo: Repo;
  readonly sessions: ReturnType<typeof fakeSessionFactory>;
  readonly runner: ReturnType<typeof createAgentRunner>;
  readonly splitter: ReturnType<typeof createSplitter>;
  readonly splitPort: ReturnType<typeof fakeSplitPort>;
  readonly finalizer: ReturnType<typeof createFinalizer>;
  readonly ui: ReturnType<typeof recordingUi>;
  readonly idle: ReturnType<typeof scriptedIdle>;
  readonly signals: ReturnType<typeof scriptedSignals>;
  readonly log: LoopLogEntry[];
  readonly ports: LoopPorts;
  run(): Promise<Awaited<ReturnType<typeof runLoop>>>;
  dispose(): void;
}

function harness(options: HarnessOptions = {}): Harness {
  const board = options.board ?? createScriptBoard();
  const repo = options.repo ?? makeRepo();
  const sessions = fakeSessionFactory(options.scripts ?? []);
  const splitPort = fakeSplitPort(options.splitAnswers ?? [SPLIT_BATCH]);
  const runner = createAgentRunner({
    beads: board,
    sessionFactory: sessions.factory,
    cwd: repo.dir,
    includeRepoSnapshot: false,
    timeoutMs: options.timeoutMs,
    abortGraceMs: options.abortGraceMs,
    wrapUpMs: options.wrapUpMs,
  });
  const splitter = createSplitter({ agent: splitPort, beads: board }, {});
  const finalizer = createFinalizer(
    { vcs: repo.writer, beads: board },
    { dryRun: options.dryRun === true },
  );
  const ui = recordingUi();
  const idle = scriptedIdle(options.idleTexts ?? []);
  const signals = scriptedSignals();
  const log: LoopLogEntry[] = [];

  const ports: LoopPorts = {
    beads: board,
    runner,
    splitter,
    finalizer,
    git: repo.writer,
    idle,
    ui,
    signals: signals.adapter,
    log: (entry) => {
      log.push(entry);
    },
  };

  return {
    board,
    repo,
    sessions,
    runner,
    splitter,
    splitPort,
    finalizer,
    ui,
    idle,
    signals,
    log,
    ports,
    run: () =>
      runLoop(ports, {
        maxIterations: options.maxIterations,
        maxConsecutiveFailures: options.maxConsecutiveFailures,
        retryUnfitWork: options.retryUnfitWork,
        dryRun: options.dryRun === true,
        machine: options.machine,
      }),
    dispose() {
      if (options.repo === undefined) repo.dispose();
    },
  };
}

/**
 * The walked scenario needs its files dirty in the working tree first: the
 * finalizer commits what the verdict reported **and** what actually changed, and
 * a path with no change there is skipped rather than committed empty.
 */
function prepareWalk(h: Harness): void {
  h.repo.write("src/colour.ts", "export const colourMode = true;\n");
  h.repo.write("docs/colour.md", "# Colour mode\n");
}

// ── rules 1, 4, 5: the whole walk ─────────────────────────────────────────

test("the whole walk: idle → split → work → finalize → idle, with no rejected step", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [SPLIT_REQUEST] });
  prepareWalk(h);
  try {
    const result = await h.run();

    assert.equal(
      result.kind,
      "done",
      `expected a clean stop, got: ${JSON.stringify(result.transcript.rejections)}`,
    );
    assert.equal(result.exitCode, 0);
    assert.deepEqual(result.transcript.rejections, [], "no step of the walk should be rejected");

    // The board is the record: an epic carrying the request verbatim, the first
    // child closed, its dependent still open.
    assert.equal(h.board.issues.size, 3, "one epic plus two children");
    const epic = [...h.board.issues.values()].find((issue) => issue.issue_type === "epic");
    assert.ok(epic, "the human request was recorded on an epic");
    const epicRecord = `${epic.description ?? ""}\n${epic.notes ?? ""}`;
    assert.match(epicRecord, /Add a colour mode to the parser/, "the request is recorded verbatim");
    assert.equal(h.board.statusOf("tst.2"), "closed", "the first child was worked and closed");
    assert.equal(
      h.board.statusOf("tst.3"),
      "closed",
      "its dependent was worked too — the loop keeps going until the board has nothing left",
    );
    assert.equal(h.board.statusOf(epic.id), "open", "the epic is a container, not the loop's job to close");

    // Both children worked, one commit each, on top of the base commit.
    assert.equal(h.repo.commitCount(), 3, "base commit plus one per worked issue");
    assert.deepEqual(
      h.sessions.sessions.map((session) => session.spec.kind),
      ["work", "work"],
      "the split used a fake port, so the only sessions are the two work runs",
    );

    // Rule 5: one id, agreed by the commit message, the handoff key and the close.
    const body = h.repo.git("log", "--pretty=%B");
    assert.match(body, /^tst\.2: /m, "the commit subject carries the issue id");
    assert.match(body, /^tst\.3: /m);
    assert.match(body, /Loop-Handoff: loop:handoff:tst\.2/);

    const handoff = h.board.memories.get(handoffKeyFor("tst.2")) ?? "";
    assert.ok(handoff.length > 0, "a handoff memory exists for the worked issue");
    const commitOfFirstChild = h.repo
      .git("log", "--pretty=%H %s")
      .split("\n")
      .find((line) => line.includes("tst.2"))
      ?.split(" ")[0];
    assert.ok(commitOfFirstChild, "the first child's commit is in history");
    assert.ok(
      handoff.includes(commitOfFirstChild) || handoff.includes(commitOfFirstChild.slice(0, 7)),
      "the handoff note carries the hash read back from git",
    );

    // The split bound the dependency for real: docs blocked by the parser task.
    const docs = h.board.issues.get("tst.3");
    assert.ok(docs, "the docs child exists");
    assert.ok(
      normaliseDependencies(docs).some((dep) => dep.id === "tst.2"),
      "the #0 index became the real id tst.2",
    );
  } finally {
    h.dispose();
  }
});

test("the effect order on the wire is read → claim → run → commit → remember → close → drop", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [SPLIT_REQUEST] });
  prepareWalk(h);
  try {
    const result = await h.run();
    const kinds = result.transcript.effects.map((entry) => entry.kind);
    const wanted = [
      "beads.read",
      "agent.split",
      "beads.create_issue",
      "beads.set_status",
      "agent.run",
      "vcs.commit",
      "beads.remember",
      "beads.close_issue",
      "drop_context",
    ];

    const positions = new Map<string, number>();
    let cursor = -1;
    for (const kind of wanted) {
      const index = kinds.indexOf(kind, cursor + 1);
      assert.ok(index > cursor, `${kind} never appeared after position ${cursor}`);
      positions.set(kind, index);
      cursor = index;
    }
    const at = (kind: string): number => {
      const value = positions.get(kind);
      assert.ok(value !== undefined, `no recorded position for ${kind}`);
      return value;
    };
    assert.ok(
      at("beads.remember") < at("beads.close_issue"),
      "remember lands before close, not merely somewhere after commit",
    );
    assert.ok(
      at("drop_context") > at("beads.close_issue"),
      "context is dropped only once the iteration is over",
    );

    // The two reads in one batch became ONE observation — never two picks.
    const beforeSplit = kinds.slice(0, at("agent.split"));
    assert.equal(
      beforeSplit.filter((kind) => kind === "beads.read").length,
      1,
      "list_in_progress + list_ready must fold into a single board_observed",
    );
  } finally {
    h.dispose();
  }
});

test("every transition is logged with its iteration and phase, without LOOP_DEBUG", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [SPLIT_REQUEST] });
  prepareWalk(h);
  try {
    const result = await h.run();
    assert.ok(h.log.length > 5, "the phase log should have plenty to say");
    for (const entry of h.log) {
      assert.equal(typeof entry.iteration, "number");
      assert.ok(entry.phase.length > 0, "every log entry names the phase");
      assert.ok(
        entry.message.startsWith(`iteration ${entry.iteration} phase=${entry.phase} `),
        `log lines lead with iteration and phase, got: ${entry.message}`,
      );
    }
    const phases: Set<string> = new Set(h.log.map((entry) => entry.phase));
    for (const phase of ["check_work", "split", "work", "finalize"]) {
      assert.ok(phases.has(phase), `phase ${phase} should appear in the log`);
    }
    assert.equal(result.transcript.logEntries.length, h.log.length);
  } finally {
    h.dispose();
  }
});

// ── rules 6, 7: nothing carries over ───────────────────────────────────────

test("the second iteration's prompt carries the board and the memories, never the last transcript", async () => {
  const PROBE = "PURPLE-PROBE-DO-NOT-LEAK-9f3a";
  const PRIOR_NOTE = "PRIOR-HANDOFF-FOR-tst.3: the parser already handles hex colours.";

  const h = harness({
    scripts: [WORK_SCRIPTS[0] as FakeScript, { ...(WORK_SCRIPTS[1] as FakeScript), reply: `Done. ${PROBE}` }],
    idleTexts: [SPLIT_REQUEST],
  });
  prepareWalk(h);
  try {
    // Stand-in for a previous session that left a note on the second child.
    await h.board.remember(PRIOR_NOTE, handoffKeyFor("tst.3"));

    const result = await h.run();
    assert.equal(result.kind, "done");

    const sessions = h.sessions.sessions;
    assert.equal(sessions.length, 2, "one fresh session per worked issue");
    const first = sessions[0];
    const second = sessions[1];
    assert.ok(first && second, "two sessions were created");
    assert.notEqual(first.sessionId, second.sessionId, "they are not the same session");
    assert.equal(second.promptTexts.length, 1, "exactly one prompt: no continuing conversation");

    const secondPrompt = second.promptTexts.join("\n");
    assert.ok(
      !secondPrompt.includes(PROBE),
      "the previous transcript must not reach the next prompt",
    );
    assert.ok(
      !JSON.stringify(second.spec).includes(PROBE),
      "and must not be smuggled in through the session spec either",
    );

    assert.match(secondPrompt, /Write the colour-mode docs/, "the issue's own title arrives");
    assert.match(secondPrompt, /PRIOR-HANDOFF-FOR-tst\.3/, "recalled memories arrive");

    for (const session of sessions) {
      assert.equal(session.sessionFile, undefined, "in-memory sessions only");
    }
  } finally {
    h.dispose();
  }
});

test("drop_context disposes the session, and the wiring never compacts instead", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [SPLIT_REQUEST] });
  prepareWalk(h);
  try {
    const result = await h.run();
    assert.ok(result.transcript.droppedIterations.length >= 2, "each boundary dropped a context");
    for (const session of h.sessions.sessions) {
      assert.equal(session.disposeCalls, 1, "disposed exactly once");
    }
    assert.equal(h.runner.stats().live, 0, "no live session survives the run");

    for (const file of ["loop.ts", "app.ts", "main.ts"]) {
      assert.ok(
        !/\bcompact\s*\(/.test(readSource(file)),
        `${file} must not call compact() — compacting is not a context reset`,
      );
    }
  } finally {
    h.dispose();
  }
});

// ── rule 2: coverage, and unknown kinds are fatal ───────────────────────────

test("the handler map covers exactly the machine's effect kinds", () => {
  assert.deepEqual(
    [...HANDLED_EFFECT_KINDS].sort(),
    [...EFFECT_KINDS].sort(),
    "every EFFECT_KINDS entry needs a handler, and the loop must not claim extras",
  );
});

test("an effect kind with no handler stops the run instead of doing nothing", async () => {
  const bogus = { kind: "beads.telepathy", thought: "surely the board just knows" };
  const machine = (state: OrchestratorState, event: OrchestratorEvent): StepResult => {
    if (event.type === "start") {
      return { applied: true, state: { ...state, name: "check_work" }, effects: [bogus as never] };
    }
    return step(state, event);
  };
  const h = harness({ machine });
  try {
    const result = await h.run();
    assert.equal(result.kind, "fatal");
    assert.equal(result.exitCode, 2);
    assert.match(result.reason ?? "", /unhandled-effect/);
    assert.match(h.ui.warned.join("\n"), /beads\.telepathy/);
  } finally {
    h.dispose();
  }
});

test("a finalize unit that drifted from the machine's ids stops rather than closing the wrong bead", async () => {
  const h = harness({
    scripts: WORK_SCRIPTS,
    idleTexts: [SPLIT_REQUEST],
    machine: (state, event) => {
      const real = step(state, event);
      // Behaves normally until the handoff lands, then lies about what to close.
      if (real.applied && event.type === "remembered" && state.activeIssueId === "tst.2") {
        return {
          applied: true,
          state: real.state,
          effects: [{ kind: "beads.close_issue", id: "tst.9", reason: "wrong issue on purpose" } as never],
        };
      }
      return real;
    },
  });
  prepareWalk(h);
  // Someone else's bead, blocked so it is never offered as work. The point is
  // that a lie about *what to close* must not close it.
  h.board.seed({ id: "tst.9", title: "Someone else's work", status: "blocked", priority: 9 });
  try {
    const result = await h.run();
    assert.equal(result.kind, "fatal", "a drifting unit must stop the run");
    assert.match(result.reason ?? "", /unit-drift/);
    assert.notEqual(h.board.statusOf("tst.9"), "closed", "the unrelated bead was never closed");
    assert.ok(
      !h.board.calls.some((call) => call.startsWith("closeIssue(tst.9")),
      `no close write for tst.9 reached the board at all; calls were:\n${h.board.calls.join("\n")}`,
    );
  } finally {
    h.dispose();
  }
});

// ── rule 8: a failed split is one warning, then idle ───────────────────────

test("a failed split warns once, creates nothing, and is not re-asked by the loop", async () => {
  const h = harness({
    splitAnswers: [new Error("the split model is unreachable")],
    idleTexts: [SPLIT_REQUEST],
  });
  try {
    const result = await h.run();
    assert.equal(h.splitPort.attempts(), 1, "a split the splitter could not attempt must not be retried here");
    assert.equal(h.board.issues.size, 0, "a failed split creates nothing");
    const splitWarnings = h.ui.warned.filter((line) => /split/i.test(line));
    assert.equal(splitWarnings.length, 1, `expected one split warning, got: ${h.ui.warned.join(" | ")}`);
    assert.match(splitWarnings[0] ?? "", /unreachable/);
    // Back at idle, not stuck: the run takes the next human input and exits.
    assert.equal(result.kind, "done");
  } finally {
    h.dispose();
  }
});

// ── rule 9: a non-done verdict commits nothing ──────────────────────────────

test("an unfinished verdict creates no commit and closes nothing", async () => {
  const board = createScriptBoard();
  const repo = makeRepo();
  try {
    board.seed({ id: "tst.7", title: "Add colour mode", status: "open", priority: 1 });
    repo.write("src/colour.ts", "export const broken = true;\n");
    const before = repo.commitCount();

    const result = await harness({
      board,
      repo,
      scripts: [{ reply: "I got stuck; the tests still fail." }, { reply: "Still stuck." }],
      idleTexts: [],
      maxConsecutiveFailures: 2,
    }).run();

    assert.equal(repo.commitCount(), before, "no commit for a run that did not finish");
    assert.equal(board.statusOf("tst.7"), "open", "the bead stays open and re-queued");
    assert.ok(
      (board.memories.get(failureKeyFor("tst.7")) ?? "").length > 0,
      "the failure reason was remembered before the reopen",
    );
    assert.equal(result.kind, "blocked", "and the loop stopped instead of thrashing");
    assert.equal(result.exitCode, 1);
    assert.match(result.reason ?? "", /tst\.7/);
  } finally {
    repo.dispose();
  }
});

// ── unfit work: running out of the harness's room ─────────────────────────

/**
 * The case these tests exist for: one bead, a session that never comes back, and a
 * budget small enough to watch the arithmetic in a test run rather than in a
 * twenty-minute wait.
 */
function seedingTimeoutBoard(): ScriptBoard {
  const board = createScriptBoard();
  board.seed({ id: "tst.9", title: "Too big for one session", status: "open", priority: 1 });
  return board;
}

test("a run that ran out of time stops the run instead of buying a second one", async () => {
  const board = seedingTimeoutBoard();
  const h = harness({
    board,
    scripts: [{ neverSettle: true, reply: "Still reading the repo." }],
    idleTexts: [],
    timeoutMs: 25,
    abortGraceMs: 10,
  });
  try {
    const result = await h.run();

    assert.equal(h.sessions.sessions.length, 1, "one pass, not two twenty-minute ones");
    assert.equal(result.kind, "blocked");
    assert.match(result.reason ?? "", /tst\.9 timed out/u);
    assert.match(result.reason ?? "", /[Nn]ot running it again in this run/u);
    assert.equal(board.statusOf("tst.9"), "open", "nothing is stranded: the bead is open work");
    assert.match(
      board.memories.get(failureKeyFor("tst.9")) ?? "",
      /harness's limit, not a verdict/u,
      "and what the next attempt reads is about the work, not the clock",
    );
    assert.ok(
      !h.ui.warned.some((line) => /another pass/u.test(line)),
      `nothing may promise a pass that is not coming: ${h.ui.warned.join(" | ")}`,
    );
  } finally {
    h.dispose();
  }
});

test("retryUnfitWork buys the second pass back, and the streak still ends it", async () => {
  const board = seedingTimeoutBoard();
  const h = harness({
    board,
    scripts: [
      { neverSettle: true, reply: "First pass, out of time." },
      { neverSettle: true, reply: "Second pass, out of time." },
    ],
    idleTexts: [],
    timeoutMs: 20,
    abortGraceMs: 5,
    retryUnfitWork: true,
    maxConsecutiveFailures: 2,
  });
  try {
    const result = await h.run();

    assert.equal(h.sessions.sessions.length, 2, "opted in, so the retry happens");
    assert.equal(result.kind, "blocked");
    assert.match(result.reason ?? "", /failed work 2 times in a row/u);
  } finally {
    h.dispose();
  }
});

test("a run that lands on request is a verdict, so it may be tried again", async () => {
  const board = seedingTimeoutBoard();
  const h = harness({
    board,
    scripts: [
      {
        neverSettle: true,
        reply: "Midway.",
        onSteer: {
          name: "report_done",
          params: {
            done: false,
            summary: "Did the parser; the docs are untouched.",
            changed_files: ["src/colour.ts"],
            reason: "ran out of room before the docs",
          },
        },
      },
      {
        neverSettle: true,
        reply: "Midway again.",
        onSteer: {
          name: "report_done",
          params: {
            done: false,
            summary: "Still the parser.",
            changed_files: ["src/colour.ts"],
            reason: "ran out of room again",
          },
        },
      },
    ],
    idleTexts: [],
    timeoutMs: 5_000,
    abortGraceMs: 10,
    wrapUpMs: 20,
    maxConsecutiveFailures: 2,
  });
  try {
    const result = await h.run();

    // A reported "not finished" says something about the work, so the streak —
    // not the unfit-work guard — is what stops this run. That is the difference
    // between a verdict and a cut-off, and it is why the off-ramp exists.
    assert.equal(h.sessions.sessions.length, 2);
    assert.match(result.reason ?? "", /failed work 2 times in a row/u);
    assert.ok(
      !/[Nn]ot running it again/u.test(result.reason ?? ""),
      `an off-ramped run is not unfit work: ${result.reason ?? ""}`,
    );
  } finally {
    h.dispose();
  }
});

// ── rule 10: a refused claim is not work ───────────────────────────────────

test("a guard-mismatch claim refusal starts no session and commits nothing", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [], maxIterations: 3 });
  h.repo.write("src/colour.ts", "export const colourMode = true;\n");
  h.board.seed({ id: "tst.5", title: "Add colour mode", status: "open", priority: 1 });
  // Whoever owns the board keeps moving between our read and our write.
  for (let i = 0; i < 4; i += 1) {
    h.board.failNext("setStatus", new BdError({
      kind: "guard-mismatch",
      message: "tst.5 is closed, not open; bd refused the write",
      exitCode: 13,
    }));
  }
  try {
    const result = await h.run();
    assert.equal(h.runner.stats().created, 0, "a refused claim must never start work");
    assert.equal(h.repo.commitCount(), 1, "and must never commit");
    assert.match(h.ui.warned.join("\n"), /tst\.5/);
    assert.ok(
      result.transcript.transitions.some((entry) => entry.event === "claim_failed"),
      "the refusal went back through the machine as claim_failed",
    );
  } finally {
    h.dispose();
  }
});

// ── rule 11: crash recovery ────────────────────────────────────────────────

test("a lost handoff after the commit recovers without a second commit", async () => {
  const board = createScriptBoard();
  const repo = makeRepo();
  const verdict = {
    tools: [
      {
        name: "report_done",
        params: doneParams({
          summary: "Added the colour mode.",
          changed_files: ["src/colour.ts"],
        }),
      },
    ],
  };
  try {
    board.seed({ id: "tst.4", title: "Add colour mode", status: "open", priority: 1 });
    repo.write("src/colour.ts", "export const colourMode = true;\n");
    // The crash: the commit lands, then the memory write dies.
    board.failNext("remember", new BdError({
      kind: "exit-1",
      message: "bd remember: database is locked",
      exitCode: 1,
    }));

    // Pass one: the commit lands, the memory write dies.
    const first = await harness({
      board,
      repo,
      scripts: [verdict],
      idleTexts: [],
    }).run();
    // The commit stage is over and the next write failed: the loop must stop and
    // say which stage died, not spin and not pretend.
    assert.equal(first.kind, "blocked", `expected blocked, got ${first.kind}: ${first.reason ?? ""}`);
    assert.match(first.reason ?? "", /handoff/i);
    const afterCommit = repo.commitCount();
    assert.equal(afterCommit, 2, "the commit survived the failed handoff");
    assert.equal(board.statusOf("tst.4"), "in_progress", "the bead was not closed");
    assert.equal(board.memories.has(handoffKeyFor("tst.4")), false, "the memory really is missing");

    // Pass two: the finalizer must reuse that commit, not mint another.
    const second = await harness({
      board,
      repo,
      scripts: [verdict],
      idleTexts: [],
    }).run();

    assert.equal(second.kind, "done", `recovery should finish: ${second.reason ?? second.kind}`);
    assert.equal(repo.commitCount(), afterCommit, "exactly one commit exists for the issue");
    assert.equal(board.statusOf("tst.4"), "closed", "and the bead is closed at last");
    assert.ok(board.memories.has(handoffKeyFor("tst.4")), "the handoff was written this time");
  } finally {
    repo.dispose();
  }
});

// ── rule 12: fatal exits ───────────────────────────────────────────────────

test("a missing board is a fatal exit with a message, not an idle prompt", async () => {
  const h = harness({ idleTexts: [] });
  h.board.failNext("listReady", new BdError({ kind: "missing-binary", message: "bd: command not found" }));
  try {
    const result = await h.run();
    assert.equal(result.kind, "fatal");
    assert.equal(result.exitCode, 2);
    assert.match(result.reason ?? "", /bd is not installed|board could not be read/);
    assert.equal(h.ui.said.length, 0, "nothing was announced as idle");
  } finally {
    h.dispose();
  }
});

test("a directory that is not a git work tree is a fatal exit", async () => {
  const dir = mkdtempSync(join(tmpdir(), "loop-wire-notarepo-"));
  const board = createScriptBoard();
  try {
    const strayWriter = createGitWriter({
      cwd: dir,
      env: { HOME: dir, PATH: process.env.PATH ?? "/usr/bin:/bin" },
    });
    const sessions = fakeSessionFactory([]);
    const runner = createAgentRunner({ beads: board, sessionFactory: sessions.factory, cwd: dir });
    const ports: LoopPorts = {
      beads: board,
      runner,
      splitter: createSplitter({ agent: fakeSplitPort([SPLIT_BATCH]), beads: board }, {}),
      finalizer: createFinalizer({ vcs: strayWriter, beads: board }, {}),
      git: strayWriter,
      idle: scriptedIdle([]),
      ui: recordingUi(),
    };
    const result = await runLoop(ports, { preflight: true });
    assert.equal(result.kind, "fatal");
    assert.match(result.reason ?? "", /git work tree|not a git/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ── rule 13: stopping and interrupting ─────────────────────────────────────

test("an exit from idle is a clean stop with nothing left behind", async () => {
  const h = harness({ idleTexts: [] });
  try {
    const result = await h.run();
    assert.equal(result.kind, "done");
    assert.equal(result.exitCode, 0);
    assert.equal(h.idle.disposeCalls, 1, "the idle surface was torn down");
    assert.equal(h.runner.stats().created, 0, "no work session was started");
  } finally {
    h.dispose();
  }
});

test("interrupting mid-work aborts and names the bead left in flight", async () => {
  const h = harness({ scripts: [{ neverSettle: true }], idleTexts: [SPLIT_REQUEST] });
  prepareWalk(h);
  try {
    const running = h.run();
    // Determinism: wait until the work prompt has actually been issued, so the
    // signal lands on running work rather than on setup. `promptTexts` is pushed
    // synchronously at the top of the fake's `prompt()`.
    const started = async (): Promise<boolean> =>
      (h.sessions.sessions[0]?.promptTexts.length ?? 0) > 0;
    for (let i = 0; i < 200 && !(await started()); i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    assert.ok(await started(), "the work session reached the model before the signal");
    h.signals.fire("SIGINT");
    const result = await running;

    assert.equal(result.kind, "aborted");
    assert.equal(result.exitCode, 1);
    const noise = h.ui.warned.join("\n");
    assert.match(noise, /SIGINT/, "the warning names the signal");
    assert.equal(h.sessions.sessions[0]?.abortCalls, 1, "the running session was aborted, not abandoned");
    assert.equal(h.runner.stats().live, 0, "and disposed");
    const lastLine = [...h.ui.warned, ...h.ui.said].join("\n");
    assert.match(lastLine, /tst\.2/, "the last thing said names the bead left in flight");
    assert.match(lastLine, /no commit and no handoff note/, "and says what state it is in");
  } finally {
    h.dispose();
  }
});

// ── dry run ────────────────────────────────────────────────────────────────

test("a dry run prints the finalize plan and commits nothing", async () => {
  const h = harness({
    scripts: [WORK_SCRIPTS[0] as FakeScript],
    idleTexts: [],
    dryRun: true,
  });
  h.repo.write("src/colour.ts", "export const colourMode = true;\n");
  h.board.seed({ id: "tst.6", title: "Add colour mode", status: "open", priority: 1 });
  try {
    const result = await h.run();
    assert.equal(result.kind, "planned");
    assert.equal(result.exitCode, 0);
    assert.equal(h.repo.commitCount(), 1, "nothing was committed");
    assert.match(h.ui.said.join("\n"), /git/, "the printed plan names the commands it would run");
    // The dry run must not claim to cover more than it does.
    assert.match(h.ui.warned.join("\n"), /dry run/i, "the run announces what dry run does not cover");
  } finally {
    h.dispose();
  }
});

// ── epic handling ──────────────────────────────────────────────────────────

test("an epic on the board is not picked as work", async () => {
  const h = harness({ scripts: [WORK_SCRIPTS[0] as FakeScript], idleTexts: [] });
  h.repo.write("src/colour.ts", "export const colourMode = true;\n");
  h.board.seed({ id: "tst.1", title: "Umbrella", status: "open", priority: 0, issue_type: "epic" });
  h.board.seed({ id: "tst.2", title: "Add colour mode", status: "open", priority: 3 });
  try {
    const result = await h.run();
    const runDetails = result.transcript.effects
      .filter((entry) => entry.kind === "agent.run")
      .map((entry) => entry.detail);
    assert.ok(runDetails.some((detail) => detail.startsWith("tst.2:")), `the task was worked: ${runDetails}`);
    assert.ok(!runDetails.some((detail) => detail.startsWith("tst.1:")), "the epic was never worked");
    assert.equal(h.board.statusOf("tst.1"), "open", "the epic stays where it was");
  } finally {
    h.dispose();
  }
});

test("the idle line counts exactly what the loop can pick", async () => {
  // The complaint this guards against: a status line saying there is work while
  // the loop sits idle. Both sides are read off the same board here, so if the
  // filter is ever applied to one and not the other this goes red.
  const h = harness({ scripts: [], idleTexts: [] });
  h.board.seed({
    id: "tst.1",
    title: "Umbrella",
    status: "open",
    priority: 0,
    issue_type: "epic",
  });
  try {
    const result = await h.run();
    const worked = result.transcript.effects.filter((entry) => entry.kind === "agent.run");
    assert.equal(worked.length, 0, `the epic was treated as work: ${JSON.stringify(worked)}`);

    const status = idleStatusFrom(
      await h.board.listReady({}),
      await h.board.listInProgress({}),
    );
    assert.equal(status.ready, 0, "nothing pickable, and the line must not claim otherwise");
    assert.equal(status.heldOut, 1, "the epic is reported rather than quietly dropped");

    const ours = idleStatusFrom(
      await h.board.listReady({}),
      await h.board.listInProgress({}),
      { workEpics: true },
    );
    assert.equal(ours.ready, 1, "told to work epics, the same board is work");
    assert.equal(ours.heldOut, 0);
  } finally {
    h.dispose();
  }
});

// ── the composition root ───────────────────────────────────────────────────

test("buildApp wires the same loop from config, with every adapter replaceable", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [SPLIT_REQUEST] });
  prepareWalk(h);
  try {
    const app = buildApp({
      cwd: h.repo.dir,
      overrides: {
        beads: h.board,
        git: h.repo.writer,
        runner: h.runner,
        splitter: h.splitter,
        finalizer: h.finalizer,
        idle: h.idle,
        ui: h.ui,
        signals: h.signals.adapter,
      },
    });
    const result = await app.run();
    assert.equal(result.kind, "done");
    assert.equal(h.repo.commitCount(), 3, "the composed app walked the whole loop too");
    assert.equal(app.ports.beads, h.board, "the ports are the ones handed in, not secretly rebuilt");
  } finally {
    h.dispose();
  }
});

// ── the idle surface is per turn (workspace-aek) ─────────────────────────────

/**
 * A handle carrying the real module's one fatal habit: whatever it answers, it
 * is dead afterwards. `finished` flips when `next()` resolves, the way
 * `finishAndResolve()` → `teardown()` does in `src/idle.ts` — and a repaint
 * after that is an error, not a no-op, so a wrong lifecycle is caught rather
 * than silently painted over.
 */
class OneShotIdle implements IdleHandle {
  readonly turn: number;
  finished = false;
  nextCalls = 0;
  refreshes = 0;
  disposeCalls = 0;
  /** Unused here; the real surface counts paints, the port never reads them. */
  readonly renderCount = 0;
  private readonly outcome: IdleOutcome;
  private readonly gate: Promise<void>;
  private release!: () => void;

  constructor(turn: number, outcome: IdleOutcome) {
    this.turn = turn;
    this.outcome = outcome;
    this.gate = new Promise<void>((resolve) => {
      this.release = resolve;
    });
  }

  /** The submit, or the exit: the thing that ends this surface's life. */
  answer(): void {
    this.release();
  }

  async next(): Promise<IdleOutcome> {
    if (this.finished) {
      // The real module's exact habit, kept because the whole point of this
      // fake is that reuse is refused rather than quietly tolerated.
      return Promise.reject(
        new IdleError(
          "already-finished",
          "idle mode is already torn down; build a new one instead of waiting on a dead surface",
        ),
      );
    }
    this.nextCalls += 1;
    await this.gate;
    this.finished = true;
    return this.outcome;
  }

  refresh(): void {
    if (this.finished) throw new Error(`turn ${this.turn}: repaint after teardown`);
    this.refreshes += 1;
  }

  async dispose(): Promise<void> {
    if (this.finished) return;
    this.disposeCalls += 1;
    this.finished = true;
    this.release();
  }
}

function turnLedger(outcomes: readonly IdleOutcome[]): {
  made: OneShotIdle[];
  factory: () => OneShotIdle;
} {
  const made: OneShotIdle[] = [];
  const queue = [...outcomes];
  return {
    made,
    factory: (): OneShotIdle => {
      const handle = new OneShotIdle(
        made.length + 1,
        queue.shift() ?? { kind: "exit", reason: "command" },
      );
      made.push(handle);
      return handle;
    },
  };
}

/**
 * A surface out of the ledger that isn't there is a failed assertion, not a
 * crash in the middle of the next one.
 */
function at(handles: readonly OneShotIdle[], index: number): OneShotIdle {
  const handle = handles[index];
  assert.ok(handle !== undefined, `surface ${index} was never made (${handles.length} made)`);
  return handle;
}

/**
 * The presenter, wrapped so the composition's *ordering* is visible: who holds
 * the terminal, and when. It renders nothing — the assertion is about handoff,
 * not pixels.
 */
function recordingPresenter(inner: WorkPresenter, ledger: string[]): WorkPresenter {
  return {
    get isLive(): boolean {
      return inner.isLive;
    },
    get path(): "live" | "plain" {
      return inner.path;
    },
    feed: (event) => inner.feed(event),
    notice: (level, text) => inner.notice(level, text),
    say: (text) => inner.say(text),
    warn: (text) => inner.warn(text),
    setContext: (patch) => inner.setContext(patch),
    setExpanded: (expanded) => inner.setExpanded(expanded),
    toggleExpanded: () => inner.toggleExpanded(),
    acquire: () => {
      ledger.push("presenter:acquire");
      inner.acquire();
    },
    release: () => {
      ledger.push("presenter:release");
      inner.release();
    },
    flushSync: () => inner.flushSync(),
    captureFrame: () => inner.captureFrame(),
    capturePlain: () => inner.capturePlain(),
    stats: () => inner.stats(),
    dispose: () => {
      ledger.push("presenter:dispose");
      inner.dispose();
    },
  };
}

test("answering spends the idle surface, and the next turn is handed a new one", async () => {
  const { made, factory } = turnLedger([
    { kind: "input", text: "first" },
    { kind: "input", text: "second" },
  ]);
  const port = perTurnIdle(factory);

  const firstAsk = port.next();
  await settle(1);
  assert.equal(made.length, 1, "the turn built one surface");
  at(made, 0).answer();
  assert.equal((await firstAsk).kind, "input");
  assert.equal(at(made, 0).finished, true, "the answer tore the surface down");

  // This second ask is what used to kill a session: it reached the spent
  // handle and came back `already-finished`, out through cli() as a fatal.
  const secondAsk = port.next();
  await settle(1);
  assert.equal(made.length, 2, "a fresh surface was built rather than waited on");
  assert.equal(at(made, 1).turn, 2);
  at(made, 1).answer();
  assert.equal(
    (await secondAsk).kind,
    "input",
    "the next turn answers, and answers on a live surface",
  );
  assert.equal(at(made, 0).nextCalls, 1, "the dead handle was asked exactly once, ever");
});

test("an exit spends the surface the same way an input does", async () => {
  const { made, factory } = turnLedger([
    { kind: "exit", reason: "command" },
    { kind: "input", text: "back for more" },
  ]);
  const port = perTurnIdle(factory);

  const firstAsk = port.next();
  await settle(1);
  at(made, 0).answer();
  assert.equal((await firstAsk).kind, "exit");
  assert.equal(at(made, 0).finished, true, "exit tears it down too");

  const secondAsk = port.next();
  await settle(1);
  at(made, 1).answer();
  const second = await secondAsk;
  assert.equal(made.length, 2, "the following turn is a new surface, not a corpse");
  assert.equal(second.kind, "input");
  if (second.kind === "input") {
    assert.equal(second.text, "back for more", "the new surface takes the next thing said");
  }
});

test("a second ask while a turn is open reuses it instead of orphaning the first", async () => {
  const { made, factory } = turnLedger([{ kind: "input", text: "only one" }]);
  const port = perTurnIdle(factory);

  const first = port.next();
  await settle(1);
  // The real surface overwrites its resolver on every `next()`, so a second
  // call on the same handle would leave the first waiter pending forever.
  const second = port.next();
  assert.equal(made.length, 1, "an open turn does not open a second surface");
  at(made, 0).answer();
  assert.deepEqual(await first, { kind: "input", text: "only one" });
  assert.deepEqual(await second, { kind: "input", text: "only one" });
  assert.equal(at(made, 0).nextCalls, 1, "the live handle was asked once, not twice");
});

test("a surface that cannot answer is dropped, not carried into the next turn", async () => {
  const { made, factory } = turnLedger([{ kind: "input", text: "after the failure" }]);
  let calls = 0;
  const flaky = (): IdleHandle => {
    calls += 1;
    if (calls > 1) return factory();
    return {
      next: (): Promise<IdleOutcome> => Promise.reject(new Error("boot-failed")),
      refresh: (): void => undefined,
      dispose: async (): Promise<void> => undefined,
      finished: false,
      renderCount: 0,
    };
  };
  const port = perTurnIdle(flaky);

  await assert.rejects(port.next(), /boot-failed/);
  const recovered = port.next();
  await settle(1);
  at(made, 0).answer();
  assert.equal((await recovered).kind, "input", "the next turn is a live surface");
  assert.equal(calls, 2, "the broken surface was replaced, not retried");
});

test("a refresh repaints the live surface and never takes the keyboard for itself", async () => {
  const { made, factory } = turnLedger([{ kind: "input", text: "x" }]);
  const port = perTurnIdle(factory);

  await port.refresh();
  assert.equal(made.length, 0, "a status repaint must not boot a terminal");

  const pending = port.next();
  await settle(1);
  await port.refresh();
  assert.equal(at(made, 0).refreshes, 1, "the surface that is up gets the repaint");
  at(made, 0).answer();
  await pending;

  await port.refresh();
  assert.equal(made.length, 1, "a spent surface is not replaced by a refresh");
});

test("dispose disposes the current surface exactly once and forgets it", async () => {
  const { made, factory } = turnLedger([{ kind: "input", text: "x" }]);
  const port = perTurnIdle(factory);

  const pending = port.next();
  await port.dispose();
  await pending;
  assert.equal(at(made, 0).disposeCalls, 1);

  await port.dispose();
  assert.equal(made.length, 1, "a second dispose builds nothing");
  assert.equal(at(made, 0).disposeCalls, 1, "and does not re-dispose what is gone");
});

test("a real idle surface boots again for the second turn and answers it", async () => {
  const terms: FakeTerminal[] = [];
  const port = perTurnIdle(() => {
    const term = new FakeTerminal(80, 24);
    terms.push(term);
    return createIdleMode({
      terminal: term as never,
      signals: new FakeSignals() as never,
      status: async () => ({ ready: 0, inProgress: 0 }),
      goodbye: () => undefined,
    });
  });

  const first = port.next();
  await settle();
  assert.equal(terms.length, 1, "the first turn built its surface");
  terms[0]!.input("the first thing on my mind\r");
  assert.equal((await first).kind, "input");

  const second = port.next();
  await settle();
  assert.equal(terms.length, 2, "the second turn built its own surface");
  assert.equal(
    terms[1]!.calls.filter((call) => call === "start").length,
    1,
    "the new surface was started, not merely allocated",
  );
  terms[1]!.input("the second thing\r");
  const answered = await second;
  assert.equal(answered.kind, "input");
  if (answered.kind === "input") {
    assert.equal(answered.text, "the second thing", "the live prompt took the typing");
  }
  await port.dispose();
});

test("a work cycle hands the run back to a live idle surface, twice over", async () => {
  const h = harness({ scripts: WORK_SCRIPTS, idleTexts: [] });
  prepareWalk(h);
  const { made, factory } = turnLedger([
    { kind: "input", text: SPLIT_REQUEST },
    { kind: "exit", reason: "command" },
  ]);
  // A ledger of who holds the terminal, in the order things happened. The
  // presenter is recorded but not rendering — this test is about who is
  // attached when, not about pixels.
  const ledger: string[] = [];
  const presenter = recordingPresenter(createNullPresenter(), ledger);
  // Answer each surface as it is made: the turn is taken when the machine asks.
  const instant = (): IdleHandle => {
    const handle = factory();
    ledger.push(`idle:attach:${handle.turn}`);
    handle.answer();
    return handle;
  };
  try {
    const app = buildApp({
      cwd: h.repo.dir,
      overrides: {
        beads: h.board,
        git: h.repo.writer,
        runner: h.runner,
        splitter: h.splitter,
        finalizer: h.finalizer,
        // The production line, with only the handle kind substituted: the root
        // still applies its own per-turn rule.
        idleFactory: instant,
        presenter,
        ui: h.ui,
        signals: h.signals.adapter,
      },
    });
    const result = await app.run();
    assert.equal(
      result.kind,
      "done",
      `the walk finished instead of dying: ${JSON.stringify(result.transcript.rejections)}`,
    );
    assert.equal(h.repo.commitCount(), 3, "the work cycle really ran between the two turns");
    assert.equal(made.length, 2, "idle was asked twice, and got a surface twice");
    assert.equal(at(made, 1).nextCalls, 1, "the second surface was answered, not skipped");
    assert.deepEqual(
      result.transcript.rejections.filter((record) =>
        /already.?finished|already torn down/i.test(`${record.code} ${record.message}`),
      ),
      [],
      "no dead-surface error escaped the loop",
    );

    // One terminal, two surfaces: every attach happened with the work presenter
    // already released, so the two were never attached at the same time.
    const attaches = ledger.filter((entry) => entry.startsWith("idle:attach:") );
    assert.equal(attaches.length, 2, `both idle turns attached: ${ledger.join(" → ")}`);
    let held = false;
    for (const entry of ledger) {
      if (entry === "presenter:acquire") {
        held = true;
      } else if (entry === "presenter:release") {
        held = false;
      } else if (entry.startsWith("idle:attach:")) {
        assert.ok(
          !held,
          `${entry} while the work presenter still held the surface: ${ledger.join(" → ")}`,
        );
      }
    }
    assert.ok(
      ledger.indexOf("presenter:release") < ledger.indexOf(attaches[0]!),
      `the presenter came down before the first idle went up: ${ledger.join(" → ")}`,
    );
  } finally {
    h.dispose();
  }
});

// ── the surface's cadence is configured, not baked in ────────────────────────

test("the presenter's refresh cadence comes from AppConfig, defaults included", () => {
  const repo = makeRepo();
  const board = createScriptBoard();
  try {
    const configured = buildApp({
      cwd: repo.dir,
      coalesceMs: 16,
      heartbeatMs: 250,
      overrides: { beads: board, git: repo.writer },
    });
    assert.equal(
      configured.presenter.stats().coalesceMs,
      16,
      "the refresh rate is the operator's call, not a hard-coded habit",
    );
    assert.equal(configured.presenter.stats().heartbeatMs, 250);

    const defaults = buildApp({
      cwd: repo.dir,
      overrides: { beads: board, git: repo.writer },
    });
    assert.equal(defaults.presenter.stats().coalesceMs, 33, "default ~30fps");
    assert.equal(defaults.presenter.stats().heartbeatMs, 500);

    // A supplied presenter is supplied whole: the root wires it, it does not
    // re-tune something the caller already decided about.
    const handed = buildApp({
      cwd: repo.dir,
      coalesceMs: 16,
      overrides: { beads: board, git: repo.writer, presenter: createNullPresenter() },
    });
    assert.equal(
      handed.presenter.stats().coalesceMs,
      0,
      "an injected surface keeps its own numbers",
    );
  } finally {
    repo.dispose();
  }
});

// ── rules 0, 15, 16: source guards ────────────────────────────────────────

test("main.ts reads the environment in exactly one place and builds nothing itself", () => {
  const source = readSource("main.ts");
  const envReads = source.match(/process\.env/g) ?? [];
  assert.ok(
    envReads.length <= 1,
    `main.ts should read process.env at most once, found ${envReads.length}`,
  );
  for (const forbidden of [
    "createBdClient",
    "createGitWriter",
    "createAgentRunner",
    "createIdleMode",
    "createSplitter",
    "createFinalizer",
    "createAgentSession",
    "child_process",
  ]) {
    assert.ok(!source.includes(forbidden), `main.ts must not construct ${forbidden}`);
  }
  assert.match(source, /runApp|buildApp/, "main.ts only runs what the composition root built");
});

test("both thinking knobs reach the config, and a bad one stops before the run", async () => {
  const config = readEnv({ LOOP_WORK_THINKING: "High", LOOP_SPLIT_THINKING: " off " });
  assert.equal(config.workThinkingLevel, "high");
  assert.equal(config.splitThinkingLevel, "off");
  assert.equal(readEnv({}).workThinkingLevel, undefined, "absent is unset, never a default");

  const lines: string[] = [];
  const code = await runFromEnv(() => readEnv({ LOOP_WORK_THINKING: "brutal" }), (line) => {
    lines.push(line);
  });
  assert.equal(code, 2);
  assert.match(
    lines.join("\n"),
    /unknown thinking level "brutal"/u,
    "a refused knob is one clear line, not a stack trace",
  );
});

test("the thinking knobs are wired from config to the runner, not dropped on the floor", () => {
  const source = readSource("app.ts");
  assert.match(source, /workThinkingLevel:\s*config\.workThinkingLevel/u);
  assert.match(source, /splitThinkingLevel:\s*config\.splitThinkingLevel/u);
});

test("the budget knobs are read, and reach the runner rather than the readme", () => {
  const env = readEnv({ LOOP_WORK_TIMEOUT_MS: "1200000", LOOP_WRAP_UP_MS: "1020000" });
  assert.equal(env.workTimeoutMs, 1_200_000);
  assert.equal(env.wrapUpMs, 1_020_000);
  assert.equal(readEnv({}).wrapUpMs, undefined, "unset means the derived rule, not zero");
  assert.equal(readEnv({ LOOP_RETRY_UNFIT_WORK: "true" }).retryUnfitWork, true);

  const source = readSource("app.ts");
  assert.match(source, /timeoutMs:\s*config\.workTimeoutMs/u);
  assert.match(source, /wrapUpMs:\s*config\.wrapUpMs/u);
});

test("the wiring builds no ANSI by hand and spawns no processes of its own", () => {
  for (const file of ["loop.ts", "app.ts", "main.ts"]) {
    const source = readSource(file);
    assert.ok(!/\\x1b|\\u001b|\u001b/.test(source), `${file} must not contain a hand-rolled escape`);
    assert.ok(!/child_process|execSync|spawnSync|execFile/.test(source), `${file} must not spawn`);
    assert.ok(!/--assignee|--claim/.test(source), `${file} must not contain a claim flag`);
  }
});
