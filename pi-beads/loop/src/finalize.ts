/**
 * `src/finalize.ts` — FINALIZE: commit, then hand off, then close. Never any
 * other order.
 *
 * The ritual exists because a crash is not an exception handler. Anywhere in
 * this step a process can stop between one write and the next, so the writes are
 * ordered by how much they would lie if they were the last thing that happened:
 *
 *   1. **commit** — the work exists in history, and nothing else has been said.
 *      A crash here leaves the bead `in_progress` with its work uncommitted:
 *      visible, resumable, no false claim.
 *   2. **handoff memory** — the next fresh session is told what happened, with
 *      the hash. A crash here leaves a committed change with no bead closure:
 *      annoying, but the code is not orphaned and nothing was closed unfunded.
 *   3. **close** — the claim of completion, written last because it is the one
 *      statement that must never outrun the other two.
 *
 * Everything else in this module is the consequence of that ordering:
 *
 * - **No commit, no memory.** The handoff text contains the hash read back from
 *   git, so it cannot be built before the commit exists. That is the guarantee
 *   enforced by construction rather than by a check.
 * - **No memory, no close.** Same trick one stage later.
 * - **A failed commit unstages its own paths** (see `GitWriter.execute`) and
 *   the bead stays exactly where it was: `in_progress`, no memory written.
 * - **A second commit for one issue is treated as a bug.** If there is nothing
 *   left to stage and an earlier commit already carries this issue's
 *   `Loop-Handoff` trailer, that commit is reused and the run continues at the
 *   handoff stage. This is what makes a crash between stages recoverable
 *   without duplicating history.
 * - **Nothing is invented.** No reported path produced anything to stage? That
 *   is `nothing-to-commit`, not `--allow-empty`. A commit that says "done"
 *   over an empty diff is worse than no commit at all.
 * - **The plan is the command.** Dry run prints the argv arrays the live path
 *   spawns — the same objects, not a description of them. Git's come from
 *   `GitWriter.planCommit`; bd's come from the same builders `remember` and
 *   `closeIssue` call. `test/finalize.test.ts` compares a dry-run printout
 *   against a live run recorded by a fake binary, so the two cannot drift.
 *
 * Boundaries (each has a source test in `test/finalize.test.ts`):
 *
 * - No process-running import in this file. Git writes go through `src/vcs.js`
 *   and bd writes through `src/beads.js`, both named in the spawn allowlist
 *   that `test/beads.test.ts` enforces. This file is not on that list, and a
 *   stray shell call here fails the build.
 * - No claim path: no assignee flag, no claim flag, no write to the assignee
 *   field. This module closes the bead it was handed; claiming is pi-workgraph's
 *   lease fencing, not the finalizer's business.
 * - No model calls. The finalizer reads a verdict that already happened; it asks
 *   nothing of anyone.
 * - The handoff note is the only bridge to the next iteration — this loop carries
 *   no conversational context — so the note is written to be self-sufficient:
 *   issue, hash, exact paths, what changed, what was decided, what is next.
 */
import { BdError, closeArgv, rememberArgv } from "./beads.ts";
import type { BdClient } from "./beads.ts";
import { COMMIT_TRAILER, VcsError } from "./vcs.ts";
import type { CommitPlan, GitCommand, GitWriter, RefusedPath, SkippedPath } from "./vcs.ts";
import { handoffKeyFor } from "./orchestrator.ts";
import type { OrchestratorEvent, FinalizeStage } from "./orchestrator.ts";

/** The three stages, in the only order they may run. */
export type { FinalizeStage };

/** Placeholder the dry run prints where a hash that does not exist yet belongs. */
export const UNRESOLVED_HASH = "<commit-hash>";

const DEFAULT_CLOSE_PREFIX = "Done";

// ── the request ─────────────────────────────────────────────────────────────

/**
 * Everything the finalize step needs, all of it already decided upstream:
 * `title` from the board, `summary` / `changed_files` / `next_steps` from the
 * `.5` verdict. `decisions` is optional because not every run has a trade-off
 * worth recording, but when it does this is the only place it survives.
 */
