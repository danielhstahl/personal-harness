/**
 * Tests for `src/orchestrator.ts` — the pure five-step loop machine.
 *
 * No board, no pi, no clock. Every assertion is about the transition the machine
 * took and the effect list it handed back, so "the loop works" is checkable in
 * milliseconds and without side effects.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import type { Issue, IssueStatus, NewIssueSpec } from "../src/beads.ts";
import {
  EFFECT_KINDS,
  EVENT_TYPES,
  FINALIZE_STAGES,
  STATE_NAMES,
  createInitialState,
  failureKeyFor,
  handoffKeyFor,
  isTerminal,
  step,
  type Effect,
  type EffectKind,
  type OrchestratorEvent,
  type OrchestratorState,
  type StepResult,
} from "../src/orchestrator.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const ORCHESTRATOR_SRC = join(HERE, "..", "src", "orchestrator.ts");

// ── fixtures ─────────────────────────────────────────────────────────────────

function mkIssue(id: string, status: IssueStatus, priority = 2, title = `issue ${id}`): Issue {
  return { id, title, status, priority, issue_type: "task" };
}

const SPEC: NewIssueSpec = { title: "child one", priority: 1, type: "task" };

const START: OrchestratorEvent = { type: "start" };
const STOP: OrchestratorEvent = { type: "stop" };
const ABORT: OrchestratorEvent = { type: "abort", reason: "operator pulled the plug" };

function observed(inProgress: readonly Issue[] = [], ready: readonly Issue[] = []): OrchestratorEvent {
  return { type: "board_observed", inProgress, ready };
}

// ── running ──────────────────────────────────────────────────────────────────

interface StepRecord {
  readonly event: OrchestratorEventType;
  readonly applied: boolean;
  readonly effects: readonly Effect[];
  readonly to: string;
}

type OrchestratorEventType = (typeof EVENT_TYPES)[number];

function run(
  events: readonly OrchestratorEvent[],
  start: OrchestratorState = createInitialState(),
): { state: OrchestratorState; steps: StepRecord[] } {
  let state = start;
  const steps: StepRecord[] = [];
  for (const event of events) {
    const result = step(state, event);
    steps.push({
      event: event.type,
      applied: result.applied,
      effects: [...result.effects],
      to: result.state.name,
    });
    state = result.state;
  }
  return { state, steps };
}

function kinds(effects: readonly Effect[]): EffectKind[] {
  return effects.map((effect) => effect.kind);
}

function kindsSince(steps: readonly StepRecord[], fromEvent: OrchestratorEventType): EffectKind[] {
  const index = steps.findIndex((step_) => step_.event === fromEvent);
  assert.notEqual(index, -1, `no ${fromEvent} event in the run`);
  return steps.slice(index).flatMap((step_) => kinds(step_.effects));
}

const EMPTY_BOARD: OrchestratorEvent[] = [START, observed([], [])];

/** A machine sitting in `work`, with `w-1` active. */
function workingMachine(): OrchestratorState {
  return run([
    START,
    observed([], [mkIssue("w-1", "open", 1, "the one")]),
    { type: "issue_claimed", id: "w-1" },
  ]).state;
}

/** A machine part-way through finalize. */
function finalizingMachine(stage: (typeof FINALIZE_STAGES)[number]): OrchestratorState {
  const events: OrchestratorEvent[] = [
    START,
    observed([], [mkIssue("w-1", "open", 1, "the one")]),
    { type: "issue_claimed", id: "w-1" },
    { type: "work_succeeded", summary: "did the thing", changedFiles: ["src/a.ts", "src/b.ts"] },
  ];
  if (stage !== "commit") {
    events.push({ type: "committed", hash: "deadbeef" });
  }
  if (stage === "close") {
    events.push({ type: "remembered", key: handoffKeyFor("w-1") });
  }
  return run(events).state;
}

// ── (a) empty board → IDLE ───────────────────────────────────────────────────

test("(a) an empty board idles instead of inventing work", () => {
  const { state, steps } = run(EMPTY_BOARD);

  assert.equal(state.name, "idle");
  assert.equal(state.activeIssueId, null);
  assert.deepEqual(
    steps.map((step_) => `${step_.event}:${step_.to}`),
    ["start:check_work", "board_observed:idle"],
  );
  // Nothing was written to the board on the way to idle.
  assert.deepEqual(kinds(steps[1]?.effects ?? []), ["ui.say"]);
});

test("(a2) a failed board read does NOT become an idle board", () => {
  const { state } = run([START, { type: "observe_failed", reason: "db unreachable" }]);

  assert.equal(state.name, "check_work", "a read failure is not an empty board");
  assert.equal(isTerminal(state), false);
  // …and `retry` re-requests both reads.
  const retried = step(state, { type: "retry" });
  assert.equal(retried.applied, true);
  assert.deepEqual(kinds(retried.effects), ["beads.list_in_progress", "beads.list_ready"]);
});

