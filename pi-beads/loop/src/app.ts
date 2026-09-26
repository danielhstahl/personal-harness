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
import {
  createKanbanSource,
  createNullKanban,
  type KanbanColumnKey,
  type KanbanMode,
  type KanbanRead,
  type KanbanSource,
} from "./kanban.ts";
import { createNtfyPublisher, createHttpTransport, localHostname, resolveNtfyTarget } from "./ntfy.ts";
import { createNotifier, createNullNotifier, type Notifier } from "./notify.ts";
import {
  PLAIN_MONITOR_THEME,
  createBackendMonitor,
  createNullMonitor,
  resolveMonitorUrls,
  type BackendMonitor,
  type MonitorTheme,
} from "./monitor.ts";
import {
  createNullPresenter,
  createPresenterTheme,
  createWorkPresenter,
  describeOutcome,
} from "./render.ts";
import type { WorkPresenter } from "./render.ts";
import { runLoop, LoopError } from "./loop.ts";
import type { LoopConfig, LoopIdlePort, LoopPorts, LoopResult, LoopUi } from "./loop.ts";
import { createSplitter, portFromAgentRunner } from "./split.ts";
import type { Splitter } from "./split.ts";
import { resolveProviderBaseUrl, runStartupAudit } from "./startup.ts";
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

/**
 * The read-only backend monitor: the panel that shows what the inference server
 * is doing while the loop is using it.
 *
 * On by default. It costs four unauthenticated `GET`s per `intervalMs` against
 * a server that is already on the network path of every token drawn here, and it
 * answers questions that are otherwise invisible until a ticket times out — KV
 * pressure, a slot somebody else is holding, a prompt cache that went cold, a
 * drafter that is not the one you thought you were using. Off is one env
 * variable away for a server with no diagnostics endpoints at all, and turning
 * it off costs nothing but the panel.
 */
export interface MonitorSetting {
  readonly enabled: boolean;
  /** Poll cadence. Default 2000ms. */
  readonly intervalMs?: number;
  /** Per-request deadline. Default 1500ms. */
  readonly timeoutMs?: number;
  /** How often to re-read `/v1/models`. Default 30000ms. */
  readonly modelsEveryMs?: number;
  /** Override the derived base entirely (`http://host:8081` or `.../v1`). */
  readonly url?: string;
  /** Rows the panel may take. Default 2. */
  readonly lines?: number;
  /**
   * Where the panel sits. `"band"` (default) is directly above the footer —
   * always on screen, never covering the transcript. `"top"` puts it at the top
   * of the work surface, which scrolls with the transcript; the idle surface
   * always draws it at the top of the screen, where nothing scrolls.
   */
  readonly placement?: "band" | "top";
  readonly verbose?: boolean;
}

/**
 * The mini kanban: the board as three columns — what is ready, what is being
 * worked, what is done — drawn alongside the monitor.
 *
 * On by default, with defaults set by a different cost model than the
 * monitor's. Every read here is a `bd` child process rather than a socket
 * read, so the cadence is slower (5s) and consecutive failures back off to a
 * minute: a board that cannot be reached must cost one notice and a slow
 * trickle of retries, not a process spawn per second.
 *
 * `mode` picks the shape: `"row"` is one dense line (the work surface's
 * default, which already carries a transcript, a monitor and a footer);
 * `"board"` is the bordered three-column grid, which the idle surface
 * defaults to because nothing there is competing for the screen.
 */
export interface KanbanSetting {
  readonly enabled: boolean;
  /** Read cadence. Default 5000ms. */
  readonly intervalMs?: number;
  readonly mode?: KanbanMode;
  /** Rows the grid may take, borders included. */
  readonly lines?: number;
  /** How many closed tickets the `done` column keeps. Default 12. */
  readonly doneLimit?: number;
  /** Where the board sits on the work surface. Default `"band"`. */
  readonly placement?: "band" | "top";
  readonly verbose?: boolean;
}

