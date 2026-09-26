/**
 * Orchestrator — the pure decision layer of the five-step beads loop.
 *
 * THE CONTRACT: this module *decides*, it never *does*. There is no file,
 * network, process, clock or environment access here, and no injected port is
 * ever called from inside a transition. Every side effect leaves the machine as a
 * plain, serialisable {@link Effect} value; every answer the machine needs comes
 * back in as an {@link OrchestratorEvent}.
 *
 * ```
 * step(state, event) -> { applied, state, effects }
 * ```
 *
 * Because effects are data:
 * - a run is a list: `[event, effect*, event, effect* …]`, deep-comparable in
 *   tests and replayable against fakes without pi and without a real board;
 * - the interpreter (workspace-5yn.9) is a dumb dispatch loop over
 *   {@link OrchestratorPorts}, so there is exactly one place where the machine
 *   touches the world;
 * - a rejection can never have half-executed something.
 *
 * STATES (all of them; nothing outside this list is ever entered):
 *
 *   init        pre-flight, nothing consulted yet
 *   check_work  the board has been asked for; decide what, if anything, to do
 *   idle        board is empty; wait for raw human text
 *   split       turn human text into child issues
 *   pick        an issue has been selected; ownership write is in flight
 *   work        the agent is working the one active issue
 *   finalize    commit -> remember handoff -> close, in that order, never shuffled
 *   restart     the cold boundary: drop the session, then check_work
 *   done        operator stopped the run (terminal)
 *   aborted     operator aborted the run (terminal)
 *
 * WHY `restart` IS A STATE AND NOT AN IMPLIED BACK-EDGE
 * Step 5 of the loop is "restart with no conversational context carried over".
 * Per [ADR-001](../docs/ADR-001-transport-and-rendering.md) the only thing that
 * actually buys that amnesia is a brand-new session per iteration — so the
 * boundary has to be a visible thing in the trace, emitting `drop_context`, not
 * an arrow drawn back to `check_work` that a future refactor could "helpfully"
 * shortcut into a warm state. `SPLIT` and `FINALIZE` both route through here.
 *
 * INVARIANTS THE MACHINE ENFORCES (not the caller)
 * 1. In-progress work beats ready work, always — even when the ready issue has
 *    higher priority. A half-finished issue is never preempted.
 * 2. At most one active issue. `check_work` refuses to pick while
 *    `activeIssueId` is set, so a second `agent.run` cannot be emitted.
 * 3. Every path that is not terminal returns to `check_work`, and every arrival
 *    at `check_work` carries fresh board reads. There are no dead ends.
 * 4. Failed work is re-queued, never dropped: the failure is written to bd
 *    memory *before* the status is reopened, so the reason survives the reset.
 *    If that reopen write itself fails the issue stays `in_progress` and rule 1
 *    resumes it — failure-recoverable by construction.
 * 5. Finalize order is commit -> remember -> close, tracked by `finalizeStage`;
 *   no stage can be skipped and nothing is closed before its commit exists.
 * 6. A failed optimistic-guard (bd exit 13, `guard-mismatch`) is never retried
 *    here; the machine treats the world as having moved on and re-observes.
 *
 * TOTALITY: the state switch is compile-time exhaustive — adding a state to
 * `STATE_NAMES` without a case stops the build. Event coverage is per-state and
 * deliberately partial: a pair that means nothing comes back `applied: false`
 * with the state passed through untouched. The test suite walks the full
 * state × event cross-product so nothing is undefined, and proves every declared
 * event type is live in at least one state.
 *
 * DETERMINISM: no clock, no randomness, no locale-sensitive comparison. Ids are
 * compared with plain `<`/`>`. Anything that needs a timestamp (a commit's, a
 * memory's) belongs to the interpreter, outside this module.
 *
 * BLOCKING: no dependency logic lives here on purpose — `bd ready` is already
 * blocker-aware. If this module ever does need to ask "is this blocked?", it must
 * go through `normaliseDependencies()`/`dependsOn()` in `src/beads.js`, never a
 * raw `depends_on_id` read (bd's `bd show` shape does not have one, which fails
 * open as "not blocked").
 */
import type { Issue, IssueStatus, NewIssueSpec } from "./beads.ts";

/** Every state the machine can be in. Nothing outside this list is reachable. */
export const STATE_NAMES = [
  "init",
  "check_work",
  "idle",
  "split",
  "pick",
  "work",
  "finalize",
  "restart",
  "done",
  "aborted",
] as const;

export type OrchestratorStateName = (typeof STATE_NAMES)[number];

/** Every event the machine accepts. */
export const EVENT_TYPES = [
  "start",
  "board_observed",
  "observe_failed",
  "retry",
  "human_input",
  "split_proposed",
  "split_created",
  "split_failed",
  "issue_claimed",
  "claim_failed",
  "work_succeeded",
  "work_failed",
  "committed",
  "remembered",
  "closed",
  "finalize_failed",
  "stop",
  "abort",
] as const;

export type OrchestratorEventType = (typeof EVENT_TYPES)[number];

