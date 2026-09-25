/**
 * pi-beads loop — the composition root (workspace-5yn.9).
 *
 * `src/loop.ts` runs the machine; this file decides what the machine talks to.
 * Every adapter built here is the real one for production, and every one of
 * them can be replaced through {@link AppConfig} so the live spike and the
 * integration tests build the same loop the CLI does — with a fake where a
 * model or a network would otherwise be.
 *
 * The rule this file exists to satisfy: `src/main.ts` wires, it does not
 * decide. Anything that could be configured is configured here, once.
 */
import { createAgentRunner } from "./agent.ts";
import type { AgentRunner, ThinkingLevel, WorkOutcome } from "./agent.ts";
import { describeContextBudget } from "./context.ts";
import { createBdClient, selectWorkable } from "./beads.ts";
import type { BdClient, Issue } from "./beads.ts";
import { createFinalizer, describeFinalizeFailure, isFinalized } from "./finalize.ts";
import type { FinalizeOutcome, FinalizeRequest } from "./finalize.ts";
import { createIdleMode } from "./idle.ts";
import type { IdleHandle, IdleOutcome, IdleStatus } from "./idle.ts";
import { createNullPresenter, createWorkPresenter, describeOutcome } from "./render.ts";
import type { WorkPresenter } from "./render.ts";
import { runLoop } from "./loop.ts";
import type { LoopConfig, LoopIdlePort, LoopPorts, LoopResult, LoopUi } from "./loop.ts";
import { createSplitter, portFromAgentRunner } from "./split.ts";
import type { Splitter } from "./split.ts";
import { createGitWriter } from "./vcs.ts";
import type { GitWriter } from "./vcs.ts";
import { createServerProfile } from "./profile.ts";
import type { ServerProfile } from "./profile.ts";
import { resolveProbeTarget, type ProbeTarget } from "./health.ts";
import { resolveRunModel } from "./agent.ts";

export interface AppConfig extends LoopConfig {
  /** Repository and board live here. */
  readonly cwd: string;
  readonly bdBin?: string;
  readonly gitBin?: string;
  /** Explicit model. Never read from the environment below this line. */
  readonly modelRef?: { provider: string; id: string };
  readonly workTimeoutMs?: number;
  /**
   * When the work session is asked to land what it has, in ms from the start of
   * the run. Unset means the runner's own rule: the budget minus
   * `WRAP_UP_LEAD_MS`. See {@link createAgentRunner}.
   */
  readonly wrapUpMs?: number;
  /**
   * Thinking level per pass. Unset is not "low" — it is *not configured*, and the
   * runner resolves it in `src/agent.ts` against the user's own default,
   * falling back to pi's.
   *
   * Two knobs because the two passes want different things: a split plans from one
   * paragraph it can hold in mind at once, a work run has to survive a repo. The
   * loop should not have to pick one number for both.
   */
  readonly workThinkingLevel?: ThinkingLevel;
  readonly splitThinkingLevel?: ThinkingLevel;
  /**
   * Overrides the `/health` endpoint the provider's own `baseUrl` implies.
   *
   * Leave it unset and the loop asks pi which box it is going to talk to — the
   * resolved model's `baseUrl` — and derives `…/health` from that, so moving
   * `models.json` moves the probe with it. Set it only where the derivation is
   * wrong: a separate management port, or a proxy that fronts the API but not
   * the box.
   *
   * Either way the probe is what lets the loop read the box's own limits — the
   * window, the output cap, whether the thinking knob reaches the wire at all,
   * whether anything is queued — instead of trusting `models.json`.
   */
  readonly healthUrl?: string;
  /** Per-probe timeout. Default 5s. */
  readonly healthTimeoutMs?: number;
  /**
   * How long a pass may wait for a free server slot before starting anyway.
   * The wait happens before the pass clock is armed, so queued time is not
   * charged to the ticket. Default 90s; `0` observes without waiting.
   */
  readonly capacityWaitMs?: number;
  readonly themeName?: string;
  /**
   * The live surface's cadence, in ms.
   *
   * `coalesceMs` is the refresh rate: every delta that arrives inside one
   * window costs a single frame, so 33 means ~30fps and 16 means ~60fps,
   * whatever the token rate is. `heartbeatMs` is how often a held surface
   * re-checks its time-derived footer (the elapsed field) and repaints if it
   * moved. `spinnerMs` is the faster beat a pending tool call gets, so an
   * outstanding call visibly turns instead of looking hung. All are ignored
   * when `overrides.presenter` supplies the surface.
   */
  readonly coalesceMs?: number;
  readonly heartbeatMs?: number;
  readonly spinnerMs?: number;
  /** Commit identity. Defaults name the loop rather than a human. */
  readonly authorName?: string;
  readonly authorEmail?: string;
  /** Replace any adapter — the spike and the tests use exactly this seam. */
  readonly overrides?: {
    readonly beads?: BdClient;
    readonly git?: GitWriter;
    readonly runner?: AgentRunner;
    readonly splitter?: Splitter;
    readonly finalizer?: { finalize(request: FinalizeRequest): Promise<FinalizeOutcome> };
    readonly idle?: LoopIdlePort;
    /**
     * Substitute the *kind* of idle surface without substituting the port, so
     * a test can hand in handles that model a single-shot teardown and still
     * watch the lifecycle this root applies. Ignored when `idle` is given.
     */
    readonly idleFactory?: () => IdleHandle;
    readonly ui?: LoopUi;
    readonly signals?: LoopPorts["signals"];
    /**
     * The work-stream surface (`.10`). Pass one to drive or observe it from a
     * test; pass `null` for a run that must not own the terminal at all — a
     * log-only spike, where a live surface would paint over the transcript.
     */
    readonly presenter?: WorkPresenter | null;
    /**
     * A resolved server profile. {@link runApp} builds one from `healthUrl`;
     * tests hand one in over a fake fetch. Absent means no probe, and every
     * consumer falls back to the declared config.
     */
    readonly serverProfile?: ServerProfile;
  };
}