// ── (b) in_progress wins ────────────────────────────────────────────────────

test("(b) an in_progress issue is picked by its exact id; ready is ignored", () => {
  const { state, steps } = run([
    START,
    observed([mkIssue("w-resume", "in_progress", 5)], [mkIssue("w-ready", "open", 0)],),
  ]);

  assert.equal(state.name, "work");
  assert.equal(state.activeIssueId, "w-resume", "in-progress wins even at priority 5 vs 0");
  assert.deepEqual(kindsSince(steps, "board_observed"), ["agent.run"]);
  const run_ = steps.flatMap((s) => s.effects).find((e) => e.kind === "agent.run");
  assert.equal(run_?.kind === "agent.run" ? run_.issueId : null, "w-resume");
  // The ignored issue is not so much as mentioned in an effect.
  assert.doesNotMatch(JSON.stringify(steps.flatMap((s) => s.effects)), /w-ready/u);
});

test("(b2) resuming in-progress work writes nothing to the board", () => {
  const { steps } = run([START, observed([mkIssue("w-resume", "in_progress", 1)], [])]);
  const emitted = kindsSince(steps, "board_observed");

  assert.deepEqual(emitted, ["agent.run"]);
  assert.equal(
    steps.flatMap((s) => s.effects).some((e) => e.kind === "beads.set_status" || e.kind === "beads.close_issue"),
    false,
    "no redundant status write for an issue that is already in_progress",
  );
});

test("(b3) two in_progress issues: warn, but still exactly one active", () => {
  const { state, steps } = run([
    START,
    observed(
      [mkIssue("w-b", "in_progress", 2), mkIssue("w-a", "in_progress", 2)],
      [mkIssue("w-c", "open", 0)],
    ),
  ]);

  assert.equal(state.activeIssueId, "w-a", "deterministic tie-break on id");
  const emitted = kindsSince(steps, "board_observed");
  assert.deepEqual(emitted, ["ui.warn", "agent.run"]);
  assert.equal(emitted.filter((kind) => kind === "agent.run").length, 1, "at most one agent.run");
});

test("(b4) ready-only work rests in PICK until the guarded write lands", () => {
  const { state, steps } = run([START, observed([], [mkIssue("w-1", "open", 1)])]);

  assert.equal(state.name, "pick");
  assert.equal(state.activeIssueId, "w-1");
  const claim = steps.at(-1)?.effects[0];
  assert.deepEqual(claim, {
    kind: "beads.set_status",
    id: "w-1",
    status: "in_progress",
    ifStatus: "open",
  });
  assert.equal(
    steps.flatMap((s) => s.effects).some((e) => e.kind === "agent.run"),
    false,
    "nothing is worked before ownership is confirmed",
  );
});

test("(b5) claim lands → WORK; claim refused → back to CHECK_WORK with fresh reads", () => {
  const picked = run([START, observed([], [mkIssue("w-1", "open", 1)])]).state;

  const won = step(picked, { type: "issue_claimed", id: "w-1" });
  assert.equal(won.state.name, "work");
  assert.deepEqual(kinds(won.effects), ["agent.run"]);

  const lost = step(picked, { type: "claim_failed", id: "w-1", reason: "bd exit 13: guard moved" });
  assert.equal(lost.state.name, "check_work");
  assert.deepEqual(kinds(lost.effects), ["ui.warn", "beads.list_in_progress", "beads.list_ready"]);
});

test("(b6) a claim for an issue that was never selected is refused", () => {
  const picked = run([START, observed([], [mkIssue("w-1", "open", 1)])]).state;
  const mismatch = step(picked, { type: "issue_claimed", id: "someone-else" });

  assert.equal(mismatch.applied, false);
  assert.equal(mismatch.rejection.code, "claim-mismatch");

  const working = workingMachine();
  const midWork = step(working, observed([], [mkIssue("w-2", "open", 0)]));
  assert.equal(midWork.applied, false);
  assert.equal(midWork.rejection.code, "already-active-issue");
});

test("(b8) a refused claim leaves a check_work that can read the board again", () => {
  const picked = run([START, observed([], [mkIssue("w-1", "open", 1)])]).state;
  const refused = step(picked, { type: "claim_failed", id: "w-1", reason: "bd exit 13: guard moved" });

  assert.equal(refused.state.name, "check_work");
  assert.equal(refused.state.activeIssueId, null, "the active slot is cleared, so the loop cannot wedge");

  const next = step(refused.state, observed([], [mkIssue("w-2", "open", 2)]));
  assert.equal(next.applied, true);
  assert.equal(next.state.name, "pick");
  assert.equal(next.state.activeIssueId, "w-2");
});