/** Sub-stages of finalize. The order is the contract. */
export const FINALIZE_STAGES = ["commit", "handoff", "close"] as const;
export type FinalizeStage = (typeof FINALIZE_STAGES)[number];

// ── effects: data, never calls ───────────────────────────────────────────────

export type EffectKind =
  | "beads.list_in_progress"
  | "beads.list_ready"
  | "beads.set_status"
  | "beads.create_issue"
  | "beads.close_issue"
  | "beads.remember"
  | "agent.split"
  | "agent.run"
  | "vcs.commit"
  | "notify.publish"
  | "ui.say"
  | "ui.warn"
  | "drop_context";

/**
 * Every effect kind the machine can emit, as a value. The interpreter
 * (workspace-5yn.9) can type its dispatch table as `Record<EffectKind, …>` and
 * let the compiler prove it handles all of them; the test suite proves this list
 * is exactly what the machine actually emits — nothing declared that never fires,
 * nothing emitted that isn't declared.
 *
 * This is a deliberate SUBSET of `BdClient`, not a mirror of it. The constraint
 * the ticket asks for is that no transition may need a capability the adapter does
 * not offer — not that every adapter method shows up here. Three adapter methods
 * are intentionally unused so far:
 *
 * - `getIssue`: the machine picks from bd's own `ready` / `in_progress` reads,
 *   which are already blocked-filtered, so a second read would be ceremony.
 * - `addDep`: a split that wants ordering between its own children ("B blocks
 *   until A lands") is a real need, but the producer of that shape is the agent
 *   runner in .5 — inventing the split protocol here would only have to be
 *   rewritten there. Extend `split_proposed` with the dependency shape and add
 *   `beads.add_dep` when .5 says what it actually produces.
 * - `recall`: carrying a prior failure note into the next attempt across a cold
 *   boundary needs a `recalled` round trip before `agent.run`; that belongs with
 *   the runner that consumes the note.
 *
 * Adding one is mechanical: add the Effect variant, emit it from a transition, add
 * the kind to this list, and the coverage + 1:1-dispatch tests will fail until the
 * interpreter has a home for it.
 */
export const EFFECT_KINDS = [
  "beads.list_in_progress",
  "beads.list_ready",
  "beads.set_status",
  "beads.create_issue",
  "beads.close_issue",
  "beads.remember",
  "agent.split",
  "agent.run",
  "vcs.commit",
  "notify.publish",
  "ui.say",
  "ui.warn",
  "drop_context",
] as const satisfies readonly EffectKind[];

/** Ask the board for `in_progress` work. */
export interface ListInProgressEffect {
  kind: "beads.list_in_progress";
  labels?: readonly string[];
}

/** Ask the board for ready (unblocked, claimable) work. */
export interface ListReadyEffect {
  kind: "beads.list_ready";
  labels?: readonly string[];
}

/**
 * Status transition. `ifStatus` is the optimistic guard: bd refuses with exit 13
 * if the world moved, which the interpreter reports back as `claim_failed`.
 */
export interface SetStatusEffect {
  kind: "beads.set_status";
  id: string;
  status: IssueStatus;
  ifStatus?: IssueStatus;
}

export interface CreateIssueEffect {
  kind: "beads.create_issue";
  spec: NewIssueSpec;
}

export interface CloseIssueEffect {
  kind: "beads.close_issue";
  id: string;
  reason: string;
}

export interface RememberEffect {
  kind: "beads.remember";
  text: string;
  key: string;
}

export interface AgentSplitEffect {
  kind: "agent.split";
  text: string;
}

export interface AgentRunEffect {
  kind: "agent.run";
  issueId: string;
  issueTitle: string;
}

export interface VcsCommitEffect {
  kind: "vcs.commit";
  message: string;
  paths: readonly string[];
}

/**
 * A notice that a bead finished, for whoever asked to be told.
 *
 * The machine emits one on every bead it closes and knows nothing about whether
 * anybody wants it: the address lives in the environment, the environment lives
 * in the interpreter, and a transition that consulted either would stop being a
 * function of state and event. What leaves here is the set of facts the
 * interpreter cannot recover elsewhere — the id this step closed, the reason that
 * closed it, the hash that funded it — and the interpreter owns who hears about
 * it, and whether the answer is "nobody, `LOOP_NTFY_TOPIC` is unset".
 *
 * It is emitted *after* the close rather than before it on purpose: a notice sent
 * before the close is a claim of completion that outruns the close itself, and
 * that is the one lie this loop is built not to tell.
 */
export interface NotifyPublishEffect {
  kind: "notify.publish";
  issueId: string;
  title: string;
  /** The `bd close --reason` text, so the notice matches what the board says. */
  closeReason: string;
  /** The commit that carries the work. `null` only if none was ever recorded. */
  commit: string | null;
  /** The iteration that did the work — the notice is per-run, not per-bead. */
  iteration: number;
  handoffKey: string;
}

export interface UiSayEffect {
  kind: "ui.say";
  text: string;
}