/** The assembled loop, with its adapters visible for assertions and shutdown. */
export interface App {
  readonly ports: LoopPorts;
  /** The work-stream presenter: live while work runs, down when idle is up. */
  readonly presenter: WorkPresenter;
  run(): Promise<LoopResult>;
}

/** What a finished work unit left on disk, in the few words the footer has room for. */
function leftBehindFor(outcome: WorkOutcome): string | undefined {
  switch (outcome.kind) {
    case "done":
    case "incomplete": {
      const files = outcome.verdict.changedFiles;
      return files.length === 0
        ? "no changed files claimed"
        : `${files.length} changed file(s) claimed: ${files.slice(0, 3).join(", ")}`;
    }
    case "malformed-verdict":
      return `${outcome.problems.length} problem(s) with the verdict block`;
    case "context-exhausted":
      // Partial edits are exactly what this kind leaves behind — the run stopped
      // mid-ticket, on purpose, with the window full.
      return outcome.settledAfterAbort
        ? "context exhausted; session settled after abort"
        : "context exhausted; session still running when we stopped waiting";
    case "timeout":
      // The edits are still there and nothing was committed — which is the whole
      // story a timeout leaves, and the reason the next attempt is told to carry
      // on rather than start over.
      return outcome.settledAfterAbort
        ? "budget expired; nothing committed, every edit still in the working tree"
        : "budget expired; nothing committed and the session was still running when we stopped waiting";
    default:
      return undefined;
  }
}

/**
 * A {@link LoopIdlePort} that guarantees the optional members: the per-turn
 * port always has a surface to repaint and always has something it can tear
 * down, so a caller may call them without a `?.` and know it reached one.
 */
export interface PerTurnIdlePort extends LoopIdlePort {
  next(): Promise<IdleOutcome>;
  refresh(): void;
  dispose(): Promise<void>;
}

/**
 * A per-turn idle port over a handle factory.
 *
 * `src/idle.ts` is single-shot by contract, and deliberately so: any
 * resolution of `next()` — a submitted line or an exit — runs teardown, and a
 * later `next()` on the same handle rejects `already-finished`. The machine
 * comes back to idle every time a work cycle ends, which over the life of a
 * session is many times. So the composition root must not hold *an* idle mode;
 * it holds the current one and knows how to make the next.
 *
 * Handing the whole port to a fake hides this entirely — which is exactly how
 * the bug survived a green suite. A fake that never dies is a fake that never
 * gets asked the question.
 */