test("(b9) retry clears a stale active slot so a later read still works", () => {
  const stale: OrchestratorState = { ...run([START]).state, activeIssueId: "ghost-1" };
  const retried = step(stale, { type: "retry" });

  assert.equal(retried.applied, true);
  assert.equal(retried.state.activeIssueId, null);

  const read = step(retried.state, observed([], [mkIssue("w-3", "open", 1)]));
  assert.equal(read.applied, true);
  assert.equal(read.state.activeIssueId, "w-3");
});

test("(b7) while one issue is active the machine refuses to pick a second", () => {
  const working = workingMachine();
  const second = step(working, observed([], [mkIssue("w-2", "open", 0)]));

  assert.equal(second.applied, false);
  assert.equal(second.rejection.code, "already-active-issue");
  assert.equal(second.effects.length, 0);
  assert.ok(Object.is(second.state, working), "a rejection hands back the same state object");
});

// ── (c) idle text → SPLIT → CHECK_WORK ──────────────────────────────────────

test("(c) idle text splits into issues and returns to CHECK_WORK through the boundary", () => {
  const input = "make the loop stop leaking context between iterations";
  const { state, steps } = run([
    ...EMPTY_BOARD,
    { type: "human_input", text: input },
    { type: "split_proposed", specs: [SPEC, { ...SPEC, title: "child two" }] },
    { type: "split_created", createdIds: ["n-1", "n-2"] },
  ]);

  assert.equal(state.name, "check_work", "split lands on the board and re-checks");
  assert.equal(state.iteration, 2, "the boundary consumed an iteration");
  assert.equal(state.activeIssueId, null);

  const afterInput = kinds(steps.find((s) => s.event === "human_input")?.effects ?? []);
  assert.deepEqual(afterInput, ["agent.split"]);
  const splitEffect = steps.find((s) => s.event === "human_input")?.effects[0];
  assert.equal(splitEffect?.kind === "agent.split" ? splitEffect.text : null, input, "raw text, unparsed");

  const created = steps.find((s) => s.event === "split_proposed")?.effects ?? [];
  assert.deepEqual(kinds(created), ["beads.create_issue", "beads.create_issue"]);

  assert.deepEqual(kindsSince(steps, "split_created"), [
    "ui.say",
    "drop_context",
    "beads.list_in_progress",
    "beads.list_ready",
  ]);
  const boundary = state.trace.find((entry) => entry.to === "restart");
  assert.ok(boundary, "the cold boundary is visible in the trace");
  assert.equal(boundary?.event, "split_created");
});

test("(c2) blank input never becomes work", () => {
  const idle = run(EMPTY_BOARD).state;
  const blank = step(idle, { type: "human_input", text: "   \n  " });

  assert.equal(blank.applied, false);
  assert.equal(blank.rejection.code, "empty-input");
  assert.equal(blank.state.name, "idle");
});

test("(c3) a duplicate split proposal cannot create the work twice", () => {
  const proposing = run([...EMPTY_BOARD, { type: "human_input", text: "do it" }]).state;
  assert.equal(proposing.splitStage, "propose");

  const first = step(proposing, { type: "split_proposed", specs: [SPEC] });
  assert.equal(first.applied, true);
  assert.equal(first.state.splitStage, "create");
  assert.equal(kinds(first.effects).length, 1);

  const dupe = step(first.state, { type: "split_proposed", specs: [SPEC] });
  assert.equal(dupe.applied, false, "a second proposal in the create stage is refused, not re-created");
  assert.equal(dupe.effects.length, 0);
});

test("(c4) an empty proposal is refused; a failed split goes back to idle", () => {
  const proposing = run([...EMPTY_BOARD, { type: "human_input", text: "do it" }]).state;
  const empty = step(proposing, { type: "split_proposed", specs: [] });
  assert.equal(empty.applied, false);
  assert.equal(empty.rejection.code, "no-children");

  const failed = step(proposing, { type: "split_failed", reason: "the agent had no idea" });
  assert.equal(failed.state.name, "idle", "nothing was created, so nothing is on the board");
  assert.deepEqual(kinds(failed.effects), ["ui.warn"]);
});