export interface FinalizeRequest {
  readonly issueId: string;
  readonly title: string;
  readonly summary: string;
  /** Explicit, from the verdict. Nothing outside this list is ever staged. */
  readonly changedFiles: readonly string[];
  readonly nextSteps?: readonly string[];
  readonly decisions?: readonly string[];
}

export interface FinalizerPorts {
  readonly vcs: GitWriter;
  readonly beads: BdClient;
}

export interface FinalizerConfig {
  /** Print the plan and change nothing. */
  readonly dryRun?: boolean;
  /** Where a dry run prints. Defaults to nowhere, so a test can capture it. */
  readonly onPlan?: (line: string) => void;
  /** Fires as each stage lands, so `.9` can drive the machine incrementally. */
  readonly onEvent?: (event: OrchestratorEvent) => void;
  /**
   * Never make a second commit for one issue. When there is nothing left to
   * stage and an earlier commit carries this issue's handoff trailer, reuse it
   * and continue at the handoff stage. Default true.
   */
  readonly reuseExistingCommit?: boolean;
  /** Prefix for the close reason. Matches the orchestrator's `Done:` phrasing. */
  readonly closePrefix?: string;
  /** Binary names for the printed plan only; the spawners own the real ones. */
  readonly gitBin?: string;
  readonly bdBin?: string;
}

// ── the plan, as data ───────────────────────────────────────────────────────

export interface PlannedCommand {
  readonly stage: FinalizeStage;
  readonly tool: "git" | "bd";
  readonly phase: "read" | "write";
  /** argv without the binary — the same array the spawner will receive. */
  readonly argv: readonly string[];
  readonly note: string;
}

// ── the outcome ─────────────────────────────────────────────────────────────

interface OutcomeBase {
  readonly issueId: string;
  readonly handoffKey: string;
  readonly request: FinalizeRequest;
  readonly commitMessage: string;
  /** `null` until a hash exists to put in it. */
  readonly handoffText: string | null;
  readonly planned: readonly PlannedCommand[];
  readonly planText: readonly string[];
  readonly dryRun: boolean;
  /** Last stage that COMPLETED. `null` when nothing landed. */
  readonly stageReached: FinalizeStage | null;
}

export type FinalizeOutcome =
  | (OutcomeBase & {
      kind: "finalized";
      readonly commitHash: string;
      readonly committedPaths: readonly string[];
      readonly reusedCommit: boolean;
      readonly closedStatus: string;
    })
  | (OutcomeBase & {
      kind: "planned";
      readonly blocking: string | null;
    })
  | (OutcomeBase & {
      kind: "invalid-request";
      readonly problems: readonly string[];
    })
  | (OutcomeBase & {
      kind: "nothing-to-commit";
      readonly skipped: readonly SkippedPath[];
      readonly refused: readonly RefusedPath[];
    })
  | (OutcomeBase & {
      kind: "unsafe-path";
      readonly refused: readonly RefusedPath[];
    })
  | (OutcomeBase & {
      kind: "unrelated-staged";
      readonly foreign: readonly string[];
    })
  | (OutcomeBase & {
      kind: "commit-failed";
      readonly errorKind: string;
      readonly message: string;
      readonly stderr: string;
    })
  | (OutcomeBase & {
      kind: "handoff-failed";
      readonly commitHash: string;
      readonly committedPaths: readonly string[];
      readonly reusedCommit: boolean;
      readonly errorKind: string;
      readonly message: string;
    })
  | (OutcomeBase & {
      kind: "close-failed";
      readonly commitHash: string;
      readonly committedPaths: readonly string[];
      readonly reusedCommit: boolean;
      readonly errorKind: string;
      readonly message: string;
    });

export const FINALIZE_OUTCOME_KINDS = [
  "finalized",
  "planned",
  "invalid-request",
  "nothing-to-commit",
  "unsafe-path",
  "unrelated-staged",
  "commit-failed",
  "handoff-failed",
  "close-failed",
] as const;

