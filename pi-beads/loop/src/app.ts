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
import { createAgentRunner, harnessToolSchemas } from "./agent.ts";
import type { AgentRunner, ThinkingLevel, WorkOutcome } from "./agent.ts";
import { auditHeader, notableFindings, renderFinding, summariseAudit } from "./audit.ts";
import { describeContextBudget } from "./context.ts";
import { createBdClient, selectWorkable } from "./beads.ts";
import type { BdClient, Issue } from "./beads.ts";
import { createFinalizer, describeFinalizeFailure, isFinalized } from "./finalize.ts";
import type { FinalizeOutcome, FinalizeRequest } from "./finalize.ts";
import { createIdleMode } from "./idle.ts";
import type { IdleHandle, IdleOutcome, IdleStatus } from "./idle.ts";
import { createNullPresenter, createWorkPresenter, describeOutcome } from "./render.ts";
import type { WorkPresenter } from "./render.ts";
import { runLoop, LoopError } from "./loop.ts";
import type { LoopConfig, LoopIdlePort, LoopPorts, LoopResult, LoopUi } from "./loop.ts";
import { createSplitter, portFromAgentRunner } from "./split.ts";
import type { Splitter } from "./split.ts";
import { runStartupAudit } from "./startup.ts";
import type { StartupAuditResult } from "./startup.ts";
import { createGitWriter } from "./vcs.ts";
import type { GitWriter } from "./vcs.ts";

/**
 * The startup provider comparison: `GET <base>/health`, compared with the
 * `models.json` this run is about to use.
 *
 * On by default because the failures it catches are the expensive kind — a
 * window declared 6K short of what the server takes, a thinking level that
 * never reaches the template, an output ceiling that starves the verdict — and
 * all of them are visible before a ticket is claimed. `enabled: false` (or
 * `LOOP_AUDIT=0`) skips the request entirely, which is the right call when the
 * endpoint has no health page at all.
 *
 * `strict` turns an `error`-severity finding into a stop. Without it the loop
 * starts anyway, because a warning that halts a nightly run in its tracks is a
 * warning that gets switched off within a day; with it, an operator who has
 * decided the config must be right can enforce that.
 */
export interface ProviderAuditSetting {
  readonly enabled: boolean;
  readonly strict?: boolean;
  readonly verbose?: boolean;
  readonly writeMode?: "none" | "proposed" | "inplace";
  readonly healthUrl?: string;
  readonly timeoutMs?: number;
}

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
  /** The startup provider comparison. See {@link ProviderAuditSetting}. */
  readonly providerAudit?: ProviderAuditSetting;
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
    /** Replace the audit's transport (tests inject a fake `/health`). */
    readonly audit?: { readonly fetchImpl?: typeof fetch };
    /** Skip the startup audit entirely, whatever `providerAudit` says. */
    readonly skipAudit?: boolean;
  };
}

/** The assembled loop, with its adapters visible for assertions and shutdown. */
export interface App {
  readonly ports: LoopPorts;
  /** The work-stream presenter: live while work runs, down when idle is up. */
  readonly presenter: WorkPresenter;
  run(): Promise<LoopResult>;
}

/** What a finished work unit left on disk, in the few words the footer has room for. */function leftBehindFor(outcome: WorkOutcome): string | undefined {
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

  /**
   * The startup comparison, printed through the same surface everything else
   * uses so it lands in the transcript rather than floating above it.
   *
   * The audit is never the reason a run crashes: a broken health endpoint is a
   * warning line. It is only ever the reason a run *does not start* when the
   * operator switched strictness on — which is a decision taken in the
   * environment that this honours rather than second-guesses.
   */
  async function auditAtStartup(): Promise<void> {
    if (overrides.skipAudit === true) return;
    const setting = config.providerAudit ?? { enabled: true };
    if (setting.enabled === false) return;

    let result: StartupAuditResult | undefined;
    try {
      result = await runStartupAudit({
        cwd: config.cwd,
        ...(config.modelRef === undefined ? {} : { modelRef: config.modelRef }),
        levels: {
          ...(config.workThinkingLevel === undefined ? {} : { work: config.workThinkingLevel }),
          ...(config.splitThinkingLevel === undefined ? {} : { split: config.splitThinkingLevel }),
        },
        toolSchemas: harnessToolSchemas(),
        verbose: setting.verbose === true,
        writeMode: setting.writeMode ?? "none",
        ...(setting.healthUrl === undefined ? {} : { healthUrl: setting.healthUrl }),
        ...(setting.timeoutMs === undefined ? {} : { timeoutMs: setting.timeoutMs }),
        ...(overrides.audit?.fetchImpl === undefined ? {} : { fetchImpl: overrides.audit.fetchImpl }),
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      presenter.warn(`provider audit could not run: ${message}`);
      return;
    }

    const report = result?.report;
    if (report === undefined) {
      for (const line of result?.lines ?? []) presenter.notice("warn", line);
      return;
    }
    presenter.notice(report.counts.error > 0 ? "warn" : "info", auditHeader(report));
    for (const item of notableFindings(report, { showOk: setting.verbose === true })) {
      presenter.notice(
        item.severity === "error" ? "error" : item.severity === "warn" ? "warn" : "info",
        renderFinding(item, report.target),
      );
    }
    presenter.notice(report.counts.error > 0 ? "warn" : "info", summariseAudit(report));
    if (result.writtenTo !== undefined) {
      presenter.notice("info", `patched config written to ${result.writtenTo}`);
    }
    if (setting.strict === true && result.blocking) {
      throw new LoopError(
        "provider-audit",
        `startup audit stopped the run: ${report.counts.error} error(s) in the provider config `
          + `for ${report.provider}/${report.modelId} (LOOP_AUDIT_STRICT is on)`,
      );
    }
  }

  const ports: LoopPorts = {
    beads,
    runner,
    splitter,
    finalizer,
    git,
    idle,
    ui,
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
        await auditAtStartup();
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

/** Build and run. The single entry the CLI and the spike share. */
export async function runApp(config: AppConfig): Promise<LoopResult> {
  return buildApp(config).run();
}