test("(c5) a split that failed halfway still gets its created issues worked", () => {
  const creating = run([
    ...EMPTY_BOARD,
    { type: "human_input", text: "do it" },
    { type: "split_proposed", specs: [SPEC] },
  ]).state;

  const partial = step(creating, { type: "split_failed", reason: "died mid-create", createdIds: ["n-1"] });
  assert.equal(partial.state.name, "check_work");
  assert.match(JSON.stringify(partial.effects), /1 issue\(s\) were created/u);
  assert.deepEqual(kinds(partial.effects), [
    "ui.warn",
    "drop_context",
    "beads.list_in_progress",
    "beads.list_ready",
  ]);
});

// ── (d) WORK ok → FINALIZE → CHECK_WORK ─────────────────────────────────────

test("(d) success finalizes in the order commit → remember → close, then re-checks", () => {
  const working = workingMachine();
  const { state, steps } = run(
    [
      { type: "work_succeeded", summary: "did the thing", changedFiles: ["src/a.ts", "src/b.ts"] },
      { type: "committed", hash: "deadbeef" },
      { type: "remembered", key: handoffKeyFor("w-1") },
      { type: "closed", id: "w-1" },
    ],
    working,
  );

  assert.equal(state.name, "check_work");
  assert.equal(state.activeIssueId, null, "the active slot is cleared at the boundary");
  assert.equal(state.iteration, working.iteration + 1);
  assert.equal(state.commitHash, "deadbeef");

  assert.deepEqual(kindsSince(steps, "work_succeeded"), [
    "vcs.commit",
    "beads.remember",
    "beads.close_issue",
    "drop_context",
    "beads.list_in_progress",
    "beads.list_ready",
  ]);

  const [commit, remember, close] = steps.flatMap((s) => s.effects);
  assert.ok(commit?.kind === "vcs.commit");
  assert.equal(commit.message, "w-1: did the thing");
  assert.deepEqual(commit.paths, ["src/a.ts", "src/b.ts"]);
  assert.ok(remember?.kind === "beads.remember");
  assert.equal(remember.key, handoffKeyFor("w-1"));
  assert.match(remember.text, /Changed: src\/a\.ts, src\/b\.ts/u);
  assert.ok(close?.kind === "beads.close_issue");
  assert.equal(close.id, "w-1");
});

test("(d2) finalize cannot be reordered — every out-of-order report is refused", () => {
  const commitStage = finalizingMachine("commit");

  const rememberFirst = step(commitStage, { type: "remembered", key: "loop:handoff:w-1" });
  assert.equal(rememberFirst.applied, false);
  assert.equal(rememberFirst.rejection.code, "stage-mismatch");

  const closeFirst = step(commitStage, { type: "closed", id: "w-1" });
  assert.equal(closeFirst.applied, false);
  assert.equal(closeFirst.rejection.code, "stage-mismatch");

  const handoffStage = finalizingMachine("handoff");
  const closeBeforeHandoff = step(handoffStage, { type: "closed", id: "w-1" });
  assert.equal(closeBeforeHandoff.applied, false, "nothing closes before the handoff exists");
  assert.equal(closeBeforeHandoff.rejection.code, "stage-mismatch");

  const commitAgain = step(handoffStage, { type: "committed", hash: "c0ffee" });
  assert.equal(commitAgain.applied, false, "no second commit");
});

test("(d3) closing the wrong issue is refused", () => {
  const closeStage = finalizingMachine("close");
  const wrong = step(closeStage, { type: "closed", id: "w-other" });

  assert.equal(wrong.applied, false);
  assert.equal(wrong.rejection.code, "close-mismatch");
});

test("(d4) a failed finalize stage writes nothing else and retry repeats just that stage", () => {
  const commitStage = finalizingMachine("commit");
  const failed = step(commitStage, { type: "finalize_failed", stage: "commit", reason: "git index locked" });

  assert.equal(failed.applied, true);
  assert.equal(failed.state.name, "finalize");
  assert.equal(failed.state.finalizeStage, "commit");
  assert.deepEqual(kinds(failed.effects), ["ui.warn"], "only a warning; no remember, no close");
  assert.match(JSON.stringify(failed.effects), /retry/u);

  const mismatched = step(commitStage, { type: "finalize_failed", stage: "close", reason: "nonsense" });
  assert.equal(mismatched.applied, false, "a failure must name the stage actually in flight");

  const retried = step(failed.state, { type: "retry" });
  assert.equal(retried.applied, true);
  assert.deepEqual(kinds(retried.effects), ["vcs.commit"]);
  assert.equal(
    retried.effects[0]?.kind === "vcs.commit" ? retried.effects[0].message : null,
    "w-1: did the thing",
    "retry re-issues the same bytes rather than inventing new ones",
  );
});

