/**
 * pi-beads loop — the interpreter (workspace-5yn.9).
 *
 * Everything before this ticket built parts with hard edges and no way to run
 * them. This module is the place where they meet: it runs
 * {@link step} over the pure machine from `.4` and executes the effects that
 * come out against real things — `bd` (`.3`), a fresh agent session (`.5`),
 * the idle surface (`.6`), the splitter (`.7`), the finalizer (`.8`), git
 * (`.8`'s writer).
 *
 * Four rules hold this together, and `test/loop.test.ts` is the check on each.
 *
 * 1. **One machine.** `step()` is the only thing that decides what happens
 *    next. This module never branches on "what phase are we in" to choose an
 *    action of its own; it performs the effect the machine asked for and hands
 *    back the event that says what happened. A step that would be rejected is
 *    reported, not papered over.
 *
 * 2. **Every effect kind has exactly one handler.** The handler map is typed
 *    `Record<EffectKind, Handler>`, so forgetting a kind is a compile error,
 *    and a kind that arrives at runtime with no handler raises
 *    {@link LoopError} `unhandled-effect` instead of vanishing. A silently
 *    dropped effect is the single worst failure mode a loop like this has:
 *    the machine believes the world changed and nobody changed it.
 *
 * 3. **Two effects are executed as a *unit*, not separately, and this is
 *    deliberate, not a shortcut.**
 *    - `agent.split` runs `.7`'s whole transaction (propose → record the
 *      human's request verbatim → create the batch through the ledger, which
 *      is the only place intra-batch `#index` tokens become real ids). The
 *      machine's per-spec `beads.create_issue` effects are dispatched into that
 *      same ledger, so the writes the machine asked for are the writes that
 *      happen — plus the epic record and the dependency edges, which the
 *      effect vocabulary has no kind for. Those extras are logged, never
 *      invisible.
 *    - `vcs.commit` runs `.8`'s whole ritual (commit → remember → close).
 *      Running the three machine effects one by one instead would mean the
 *      handoff note is the machine's two-line summary with **no commit hash**
 *      in it — the hash exists only in the executor, and `.8` rule 10 requires
 *      the note to carry it. So the finalizer performs the trio, and the
 *      events it produces are fed back through `step()`. The machine's own
 *      follow-up `beads.remember` / `beads.close_issue` effects are then
 *      marked *already performed by the unit* rather than run a second time,
 *      under a drift guard: if the key or the id disagrees with what the unit
 *      actually did, that is `LoopError` `unit-drift` and the run stops.
 *
 * 4. **Nothing is invented when a stage fails.** A non-`done` verdict never
 *    reaches the commit stage; a blocked finalize ends the run in the
 *    `blocked` result naming the stage, the commit that exists and the bead
 *    that may still be open. No auto-retry of a write, because a write that
 *    failed once is a fact about the world, not a hiccup.
 *
 * Context does not carry. That is not this module's job to enforce so much as
 * its job to *not break*: each iteration's work goes through `AgentRunner`,
 * which builds a fresh in-memory session and an assembled prompt out of board
 * payloads and named `bd` memories only. The loop keeps no transcript of its
 * own, drops every session at `drop_context`, and starts the next iteration
 * from a board read. See rule 6/7 in `test/loop.test.ts`.
 */
import { BdError, selectWorkable } from "./beads.ts";
import type { BdClient, Issue } from "./beads.ts";
import type { AgentRunner, WorkOutcome } from "./agent.ts";
import { toWorkEvent } from "./agent.ts";
import type { FinalizeOutcome, FinalizeRequest } from "./finalize.ts";
import { describeFinalizeFailure, toFinalizeEvents } from "./finalize.ts";
import type { IdleOutcome } from "./idle.ts";
import { createInitialState, failureKeyFor, handoffKeyFor, step } from "./orchestrator.ts";
import type {
  AgentRunEffect,
  CloseIssueEffect,
  CreateIssueEffect,
  DropContextEffect,
  Effect,
  OrchestratorEvent,
  OrchestratorState,
  OrchestratorStateName,
  RememberEffect,
  SetStatusEffect,
  StepResult,
  VcsCommitEffect,
} from "./orchestrator.ts";
import type { SplitLedger, Splitter, SplitProposal } from "./split.ts";
import {
  createSplitLedger,
  formatSplitProblem,
  recordHumanRequest,
  toNewIssueSpecs,
} from "./split.ts";
import type { GitWriter } from "./vcs.ts";

// ── the seam: what the loop needs from outside ───────────────────────────────

/** Where human-facing lines go. The loop builds no styling of its own (rule 16). */
export interface LoopUi {
  say(text: string): void;
  warn(text: string): void;
}

/**
 * The idle surface (`.6`), narrowed to what the loop uses.
 *
 * `refresh` and `dispose` are optional so a scripted test port can be tiny, but
 * `createIdleMode`'s {@link IdleHandle} satisfies this shape directly.
 */
export interface LoopIdlePort {
  next(): Promise<IdleOutcome>;
  refresh?(): void | Promise<void>;
  dispose?(): Promise<void> | void;
}

/** Injectable signal source, so a test never has to kill its own process. */
export interface LoopSignalAdapter {
  on(signal: string, handler: () => void): () => void;
}

export interface LoopLogEntry {
  readonly at: number;
  readonly iteration: number;
  readonly phase: OrchestratorStateName;
  readonly level: "info" | "debug";
  /** Pre-formatted so a bare log file still reads: `iteration 2 phase=work …`. */
  readonly message: string;
}