export type FinalizeOutcomeKind = (typeof FINALIZE_OUTCOME_KINDS)[number];

/** Only this kind means the iteration is over and the bead is closed. */
export function isFinalized(outcome: FinalizeOutcome): boolean {
  return outcome.kind === "finalized";
}

/** Ids that exist on the board as a result of this call — one, if it closed. */
export function closedId(outcome: FinalizeOutcome): string | null {
  return outcome.kind === "finalized" ? outcome.issueId : null;
}

export function describeFinalizeFailure(outcome: FinalizeOutcome): string {
  switch (outcome.kind) {
    case "finalized":
      return `${outcome.issueId} committed ${outcome.commitHash.slice(0, 9)}, remembered and closed`;
    case "planned":
      return `dry run: ${outcome.planText.length} command(s) printed, nothing changed`;
    case "invalid-request":
      return `finalize request is unusable: ${outcome.problems.join("; ")}`;
    case "nothing-to-commit":
      return `nothing to commit for ${outcome.issueId}; no commit, no memory, no close`;
    case "unsafe-path":
      return `refused unsafe path(s) for ${outcome.issueId}: ` +
        outcome.refused.map((r) => `${r.path} (${r.reason})`).join("; ");
    case "unrelated-staged":
      return `refusing to commit unrelated staged path(s) for ${outcome.issueId}: ` +
        outcome.foreign.join(", ");
    case "commit-failed":
      return `commit failed for ${outcome.issueId} (${outcome.errorKind}): ${outcome.message}`;
    case "handoff-failed":
      return `commit ${outcome.commitHash.slice(0, 9)} exists but the handoff failed for ` +
        `${outcome.issueId} (${outcome.errorKind}): ${outcome.message}. The bead is still open.`;
    case "close-failed":
      return `commit ${outcome.commitHash.slice(0, 9)} and handoff ${outcome.handoffKey} exist ` +
        `but ${outcome.issueId} could not be closed (${outcome.errorKind}): ${outcome.message}`;
  }
}

/**
 * The machine's view of an outcome. Stages that landed emit their event; the
 * first stage that did not emits `finalize_failed`, which is the only event
 * that lets `.9` distinguish "commit is safe, handoff is not" from "nothing
 * happened".
 */
export function toFinalizeEvents(outcome: FinalizeOutcome): OrchestratorEvent[] {
  switch (outcome.kind) {
    case "finalized":
      return [
        { type: "committed", hash: outcome.commitHash },
        { type: "remembered", key: outcome.handoffKey },
        { type: "closed", id: outcome.issueId },
      ];
    case "planned":
      return [];
    case "handoff-failed":
      return [
        { type: "committed", hash: outcome.commitHash },
        { type: "finalize_failed", stage: "handoff", reason: outcome.message },
      ];
    case "close-failed":
      return [
        { type: "committed", hash: outcome.commitHash },
        { type: "remembered", key: outcome.handoffKey },
        { type: "finalize_failed", stage: "close", reason: outcome.message },
      ];
    case "nothing-to-commit":
    case "unsafe-path":
    case "unrelated-staged":
    case "commit-failed":
    case "invalid-request":
      return [
        {
          type: "finalize_failed",
          stage: "commit",
          reason: describeFinalizeFailure(outcome),
        },
      ];
  }
}

// ── rendering ───────────────────────────────────────────────────────────────

/**
 * The commit message. Subject is `<issue-id>: <title>` so `git log --oneline`
 * alone identifies the bead; body carries the verdict summary and the exact
 * path list; the trailer names the handoff key so a reader of the commit can
 * find the full note in bd without asking anyone.
 */