export interface UiWarnEffect {
  kind: "ui.warn";
  text: string;
}

/**
 * The cold boundary. The interpreter must dispose the current agent session and
 * start the next one from nothing — see ADR-001. `iteration` is the iteration
 * being thrown away, so a log reads as a sequence of disjoint runs.
 */
export interface DropContextEffect {
  kind: "drop_context";
  iteration: number;
  reason: string;
}

export type Effect =
  | ListInProgressEffect
  | ListReadyEffect
  | SetStatusEffect
  | CreateIssueEffect
  | CloseIssueEffect
  | RememberEffect
  | AgentSplitEffect
  | AgentRunEffect
  | VcsCommitEffect
  | NotifyPublishEffect
  | UiSayEffect
  | UiWarnEffect
  | DropContextEffect;

// ── events: the world talking back ───────────────────────────────────────────

export type OrchestratorEvent =
  /** Begin the run (only meaningful from `init`). */
  | { type: "start" }
  /** The two board reads, as observed together so the pick decision is atomic. */
  | { type: "board_observed"; inProgress: readonly Issue[]; ready: readonly Issue[] }
  /** The board read itself failed (adapter error, board missing, …). */
  | { type: "observe_failed"; reason: string }
  /** Raw human text. Never parsed here — the split agent parses it. */
  | { type: "human_input"; text: string }
  /** The split agent proposed children; the machine will create them. */
  | { type: "split_proposed"; specs: readonly NewIssueSpec[] }
  /** The created children, after the create effects ran. */
  | { type: "split_created"; createdIds: readonly string[] }
  /** Splitting failed, possibly after creating some children. */
  | { type: "split_failed"; reason: string; createdIds?: readonly string[] }
  /** The status write landed; this issue is ours to work. */
  | { type: "issue_claimed"; id: string }
  /** The status write was refused (usually bd's exit-13 guard). */
  | { type: "claim_failed"; id: string; reason: string }
  /** The agent finished the active issue. */
  | { type: "work_succeeded"; summary: string; changedFiles: readonly string[] }
  /** The agent could not finish the active issue. */
  | { type: "work_failed"; reason: string }
  /** Finalize stage 1: the commit exists. */
  | { type: "committed"; hash: string }
  /** Finalize stage 2: the handoff memory was written. */
  | { type: "remembered"; key: string }
  /** Finalize stage 3: the issue is closed. */
  | { type: "closed"; id: string }
  /** A finalize stage failed. `stage` must be the one currently in flight. */
  | { type: "finalize_failed"; stage: FinalizeStage; reason: string }
  /** Re-issue the effect that is currently pending. Never advances on its own. */
  | { type: "retry" }
  /** Operator stop. Accepted only at a safe boundary. */
  | { type: "stop" }
  /** Operator abort. Legal anywhere, and says what it leaves behind. */
  | { type: "abort"; reason: string };

// ── state ────────────────────────────────────────────────────────────────────

export interface TraceEntry {
  readonly seq: number;
  readonly from: OrchestratorStateName;
  readonly event: OrchestratorEventType;
  readonly to: OrchestratorStateName;
  readonly effects: readonly EffectKind[];
}

export interface OrchestratorState {
  readonly name: OrchestratorStateName;
  /** 1-based. Incremented at every cold boundary. */
  readonly iteration: number;
  /** At most one. Set when work begins, cleared by the cold boundary. */
  readonly activeIssueId: string | null;
  /** Title of the active issue, carried so `agent.run` can name what it runs. */
  readonly activeIssueTitle: string | null;
  /** Human text awaiting its split. */
  readonly pendingText: string | null;
  /** Which half of SPLIT we are in: proposing, or creating what was proposed. */
  readonly splitStage: "propose" | "create" | null;
  /** Children created by the current split, in creation order. */
  readonly createdIssueIds: readonly string[];
  /** Pending finalize stage, or null when not finalizing. */
  readonly finalizeStage: FinalizeStage | null;
  /** Last commit hash, once finalize stage 1 lands. */
  readonly commitHash: string | null;
  /** Key the handoff memory was written under. */
  readonly handoffKey: string | null;
  /** Kept so a `retry` re-emits identical bytes instead of inventing new ones. */
  readonly pendingCommitMessage: string | null;
  readonly pendingHandoffText: string | null;
  readonly pendingCloseReason: string | null;
  /** Last failure seen, carried across the boundary as a note, not a blocker. */
  readonly lastFailure: { stage: string; reason: string } | null;
  readonly trace: readonly TraceEntry[];
}

/** Everything a transition may change, except the state name and the trace. */
type StatePatch = Partial<Omit<OrchestratorState, "name" | "trace">>;

export interface Rejection {
  readonly code: string;
  readonly message: string;
}

export type StepResult =
  | {
      readonly applied: true;
      readonly state: OrchestratorState;
      readonly effects: readonly Effect[];
    }
  | {
      readonly applied: false;
      readonly state: OrchestratorState;
      readonly effects: readonly [];
      readonly rejection: Rejection;
    };