export function perTurnIdle(makeIdle: () => IdleHandle): PerTurnIdlePort {
  let current: IdleHandle | undefined;
  /**
   * The turn that is open right now. Worth its own slot because the real
   * surface's `next()` overwrites its internal resolver: a second call while
   * one is pending would orphan the first waiter forever. Handing back the same
   * promise makes a re-entrant ask harmless instead of fatal.
   */
  let open: Promise<IdleOutcome> | undefined;

  /** Forget a surface that is out of service, so the next ask builds a new one. */
  function retire(handle: IdleHandle): void {
    if (current === handle) current = undefined;
  }

  return {
    next: async () => {
      if (open !== undefined) return open;
      if (current === undefined || current.finished) current = makeIdle();
      const handle = current;
      let asked: Promise<IdleOutcome>;
      try {
        asked = Promise.resolve(handle.next());
      } catch (error) {
        // A surface that throws on the ask is not a surface to keep.
        retire(handle);
        throw error;
      }
      const turn = asked.then(
        (outcome) => {
          // Whatever it answered with — a submitted line or an exit — this
          // surface has run its one turn. Do not let the next turn inherit it.
          open = undefined;
          retire(handle);
          return outcome;
        },
        (error: unknown) => {
          // Same for a surface that failed to boot: drop it, and let the next
          // turn make a real one rather than inherit a dead end.
          open = undefined;
          retire(handle);
          throw error;
        },
      );
      open = turn;
      return turn;
    },
    /**
     * A refresh repaints a surface that exists; it must never build one. The
     * status line is polled before the prompt is up, and booting a terminal to
     * repaint nothing would take the keyboard for no reason.
     */
    refresh: async () => {
      if (current !== undefined && !current.finished) current.refresh();
    },
    dispose: async () => {
      const handle = current;
      current = undefined;
      try {
        await handle?.dispose();
      } finally {
        // A turn that was still open on the surface we just tore down is
        // dropped, never handed out again — and it is dropped *after* the
        // teardown settles, so no new surface can be built and attached while
        // the old one is still coming down.
        open = undefined;
      }
    },
  };
}

/**
 * Build the loop without running it. Returns the ports too, so a caller (a
 * spike, a test) can inspect or drive the pieces after the run.
 */
/**
 * Board reads → the idle line's numbers, through the loop's own definition of
 * pickable work ({@link selectWorkable}). Kept exported and pure so a test can
 * set the status line and the pick list side by side and check they still agree
 * — which is the whole reason they share the filter.
 */
export function idleStatusFrom(
  ready: readonly Issue[],
  inProgress: readonly Issue[],
  options: {
    workEpics?: boolean;
    model?: { provider: string; id: string };
  } = {},
): IdleStatus {
  const workEpics = options.workEpics === true;
  const readyPick = selectWorkable(ready, { workEpics });
  const progressPick = selectWorkable(inProgress, { workEpics });
  return {
    ready: readyPick.pickable.length,
    inProgress: progressPick.pickable.length,
    heldOut: readyPick.heldOut.length + progressPick.heldOut.length,
    model: options.model,
  };
}