export function renderCommitMessage(
  request: FinalizeRequest,
  handoffKey: string,
  committedPaths: readonly string[] = request.changedFiles,
): string {
  const lines = [
    `${request.issueId.trim()}: ${request.title.trim()}`,
    "",
    request.summary.trim(),
    "",
    "Changed:",
    ...unique(committedPaths).map((path) => `- ${path}`),
    "",
    `${COMMIT_TRAILER}: ${handoffKey}`,
  ];
  return lines.join("\n");
}

/**
 * The handoff note — the only thing that survives the cold boundary, so it is
 * written for a reader who knows nothing: what was done, exactly which files,
 * what was decided, what to do next, and the hash to go read.
 */
export function renderHandoff(
  request: FinalizeRequest,
  commitHash: string,
  committedPaths: readonly string[],
  handoffKey: string,
  options: { readonly reused?: boolean } = {},
): string {
  const lines: string[] = [
    `Finalized ${request.issueId}: ${request.title.trim()}`,
    `Commit: ${commitHash}${options.reused ? " (reused from an earlier run — not committed again)" : ""}`,
    `Summary: ${request.summary.trim()}`,
    `Changed: ${unique(committedPaths).length === 0 ? "(none recorded)" : unique(committedPaths).join(", ")}`,
  ];
  const decisions = (request.decisions ?? []).map((d) => d.trim()).filter((d) => d !== "");
  if (decisions.length > 0) {
    lines.push(`Decisions: ${decisions.join(" | ")}`);
  }
  const next = (request.nextSteps ?? []).map((s) => s.trim()).filter((s) => s !== "");
  lines.push(`Next: ${next.length === 0 ? "(none recorded)" : next.map((s, i) => `${i + 1}) ${s}`).join(" ")}`);
  lines.push(`Handoff key: ${handoffKey}`);
  return lines.join("\n");
}

function unique(values: readonly string[]): string[] {
  const out: string[] = [];
  for (const value of values) {
    const trimmed = value.trim();
    if (trimmed !== "" && !out.includes(trimmed)) out.push(trimmed);
  }
  return out;
}

/**
 * Render a plan for a dry run: `WRITE git commit …` / `read   bd recall …`.
 * The lines are built from the argv arrays themselves, so what a human reads is
 * what would be spawned.
 */
export function formatPlan(commands: readonly PlannedCommand[], bins: { git?: string; bd?: string } = {}): string[] {
  const gitBin = bins.git ?? "git";
  const bdBin = bins.bd ?? "bd";
  return commands.map((command) => {
    const line = `${command.tool === "git" ? gitBin : bdBin} ${command.argv.join(" ")}`;
    return `${command.phase === "write" ? "WRITE" : "read "} [${command.stage}] ${line}`;
  });
}

// ── validation ──────────────────────────────────────────────────────────────

export function validateFinalizeRequest(request: Partial<FinalizeRequest> | null | undefined): string[] {
  if (request === null || typeof request !== "object") {
    return ["the finalize request must be an object"];
  }
  const problems: string[] = [];
  if (typeof request.issueId !== "string" || request.issueId.trim() === "") {
    problems.push("`issueId` is required and must be a non-empty string");
  }
  if (typeof request.title !== "string" || request.title.trim() === "") {
    problems.push("`title` is required and must be a non-empty string — a commit titled " +
      '"id: " tells the next reader nothing"');
  }
  if (typeof request.summary !== "string" || request.summary.trim() === "") {
    problems.push("`summary` is required and must be a non-empty string — it becomes the " +
      "handoff, so an empty one is a note that says nothing");
  }
  if (!Array.isArray(request.changedFiles)) {
    problems.push("`changedFiles` is required and must be an array of paths (empty is fine)");
  } else {
    request.changedFiles.forEach((path, index) => {
      if (typeof path !== "string" || path.trim() === "") {
        problems.push(`\`changedFiles[${index}]\` must be a non-empty path string`);
      }
    });
  }
  for (const field of ["nextSteps", "decisions"] as const) {
    const value = request[field];
    if (value !== undefined && !Array.isArray(value)) {
      problems.push(`\`${field}\` must be an array of strings when present`);
    } else if (Array.isArray(value)) {
      value.forEach((entry, index) => {
        if (typeof entry !== "string") {
          problems.push(`\`${field}[${index}]\` must be a string`);
        }
      });
    }
  }
  return problems;
}