export function createInitialState(): OrchestratorState {
  return {
    name: "init",
    iteration: 1,
    activeIssueId: null,
    activeIssueTitle: null,
    pendingText: null,
    splitStage: null,
    createdIssueIds: [],
    finalizeStage: null,
    commitHash: null,
    handoffKey: null,
    pendingCommitMessage: null,
    pendingHandoffText: null,
    pendingCloseReason: null,
    lastFailure: null,
    trace: [],
  };
}

export function isTerminal(state: OrchestratorState): boolean {
  return state.name === "done" || state.name === "aborted";
}

/** Handoff/failure memory keys, centralised so the loop's memory layout is greppable. */
export function handoffKeyFor(issueId: string): string {
  return `loop:handoff:${issueId}`;
}

export function failureKeyFor(issueId: string): string {
  return `loop:failure:${issueId}`;
}

// ── internal helpers ─────────────────────────────────────────────────────────

interface Built {
  readonly state: OrchestratorState;
  readonly effects: readonly Effect[];
}

/** Fresh objects every call — no shared references leak into two results. */
function listInProgress(labels?: readonly string[]): ListInProgressEffect {
  return labels === undefined ? { kind: "beads.list_in_progress" } : { kind: "beads.list_in_progress", labels };
}

function listReady(labels?: readonly string[]): ListReadyEffect {
  return labels === undefined ? { kind: "beads.list_ready" } : { kind: "beads.list_ready", labels };
}

function say(text: string): UiSayEffect {
  return { kind: "ui.say", text };
}

function warn(text: string): UiWarnEffect {
  return { kind: "ui.warn", text };
}

/** Record one hop. Chained hops pass the effects accumulated so far. */
function hop(
  state: OrchestratorState,
  event: OrchestratorEvent,
  to: OrchestratorStateName,
  effects: readonly Effect[] = [],
  patch: StatePatch = {},
): Built {
  const entry: TraceEntry = {
    seq: state.trace.length,
    from: state.name,
    event: event.type,
    to,
    effects: effects.map((effect) => effect.kind),
  };
  return {
    state: { ...state, ...patch, name: to, trace: [...state.trace, entry] },
    effects: [...effects],
  };
}

/**
 * Arrive at `check_work` the only correct way: through the cold boundary.
 * Drops the session, bumps the iteration, and re-requests both board reads so
 * the next decision never runs on a stale snapshot.
 */
function throughBoundary(
  state: OrchestratorState,
  event: OrchestratorEvent,
  reason: string,
  before: readonly Effect[] = [],
  patch: StatePatch = {},
): Built {
  const dropped = hop(
    state,
    event,
    "restart",
    [...before, { kind: "drop_context", iteration: state.iteration, reason }],
    { ...patch, activeIssueId: null, activeIssueTitle: null, pendingText: null, splitStage: null, finalizeStage: null, createdIssueIds: [] },
  );
  return hop(dropped.state, event, "check_work", [...dropped.effects, listInProgress(), listReady()], {
    iteration: state.iteration + 1,
  });
}

function applied(built: Built): StepResult {
  return { applied: true, state: built.state, effects: built.effects };
}

function rejected(state: OrchestratorState, code: string, message: string): StepResult {
  return { applied: false, state, effects: [], rejection: { code, message } };
}

/**
 * Deterministic pick order: lowest priority number first, then id by byte
 * comparison (no locale, so two machines agree on the same board).
 */
function ordered(issues: readonly Issue[]): readonly Issue[] {
  return [...issues].sort((a, b) => {
    if (a.priority !== b.priority) return a.priority < b.priority ? -1 : 1;
    if (a.id === b.id) return 0;
    return a.id < b.id ? -1 : 1;
  });
}

function isBlank(text: string): boolean {
  return text.trim() === "";
}

/**
 * The first line of a multi-line message.
 *
 * A failure reason is written for two readers at once: the board note, which the
 * next attempt reads whole, and the warn line, which a human glances at while
 * something else is running. The headline is what belongs to the second one —
 * appending a sentence to a multi-line note parks it on the end of a quote, and
 * that is what a plain log ends up showing.
 */
function headlineOf(text: string): string {
  return (text.split("\n")[0] ?? text).trim();
}

/** One reachable state's worth of "that cannot happen here". */
function unhandled(state: OrchestratorState, event: OrchestratorEvent): StepResult {
  if (event.type === "stop") {
    return rejected(
      state,
      "stop-not-at-boundary",
      `stop is only accepted from check_work or idle; in ${state.name} use abort`,
    );
  }
  if (event.type === "retry") {
    return rejected(state, "nothing-to-retry", `no pending effect in ${state.name}`);
  }
  return rejected(
    state,
    "unexpected-event",
    `event ${event.type} has meaning in ${state.name}; every other (state, event) pair is rejected, not ignored`,
  );
}

// ── the machine ──────────────────────────────────────────────────────────────

