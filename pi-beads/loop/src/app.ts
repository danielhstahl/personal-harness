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
import { createAgentRunner } from "./agent.js";
import type { AgentRunner } from "./agent.js";
import { createBdClient } from "./beads.js";
import type { BdClient } from "./beads.js";
import { createFinalizer } from "./finalize.js";
import type { FinalizeOutcome, FinalizeRequest } from "./finalize.js";
import { createIdleMode } from "./idle.js";
import type { IdleStatus } from "./idle.js";
import { runLoop } from "./loop.js";
import type { LoopConfig, LoopIdlePort, LoopPorts, LoopResult, LoopUi } from "./loop.js";
import { createSplitter, portFromAgentRunner } from "./split.js";
import type { Splitter } from "./split.js";
import { createGitWriter } from "./vcs.js";
import type { GitWriter } from "./vcs.js";

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
    readonly ui?: LoopUi;
    readonly signals?: LoopPorts["signals"];
  };
}

/** The assembled loop, with its adapters visible for assertions and shutdown. */
export interface App {
  readonly ports: LoopPorts;
  run(): Promise<LoopResult>;
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

  const runner =
    overrides.runner ??
    createAgentRunner({
      beads,
      cwd: config.cwd,
      modelRef: config.modelRef,
      timeoutMs: config.workTimeoutMs,
    });

  const splitter =
    overrides.splitter ??
    createSplitter({ agent: portFromAgentRunner(runner), beads }, {
      epicId: config.epicId ?? null,
      epicTitle: config.epicTitle,
    });

  const finalizer =
    overrides.finalizer ??
    createFinalizer({ vcs: git, beads }, {
      dryRun: config.dryRun === true,
      onPlan: (line: string) => ui.say(line),
    });

  const ui: LoopUi = overrides.ui ?? {
    say: (text: string) => void process.stdout.write(`${text}\n`),
    warn: (text: string) => void process.stderr.write(`warning: ${text}\n`),
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

  const idle: LoopIdlePort =
    overrides.idle ??
    (() => {
      const handle = createIdleMode({ status: statusProvider, cwd: config.cwd, themeName: config.themeName });
      return {
        next: () => handle.next(),
        refresh: () => handle.refresh(),
        dispose: () => handle.dispose(),
      };
    })();

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
    run: () => runLoop(ports, config),
  };
}

/** Build and run. The single entry the CLI and the spike share. */
export async function runApp(config: AppConfig): Promise<LoopResult> {
  return buildApp(config).run();
}