// ── the finalizer ───────────────────────────────────────────────────────────

export interface PlanOk {
  readonly ok: true;
  readonly commitMessage: string;
  readonly handoffKey: string;
  readonly plan: CommitPlan;
  readonly planned: readonly PlannedCommand[];
  readonly planText: readonly string[];
}

export type PlanFailure =
  | { readonly ok: false; readonly reason: "invalid-request"; readonly problems: readonly string[] }
  | { readonly ok: false; readonly reason: "read-failed"; readonly error: unknown };

export type PlanResult = PlanOk | PlanFailure;

export interface Finalizer {
  /** Run the whole ritual, in order, and say exactly where it ended. */
  finalize(request: FinalizeRequest): Promise<FinalizeOutcome>;
  /** Build the plan without writing anything. Read-only; safe to call anywhere. */
  plan(request: FinalizeRequest): Promise<PlanResult>;
  lastOutcome(): FinalizeOutcome | null;
}

/**
 * Build a finalizer bound to one repo and one board.
 *
 * The dry-run/live split lives in one place: both paths call `plan()`, which is
 * read-only, and the live path then hands the plan's own write commands to
 * `GitWriter.execute`. `bd`'s commands are printed from `rememberArgv` /
 * `closeArgv` — the very builders the client calls — so the printed plan is
 * not a claim about the command but the command itself.
 */