/**
 * The completion notice: one ntfy publish per closed bead, to a topic named in
 * the environment.
 *
 * Off unless a topic is given, which is the whole switch: `LOOP_NTFY_TOPIC`
 * unset means no publisher, no socket, and a transcript line that says nobody
 * was told rather than silence about it. For a feature whose entire trigger is
 * "something finished, repeatedly, unattended", off-by-default is the only sane
 * starting position.
 *
 * The refusal cases — a URL with no scheme, credentials stuffed into the URL, a
 * priority that is neither 1–5 nor a name — stop the run at startup instead of
 * warning per bead. A typo is found once, immediately, in the terminal it was
 * typed in; found at the first closed bead it means every notice for the rest of
 * the run went nowhere and nobody noticed a day later.
 *
 * A *delivery* failure is a different animal and stops nothing: the work is
 * committed and closed whatever the ntfy server does. See
 * [`src/notify.ts`](./notify.ts) and
 * [ADR-007](../docs/ADR-007-ntfy-notices.md).
 */
export interface NotifySetting {
  readonly enabled?: boolean;
  /** The ntfy topic: a bare name, or a full `http(s)://host/topic` URL. */
  readonly topic?: string;
  /**
   * The ntfy server, for a bare topic. Default `https://ntfy.sh`; point it at a
   * self-hosted instance (`http://192.168.1.20:8080`) and nothing downstream
   * knows the difference.
   */
  readonly url?: string;
  /** Bearer token, for a server that requires one. */
  readonly token?: string;
  /** ntfy priority: 1–5 or `min`/`low`/`default`/`high`/`urgent`. */
  readonly priority?: string;
  /** Emoji names shown on the notification, e.g. `["+1"]`. */
  readonly tags?: readonly string[];
  /** URL the notification opens. */
  readonly click?: string;
  /** The `[pi-beads]` in the title. */
  readonly titlePrefix?: string;
  /** Request deadline. Default 10s. */
  readonly timeoutMs?: number;
  /** Stop trying after this many failed publishes in a row. Default 3. */
  readonly maxConsecutiveFailures?: number;
}

/**
 * What to do when something else is holding `.git/index.lock`.
 *
 * See [ADR-006](../docs/ADR-006-index-lock.md) for why the two numbers here are
 * different knobs and why removal is opt-in: contention with a live process is
 * normal and should be waited out, while a lock left by a killed git should be
 * cleared — and the two look identical until you check the lock's age.
 */
export interface GitLockSetting {
  /** How long a write waits out somebody else's lock. Default 30s. */
  readonly waitMs?: number;
  /** Grace after `SIGTERM` before `SIGKILL`. Default 5s. */
  readonly killGraceMs?: number;
  /** A lock older than this looks abandoned. Default 60s. */
  readonly staleAfterMs?: number;
  /** `report` (default) leaves it alone; `remove` clears a stale one once.` */
  readonly stalePolicy?: "report" | "remove";
}