export function buildApp(config: AppConfig): App {
  const overrides = config.overrides ?? {};
  const labels = config.labels === undefined ? undefined : { labels: config.labels };

  const beads = overrides.beads ?? createBdClient({ bin: config.bdBin, cwd: config.cwd });
  const git = overrides.git ?? createGitWriter({
    cwd: config.cwd,
    bin: config.gitBin,
    authorName: config.authorName ?? "pi-loop",
    authorEmail: config.authorEmail ?? "pi-loop@localhost",
  });

  // ── the work-stream surface (`.10`) ───────────────────────────────────
  //
  // One presenter, built once, fed from three places: the runner's `onEvent`
  // seam, the loop's `ui` port, and the footer context this root supplies from
  // the calls the loop already makes. It never decides anything — the loop tells
  // it what is running by calling `run(issueId)`.
  const presenter: WorkPresenter =
    overrides.presenter === null
      ? createNullPresenter()
      : overrides.presenter ??
        createWorkPresenter({
          themeName: config.themeName,
          coalesceMs: config.coalesceMs,
          heartbeatMs: config.heartbeatMs,
          spinnerMs: config.spinnerMs,
        });

  /**
   * Work-unit identity for the surface. Every work, split and finalize unit takes
   * the next number, so the presenter can tell "a new unit has started" from
   * "the same unit is still going" — which is the only way it can tell a
   * re-queued ticket from one that has simply been running a long time.
   */
  let unitsIssued = 0;
  const nextRunId = (): number => {
    unitsIssued += 1;
    return unitsIssued;
  };

  // What the probe learned, printed before anything runs. These lines are the
  // difference between a configured value and a value that arrived off the
  // network: without them the two look identical in the terminal, and the only
  // way to tell them apart is to read the code that chose them.
  for (const line of overrides.serverProfile?.describe() ?? []) {
    presenter.notice("info", line);
  }
  // The full adoption trail under `verbose`. The compact line says what the loop
  // decided; this says why, one field at a time, which is the difference between
  // grepping a log and reading a module.
  if (config.verbose === true) {
    for (const note of overrides.serverProfile?.notes() ?? []) {
      presenter.notice("info", `  · ${note}`);
    }
  }

  /**
   * Only one surface may hold the terminal at a time. While the idle prompt is
   * up it owns the keyboard, so the presenter must not take the live path —
   * its text still goes out, as plain lines, in order.
   */
  let idleHoldsTerminal = false;

  function takeWorkSurface(): void {
    if (idleHoldsTerminal) return;
    presenter.acquire();
  }

  function handSurfaceOver(): void {
    presenter.release();
  }

  const baseRunner =
    overrides.runner ??
    createAgentRunner({
      beads,
      cwd: config.cwd,
      modelRef: config.modelRef,
      timeoutMs: config.workTimeoutMs,
      wrapUpMs: config.wrapUpMs,
      workThinkingLevel: config.workThinkingLevel,
      splitThinkingLevel: config.splitThinkingLevel,
      serverProfile: overrides.serverProfile,
      // The streaming half of the seam: every runner event lands on the
      // presenter, which is the only thing that draws them.
      onEvent: (event) => presenter.feed(event),
    });

  /**
   * The runner, wrapped so the surface knows what it is watching and says what
   * happened when the unit ends. The decorators read facts the caller supplied
   * (`issueId` in, `WorkOutcome` out); they add no state machine of their own.
   */
  const runner: AgentRunner = {
    run: async (issueId: string): Promise<WorkOutcome> => {
      presenter.setContext({ issueId, phase: "work", runId: nextRunId() });
      takeWorkSurface();
      const outcome = await baseRunner.run(issueId);
      const said = describeOutcome({
        kind: outcome.kind,
        issueId: outcome.issueId,
        message: outcome.kind === "error" ? outcome.message : undefined,
        budgetMs: outcome.kind === "timeout" ? outcome.budgetMs : undefined,
        elapsedMs: outcome.kind === "timeout" ? outcome.elapsedMs : undefined,
        settledAfterAbort:
          outcome.kind === "timeout" ? outcome.settledAfterAbort : undefined,
        turns: outcome.kind === "context-exhausted" ? outcome.turns : undefined,
        contextText:
          outcome.kind === "context-exhausted"
            ? describeContextBudget(outcome.budget)
            : undefined,
        leftBehind: leftBehindFor(outcome),
      });
      presenter.notice(said.level, said.text);
      return outcome;
    },
    split: async (text: string) => {
      presenter.setContext({ phase: "split", runId: nextRunId() });
      takeWorkSurface();
      return baseRunner.split(text);
    },
    dispose: (): Promise<number> => baseRunner.dispose(),
    liveSessionIds: (): readonly string[] => baseRunner.liveSessionIds(),
    stats: () => baseRunner.stats(),
  };

  const splitter =
    overrides.splitter ??
    createSplitter({ agent: portFromAgentRunner(runner), beads }, {
      epicId: config.epicId ?? null,
      epicTitle: config.epicTitle,
    });

  const baseFinalizer =
    overrides.finalizer ??
    createFinalizer({ vcs: git, beads }, {
      dryRun: config.dryRun === true,
      onPlan: (line: string) => ui.say(line),
    });

  /**
   * The finalize step is part of the stream too: the footer says so while it
   * runs, and the outcome is stated in its own colour instead of vanishing
   * into a return value nobody reads.
   */
  const finalizer = {
    async finalize(request: FinalizeRequest): Promise<FinalizeOutcome> {
      presenter.setContext({
        issueId: request.issueId,
        phase: "finalize",
        runId: nextRunId(),
      });
      takeWorkSurface();
      const outcome = await baseFinalizer.finalize(request);
      presenter.notice(
        isFinalized(outcome) || outcome.kind === "planned" ? "info" : "error",
        describeFinalizeFailure(outcome),
      );
      return outcome;
    },
  };

  /**
   * The loop's text goes through the same surface as the agent's, so a `say`
   * after a tool summary lands *below* it rather than racing past it. Override
   * it wholesale and the presenter is simply not fed — that is the caller's
   * choice, not a fallback hidden in here.
   */
  const ui: LoopUi = overrides.ui ?? {
    say: (text: string) => {
      takeWorkSurface();
      presenter.say(text);
    },
    warn: (text: string) => {
      takeWorkSurface();
      presenter.warn(text);
    },
  };

  /**
   * The idle status line reads the board rather than a cached count, so what it
   * shows is what the next pick will see. A read failure degrades to zeroes —
   * a stale number beats a crash in a status line.
   */
  async function statusProvider(): Promise<IdleStatus> {
    try {
      const [ready, inProgress] = await Promise.all([
        beads.listReady(labels ?? {}),
        beads.listInProgress(labels ?? {}),
      ]);
      return idleStatusFrom(ready, inProgress, {
        workEpics: config.workEpics === true,
        model: config.modelRef,
      });
    } catch {
      return { ready: 0, inProgress: 0, model: config.modelRef };
    }
  }

  /**
   * How an idle surface gets made. The real one in production; a test can hand
   * in a different handle kind and still be exercising the rule that matters,
   * which is the one below — a new surface per turn.
   */
  const makeIdle: () => IdleHandle =
    overrides.idleFactory ??
    ((): IdleHandle =>
      createIdleMode({
        status: statusProvider,
        cwd: config.cwd,
        themeName: config.themeName,
      }));

  const baseIdle: LoopIdlePort = overrides.idle ?? perTurnIdle(makeIdle);

  /**
   * The handoff, applied to *whatever* idle port we resolved — real or
   * injected — so the two surfaces can never both be attached to the terminal:
   * the work presenter is released before idle comes up, and taken back only
   * once idle is gone.
   */
  const idle: LoopIdlePort = {
    next: async () => {
      handSurfaceOver();
      idleHoldsTerminal = true;
      try {
        return await baseIdle.next();
      } finally {
        idleHoldsTerminal = false;
      }
    },
    refresh: async () => {
      await baseIdle.refresh?.();
    },
    dispose: async () => {
      handSurfaceOver();
      await baseIdle.dispose?.();
    },
  };

  const ports: LoopPorts = {
    beads,
    runner,
    splitter,
    finalizer,
    git,
    idle,
    ui,
    // Admission, asked before each pass so queue time is not charged to the
    // ticket. Undefined without a probe: the loop then behaves exactly as it did
    // before, blind to the box and no worse off for it.
    capacity:
      overrides.serverProfile === undefined
        ? undefined
        : () => overrides.serverProfile!.awaitCapacity(),
    blockers:
      overrides.serverProfile === undefined
        ? undefined
        : async () => overrides.serverProfile!.blockers(),
    signals: overrides.signals ?? {
      on(signal: string, handler: () => void) {
        process.on(signal, handler);
        return () => {
          process.removeListener(signal, handler);
        };
      },
    },
  };

  return {
    ports,
    presenter,
    run: async (): Promise<LoopResult> => {
      try {
        return await runLoop(ports, config);
      } finally {
        // Nothing outlives the run: no live screen, no footer claiming to be
        // current, no half-painted frame left in scrollback.
        handSurfaceOver();
        presenter.dispose();
      }
    },
  };
}