/**
 * One pure step. Never throws for a legal (state, event) pair, and never returns
 * `undefined`: an impossible pair comes back as `applied: false` with the state
 * object passed through untouched (same reference), so a rejection is
 * provably inert.
 */
export function step(state: OrchestratorState, event: OrchestratorEvent): StepResult {
  const name: OrchestratorStateName = state.name;
  switch (name) {
    case "init": {
      switch (event.type) {
        case "start":
          return applied(hop(state, event, "check_work", [listInProgress(), listReady()]));
        case "stop":
          return applied(hop(state, event, "done", [say("Stopped before the first board read.")]));
        case "abort":
          return applied(hop(state, event, "aborted", [warn(`Aborted: ${event.reason}`)]));
        default:
          return unhandled(state, event);
      }
    }

    case "check_work": {
      switch (event.type) {
        case "board_observed": {
          // Invariant 2: never pick a second issue while one is active.
          if (state.activeIssueId !== null) {
            return rejected(
              state,
              "already-active-issue",
              `${state.activeIssueId} is still active; the machine will not run two issues at once`,
            );
          }

          const inProgress = ordered(event.inProgress);
          const ready = ordered(event.ready);
          const resumed = inProgress[0];
          const fresh = ready[0];

          // Invariant 1: in-progress wins outright, whatever the priorities say.
          if (resumed !== undefined) {
            const notice: Effect[] =
              inProgress.length > 1
                ? [
                    warn(
                      `${inProgress.length} issues are in_progress; at most one may be active. ` +
                        `Resuming ${resumed.id} and leaving the others alone.`,
                    ),
                  ]
                : [];
            const picked = hop(state, event, "pick", notice, {
              activeIssueId: resumed.id,
              activeIssueTitle: resumed.title,
            });
            // Resumed work is already in_progress: no status write to make, and
            // asserting one would be a write we do not need.
            return applied(
              hop(picked.state, event, "work", [...picked.effects, runEffect(resumed.id, resumed.title)], {
                activeIssueId: resumed.id,
                activeIssueTitle: resumed.title,
              }),
            );
          }

          if (fresh !== undefined) {
            // Rest in `pick`: ownership is not ours until the guarded write lands.
            return applied(
              hop(state, event, "pick", [
                {
                  kind: "beads.set_status",
                  id: fresh.id,
                  status: "in_progress",
                  ifStatus: "open",
                },
              ], {
                activeIssueId: fresh.id,
                activeIssueTitle: fresh.title,
              }),
            );
          }

          return applied(
            hop(state, event, "idle", [
              say("Board is empty — nothing ready, nothing in flight. Waiting for input."),
            ]),
          );
        }

        case "observe_failed":
          // Do NOT idle here: a failed read is not an empty board, and idling on
          // that fiction would invite splitting work nobody asked for.
          return applied(
            hop(state, event, "check_work", [
              warn(`Could not read the board: ${event.reason}. Send retry to read it again.`),
            ]),
          );

        case "retry":
          // Being at check_work means nothing is in flight. Clearing the slot here
          // means a stale pick can never block a later read.
          return applied(hop(state, event, "check_work", [listInProgress(), listReady()], {
            activeIssueId: null,
            activeIssueTitle: null,
          }));

        case "stop":
          return applied(
            hop(state, event, "done", [
              say(`Stopped at a clean boundary after ${state.iteration - 1} iteration(s).`),
            ]),
          );

        case "abort":
          return applied(hop(state, event, "aborted", [warn(`Aborted: ${event.reason}`)]));

        default:
          return unhandled(state, event);
      }
    }

    case "pick": {
      switch (event.type) {
        case "board_observed":
          // A fresh read cannot preempt a selection already in flight.
          return rejected(
            state,
            "already-active-issue",
            `${state.activeIssueId ?? "an issue"} is already picked; a new board read cannot take a second`,
          );

        case "issue_claimed": {
          const active = state.activeIssueId;
          if (active === null) {
            return rejected(state, "no-pending-pick", "claimed an issue the machine never selected");
          }
          if (event.id !== active) {
            return rejected(
              state,
              "claim-mismatch",
              `selected ${active} but the claim came back for ${event.id}`,
            );
          }
          return applied(
            hop(
              state,
              event,
              "work",
              [runEffect(event.id, state.activeIssueTitle ?? "")],
              { activeIssueId: event.id, activeIssueTitle: state.activeIssueTitle ?? "" },
            ),
          );
        }

        case "claim_failed": {
          const active = state.activeIssueId;
          if (active !== null && event.id !== active) {
            return rejected(
              state,
              "claim-mismatch",
              `claim_failed for ${event.id} but ${active} was selected`,
            );
          }
          // Someone else owns it now, or the guard moved. Nothing was worked and no
          // agent session exists yet this iteration, so there is no context to
          // drop: re-read the board instead of burning an iteration. The active
          // slot is cleared here on purpose — leaving it set would make the next
          // board read reject with `already-active-issue` and wedge the loop.
          return applied(
            hop(state, event, "check_work", [
              warn(`Could not take ${event.id}: ${event.reason}. Re-reading the board.`),
              listInProgress(),
              listReady(),
            ], {
              activeIssueId: null,
              activeIssueTitle: null,
            }),
          );
        }

        case "stop":
          return applied(hop(state, event, "done", [say("Stopped; the picked issue was left on the board.")]));

        case "abort":
          return applied(
            hop(state, event, "aborted", [
              warn(`Aborted: ${event.reason}. ${state.activeIssueId ?? "No"} issue was left mid-pick.`),
            ]),
          );

        default:
          return unhandled(state, event);
      }
    }

    case "idle": {
      switch (event.type) {
        case "human_input": {
          if (isBlank(event.text)) {
            return rejected(state, "empty-input", "idle needs real text; an empty string does not become work");
          }
          return applied(
            hop(state, event, "split", [agentSplit(event.text)], {
              pendingText: event.text,
              splitStage: "propose",
              createdIssueIds: [],
            }),
          );
        }

        case "stop":
          return applied(hop(state, event, "done", [say("Stopped while idle.")]));

        case "abort":
          return applied(hop(state, event, "aborted", [warn(`Aborted: ${event.reason}`)]));

        default:
          return unhandled(state, event);
      }
    }

    case "split": {
      if (state.splitStage === "create") {
        switch (event.type) {
          case "split_created": {
            const count = event.createdIds.length;
            if (count === 0) {
              return rejected(state, "no-children", "split reported creating zero issues");
            }
            return applied(
              throughBoundary(
                state,
                event,
                `split created ${count} issue(s) from human input`,
                [say(`Split the request into ${count} issue(s). Back to the board.`)],
                { createdIssueIds: event.createdIds },
              ),
            );
          }

          case "split_failed": {
            // Some children may already exist. Do not strand them: go read the
            // board and take what is there.
            const partial = (event.createdIds ?? []).length;
            return applied(
              throughBoundary(
                state,
                event,
                `split failed after creating ${partial} issue(s)`,
                [
                  warn(
                    `Split failed: ${event.reason}. ` +
                      (partial > 0
                        ? `${partial} issue(s) were created and are on the board.`
                        : "Nothing was created."),
                  ),
                ],
                { createdIssueIds: event.createdIds ?? [] },
              ),
            );
          }

          default:
            return unhandled(state, event);
        }
      }

      switch (event.type) {
        case "split_proposed": {
          if (event.specs.length === 0) {
            return rejected(state, "no-children", "the split agent proposed no issues");
          }
          const effects: Effect[] = event.specs.map((spec) => ({ kind: "beads.create_issue", spec }));
          // Stay in `split`, but flip the stage: a second `split_proposed` from
          // here is a duplicate and gets refused instead of duplicating work.
          return applied(
            hop(state, event, "split", effects, {
              splitStage: "create",
              createdIssueIds: [],
            }),
          );
        }

        case "split_failed":
          return applied(
            hop(state, event, "idle", [
              warn(`Could not split that input: ${event.reason}. Still idle — try again.`),
            ]),
          );

        case "stop":
          return applied(hop(state, event, "done", [say("Stopped mid-split; nothing was created.")]));

        case "abort":
          return applied(hop(state, event, "aborted", [warn(`Aborted: ${event.reason}`)]));

        default:
          return unhandled(state, event);
      }
    }

    case "work": {
      switch (event.type) {
        case "board_observed":
          return rejected(
            state,
            "already-active-issue",
            `${state.activeIssueId ?? "an issue"} is being worked; a board read cannot start a second issue`,
          );

        case "work_succeeded": {
          const issueId = state.activeIssueId;
          if (issueId === null) {
            return rejected(state, "no-active-issue", "work succeeded with no active issue to finalize");
          }
          const message = `${issueId}: ${event.summary}`;
          const handoff =
            `Completed ${issueId}: ${event.summary}\n` +
            `Changed: ${event.changedFiles.length === 0 ? "(no files reported)" : event.changedFiles.join(", ")}`;
          const closeReason = `Done: ${event.summary}`;
          return applied(
            hop(state, event, "finalize", [vcsCommit(message, event.changedFiles)], {
              finalizeStage: "commit",
              pendingCommitMessage: message,
              pendingHandoffText: handoff,
              pendingCloseReason: closeReason,
              lastFailure: null,
            }),
          );
        }

        case "work_failed": {
          const issueId = state.activeIssueId;
          if (issueId === null) {
            return rejected(state, "no-active-issue", "work failed with no active issue to re-queue");
          }
          const note = `Work on ${issueId} failed: ${event.reason}`;
          // Invariant 4: remember the reason *before* reopening, so the next
          // iteration can read why. If the reopen itself is refused the issue
          // stays in_progress and rule 1 resumes it — never simply lost.
          //
          // And no "re-queued for another pass": whether another pass happens is
          // the interpreter's guard, not this transition's promise. The machine
          // knows the bead is open again; it does not know whether this run is
          // about to stop, and saying it would be the one sentence in this trace
          // that a reader acts on and the loop does not honour.
          return applied(
            throughBoundary(
              state,
              event,
              `work failed on ${issueId}`,
              [
                { kind: "beads.remember", text: note, key: failureKeyFor(issueId) },
                { kind: "beads.set_status", id: issueId, status: "open", ifStatus: "in_progress" },
                warn(
                  `Work on ${issueId} failed: ${headlineOf(event.reason)}. ` +
                    "The bead is open on the board again.",
                ),
              ],
              { lastFailure: { stage: "work", reason: event.reason } },
            ),
          );
        }

        case "abort":
          return applied(
            hop(state, event, "aborted", [
              warn(
                `Aborted: ${event.reason}. ${state.activeIssueId ?? "No"} issue was left in_progress ` +
                  `and will be resumed by the next run.`,
              ),
            ]),
          );

        default:
          return unhandled(state, event);
      }
    }

    case "finalize": {
      switch (event.type) {
        case "committed": {
          if (state.finalizeStage !== "commit") {
            return rejected(
              state,
              "stage-mismatch",
              `commit reported while finalizing the ${state.finalizeStage ?? "unknown"} stage`,
            );
          }
          const issueId = state.activeIssueId;
          if (issueId === null) {
            return rejected(state, "no-active-issue", "commit landed with no active issue to hand off");
          }
          const key = handoffKeyFor(issueId);
          const text = state.pendingHandoffText ?? `Completed ${issueId}`;
          return applied(
            hop(state, event, "finalize", [{ kind: "beads.remember", text, key }], {
              finalizeStage: "handoff",
              commitHash: event.hash,
              handoffKey: key,
              pendingHandoffText: text,
            }),
          );
        }

        case "remembered": {
          if (state.finalizeStage !== "handoff") {
            return rejected(
              state,
              "stage-mismatch",
              `handoff reported while finalizing the ${state.finalizeStage ?? "unknown"} stage`,
            );
          }
          if (state.handoffKey !== null && event.key !== state.handoffKey) {
            return rejected(
              state,
              "handoff-key-mismatch",
              `asked to remember ${state.handoffKey} but ${event.key} was reported`,
            );
          }
          const issueId = state.activeIssueId;
          if (issueId === null) {
            return rejected(state, "no-active-issue", "handoff written with no active issue to close");
          }
          const reason = state.pendingCloseReason ?? `Closed ${issueId}`;
          return applied(
            hop(state, event, "finalize", [{ kind: "beads.close_issue", id: issueId, reason }], {
              finalizeStage: "close",
              pendingCloseReason: reason,
            }),
          );
        }

        case "closed": {
          if (state.finalizeStage !== "close") {
            return rejected(
              state,
              "stage-mismatch",
              `close reported while finalizing the ${state.finalizeStage ?? "unknown"} stage`,
            );
          }
          if (event.id !== state.activeIssueId) {
            return rejected(
              state,
              "close-mismatch",
              `closed ${event.id} but ${state.activeIssueId ?? "nothing"} was the active issue`,
            );
          }
          return applied(
            throughBoundary(
              state,
              event,
              `${event.id} committed, remembered and closed`,
              [notifyPublish(state, event.id)],
              {
                commitHash: state.commitHash,
                handoffKey: state.handoffKey,
                lastFailure: null,
              },
            ),
          );
        }

        case "finalize_failed": {
          if (event.stage !== state.finalizeStage) {
            return rejected(
              state,
              "stage-mismatch",
              `failure reported for the ${event.stage} stage but the ${state.finalizeStage ?? "unknown"} stage was in flight`,
            );
          }
          // No auto-retry: a half-finalized iteration must not silently repeat a
          // commit or a close. `retry` re-issues the same pending effect.
          return applied(
            hop(state, event, "finalize", [
              warn(
                `Finalize ${event.stage} failed: ${event.reason}. ` +
                  `Nothing else was written; send retry to repeat just that stage.`,
              ),
            ]),
          );
        }

        case "retry": {
          const stage = state.finalizeStage;
          if (stage === null) {
            return rejected(state, "nothing-to-retry", "not finalizing");
          }
          const effect = pendingFinalizeEffect(state, stage);
          if (effect === null) {
            return rejected(state, "nothing-to-retry", `cannot rebuild the ${stage} stage effect`);
          }
          return applied(hop(state, event, "finalize", [effect]));
        }

        case "abort":
          return applied(
            hop(state, event, "aborted", [
              warn(
                `Aborted during finalize (${state.finalizeStage ?? "unknown"}). ` +
                  `Commit ${state.commitHash ?? "none"} exists; ${state.activeIssueId ?? "no issue"} may still be open.`,
              ),
            ]),
          );

        default:
          return unhandled(state, event);
      }
    }

    case "restart": {
      // `restart` never rests — `throughBoundary` exits it inside the same step.
      // This is the defensive path for a boundary state handed back in, and it
      // keeps the rule that a rejected step changes nothing: if the event is not
      // usable one step later, the boundary is not crossed either.
      const clear: StatePatch = {
        iteration: state.iteration + 1,
        activeIssueId: null,
        activeIssueTitle: null,
        pendingText: null,
        splitStage: null,
        finalizeStage: null,
      };
      if (event.type === "stop" || event.type === "abort") {
        // The run is ending, so there is nothing left to be cold for.
        return step({ ...state, ...clear, name: "check_work" }, event);
      }
      const crossed = hop(state, event, "check_work", [listInProgress(), listReady()], clear);
      if (event.type === "retry") {
        // Crossing the boundary already re-requests both board reads; that is the
        // retry, and emitting the reads twice would be noise in the trace.
        return { applied: true, state: crossed.state, effects: crossed.effects };
      }
      const replayed = step(crossed.state, event);
      if (!replayed.applied) {
        return rejected(state, replayed.rejection.code, `restart: ${replayed.rejection.message}`);
      }
      return { applied: true, state: replayed.state, effects: [...crossed.effects, ...replayed.effects] };
    }

    case "done":
    case "aborted": {
      return rejected(
        state,
        "terminal-state",
        `${state.name} is terminal; a fresh run needs a fresh machine, not a resurrected one`,
      );
    }

    default: {
      // Exhaustiveness: if a state is ever added to STATE_NAMES without a case
      // above, this line stops compiling rather than falling through to a
      // silently undefined transition.
      const exhausted: never = name;
      return rejected(state, "unknown-state", `unreachable state ${String(exhausted)}`);
    }
  }
}