test("(d5) nothing closes before its commit exists, across a whole iteration", () => {
  const { steps } = run(
    [
      { type: "work_succeeded", summary: "s", changedFiles: [] },
      { type: "finalize_failed", stage: "commit", reason: "locked" },
      { type: "retry" },
      { type: "committed", hash: "1234abcd" },
      { type: "remembered", key: handoffKeyFor("w-1") },
      { type: "closed", id: "w-1" },
    ],
    workingMachine(),
  );

  const order = steps.flatMap((s) => kinds(s.effects));
  const firstClose = order.indexOf("beads.close_issue");
  const firstCommit = order.indexOf("vcs.commit");
  assert.ok(firstCommit >= 0 && firstClose > firstCommit, "close strictly follows a commit");
  assert.ok(order.indexOf("beads.remember") < firstClose, "the handoff lands before the close");
});

// ── (e) failure re-queues, and nothing is a dead end ─────────────────────────

test("(e) failed work is remembered before it is re-queued, then back to CHECK_WORK", () => {
  const working = workingMachine();
  const { state, steps } = run([{ type: "work_failed", reason: "agent crashed" }], working);

  assert.equal(state.name, "check_work");
  assert.equal(state.activeIssueId, null);
  assert.deepEqual(kinds(steps[0]?.effects ?? []), [
    "beads.remember",
    "beads.set_status",
    "ui.warn",
    "drop_context",
    "beads.list_in_progress",
    "beads.list_ready",
  ]);

  const [remember, reopen] = steps[0]?.effects ?? [];
  assert.ok(remember?.kind === "beads.remember", "the reason is persisted first");
  assert.equal(remember.key, failureKeyFor("w-1"));
  assert.match(remember.text, /Work on w-1 failed: agent crashed/u);
  assert.ok(reopen?.kind === "beads.set_status");
  assert.equal(reopen.id, "w-1");
  assert.deepEqual({ status: reopen.status, ifStatus: reopen.ifStatus }, { status: "open", ifStatus: "in_progress" });
  assert.deepEqual(state.lastFailure, { stage: "work", reason: "agent crashed" });
});

/** A signature collapses the fields that make a resting state distinct. */
function sig(state: OrchestratorState): string {
  return `${state.name}|${state.finalizeStage ?? "-"}|${state.splitStage ?? "-"}|${state.activeIssueId ?? "-"}`;
}

const CHECK_WORK_SIG = "check_work|-|-|-";

function eventVariants(): OrchestratorEvent[] {
  return [
    { type: "start" },
    observed([], []),
    observed([mkIssue("w-resume", "in_progress", 5)], [mkIssue("w-ready", "open", 0)]),
    observed([], [mkIssue("w-1", "open", 1)]),
    observed([mkIssue("w-x", "in_progress", 1), mkIssue("w-y", "in_progress", 1)], []),
    { type: "observe_failed", reason: "db unreachable" },
    { type: "retry" },
    { type: "human_input", text: "split this request please" },
    { type: "human_input", text: "   " },
    { type: "split_proposed", specs: [SPEC] },
    { type: "split_proposed", specs: [] },
    { type: "split_created", createdIds: ["n-1"] },
    { type: "split_created", createdIds: [] },
    { type: "split_failed", reason: "nope" },
    { type: "split_failed", reason: "halfway", createdIds: ["n-1"] },
    { type: "issue_claimed", id: "w-1" },
    { type: "issue_claimed", id: "w-resume" },
    { type: "claim_failed", id: "w-1", reason: "guard moved" },
    { type: "work_succeeded", summary: "s", changedFiles: ["a.ts"] },
    { type: "work_failed", reason: "crash" },
    { type: "committed", hash: "abcd" },
    { type: "remembered", key: handoffKeyFor("w-1") },
    { type: "closed", id: "w-1" },
    { type: "finalize_failed", stage: "commit", reason: "locked" },
    { type: "finalize_failed", stage: "handoff", reason: "memory store down" },
    { type: "finalize_failed", stage: "close", reason: "bd refused" },
    { type: "stop" },
    { type: "abort", reason: "operator" },
  ];
}

/** Every distinct resting state the machine can sit in, by construction. */
function canonicalStates(): OrchestratorState[] {
  const pick = run([START, observed([], [mkIssue("w-1", "open", 1)])]).state;
  const work = workingMachine();
  return [
    createInitialState(),
    run([START]).state,
    run(EMPTY_BOARD).state,
    run([...EMPTY_BOARD, { type: "human_input", text: "go" }]).state,
    run([...EMPTY_BOARD, { type: "human_input", text: "go" }, { type: "split_proposed", specs: [SPEC] }]).state,
    pick,
    step(pick, { type: "issue_claimed", id: "w-1" }).state,
    work,
    finalizingMachine("commit"),
    finalizingMachine("handoff"),
    finalizingMachine("close"),
    run([START, observed([], [mkIssue("w-1", "open", 1)]), STOP]).state,
    run([...EMPTY_BOARD, ABORT]).state,
    // `restart` never rests, but it must still be walkable, so give it a body.
    { ...run([START]).state, name: "restart" },
  ];
}