export function createFinalizer(ports: FinalizerPorts, config: FinalizerConfig = {}): Finalizer {
  const dryRun = config.dryRun === true;
  const reuseExistingCommit = config.reuseExistingCommit !== false;
  const closePrefix = config.closePrefix ?? DEFAULT_CLOSE_PREFIX;
  const onPlan = config.onPlan;
  const onEvent = config.onEvent;
  const gitBin = config.gitBin ?? "git";
  const bdBin = config.bdBin ?? "bd";

  let last: FinalizeOutcome | null = null;

  function emit(event: OrchestratorEvent): void {
    onEvent?.(event);
  }

  function base(
    request: FinalizeRequest,
    handoffKey: string,
    commitMessage: string,
    planned: readonly PlannedCommand[],
    planText: readonly string[],
    stageReached: FinalizeStage | null,
    handoffText: string | null = null,
  ): OutcomeBase {
    return {
      issueId: request.issueId.trim(),
      handoffKey,
      request,
      commitMessage,
      handoffText,
      planned,
      planText,
      dryRun,
      stageReached,
    };
  }

  function gitPlanCommands(plan: CommitPlan): PlannedCommand[] {
    return plan.commands.map((command: GitCommand) => ({
      stage: "commit" as FinalizeStage,
      tool: "git" as const,
      phase: command.phase,
      argv: command.argv,
      note: command.note,
    }));
  }

  async function buildPlan(
    request: FinalizeRequest,
    handoffKey: string,
    commitMessage: string,
  ): Promise<PlanResult> {
    let plan: CommitPlan;
    try {
      plan = await ports.vcs.planCommit(commitMessage, request.changedFiles);
    } catch (error) {
      // A failed read is not "nothing to do". Hand it back typed so the caller
      // reports a commit-stage failure instead of an empty success.
      return { ok: false, reason: "read-failed", error };
    }
    const handoffPreview = renderHandoff(
      request,
      UNRESOLVED_HASH,
      plan.stageable,
      handoffKey,
    );
    const planned: PlannedCommand[] = [
      ...gitPlanCommands(plan),
      {
        stage: "handoff",
        tool: "bd",
        phase: "write",
        argv: rememberArgv(handoffPreview, handoffKey),
        note: "write the handoff note (hash shown as <commit-hash> until the commit lands)",
      },
      {
        stage: "close",
        tool: "bd",
        phase: "write",
        argv: closeArgv(request.issueId.trim(), `${closePrefix}: ${request.summary.trim()}`),
        note: "close the bead — last, because it is the claim of completion",
      },
    ];
    return {
      ok: true,
      commitMessage,
      handoffKey,
      plan,
      planned,
      planText: formatPlan(planned, { git: gitBin, bd: bdBin }),
    };
  }

  async function plan(request: FinalizeRequest): Promise<PlanResult> {
    const problems = validateFinalizeRequest(request);
    if (problems.length > 0) return { ok: false, reason: "invalid-request", problems };
    const handoffKey = handoffKeyFor(request.issueId);
    const commitMessage = renderCommitMessage(request, handoffKey);
    return buildPlan(request, handoffKey, commitMessage);
  }

  async function finalize(request: FinalizeRequest): Promise<FinalizeOutcome> {
    const problems = validateFinalizeRequest(request);
    if (problems.length > 0) {
      const outcome = {
        ...base(request, safeKey(request), "", [], [], null),
        kind: "invalid-request" as const,
        problems,
      };
      last = outcome;
      emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
      return outcome;
    }

    const handoffKey = handoffKeyFor(request.issueId);
    const commitMessage = renderCommitMessage(request, handoffKey);
    const built = await buildPlan(request, handoffKey, commitMessage);
    if (!built.ok) {
      const outcome = built.reason === "invalid-request"
        ? {
            ...base(request, handoffKey, commitMessage, [], [], null),
            kind: "invalid-request" as const,
            problems: built.problems,
          }
        : failureFromCommitError(request, handoffKey, commitMessage, built.error);
      last = outcome;
      emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
      return outcome;
    }

    const { plan, planned, planText } = built;

    if (dryRun) {
      onPlan?.(`dry run — nothing will be changed for ${request.issueId}`);
      for (const line of planText) onPlan?.(line);
      if (plan.blocking !== null) {
        onPlan?.(`note: ${plan.blocking.message}`);
      }
      const outcome = {
        ...base(request, handoffKey, commitMessage, planned, planText, null),
        kind: "planned" as const,
        blocking: plan.blocking?.kind ?? null,
      };
      last = outcome;
      return outcome;
    }

    // ── stage 1: commit ────────────────────────────────────────────────────
    let commitHash: string;
    let committedPaths: readonly string[];
    let reusedCommit = false;

    if (plan.blocking !== null) {
      const block = plan.blocking;
      if (block.kind === "nothing-to-commit" && reuseExistingCommit) {
        const existing = await ports.vcs.findCommitByTrailer(handoffKey);
        if (existing === null) {
          const outcome = {
            ...base(request, handoffKey, commitMessage, planned, planText, null),
            kind: "nothing-to-commit" as const,
            skipped: plan.skipped,
            refused: plan.refused,
          };
          last = outcome;
          emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
          return outcome;
        }
        commitHash = existing;
        committedPaths = await ports.vcs.commitPaths(existing);
        reusedCommit = true;
      } else if (block.kind === "unsafe-path") {
        const outcome = {
          ...base(request, handoffKey, commitMessage, planned, planText, null),
          kind: "unsafe-path" as const,
          refused: plan.refused,
        };
        last = outcome;
        emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
        return outcome;
      } else if (block.kind === "unrelated-staged") {
        const outcome = {
          ...base(request, handoffKey, commitMessage, planned, planText, null),
          kind: "unrelated-staged" as const,
          foreign: plan.preexistingStaged,
        };
        last = outcome;
        emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
        return outcome;
      } else {
        const outcome = {
          ...base(request, handoffKey, commitMessage, planned, planText, null),
          kind: "nothing-to-commit" as const,
          skipped: plan.skipped,
          refused: plan.refused,
        };
        last = outcome;
        emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
        return outcome;
      }
    } else {
      try {
        const result = await ports.vcs.execute(plan);
        commitHash = result.hash;
        committedPaths = result.paths;
      } catch (error) {
        const outcome = failureFromCommitError(request, handoffKey, commitMessage, error, planned, planText);
        last = outcome;
        emit({ type: "finalize_failed", stage: "commit", reason: describeFinalizeFailure(outcome) });
        return outcome;
      }
    }

    // Stage 1 is over: the work exists in history. The handoff text cannot be
    // built without this hash, which is why "no commit, no memory" needs no
    // guard clause.
    emit({ type: "committed", hash: commitHash });
    const handoffText = renderHandoff(request, commitHash, committedPaths, handoffKey, {
      reused: reusedCommit,
    });

    // ── stage 2: handoff memory ────────────────────────────────────────────
    try {
      await ports.beads.remember(handoffText, handoffKey);
    } catch (error) {
      const outcome = {
        ...base(request, handoffKey, commitMessage, planned, planText, "commit", handoffText),
        kind: "handoff-failed" as const,
        commitHash,
        committedPaths,
        reusedCommit,
        errorKind: bdKind(error),
        message: errorMessage(error),
      };
      last = outcome;
      emit({ type: "finalize_failed", stage: "handoff", reason: describeFinalizeFailure(outcome) });
      return outcome;
    }
    emit({ type: "remembered", key: handoffKey });

    // ── stage 3: close ─────────────────────────────────────────────────────
    const closeReason = `${closePrefix}: ${request.summary.trim()}`;
    try {
      const issue = await ports.beads.closeIssue(request.issueId.trim(), closeReason);
      if (issue.status !== "closed") {
        // bd said it closed something and the status disagrees. Report it as a
        // failed close with the status shown, rather than trusting the call.
        const outcome = {
          ...base(request, handoffKey, commitMessage, planned, planText, "handoff", handoffText),
          kind: "close-failed" as const,
          commitHash,
          committedPaths,
          reusedCommit,
          errorKind: "unexpected-status",
          message: `bd returned status "${issue.status}" after a close; expected "closed"`,
        };
        last = outcome;
        emit({ type: "finalize_failed", stage: "close", reason: describeFinalizeFailure(outcome) });
        return outcome;
      }
      const outcome = {
        ...base(request, handoffKey, commitMessage, planned, planText, "close", handoffText),
        kind: "finalized" as const,
        commitHash,
        committedPaths,
        reusedCommit,
        closedStatus: issue.status,
      };
      last = outcome;
      emit({ type: "closed", id: outcome.issueId });
      return outcome;
    } catch (error) {
      const outcome = {
        ...base(request, handoffKey, commitMessage, planned, planText, "handoff", handoffText),
        kind: "close-failed" as const,
        commitHash,
        committedPaths,
        reusedCommit,
        errorKind: bdKind(error),
        message: errorMessage(error),
      };
      last = outcome;
      emit({ type: "finalize_failed", stage: "close", reason: describeFinalizeFailure(outcome) });
      return outcome;
    }
  }

  function failureFromCommitError(
    request: FinalizeRequest,
    handoffKey: string,
    commitMessage: string,
    error: unknown,
    planned: readonly PlannedCommand[] = [],
    planText: readonly string[] = [],
  ): FinalizeOutcome {
    return {
      ...base(request, handoffKey, commitMessage, planned, planText, null),
      kind: "commit-failed" as const,
      errorKind: VcsError.is(error) ? error.kind : error instanceof Error ? "error" : String(error),
      message: errorMessage(error),
      stderr: VcsError.is(error) ? error.stderr : "",
    };
  }

  return {
    finalize,
    plan,
    lastOutcome: () => last,
  };
}

function safeKey(request: Partial<FinalizeRequest> | null | undefined): string {
  const id = typeof request?.issueId === "string" ? request.issueId.trim() : "";
  return id === "" ? "loop:handoff:<unknown>" : handoffKeyFor(id);
}

function bdKind(error: unknown): string {
  return BdError.is(error) ? error.kind : error instanceof Error ? "error" : String(error);
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

/** Re-exported for `.9`: the trailer value that ties a commit to its note. */
export { COMMIT_TRAILER };