export interface LoopPorts {
  readonly beads: BdClient;
  readonly runner: AgentRunner;
  readonly splitter: Splitter;
  readonly finalizer: FinalizerLike;
  readonly git: GitWriter;
  readonly idle: LoopIdlePort;
  readonly ui: LoopUi;
  readonly signals?: LoopSignalAdapter;
  readonly log?: (entry: LoopLogEntry) => void;
  readonly now?: () => number;
}

/** Narrowed so a test can hand in a scripted finalizer. */
export interface FinalizerLike {
  finalize(request: FinalizeRequest): Promise<FinalizeOutcome>;
}

export interface LoopConfig {
  /** Print the finalize plan; the loop reports it and stops without writing. */
  readonly dryRun?: boolean;
  /** Log every effect and every completed handler. */
  readonly verbose?: boolean;
  /** Runaway guard. Default 1000 iterations. */
  readonly maxIterations?: number;
  /**
   * Stop after the same issue fails work this many times in a row.
   *
   * Without this the loop can thrash: a failure re-queues the issue, the next
   * read finds it ready, and nothing about the world has changed. Default 2 —
   * one retry, then stop and tell a human. The reason is already on the board
   * under the failure key, so "what went wrong" is not lost by stopping.
   */
  readonly maxConsecutiveFailures?: number;
  /**
   * Stop after this many claim attempts in a row that the board refused.
   *
   * A refused claim re-reads the board and tries again inside the same
   * iteration, so without a cap this spins on `pick → claim_failed → read →
   * pick` forever and never bumps the iteration counter that would otherwise
   * save us. Default 3.
   */
  readonly maxConsecutiveClaimFailures?: number;
  /**
   * Stop after this many idle turns in a row that did not move the machine.
   *
   * A person typing for an hour is not a runaway; input that the machine keeps
   * refusing is. Default 3.
   */
  readonly maxIdleSpins?: number;
  /** Record the human request on this epic instead of minting one. */
  readonly epicId?: string | null;
  readonly epicTitle?: string;
  /** An epic is a container, not a task: default 4 so children sort first. */
  readonly epicPriority?: 0 | 1 | 2 | 3 | 4;
  /** Let epic-type issues be picked as work. Default false. */
  readonly workEpics?: boolean;
  /** Re-read the board once after a failed read. Default true. */
  readonly autoRetryObserve?: boolean;
  /** Check that `bd` and a git work tree are reachable before starting. */
  readonly preflight?: boolean;
  /** Labels forwarded to the board reads. */
  readonly labels?: readonly string[];
  /**
   * The state function to run. Defaults to the real `step`.
   *
   * Injectable for one reason: the two worst failures this loop can have — an
   * effect kind nobody handles, and a unit that drifted from what the machine
   * asked for — cannot be produced by the real machine, so a test can only
   * check that they are survivable by substituting one that does them.
   */
  readonly machine?: (state: OrchestratorState, event: OrchestratorEvent) => StepResult;
}

// ── what comes back ─────────────────────────────────────────────────────────

export interface TransitionRecord {
  readonly seq: number;
  readonly from: OrchestratorStateName;
  readonly event: string;
  readonly to: OrchestratorStateName;
}

export interface EffectRecord {
  readonly kind: string;
  readonly detail: string;
}

export interface RejectionRecord {
  readonly state: OrchestratorStateName;
  readonly event: string;
  readonly code: string;
  readonly message: string;
}

/** The whole run as data, so a test asserts a history rather than a side effect. */
export interface LoopTranscript {
  readonly transitions: readonly TransitionRecord[];
  readonly effects: readonly EffectRecord[];
  readonly rejections: readonly RejectionRecord[];
  readonly notes: readonly string[];
  readonly createdIssueIds: readonly string[];
  readonly closedIssueIds: readonly string[];
  readonly droppedIterations: readonly number[];
  readonly logEntries: readonly LoopLogEntry[];
}

export type LoopResultKind =
  | "done"
  | "aborted"
  | "blocked"
  | "planned"
  | "iteration-limit"
  | "fatal";

export interface LoopResult {
  readonly kind: LoopResultKind;
  readonly exitCode: number;
  readonly iterations: number;
  readonly reason: string | null;
  readonly transcript: LoopTranscript;
}

/** Everything the loop refuses to paper over arrives as one of these. */
export class LoopError extends Error {
  readonly code: string;
  readonly detail: unknown;

  constructor(code: string, message: string, detail: unknown = null) {
    super(message);
    this.name = "LoopError";
    this.code = code;
    this.detail = detail;
  }

  static is(value: unknown): value is LoopError {
    return value instanceof LoopError;
  }
}

const EXIT_BY_KIND: Readonly<Record<LoopResultKind, number>> = {
  done: 0,
  planned: 0,
  aborted: 1,
  blocked: 1,
  "iteration-limit": 0,
  fatal: 2,
};

const READ_EFFECTS: readonly string[] = ["beads.list_in_progress", "beads.list_ready"];

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function kindOf(error: unknown): string {
  if (BdError.is(error)) return error.kind;
  if (error instanceof Error) return error.name;
  return "unknown";
}

/** A bd status write is a *claim* only while the machine is in `pick`. */
function isClaim(state: OrchestratorState, effect: SetStatusEffect): boolean {
  return state.name === "pick" && effect.status === "in_progress";
}

function describeProposalFailure(proposal: SplitProposal): string {
  if (proposal.modelMessage !== null && proposal.problems.length === 0) {
    return `the split agent failed: ${proposal.modelMessage}`;
  }
  if (proposal.problems.length > 0) {
    return `the split was rejected: ${proposal.problems.map(formatSplitProblem).join("; ")}`;
  }
  return "the split produced no issues";
}