export interface AppConfig extends LoopConfig {
  /** Repository and board live here. */
  readonly cwd: string;
  readonly bdBin?: string;
  readonly gitBin?: string;
  /** Index-lock policy for every git write this run makes. */
  readonly gitLock?: GitLockSetting;
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
  /** The read-only server panel. See {@link MonitorSetting}. */
  readonly monitor?: MonitorSetting;
  readonly kanban?: KanbanSetting;
  /** The completion notice. Off unless `LOOP_NTFY_TOPIC` names a topic. */
  readonly notify?: NotifySetting;
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
    /** Substitute the monitor wholesale (`createNullMonitor()` to silence). */
    readonly monitor?: BackendMonitor;
    /** Substitute the mini kanban wholesale. */
    readonly kanban?: KanbanSource;
    /**
     * Substitute the completion notifier wholesale — a recording fake for a test
     * that wants to assert what a closed bead *said*, with no relay anywhere near
     * it. `createNullNotifier()` silences the capability entirely.
     */
    readonly notifier?: Notifier;
    /** Where the monitor finds its provider's base URL (tests: a fixture path). */
    readonly monitorBaseUrl?: string;
    /** Read `models.json` from somewhere else when resolving the base. */
    readonly modelsPath?: string;
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
     * Replace the transport to the inference backend. The startup audit reads
     * `/health` through it and the monitor reads all four diagnostic endpoints
     * through it, which is deliberate: one injection point controls every byte
     * a run puts on the wire, so a test can assert on the whole conversation
     * rather than on one participant's view of it.
     */
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
  /** The backend monitor: polled while the run is, drawn by both surfaces. */
  readonly monitor: BackendMonitor;
  /** The mini kanban: read-only board view, drawn by both surfaces. */
  readonly kanban: KanbanSource;
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

/**
 * Build the monitor source the run draws.
 *
 * Everything about this is allowed to fail into "off". No base URL could be
 * resolved, the theme is unreachable, the endpoints are all absent — in every
 * one of those cases the answer is a monitor that renders nothing, because the
 * panel is a window into the server and a window that will not open is not a
 * reason for the house to stop.
 *
 * The base URL is resolved the same way the startup audit resolves it (pi's
 * default-model precedence through `models.json`), so the monitor is looking at
 * the server this run is actually talking to. An explicit `LOOP_MONITOR_URL`
 * overrides that for the cases where the answer is "no, look over there".
 */
function buildMonitor(
  config: AppConfig,
  overrides: AppConfig["overrides"] = {},
): BackendMonitor {
  if (overrides.monitor !== undefined) return overrides.monitor;
  const setting = config.monitor ?? { enabled: true };
  if (setting.enabled === false) return createNullMonitor("switched off by LOOP_MONITOR=0");

  const resolved = resolveProviderBaseUrl({
    cwd: config.cwd,
    ...(config.modelRef === undefined ? {} : { modelRef: config.modelRef }),
    ...(overrides.modelsPath === undefined ? {} : { modelsPath: overrides.modelsPath }),
  });
  // The explicit monitor URL beats the audited health URL, which beats the
  // provider's own base — most specific hint wins.
  const baseUrl = overrides.monitorBaseUrl ?? resolved.baseUrl;
  const urls = resolveMonitorUrls({
    ...(setting.url === undefined ? {} : { monitorUrl: setting.url }),
    ...(config.providerAudit?.healthUrl === undefined
      ? {}
      : { healthUrl: config.providerAudit.healthUrl }),
    ...(baseUrl === undefined ? {} : { baseUrl }),
  });
  if (Object.values(urls).every((value) => value === undefined)) {
    // Nothing named the server: no monitor URL, no audited health URL, no
    // provider base to derive from. Saying so is the difference between a
    // missing panel and a panel that was never asked to exist.
    return createNullMonitor("no backend url to read — set LOOP_MONITOR_URL or a provider baseUrl");
  }

  let theme: MonitorTheme;
  try {
    theme = createPresenterTheme(config.themeName);
  } catch {
    // A panel with no colour still tells the truth; a panel that throws does not.
    theme = PLAIN_MONITOR_THEME;
  }

  return createBackendMonitor({
    urls,
    theme,
    ...(setting.intervalMs === undefined ? {} : { intervalMs: setting.intervalMs }),
    ...(setting.timeoutMs === undefined ? {} : { timeoutMs: setting.timeoutMs }),
    ...(setting.modelsEveryMs === undefined ? {} : { modelsEveryMs: setting.modelsEveryMs }),
    maxLines: setting.lines ?? 2,
    verbose: setting.verbose === true,
    ...(overrides.audit?.fetchImpl === undefined ? {} : { fetchImpl: overrides.audit.fetchImpl }),
  });
}

/**
 * Build the completion notifier — or the thing that says why nothing will be sent.
 *
 * The two off states are distinct on purpose, because they are distinct answers:
 * "no address was ever given" and "an address was given and switched off" read
 * differently in a transcript, and one of them is a question somebody asks.
 *
 * Everything unusable is refused here, at build time, with a `notify-config`
 * `LoopError` the CLI turns into one line and exit 2. A bad endpoint and a bad
 * priority are operator errors, and the place to report an operator error is the
 * terminal they typed it into — not a per-bead warning in a log nobody is
 * reading at 3am.
 */
/**
 * Build the completion notifier — or the thing that says why nothing will be sent.
 *
 * The two off states are distinct on purpose, because they are distinct
 * answers: "no topic was ever given" and "a topic was given and switched off"
 * read differently in a transcript, and one of them is a question somebody asks.
 *
 * Everything unusable is refused here, at build time, with a `notify-config`
 * `LoopError` the CLI turns into one line and exit 2. An endpoint with no scheme
 * and a priority that is not a priority are operator errors, and the place to
 * report an operator error is the terminal it was typed into — not a per-bead
 * warning in a log nobody is reading at 3am.
 */
function buildNotifier(
  config: AppConfig,
  overrides: AppConfig["overrides"] = {},
): Notifier {
  if (overrides.notifier !== undefined) return overrides.notifier;
  const setting = config.notify;
  if (setting === undefined || setting.enabled === false) {
    return createNullNotifier(
      "LOOP_NTFY_TOPIC is unset, so no one is told when a bead closes",
    );
  }
  if ((setting.topic ?? "").trim() === "") {
    return createNullNotifier(
      "LOOP_NTFY_TOPIC is set but empty, so there is nowhere to tell when a bead closes",
    );
  }
  const hostname = localHostname();

  let target;
  try {
    target = resolveNtfyTarget({
      topic: setting.topic,
      ...(setting.url === undefined ? {} : { url: setting.url }),
    });
  } catch (error) {
    throw new LoopError(
      "notify-config",
      `the completion notice cannot be published: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  if (setting.priority !== undefined && !isNtfyPriority(setting.priority)) {
    throw new LoopError(
      "notify-config",
      `LOOP_NTFY_PRIORITY must be 1-5 or one of ${NTFY_PRIORITY_NAMES.join(", ")}, ` +
        `not "${setting.priority}"`,
    );
  }

  /**
   * The give-up threshold, checked before it can mean something unintended.
   *
   * `0` reads as "never give up" to one reader and "give up immediately" to
   * another, and a non-integer is meaningless as a count of tries. Both are
   * refused here rather than clamped: the notifier's default is a decision, and
   * quietly falling back to it is how a typo ends up costing a day of notices.
   */
  const maxFailures = setting.maxConsecutiveFailures;
  if (maxFailures !== undefined && (!Number.isSafeInteger(maxFailures) || maxFailures < 1)) {
    throw new LoopError(
      "notify-config",
      `LOOP_NTFY_MAX_FAILURES must be a whole number of tries, at least 1, not ${maxFailures}`,
    );
  }

  const model =
    config.modelRef === undefined
      ? undefined
      : `${config.modelRef.provider}/${config.modelRef.id}`;

  return createNotifier({
    publisher: createNtfyPublisher({
      target,
      transport: createHttpTransport(),
      ...(setting.token === undefined ? {} : { token: setting.token }),
      ...(setting.timeoutMs === undefined ? {} : { timeoutMs: setting.timeoutMs }),
    }),
    context: {
      cwd: config.cwd,
      hostname,
      ...(model === undefined ? {} : { model }),
      ...(setting.titlePrefix === undefined ? {} : { titlePrefix: setting.titlePrefix }),
    },
    ...(setting.priority === undefined ? {} : { priority: setting.priority }),
    ...(setting.tags === undefined ? {} : { tags: setting.tags }),
    ...(setting.click === undefined ? {} : { click: setting.click }),
    ...(maxFailures === undefined ? {} : { maxConsecutiveFailures: maxFailures }),
  });
}

const NTFY_PRIORITY_NAMES = ["min", "low", "default", "high", "urgent"] as const;

/** ntfy takes a priority as 1–5 or as one of five names. Nothing else. */
function isNtfyPriority(value: string): boolean {
  const raw = value.trim().toLowerCase();
  if (NTFY_PRIORITY_NAMES.includes(raw as (typeof NTFY_PRIORITY_NAMES)[number])) return true;
  return /^[1-5]$/.test(raw);
}

export function buildApp(config: AppConfig): App {
  const overrides = config.overrides ?? {};
  const labels = config.labels === undefined ? undefined : { labels: config.labels };

  const baseBeads = overrides.beads ?? createBdClient({ bin: config.bdBin, cwd: config.cwd });
  const git = overrides.git ?? createGitWriter({
    cwd: config.cwd,
    bin: config.gitBin,
    authorName: config.authorName ?? "pi-loop",
    authorEmail: config.authorEmail ?? "pi-loop@localhost",
    ...(config.gitLock?.waitMs === undefined ? {} : { lockWaitMs: config.gitLock.waitMs }),
    ...(config.gitLock?.killGraceMs === undefined
      ? {}
      : { killGraceMs: config.gitLock.killGraceMs }),
    ...(config.gitLock?.staleAfterMs === undefined
      ? {}
      : { staleLockAfterMs: config.gitLock.staleAfterMs }),
    ...(config.gitLock?.stalePolicy === undefined
      ? {}
      : { staleLockPolicy: config.gitLock.stalePolicy }),
  });

  // ── the backend monitor ──────────────────────────────────────────────────
  //
  // Built here, drawn by both surfaces: one poller, one snapshot, two places to
  // look at it. The presenter gets it in the band above the footer, the idle
  // prompt gets it as the top line of the screen, and neither of them can make
  // a request — they read what this already has.
  const monitor: BackendMonitor = buildMonitor(config, overrides);

  // ── the mini kanban ─────────────────────────────────────────────────────
  //
  // Built here for the same reason as the monitor: one read of the board, one
  // picture, drawn in two places. It is given a *reader*, not the client —
  // `src/kanban.ts` has no `BdClient` in its signature, so there is nothing
  // in the module that could move a ticket even by accident.
  const kanban: KanbanSource = buildKanban(config, baseBeads, overrides, labels);

  // ── the completion notice ────────────────────────────────────────────────
  //
  // Built once, per run, from the environment: the address, the relay, the
  // identity. The loop asks nothing of it beyond `notifyCompletion`, and is
  // perfectly content when the answer is "nobody is listening, `LOOP_NTFY_TOPIC`
  // is unset" — which is the state every run of this loop was in before the
  // feature existed, and still is for anyone who does not set it.
  const notifier: Notifier = buildNotifier(config, overrides);

  // The real client gets wrapped so this run's own writes refresh the picture
  // immediately. An injected fake is not wrapped: it is exactly the object the
  // test asked for, and a refresh bolted onto it would change what that test is
  // counting.
  const beads: BdClient =
    overrides.beads === undefined ? refreshBoardOnWrite(baseBeads, kanban) : baseBeads;

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
          monitor,
          monitorPlacement: config.monitor?.placement ?? "band",
          monitorLines: config.monitor?.lines ?? 2,
          kanban,
          kanbanMode: config.kanban?.mode ?? "row",
          kanbanPlacement: config.kanban?.placement ?? "band",
          ...(config.kanban?.lines === undefined ? {} : { kanbanLines: config.kanban.lines }),
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
        monitor,
        monitorLines: config.monitor?.lines ?? 2,
        kanban,
        kanbanMode: config.kanban?.mode ?? "board",
        kanbanLines: config.kanban?.lines ?? 5,
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
    notify: notifier,
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
    monitor,
    kanban,
    run: async (): Promise<LoopResult> => {
      try {
        // The monitor starts before the audit so the panel is already showing
        // something by the time the first finding is printed under it, and so a
        // first poll that fails is on screen as `✕ unreachable` rather than a
        // blank row that might just be slow.
        monitor.start();
        kanban.start();
        if (config.kanban?.verbose === true) {
          await kanban.refresh().catch(() => undefined);
          for (const line of kanban.describe()) presenter.notice("info", line);
        }
        if (config.monitor?.verbose === true) {
          // Join the cycle that `start()` kicked off before describing it:
          // `describe()` printed now says what answered and what it exposed,
          // instead of a column of "not asked" about requests still in flight.
          await monitor.poll().catch(() => undefined);
          for (const line of monitor.describe()) presenter.notice("info", line);
        }
        await auditAtStartup();
        return await runLoop(ports, config);
      } finally {
        // Nothing outlives the run: no live screen, no footer claiming to be
        // current, no half-painted frame left in scrollback — and no poller
        // still asking a server about itself after the thing that cared about
        // the answer has gone away.
        monitor.stop();
        kanban.stop();
        handSurfaceOver();
        presenter.dispose();
      }
    },
  };
}

/**
 * Wrap a beads client so a write from this run refreshes the board.
 *
 * The board's own timer would catch the change within an interval anyway; this
 * just removes the lag between the loop finishing a ticket and the picture
 * agreeing with it. The refresh is fire-and-forget on purpose: the write has
 * already succeeded, and a board read that fails after it is the board's
 * problem — it shows `?` and backs off — never the write's.
 *
 * Exported because the claim it makes — *every* mutating call refreshes, and no
 * read-only one does — is worth pinning directly, and cannot be reached through
 * `buildApp` without a real `bd`: the wrapper is applied only to the real
 * client, never to a fake a test handed in.
 */
export function refreshBoardOnWrite(client: BdClient, kanban: KanbanSource): BdClient {
  const later = (): void => {
    void kanban.refresh().catch(() => undefined);
  };
  return {
    listReady: (options) => client.listReady(options),
    listInProgress: (options) => client.listInProgress(options),
    listClosed: (options) => client.listClosed(options),
    getIssue: (id) => client.getIssue(id),
    async createIssue(spec) {
      const created = await client.createIssue(spec);
      later();
      return created;
    },
    async addDep(id, dependsOnId, type) {
      await client.addDep(id, dependsOnId, type);
      later();
    },
    async appendNote(id, text) {
      const updated = await client.appendNote(id, text);
      later();
      return updated;
    },
    async setStatus(id, status, options) {
      const updated = await client.setStatus(id, status, options);
      later();
      return updated;
    },
    async closeIssue(id, reason) {
      const closed = await client.closeIssue(id, reason);
      later();
      return closed;
    },
    remember: (text, key) => client.remember(text, key),
    recall: (key) => client.recall(key),
  };
}

/**
 * Build the mini kanban.
 *
 * The three reads are settled independently rather than awaited together, so
 * one column failing does not black out the other two: `Promise.allSettled`
 * maps directly onto the `failed` field of {@link KanbanRead}, which is what
 * lets the renderer draw `?` for the column that failed and real counts for
 * the columns that answered.
 */
function buildKanban(
  config: AppConfig,
  beads: BdClient,
  overrides: AppConfig["overrides"] = {},
  labels: { labels: readonly string[] } | undefined,
): KanbanSource {
  const setting = config.kanban ?? { enabled: true };
  if (setting.enabled === false) {
    return createNullKanban("switched off by LOOP_KANBAN=0");
  }
  if (overrides.kanban !== undefined) return overrides.kanban;

  const doneLimit = Math.max(1, setting.doneLimit ?? 12);
  let theme: MonitorTheme;
  try {
    theme = createPresenterTheme(config.themeName);
  } catch {
    theme = PLAIN_MONITOR_THEME;
  }

  const read = async (): Promise<KanbanRead> => {
    // Read more closed tickets than the column shows: bd's own order is not
    // guaranteed to be by close time, so the recency sort has to happen over a
    // window wider than the display.
    const windowSize = Math.max(doneLimit * 3, 30);
    const [ready, inProgress, closed] = await Promise.allSettled([
      beads.listReady(labels ?? {}),
      beads.listInProgress(labels ?? {}),
      beads.listClosed({ ...labels, limit: windowSize }),
    ]);
    const failed: KanbanColumnKey[] = [];
    const firstError = [ready, inProgress, closed].find(
      (result) => result.status === "rejected",
    ) as PromiseRejectedResult | undefined;
    if (ready.status === "rejected") failed.push("ready");
    if (inProgress.status === "rejected") failed.push("progress");
    if (closed.status === "rejected") failed.push("done");
    const readResult: KanbanRead = {
      ...(ready.status === "fulfilled" ? { ready: ready.value } : {}),
      ...(inProgress.status === "fulfilled" ? { inProgress: inProgress.value } : {}),
      ...(closed.status === "fulfilled" ? { closed: closed.value } : {}),
      // Tell the view what the window was, so a full window reads as a floor
      // (`done 36+`) rather than as a count that happens to be exact.
      closedWindow: windowSize,
      ...(failed.length === 0 ? {} : { failed }),
      ...(firstError === undefined
        ? {}
        : {
            error:
              firstError.reason instanceof Error
                ? firstError.reason.message
                : String(firstError.reason),
          }),
    };
    return readResult;
  };

  return createKanbanSource({
    read,
    theme,
    mode: setting.mode ?? "board",
    maxLines: setting.lines ?? 4,
    doneLimit,
    ...(setting.intervalMs === undefined ? {} : { intervalMs: setting.intervalMs }),
    verbose: setting.verbose === true,
  });
}

/** Build and run. The single entry the CLI and the spike share. */
export async function runApp(config: AppConfig): Promise<LoopResult> {
  return buildApp(config).run();
}