// ── effect factories used above (kept separate so `retry` can rebuild them) ───

function runEffect(issueId: string, issueTitle: string): AgentRunEffect {
  return { kind: "agent.run", issueId, issueTitle };
}

function agentSplit(text: string): AgentSplitEffect {
  return { kind: "agent.split", text };
}

function vcsCommit(message: string, paths: readonly string[]): VcsCommitEffect {
  return { kind: "vcs.commit", message, paths: [...paths] };
}

/** Build the completion notice from the only state that can still see all of it. */
function notifyPublish(state: OrchestratorState, issueId: string): NotifyPublishEffect {
  return {
    kind: "notify.publish",
    issueId,
    title: state.activeIssueTitle ?? issueId,
    closeReason: state.pendingCloseReason ?? `Closed ${issueId}`,
    commit: state.commitHash,
    iteration: state.iteration,
    handoffKey: state.handoffKey ?? handoffKeyFor(issueId),
  };
}

/** Rebuild the currently pending finalize effect, byte-for-byte. */
function pendingFinalizeEffect(
  state: OrchestratorState,
  stage: FinalizeStage,
): Effect | null {
  if (stage === "commit") {
    if (state.pendingCommitMessage === null) return null;
    return vcsCommit(state.pendingCommitMessage, []);
  }
  if (stage === "handoff") {
    const key = state.handoffKey ?? (state.activeIssueId === null ? null : handoffKeyFor(state.activeIssueId));
    if (key === null || state.pendingHandoffText === null) return null;
    return { kind: "beads.remember", text: state.pendingHandoffText, key };
  }
  if (state.activeIssueId === null) return null;
  return {
    kind: "beads.close_issue",
    id: state.activeIssueId,
    reason: state.pendingCloseReason ?? `Closed ${state.activeIssueId}`,
  };
}