interface Edge {
  readonly from: string;
  readonly to: string;
  readonly event: string;
}

function explore(from: OrchestratorState): { seen: Map<string, OrchestratorState>; edges: Edge[] } {
  const seen = new Map<string, OrchestratorState>([[sig(from), from]]);
  const queue: OrchestratorState[] = [from];
  const edges: Edge[] = [];
  while (queue.length > 0) {
    const current = queue.shift() as OrchestratorState;
    for (const event of eventVariants()) {
      const result = step(current, event);
      if (!result.applied) continue;
      edges.push({ from: sig(current), to: sig(result.state), event: event.type });
      if (!seen.has(sig(result.state))) {
        seen.set(sig(result.state), result.state);
        queue.push(result.state);
      }
    }
  }
  return { seen, edges };
}

test("(e2) from every non-terminal state, CHECK_WORK is reachable — no dead ends", () => {
  for (const state of canonicalStates()) {
    if (isTerminal(state)) continue;
    const { seen } = explore(state);
    assert.ok(
      seen.has(CHECK_WORK_SIG) || sig(state) === CHECK_WORK_SIG,
      `state ${sig(state)} cannot get back to check_work: reachable = ${[...seen.keys()].join(", ")}`,
    );
  }
});

test("(e2b) no non-terminal state is a corner: some event always changes the state",
  () => {
    for (const canonical of canonicalStates()) {
      const { seen } = explore(canonical);
      for (const [signature, state] of seen) {
        if (signature.startsWith("done|") || signature.startsWith("aborted|")) continue;
        const moves = eventVariants().filter(
          (event) => step(state, event).applied && sig(step(state, event).state) !== signature,
        );
        assert.ok(moves.length > 0, `state ${signature} has no event that makes progress`);
      }
    }
  });

test("(e3) the machine never enters a state outside STATE_NAMES", () => {
  for (const state of canonicalStates()) {
    const { seen } = explore(state);
    for (const entry of seen.keys()) {
      const [name] = entry.split("|") as [string];
      assert.ok((STATE_NAMES as readonly string[]).includes(name), `unknown state ${name}`);
    }
  }
});

test("(e4) only stop/abort may enter a terminal state", () => {
  for (const state of canonicalStates()) {
    const { edges } = explore(state);
    for (const edge of edges) {
      if (edge.to.startsWith("done") || edge.to.startsWith("aborted")) {
        assert.ok(
          edge.event === "stop" || edge.event === "abort",
          `${edge.event} must not reach a terminal state (${edge.from} → ${edge.to})`,
        );
      }
    }
  }
});

test("(e5) terminal states are inert; the cold boundary is transparent", () => {
  for (const terminal of [
    run([START, STOP]).state,
    run([...EMPTY_BOARD, ABORT]).state,
  ]) {
    assert.equal(isTerminal(terminal), true);
    for (const event of eventVariants()) {
      const result = step(terminal, event);
      assert.equal(result.applied, false, `nothing may leave ${terminal.name}`);
      assert.equal(result.rejection.code, "terminal-state");
      assert.ok(Object.is(result.state, terminal));
    }
  }

  // A machine caught mid-boundary is pushed through and then behaves as check_work.
  const midBoundary: OrchestratorState = { ...run([START]).state, name: "restart" };
  const crossed = step(midBoundary, { type: "retry" });
  assert.equal(crossed.applied, true);
  assert.equal(crossed.state.name, "check_work");
});

// ── (f) determinism ─────────────────────────────────────────────────────────

const SCRIPT: OrchestratorEvent[] = [
  START,
  observed([mkIssue("w-9", "in_progress", 3)], [mkIssue("w-1", "open", 0), mkIssue("w-2", "open", 1)]),
  { type: "work_failed", reason: "first pass failed" },
  observed([], [mkIssue("w-1", "open", 0)]),
  { type: "issue_claimed", id: "w-1" },
  { type: "work_succeeded", summary: "second pass", changedFiles: ["src/x.ts"] },
  { type: "committed", hash: "1111" },
  { type: "remembered", key: handoffKeyFor("w-1") },
  { type: "closed", id: "w-1" },
  observed([], []),
  { type: "human_input", text: "now split this" },
  { type: "split_proposed", specs: [SPEC] },
  { type: "split_created", createdIds: ["n-1"] },
  observed([], [mkIssue("n-1", "open", 1)]),
  { type: "issue_claimed", id: "n-1" },
  { type: "stop" },
];