/**
 * Where to probe, and why.
 *
 * The provider's own `baseUrl` is the default because it is the address the run
 * will actually use. A second URL in the environment describes the same box
 * twice, and the second one goes stale the first time `models.json` moves —
 * after which the probe reports the limits of a machine nobody is talking to,
 * which is worse than reporting nothing. `LOOP_HEALTH_URL` stays as the
 * override for what the guess cannot express: a separate management port, or a
 * proxy that fronts the API but not the box.
 *
 * A model that will not resolve is reported as the reason there is no probe
 * rather than raised here — the run raises it itself, better, at the point where
 * it needs a model.
 */
async function probeTargetForRun(config: AppConfig): Promise<ProbeTarget> {
  let providerBaseUrl: string | undefined;
  let providerNote: string | undefined;
  try {
    const model = await resolveRunModel(config.cwd, config.modelRef);
    providerBaseUrl = model?.baseUrl;
    if (model === undefined) {
      providerNote =
        "pi has no configured default model, so there is no box to ask; " +
        "using the declared model config";
    }
  } catch (error) {
    providerNote =
      `the box is unknown because the model could not be resolved ` +
      `(${error instanceof Error ? error.message : String(error)}); using the declared model config`;
  }
  return resolveProbeTarget({
    configuredUrl: config.healthUrl,
    providerBaseUrl,
    providerNote,
  });
}

/**
 * Build and run. The single entry the CLI and the spike share.
 *
 * The probe happens here rather than inside `buildApp` because it is the one
 * piece of setup that needs the network, and `buildApp` stays synchronous — a
 * composition root that blocks on a GET is a composition root nobody can test.
 * A failed probe is not an error here: the profile carries the reason, prints
 * it, and the declared config stands.
 */
export async function runApp(config: AppConfig): Promise<LoopResult> {
  if (config.overrides?.serverProfile !== undefined) {
    return buildApp(config).run();
  }
  const serverProfile = await createServerProfile({
    target: await probeTargetForRun(config),
    timeoutMs: config.healthTimeoutMs,
    waitMs: config.capacityWaitMs,
  });
  return buildApp({
    ...config,
    overrides: { ...config.overrides, serverProfile },
  }).run();
}