function stripIssuePrefix(message: string, issueId: string): string {
  const prefix = `${issueId}: `;
  return message.startsWith(prefix) ? message.slice(prefix.length) : message;
}

// ── the engine ──────────────────────────────────────────────────────────────

/** A unit in flight: a group of machine effects executed as one transaction. */
type Unit =
  | {
      readonly kind: "split";
      readonly ledger: SplitLedger;
      readonly created: { index: number; id: string; title: string }[];
      readonly failed: { index: number; title: string; errorKind: string }[];
    }
  | { readonly kind: "finalize"; readonly request: FinalizeRequest };

/**
 * Run one loop: board → idle → split → work → finalize → board → …
 *
 * Returns a {@link LoopResult}. It does not throw for a normal stop; it throws
 * nothing at all — failures arrive as `kind: "fatal"` with the message that
 * should reach the human's eyes.
 */
export async function runLoop(
  ports: LoopPorts,
  config: LoopConfig = {},
): Promise<LoopResult> {
  const now = ports.now ?? ((): number => Date.now());
  const maxIterations = config.maxIterations ?? 1000;
  const maxConsecutiveFailures = config.maxConsecutiveFailures ?? 2;
  const maxConsecutiveClaimFailures = config.maxConsecutiveClaimFailures ?? 3;
  const maxIdleSpins = config.maxIdleSpins ?? 3;
  const workEpics = config.workEpics === true;

  const transitions: TransitionRecord[] = [];
  const effects: EffectRecord[] = [];
  const rejections: RejectionRecord[] = [];
  const notes: string[] = [];
  const createdIssueIds: string[] = [];
  const closedIssueIds: string[] = [];
  const droppedIterations: number[] = [];
  const logEntries: LoopLogEntry[] = [];

  let state: OrchestratorState = createInitialState();
  let unit: Unit | null = null;
  let lastWork: WorkOutcome | null = null;
  // A holder object, not a bare `let`: the driver reads this after a nested
  // function has written it, and a property reference is not narrowed away the
  // way a captured local would be.
  const dryPlan: { lines: readonly string[] | null } = { lines: null };
  let observeRetries = 0;
  let idleSpins = 0;
  let blockedReason: string | null = null;
  let failedIssue: string | null = null;
  let consecutiveFailures = 0;
  let refusedClaimIssue: string | null = null;
  let refusedClaimStreak = 0;
  /**
   * A runaway guard that tripped in the middle of the pump.
   *
   * The pump feeds itself: every boundary hands it new effects, so a guard
   * checked only *between* pumps would never be reached while the loop is
   * actually spinning. Both guards live here so the pump can stop itself and
   * the driver can say what stopped it.
   */
  let guardTripped: { kind: LoopResultKind; reason: string } | null = null;

  function guardTrip(): { kind: LoopResultKind; reason: string } | null {
    if (guardTripped !== null) return guardTripped;
    if (state.iteration > maxIterations) {
      guardTripped = {
        kind: "iteration-limit",
        reason: `stopped after ${maxIterations} iterations; this looks like a runaway loop`,
      };
      return guardTripped;
    }
    if (failedIssue !== null && consecutiveFailures >= maxConsecutiveFailures) {
      guardTripped = {
        kind: "blocked",
        reason:
          `${failedIssue} failed work ${consecutiveFailures} times in a row; stopping instead ` +
          `of thrashing. The reason is on the board under ${failureKeyFor(failedIssue)}.`,
      };
      return guardTripped;
    }
    if (refusedClaimIssue !== null && refusedClaimStreak >= maxConsecutiveClaimFailures) {
      guardTripped = {
        kind: "blocked",
        reason:
          `${refusedClaimIssue} was refused ${refusedClaimStreak} claims in a row; the board and ` +
          "our view of it disagree, and re-reading has not fixed that. Someone should look.",
      };
      return guardTripped;
    }
    return null;
  }
  let signalName: string | null = null;
  /** True when the signal arrived while something was actually running. */
  let signalMidWork = false;
  /** The bead that was in flight at that same instant, captured with it. */
  let signalLeftIssue: string | null = null;
  let signalLeftTitle: string | null = null;
  let finished = false;

  function log(level: "info" | "debug", message: string): void {
    const entry: LoopLogEntry = {
      at: now(),
      iteration: state.iteration,
      phase: state.name,
      level,
      message: `iteration ${state.iteration} phase=${state.name} ${message}`,
    };
    logEntries.push(entry);
    ports.log?.(entry);
  }

  function say(text: string): void {
    notes.push(`say: ${text}`);
    ports.ui.say(text);
  }

  function warn(text: string): void {
    notes.push(`warn: ${text}`);
    ports.ui.warn(text);
    log("info", `warn: ${text}`);
  }

  /**
   * The only place `step` is called. An applied transition advances the state
   * and yields its effects; a rejected one leaves the state exactly as it was
   * and is reported loudly — a rejection means the wiring asked for something
   * the machine forbids, which is a bug worth seeing, not swallowing.
   */
  function feed(event: OrchestratorEvent): Effect[] {
    const from = state.name;
    const result = (config.machine ?? step)(state, event);
    if (!result.applied) {
      rejections.push({
        state: from,
        event: event.type,
        code: result.rejection.code,
        message: result.rejection.message,
      });
      warn(
        `machine rejected ${event.type} in ${from}: ${result.rejection.code} — ` +
          `${result.rejection.message}`,
      );
      return [];
    }
    state = result.state;
    transitions.push({ seq: transitions.length + 1, from, event: event.type, to: state.name });
    log("info", `event=${event.type} ${from} -> ${state.name}`);
    if (config.verbose === true && result.effects.length > 0) {
      log("debug", `effects: ${result.effects.map((effect) => effect.kind).join(", ")}`);
    }
    return [...result.effects];
  }

  function finish(kind: LoopResultKind, reason: string | null): LoopResult {
    finished = true;
    log("info", `run ${kind}${reason === null ? "" : `: ${reason}`}`);
    return {
      kind,
      exitCode: EXIT_BY_KIND[kind],
      iterations: Math.max(0, state.iteration - 1),
      reason,
      transcript: {
        transitions,
        effects,
        rejections,
        notes,
        createdIssueIds,
        closedIssueIds,
        droppedIterations,
        logEntries,
      },
    };
  }

  // ── board reads: two effects, one atomic observation ─────────────────────

  /**
   * `check_work` emits `list_in_progress` and `list_ready` together because the
   * pick decision has to see both at one instant. They are executed as a pair
   * here and produce exactly **one** `board_observed`.
   *
   * Epic-type issues are held back from the lists handed to the machine (unless
   * `workEpics`), because an epic is a container: picking it means "work the
   * umbrella", which is never what the human meant. The exclusion is stated in
   * the log, so the observation is never quietly smaller than the board.
   */
  async function observeBoard(requested: readonly Effect[]): Promise<Effect[]> {
    const wantInProgress = requested.some((effect) => effect.kind === "beads.list_in_progress");
    const wantReady = requested.some((effect) => effect.kind === "beads.list_ready");
    const labels = config.labels === undefined ? {} : { labels: config.labels };

    let inProgress: Issue[] = [];
    let ready: Issue[] = [];
    try {
      if (wantInProgress) inProgress = await ports.beads.listInProgress(labels);
      if (wantReady) ready = await ports.beads.listReady(labels);
    } catch (error) {
      const reason = `${kindOf(error)}: ${messageOf(error)}`;
      effects.push({ kind: "beads.read", detail: `failed(${reason})` });
      return feed({ type: "observe_failed", reason });
    }

    // One definition of "pickable", shared with the idle status line by way of
    // `selectWorkable`, so a count on screen and a pick in the machine cannot
    // drift apart. Epics are held out because picking one means "work the
    // umbrella", which is never what the human meant.
    const progressPick = selectWorkable(inProgress, { workEpics });
    const readyPick = selectWorkable(ready, { workEpics });
    const hiddenEpics = progressPick.heldOut.length + readyPick.heldOut.length;
    inProgress = progressPick.pickable;
    ready = readyPick.pickable;
    if (hiddenEpics > 0) {
      notes.push(`board: ${hiddenEpics} epic(s) held out of work selection`);
      log("debug", `held ${hiddenEpics} epic(s) out of the pick lists (workEpics=false)`);
    }

    effects.push({
      kind: "beads.read",
      detail: `in_progress=${inProgress.length} ready=${ready.length}`,
    });
    return feed({ type: "board_observed", inProgress, ready });
  }

  // ── handlers, one per effect kind ────────────────────────────────────────

  type Handler = (effect: Effect) => Promise<readonly Effect[]>;

  async function handleSetStatus(effect: SetStatusEffect): Promise<readonly Effect[]> {
    const claim = isClaim(state, effect);
    try {
      await ports.beads.setStatus(effect.id, effect.status,
        effect.ifStatus === undefined ? {} : { ifStatus: effect.ifStatus });
      if (claim) {
        refusedClaimIssue = null;
        refusedClaimStreak = 0;
      }
    } catch (error) {
      const reason = `${kindOf(error)}: ${messageOf(error)}`;
      if (claim) {
        refusedClaimStreak = refusedClaimIssue === effect.id ? refusedClaimStreak + 1 : 1;
        refusedClaimIssue = effect.id;
        return feed({ type: "claim_failed", id: effect.id, reason });
      }
      // A failed fire-and-forget write (e.g. re-opening after a failed run) has
      // no event that represents it. Say so: the board is not what we wanted.
      warn(`could not set ${effect.id} to ${effect.status}: ${reason}. The board was left as found.`);
      return [];
    }
    if (claim) return feed({ type: "issue_claimed", id: effect.id });
    return [];
  }

  async function handleCreateIssue(effect: CreateIssueEffect): Promise<readonly Effect[]> {
    if (unit === null || unit.kind !== "split") {
      throw new LoopError(
        "orphaned-effect",
        "a beads.create_issue effect arrived outside a split unit. Children are created by " +
          "the split ledger, which binds intra-batch #index tokens to real ids; creating one " +
          "here would put a dependency on an id nobody resolved.",
      );
    }
    const active = unit;
    let created;
    try {
      created = await active.ledger.createForSpec(effect.spec);
    } catch (error) {
      active.failed.push({
        index: -1,
        title: effect.spec.title,
        errorKind: kindOf(error),
      });
      warn(`create failed for "${effect.spec.title}": ${messageOf(error)}`);
      return [];
    }
    if (created === null) {
      const report = active.ledger.report();
      const failure = report.failed.find((entry) => entry.title === effect.spec.title);
      active.failed.push({
        index: failure?.index ?? -1,
        title: effect.spec.title,
        errorKind: failure?.errorKind ?? "not-created",
      });
      if (report.skipped.length > 0) {
        notes.push(`split skipped ${report.skipped.length} item(s): unmet dependency`);
      }
      return [];
    }
    active.created.push(created);
    createdIssueIds.push(created.id);
    return [];
  }

  async function handleRemember(effect: RememberEffect): Promise<readonly Effect[]> {
    if (unit !== null && unit.kind === "finalize") {
      // The finalizer already attempted this write, with the commit hash in the
      // note. Re-issuing it here would duplicate the memory — and if the unit
      // failed at this stage, re-issuing is exactly the blind retry `.8` rule 9
      // forbids.
      if (effect.key !== handoffKeyFor(unit.request.issueId)) {
        throw new LoopError(
          "unit-drift",
          `the finalize unit was asked to remember ${unit.request.issueId} but the machine ` +
            `reported key ${effect.key}; refusing to claim a write we did not make`,
        );
      }
      effects.push({ kind: effect.kind, detail: `performed by finalize unit (${effect.key})` });
      return [];
    }
    try {
      await ports.beads.remember(effect.text, effect.key);
    } catch (error) {
      warn(`could not record ${effect.key}: ${kindOf(error)}: ${messageOf(error)}`);
    }
    return [];
  }

  async function handleCloseIssue(effect: CloseIssueEffect): Promise<readonly Effect[]> {
    if (unit !== null && unit.kind === "finalize") {
      if (effect.id !== unit.request.issueId) {
        throw new LoopError(
          "unit-drift",
          `the finalize unit was closing ${unit.request.issueId} but the machine reported ` +
            `closing ${effect.id}`,
        );
      }
      effects.push({ kind: effect.kind, detail: `performed by finalize unit (${effect.id})` });
      return [];
    }
    try {
      await ports.beads.closeIssue(effect.id, effect.reason);
      closedIssueIds.push(effect.id);
    } catch (error) {
      warn(`could not close ${effect.id}: ${kindOf(error)}: ${messageOf(error)}`);
    }
    return [];
  }

  async function handleRun(effect: AgentRunEffect): Promise<readonly Effect[]> {
    const outcome = await ports.runner.run(effect.issueId);
    lastWork = outcome;
    if (outcome.kind === "done") {
      failedIssue = null;
      consecutiveFailures = 0;
    } else {
      consecutiveFailures = failedIssue === effect.issueId ? consecutiveFailures + 1 : 1;
      failedIssue = effect.issueId;
    }
    effects.push({ kind: effect.kind, detail: `${effect.issueId}:${outcome.kind}` });
    log("info", `work on ${effect.issueId} ended ${outcome.kind} (${outcome.elapsedMs}ms)`);
    return feed(toWorkEvent(outcome));
  }

  async function handleDropContext(effect: DropContextEffect): Promise<readonly Effect[]> {
    const disposed = await ports.runner.dispose();
    droppedIterations.push(effect.iteration);
    effects.push({ kind: effect.kind, detail: `iteration ${effect.iteration} (${disposed} session(s))` });
    notes.push(`context dropped: iteration ${effect.iteration} (${effect.reason})`);
    log("info", `context dropped (iteration ${effect.iteration}, ${disposed} session(s) disposed)`);
    return [];
  }

  async function handleSay(effect: Effect & { text: string }): Promise<readonly Effect[]> {
    say(effect.text);
    return [];
  }

  async function handleWarn(effect: Effect & { text: string }): Promise<readonly Effect[]> {
    warn(effect.text);
    return [];
  }

  /**
   * The map is `Record<EffectKind, Handler>`, so a new effect kind in
   * `src/orchestrator.ts` breaks the build here until it is handled — which is
   * the point. `test/loop.test.ts` additionally asserts coverage against the
   * exported `EFFECT_KINDS` list, and that a runtime-unknown kind raises
   * rather than no-op.
   */
  const handlers: Record<HandledEffectKind, Handler> = {
    "beads.list_in_progress": (effect) => observeBoard([effect]),
    "beads.list_ready": (effect) => observeBoard([effect]),
    "beads.set_status": (effect) => handleSetStatus(effect as SetStatusEffect),
    "beads.create_issue": (effect) => handleCreateIssue(effect as CreateIssueEffect),
    "beads.close_issue": (effect) => handleCloseIssue(effect as CloseIssueEffect),
    "beads.remember": (effect) => handleRemember(effect as RememberEffect),
    "agent.split": (effect) => runSplitUnit((effect as { text: string }).text),
    "agent.run": (effect) => handleRun(effect as AgentRunEffect),
    "vcs.commit": (effect) => runFinalizeUnit(effect as VcsCommitEffect),
    "ui.say": (effect) => handleSay(effect as Effect & { text: string }),
    "ui.warn": (effect) => handleWarn(effect as Effect & { text: string }),
    drop_context: (effect) => handleDropContext(effect as DropContextEffect),
  };

  async function dispatch(effect: Effect): Promise<readonly Effect[]> {
    const handler = handlers[effect.kind as HandledEffectKind] as Handler | undefined;
    if (handler === undefined) {
      throw new LoopError(
        "unhandled-effect",
        `no handler for effect kind ${JSON.stringify(effect.kind)}: executing nothing for a ` +
          `requested effect would leave the machine believing the world changed`,
        effect,
      );
    }
    if (config.verbose === true) log("debug", `effect ${effect.kind}`);
    // Recorded *before* the handler runs, then completed in place: a handler can
    // dispatch effects of its own (the split unit creates issues from inside
    // `agent.split`), and a log that only appended on completion would show the
    // children before the parent that caused them.
    const record: { kind: string; detail: string } = { kind: effect.kind, detail: "began" };
    effects.push(record);
    const produced = await handler(effect);
    record.detail = "ok";
    return produced;
  }

  // ── the split unit ───────────────────────────────────────────────────────

  /**
   * SPLIT as `.7` defines it, presented to the machine as the machine's own
   * events: `split_proposed` (which makes it ask for one create per spec), the
   * creates executed through the ledger, then `split_created` or
   * `split_failed` carrying the ids that actually exist.
   *
   * The two writes the machine has no effect kind for — the verbatim record of
   * the human's request, and the `#index`-to-id dependency bindings — happen
   * inside the ledger and are named in the transcript.
   */
  async function runSplitUnit(text: string): Promise<readonly Effect[]> {
    if (unit !== null) {
      throw new LoopError("unit-nesting", `a ${unit.kind} unit is already in flight`);
    }

    const proposal = await ports.splitter.propose(text);
    if (!proposal.ok) {
      return feed({ type: "split_failed", reason: describeProposalFailure(proposal), createdIds: [] });
    }

    let epicId: string;
    try {
      const epic = await recordHumanRequest(ports.beads, text, {
        epicId: config.epicId ?? null,
        epicTitle: config.epicTitle,
        epicPriority: config.epicPriority ?? 4,
      });
      notes.push(
        `recorded the human request verbatim on ${epic.epicId} (${epic.landedIn}, ${epic.mode})`,
      );
      epicId = epic.epicId;
    } catch (error) {
      return feed({
        type: "split_failed",
        reason:
          `the request could not be recorded (${kindOf(error)}: ${messageOf(error)}); ` +
          "no children were created, because an unrecorded request is a request nobody can audit",
        createdIds: [],
      });
    }

    const specs = toNewIssueSpecs(proposal.items, epicId);
    const ledger = createSplitLedger({
      items: proposal.items,
      beads: ports.beads,
      epicId,
      onEvent: (event) => {
        if (event.type === "created" || event.type === "failed" || event.type === "skipped") {
          notes.push(`ledger ${event.type}: ${event.title}`);
        }
      },
    });
    unit = { kind: "split", ledger, created: [], failed: [] };

    try {
      const requested = feed({ type: "split_proposed", specs });
      for (const requestedEffect of requested) {
        if (requestedEffect.kind !== "beads.create_issue") {
          throw new LoopError(
            "unit-drift",
            `the split machine asked for ${requestedEffect.kind} mid-split; the ledger only creates issues`,
          );
        }
        await dispatch(requestedEffect);
      }
      const active = unit;
      const ids = active.created.map((issue) => issue.id);
      if (active.failed.length === 0 && ids.length > 0) {
        return feed({ type: "split_created", createdIds: ids });
      }
      return feed({
        type: "split_failed",
        reason:
          `the split only partly landed: ${ids.length} created, ${active.failed.length} failed ` +
          `(a partial split is reported as a failure — the created issues are on the board, but ` +
          `the batch is not whole)`,
        createdIds: ids,
      });
    } finally {
      unit = null;
    }
  }

  // ── the finalize unit ────────────────────────────────────────────────────

  async function buildFinalizeRequest(
    issueId: string,
    effect: VcsCommitEffect,
  ): Promise<FinalizeRequest> {
    let issue: Issue | null = null;
    try {
      issue = await ports.beads.getIssue(issueId);
    } catch (error) {
      // A title is worth a warning, not a failed finalize: the finalizer falls
      // back to the id and still records the hash it read back from git.
      warn(`could not re-read ${issueId} for the handoff note: ${messageOf(error)}`);
    }
    const verdict =
      lastWork !== null && (lastWork.kind === "done" || lastWork.kind === "incomplete")
        ? lastWork.verdict
        : null;
    const summary = verdict?.summary ?? stripIssuePrefix(effect.message, issueId);
    const changedFiles =
      verdict !== null && verdict.changedFiles.length > 0 ? verdict.changedFiles : effect.paths;
    const request: FinalizeRequest = {
      issueId,
      title: issue?.title ?? state.activeIssueTitle ?? issueId,
      summary,
      changedFiles,
      nextSteps: verdict?.nextSteps ?? [],
    };
    if (verdict === null) {
      notes.push("finalize: no verdict in hand; the commit effect's own message was used");
    }
    return request;
  }

  /**
   * Finalize as `.8` defines it. See rule 3 in the header for why this is one
   * unit rather than three dispatched effects.
   */
  async function runFinalizeUnit(effect: VcsCommitEffect): Promise<readonly Effect[]> {
    if (unit !== null) {
      throw new LoopError("unit-nesting", `a ${unit.kind} unit is already in flight`);
    }
    const issueId = state.activeIssueId;
    if (issueId === null) {
      throw new LoopError(
        "finalize-without-issue",
        "the machine asked to commit while no issue was active; there is nothing to hand off",
      );
    }

    const request = await buildFinalizeRequest(issueId, effect);
    unit = { kind: "finalize", request };
    try {
      const outcome = await ports.finalizer.finalize(request);
      effects.push({ kind: "vcs.commit", detail: `unit:${outcome.kind}` });
      log("info", `finalize unit ended ${outcome.kind} for ${issueId}`);

      if (outcome.kind === "planned") {
        dryPlan.lines = outcome.planText;
        for (const line of outcome.planText) say(line);
        return [];
      }
      if (outcome.kind === "finalized") closedIssueIds.push(outcome.issueId);
      if (outcome.kind !== "finalized") {
        // A failed stage is the run's last word: it names what exists on disk
        // and what is still open, which is exactly what a human needs. Do not
        // go looking for more work after a half-finished handoff.
        const why = describeFinalizeFailure(outcome);
        blockedReason = why;
        guardTripped = { kind: "blocked", reason: why };
      }

      const deferred: Effect[] = [];
      for (const finalizeEvent of toFinalizeEvents(outcome)) {
        for (const producedEffect of feed(finalizeEvent)) {
          // `remember`/`close` were already done by the unit, with the commit hash
          // in the note. `dispatch` performs the drift check against what the
          // unit actually did before it lets either pass as performed.
          if (
            producedEffect.kind === "beads.remember" ||
            producedEffect.kind === "beads.close_issue"
          ) {
            await dispatch(producedEffect);
            continue;
          }
          // Board reads must go back to the pump so the pair is coalesced into
          // one observation; running them here would fire two separate picks.
          if (READ_EFFECTS.includes(producedEffect.kind)) {
            deferred.push(producedEffect);
            continue;
          }
          await dispatch(producedEffect);
        }
      }
      return deferred;
    } finally {
      unit = null;
    }
  }

  // ── the pump ────────────────────────────────────────────────────────────

  /**
   * Run effects until the queue is empty.
   *
   * Adjacent board reads are coalesced into a single `board_observed`, because
   * the pick decision has to see both lists at one instant. Coalescing is by
   * *adjacency*, not by grouping the whole batch: a read that comes after
   * another effect stays after it, so the trace order is the real order.
   */
  async function pump(initial: readonly Effect[]): Promise<void> {
    let pending = [...initial];
    while (pending.length > 0) {
      const batch = pending;
      pending = [];
      let reads: Effect[] = [];
      const flushReads = async (): Promise<void> => {
        if (reads.length === 0) return;
        const group = reads;
        reads = [];
        // A tripped guard stops the loop from *starting* another pass. Finishing
        // the current one is still allowed — which is why this check sits here
        // and not at the top of the batch: dropping a failure's own effects is
        // how an issue gets stranded `in_progress` with its note still queued.
        const trip = guardTrip();
        if (trip !== null) {
          notes.push(`skipped ${group.length} board read(s): ${trip.reason}`);
          return;
        }
        pending.push(...(await observeBoard(group)));
      };
      for (const effect of batch) {
        if (READ_EFFECTS.includes(effect.kind)) {
          reads.push(effect);
          continue;
        }
        await flushReads();
        if (signalName !== null) {
          notes.push(
            `stopped before running ${batch.length} queued effect(s) after ${signalName} ` +
              `(${batch.map((entry) => entry.kind).join(", ")})`,
          );
          break;
        }
        pending.push(...(await dispatch(effect)));
      }
      await flushReads();
    }
  }

  // ── idle ────────────────────────────────────────────────────────────────

  /**
   * One turn at the terminal: refresh the status line, take one input, feed it.
   *
   * Returns the effects the machine produced, plus whether the machine actually
   * moved. A turn that changed nothing (a rejected input) is the signature of a
   * spin, and the driver counts those instead of counting every question asked
   * of a human — a long chat session is not a runaway.
   */
  async function idleRound(): Promise<{ effects: Effect[]; moved: boolean }> {
    try {
      await ports.idle.refresh?.();
    } catch (error) {
      // The status line is cosmetic. A stale count must not end a run.
      log("debug", `idle status refresh failed: ${messageOf(error)}`);
    }
    const before = transitions.length;
    const outcome = await ports.idle.next();
    if (outcome.kind === "exit") {
      effects.push({ kind: "idle", detail: `exit(${outcome.reason})` });
      return { effects: feed({ type: "stop" }), moved: transitions.length > before };
    }
    effects.push({ kind: "idle", detail: `input(${outcome.text.length} chars)` });
    return { effects: feed({ type: "human_input", text: outcome.text }), moved: transitions.length > before };
  }

  // ── preflight ───────────────────────────────────────────────────────────

  async function preflight(): Promise<string | null> {
    try {
      await ports.beads.listReady({ limit: 1 });
    } catch (error) {
      const kind = kindOf(error);
      if (kind === "missing-binary") {
        return "bd is not installed or not on PATH — this loop has no board without it";
      }
      return `the board could not be read (${kind}): ${messageOf(error)}. ` +
        "Is this directory's beads database initialised?";
    }
    try {
      await ports.git.repoRoot();
    } catch (error) {
      return `this is not a git work tree (${kindOf(error)}): ${messageOf(error)}. ` +
        "A loop that cannot commit has nowhere to put its work";
    }
    return null;
  }

  // ── signals ─────────────────────────────────────────────────────────────

  const detachSignalHandlers: (() => void)[] = [];
  if (ports.signals !== undefined) {
    for (const name of ["SIGINT", "SIGTERM"]) {
      detachSignalHandlers.push(
        ports.signals.on(name, () => {
          if (finished || signalName !== null) return;
          signalName = name;
          // Remember *now* whether work was in flight. By the time the driver
          // reads this, the disposal has ended that work and the state has moved;
          // "interrupted a running job" and "stopped at a quiet boundary" are
          // different answers and cannot be reconstructed later.
          signalMidWork = !stoppable.has(state.name);
          signalLeftIssue = state.activeIssueId;
          signalLeftTitle = state.activeIssueTitle;
          // Stop the thing that is running, so the await returns and the driver
          // below can decide between `stop` and `abort` at a real boundary.
          void ports.runner.dispose().catch(() => {});
        }),
      );
    }
  }

  const stoppable = new Set<OrchestratorStateName>(["idle", "check_work"]);

  // ── the driver ──────────────────────────────────────────────────────────

  try {
    if (config.preflight !== false) {
      const problem = await preflight();
      if (problem !== null) return finish("fatal", problem);
    }

    if (config.dryRun === true) {
      // Say what dry run does *not* cover, so nobody reads a clean run as "the
      // board was untouched". Claims and status writes are real in a dry run.
      warn(
        "dry run: the finalize stage prints its commands instead of executing them. " +
          "Board claims and status writes still happen — dry run does not cover them.",
      );
    }

    let pending = feed({ type: "start" });
    for (;;) {
      if (state.iteration > maxIterations) {
        return finish(
          "iteration-limit",
          `stopped after ${maxIterations} iterations; this looks like a runaway loop`,
        );
      }
      await pump(pending);
      // Nothing survives a pump: the next round starts from whatever the idle
      // turn (or a retry) hands back, never from a stale list.
      pending = [];

      if (dryPlan.lines !== null) {
        return finish(
          "planned",
          `dry run: ${dryPlan.lines.length} command(s) printed, nothing changed`,
        );
      }

      if (signalName !== null) {
        const name = signalName;
        // Act on the signal once. Leaving it set makes this branch re-enter with
        // the machine already terminal, and `abort` in `done` is rejected — the
        // loop would spin on rejections forever.
        signalName = null;
        if (state.name === "done" || state.name === "aborted") break;
        const midWork = signalMidWork;
        const leftInProgress = signalLeftIssue;
        const leftTitle = signalLeftTitle;
        if (!midWork && stoppable.has(state.name)) {
          pending = feed({ type: "stop" });
        } else {
          pending = feed({
            type: "abort",
            reason: `received ${name} while ${midWork ? "work was running" : state.name}`,
          });
          if (midWork) {
            // The last line a person reads is the one they act on, so it names
            // the bead and says what state it is actually in — not "aborted".
            warn(
              leftInProgress === null
                ? `Interrupted by ${name} with no issue active: nothing is left mid-flight.`
                : `Interrupted by ${name}: ${leftInProgress} (${leftTitle ?? "no title"}) is still ` +
                  "in_progress with no commit and no handoff note. Re-run and it will be picked up again.",
            );
          }
        }
        continue;
      }

      const trip = guardTrip();
      if (trip !== null) {
        return finish(trip.kind, trip.reason);
      }

      if (state.name === "done" || state.name === "aborted") {
        break;
      }

      if (state.name === "idle") {
        const turn = await idleRound();
        if (turn.moved) {
          idleSpins = 0;
        } else {
          idleSpins += 1;
          if (idleSpins >= maxIdleSpins) {
            return finish(
              "aborted",
              `${idleSpins} idle turns in a row left the machine exactly where it was — ` +
                "the input is not advancing it, so stopping rather than asking forever",
            );
          }
        }
        pending = turn.effects;
        continue;
      }

      // Nothing is pending and the machine is not resting. The only honest ways
      // out of this are a retry of a failed board read, or telling a human
      // that a stage is stuck with artifacts behind it.
      if (state.name === "check_work" && config.autoRetryObserve !== false && observeRetries < 1) {
        observeRetries += 1;
        notes.push("board read failed once; retrying the read");
        pending = feed({ type: "retry" });
        continue;
      }

      const stalled = state.name;
      const hint =
        blockedReason ??
        (stalled === "finalize"
          ? "a finalize stage is blocked. Whatever committed stays committed; the bead may still " +
            "be open. Re-run after fixing the cause — the finalizer reuses the existing commit " +
            "instead of making a second one."
          : `the machine is waiting in ${stalled} with no pending effect`);
      return finish("blocked", hint);
    }

    const kind: LoopResultKind = state.name === "done" ? "done" : "aborted";
    const lastTransition = transitions[transitions.length - 1];
    return finish(kind, lastTransition?.event ?? null);
  } catch (error) {
    if (LoopError.is(error)) {
      warn(`${error.code}: ${error.message}`);
      return finish("fatal", `${error.code}: ${error.message}`);
    }
    return finish("fatal", `unexpected failure: ${messageOf(error)}`);
  } finally {
    for (const detach of detachSignalHandlers) {
      try {
        detach();
      } catch {
        // A signal adapter that cannot be unhooked is not worth failing a run.
      }
    }
    try {
      await ports.runner.dispose();
    } catch (error) {
      log("debug", `runner disposal during teardown failed: ${messageOf(error)}`);
    }
    try {
      await ports.idle.dispose?.();
    } catch (error) {
      log("debug", `idle disposal during teardown failed: ${messageOf(error)}`);
    }
  }
}

/**
 * Every effect kind the interpreter handles, written out explicitly so that a
 * deletion is a visible diff rather than a silent gap. `test/loop.test.ts`
 * asserts this list is exactly `EFFECT_KINDS` from the machine — so a new
 * effect kind in `src/orchestrator.ts` fails the build here, and fails the
 * test there, and cannot be quietly unhandled.
 */
export const HANDLED_EFFECT_KINDS = [
  "beads.list_in_progress",
  "beads.list_ready",
  "beads.set_status",
  "beads.create_issue",
  "beads.close_issue",
  "beads.remember",
  "agent.split",
  "agent.run",
  "vcs.commit",
  "ui.say",
  "ui.warn",
  "drop_context",
] as const satisfies readonly Effect["kind"][];

export type HandledEffectKind = (typeof HANDLED_EFFECT_KINDS)[number];