test("(f) the same script produces a byte-identical trace, every time", () => {
  const first = run(SCRIPT).state;
  const second = run(SCRIPT).state;
  const third = run(SCRIPT).state;

  assert.equal(JSON.stringify(second.trace), JSON.stringify(first.trace));
  assert.equal(JSON.stringify(third.trace), JSON.stringify(first.trace));
  assert.equal(first.trace.length > 10, true, "the script actually exercised the machine");
  assert.equal(JSON.stringify(first), JSON.stringify(second), "the whole final state matches too");
});

test("(f2) the trace records from/event/to/effect-kinds for every hop", () => {
  const { state } = run(SCRIPT);
  for (const entry of state.trace) {
    assert.ok((STATE_NAMES as readonly string[]).includes(entry.from), `bad from ${entry.from}`);
    assert.ok((STATE_NAMES as readonly string[]).includes(entry.to), `bad to ${entry.to}`);
    assert.ok((EVENT_TYPES as readonly string[]).includes(entry.event), `bad event ${entry.event}`);
    for (const kind of entry.effects) {
      assert.equal(typeof kind, "string");
    }
  }
  const seqs = state.trace.map((entry) => entry.seq);
  assert.deepEqual(seqs, [...seqs.keys()], "trace is densely numbered from 0");
});

test("(f3) replaying one event on one state is idempotent", () => {
  const state = workingMachine();
  const event: OrchestratorEvent = { type: "work_failed", reason: "same reason" };
  const a = step(state, event);
  const b = step(state, event);

  assert.deepEqual(JSON.stringify(b), JSON.stringify(a), "same state + same event → same result");
});

// ── totality over the whole cross-product ────────────────────────────────────

test("totality: every (state, event) pair is defined — applied, or a rejection that changes nothing", () => {
  const states = canonicalStates();
  const events = eventVariants();
  let appliedCount = 0;
  let rejectedCount = 0;

  for (const state of states) {
    for (const event of events) {
      const result: StepResult | undefined = step(state, event);
      assert.ok(result, `${state.name} × ${event.type} returned nothing`);
      assert.equal(typeof result.applied, "boolean");
      assert.ok(Array.isArray(result.effects));
      assert.ok(state.name !== undefined);

      if (result.applied) {
        appliedCount += 1;
        assert.ok((STATE_NAMES as readonly string[]).includes(result.state.name));
        assert.notEqual(result.state, state, "an applied step returns a new state object");
      } else {
        rejectedCount += 1;
        assert.ok(result.rejection.code.length > 0, `${state.name} × ${event.type}: rejection without a code`);
        assert.ok(result.rejection.message.length > 0);
        assert.equal(result.effects.length, 0, "a rejected step emits no effects");
        assert.ok(Object.is(result.state, state), "a rejected step hands back the same state");
      }
    }
  }

  assert.ok(appliedCount > 30, `expected plenty of applied pairs, got ${appliedCount}`);
  assert.ok(rejectedCount > 100, `expected plenty of rejected pairs, got ${rejectedCount}`);
});

test("every declared event type is live in at least one state", () => {
  const live = new Set<string>();
  for (const state of canonicalStates()) {
    for (const event of eventVariants()) {
      if (step(state, event).applied) live.add(event.type);
    }
  }
  const dead = EVENT_TYPES.filter((type) => !live.has(type));
  assert.deepEqual(dead, [], "an event type nobody can use should not be declared");
});

test("every declared state has a resting canonical form (restart excepted: it never rests)", () => {
  const covered = new Set(canonicalStates().map((state) => state.name));
  const missing = STATE_NAMES.filter((name) => !covered.has(name));
  assert.deepEqual(missing, []);
});

test("the effect vocabulary is exactly what the machine emits", () => {
  const emitted = new Set<EffectKind>();
  for (const canonical of canonicalStates()) {
    const { seen } = explore(canonical);
    for (const state of seen.values()) {
      for (const event of eventVariants()) {
        for (const effect of step(state, event).effects) emitted.add(effect.kind);
      }
    }
  }

  const undeclared = [...emitted].filter(
    (kind) => !(EFFECT_KINDS as readonly string[]).includes(kind),
  );
  assert.deepEqual(undeclared, [], "emitted an effect that is not declared");
  const neverEmitted = EFFECT_KINDS.filter((kind) => !emitted.has(kind));
  assert.deepEqual(neverEmitted, [], "a declared effect nothing emits is dead weight in the interpreter");
});

