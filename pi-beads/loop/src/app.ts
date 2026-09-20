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
import type { AgentRunner, WorkOutcome } from "./agent.ts";
import { createBdClient } from "./beads.ts";
import type { BdClient } from "./beads.ts";
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

export interface AppConfig extends LoopConfig {
  /** Repository and board live here. */
  readonly cwd: string;
  readonly bdBin?: string;
  readonly gitBin?: string;
  /** Explicit model. Never read from the environment below this line. */
  readonly modelRef?: { provider: string; id: string };
  readonly workTimeoutMs?: number;
  readonly themeName?: string;
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
    case "timeout":
      return outcome.settledAfterAbort
        ? "budget expired; session settled after abort"
        : "budget expired; session still running when we stopped waiting";
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
      : overrides.presenter ?? createWorkPresenter({ themeName: config.themeName });

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
      presenter.setContext({ issueId, phase: "work" });
      takeWorkSurface();
      const outcome = await baseRunner.run(issueId);
      const said = describeOutcome({
        kind: outcome.kind,
        issueId: outcome.issueId,
        message: outcome.kind === "error" ? outcome.message : undefined,
        budgetMs: outcome.kind === "timeout" ? outcome.budgetMs : undefined,
        settledAfterAbort:
          outcome.kind === "timeout" ? outcome.settledAfterAbort : undefined,
        leftBehind: leftBehindFor(outcome),
      });
      presenter.notice(said.level, said.text);
      return outcome;
    },
    split: async (text: string) => {
      presenter.setContext({ phase: "split" });
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
      presenter.setContext({ issueId: request.issueId, phase: "finalize" });
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
      return {
        ready: ready.length,
        inProgress: inProgress.length,
        model: config.modelRef,
      };
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

/** Build and run. The single entry the CLI and the spike share. */
export async function runApp(config: AppConfig): Promise<LoopResult> {
  return buildApp(config).run();
}