// ── ports (types only: the machine never calls them) ──────────────────────────

/**
 * What the interpreter (workspace-5yn.9) must supply. Each effect kind maps to
 * exactly one method here, so `for (const effect of result.effects) await
 * dispatch[effect.kind](effect)` is the whole boundary — and the only place the
 * machine's decisions become real.
 *
 * `beads` is the adapter from `src/beads.js` by name-for-name: `beads.list_ready`
 * → `listReady`, `beads.set_status` → `setStatus`, and so on. It is imported as
 * a *type only*, so this module cannot reach the board even by accident.
 */
export interface OrchestratorPorts {
  readonly beads: import("./beads.ts").BdClient;
  readonly agent: {
    split(text: string): Promise<NewIssueSpec[]>;
    run(issueId: string): Promise<unknown>;
  };
  readonly vcs: {
    commit(message: string, paths: readonly string[]): Promise<string>;
  };
  readonly ui: {
    say(text: string): void;
    warn(text: string): void;
  };
  /**
   * Where `notify.publish` lands. Optional: a run with nobody to tell has no
   * notifier, and the interpreter reports the effect as *skipped* rather than
   * pretending it went somewhere.
   *
   * Spelled structurally rather than imported: this module may name `beads` as a
   * type and nothing else (there is a test about it), so the port is declared
   * here and `Notifier` in `src/notify.ts` satisfies it by shape.
   */
  readonly notify?: {
    notifyCompletion(completion: NotifyPublishEffect): Promise<unknown>;
  };
  /**
   * Where `drop_context` lands. ADR-001: this must dispose the agent session, not
   * summarize it — `compact()` keeps model-summarised turns, which is the exact
   * thing this boundary exists to avoid.
   */
  readonly session: {
    dispose(): void | Promise<void>;
  };
}