test("every effect kind has exactly one dispatch target in the ports", () => {
  const dispatch: Record<EffectKind, string> = {
    "beads.list_in_progress": "beads.listInProgress",
    "beads.list_ready": "beads.listReady",
    "beads.set_status": "beads.setStatus",
    "beads.create_issue": "beads.createIssue",
    "beads.close_issue": "beads.closeIssue",
    "beads.remember": "beads.remember",
    "agent.split": "agent.split",
    "agent.run": "agent.run",
    "vcs.commit": "vcs.commit",
    "ui.say": "ui.say",
    "ui.warn": "ui.warn",
    drop_context: "session.dispose",
  };

  assert.deepEqual(
    EFFECT_KINDS.filter((kind) => dispatch[kind] === undefined),
    [],
    "an effect with nowhere to go",
  );
  const targets = Object.values(dispatch);
  assert.equal(new Set(targets).size, targets.length, "two effects must not share one dispatch target");
});

// ── purity ───────────────────────────────────────────────────────────────────

/** Comments are allowed to talk about the outside world; code is not. */
function codeOnly(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//gu, "").replace(/\/\/.*$/gmu, "");
}

test("purity: the orchestrator imports nothing at runtime — not even the beads adapter", () => {
  const code = codeOnly(readFileSync(ORCHESTRATOR_SRC, "utf8"));
  const imports = code.match(/^[ \t]*import[^\n]*/gmu) ?? [];

  assert.ok(imports.length > 0, "expected at least the type-only import");
  for (const line of imports) {
    assert.match(line.trim(), /^import\s+type\b/u, `runtime import in the orchestrator: ${line.trim()}`);
  }
});

test("purity: no I/O, no clock, no randomness, no environment access", () => {
  const code = codeOnly(readFileSync(ORCHESTRATOR_SRC, "utf8"));
  const forbidden: readonly (readonly [string, RegExp])[] = [
    ["any node: specifier", /node:/u],
    ["filesystem", /\bfs\b|readFileSync|writeFileSync/u],
    ["process/global spawn", /child_process|execFile|execSync|\bspawn\b/u],
    ["environment", /process\.[a-zA-Z]/u],
    ["logging", /console\./u],
    ["clock", /\bDate\b|performance\./u],
    ["randomness", /Math\.random|randomUUID/u],
    ["timers", /setTimeout|setInterval|setImmediate/u],
    ["network", /\bfetch\(|XMLHttpRequest|net\.connect/u],
    ["async (the machine is synchronous)", /\basync\b|\bawait\b|\.then\(/u],
  ];
  for (const [label, pattern] of forbidden) {
    const match = code.match(pattern);
    assert.equal(match, null, `orchestrator must not contain ${label}${match ? `: ${match[0]}` : ""}`);
  }
});

test("purity: the only module it can even name is beads, as a type", () => {
  const code = codeOnly(readFileSync(ORCHESTRATOR_SRC, "utf8"));
  const specifiers = [...code.matchAll(/from\s+"([^"]+)"/gu)].map((m) => m[1] as string);
  assert.deepEqual(specifiers, ["./beads.ts"]);
});

// ── reference safety ─────────────────────────────────────────────────────────

test("the state holds no live reference to caller-supplied issue objects", () => {
  const issue = mkIssue("w-mut", "open", 1, "original title");
  const state = run([START, observed([], [issue]), { type: "issue_claimed", id: "w-mut" }]).state;

  issue.title = "mutated after the fact";
  issue.priority = 0;

  assert.equal(state.activeIssueTitle, "original title");
  assert.equal(state.activeIssueId, "w-mut");
});

test("mutating a returned effect cannot reach the state", () => {
  const { state, steps } = run([
    START,
    observed([], [mkIssue("w-1", "open", 1)]),
    { type: "issue_claimed", id: "w-1" },
  ]);

  const effect = steps.at(-1)?.effects[0] as { kind: string; issueId?: string } | undefined;
  assert.ok(effect);
  effect.issueId = "hijacked";

  assert.equal(state.activeIssueId, "w-1");
  assert.doesNotMatch(JSON.stringify(state), /hijacked/u);
});

test("split specs are carried into the create effect without aliasing state", () => {
  const spec: NewIssueSpec = { title: "child one", priority: 1 };
  const creating = run([...EMPTY_BOARD, { type: "human_input", text: "go" }, { type: "split_proposed", specs: [spec] }])
    .state;

  spec.title = "changed later";
  assert.doesNotMatch(JSON.stringify(creating.createdIssueIds), /changed later/u);
  const effect = step(
    run([...EMPTY_BOARD, { type: "human_input", text: "go" }, { type: "split_proposed", specs: [{ title: "fresh", priority: 2 }] }]).state,
    { type: "split_created", createdIds: ["n-1"] },
  );
  assert.equal(effect.state.name, "check_work");
});
