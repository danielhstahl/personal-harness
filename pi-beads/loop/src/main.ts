#!/usr/bin/env node
/**
 * pi-beads loop — CLI entry point (workspace-5yn.9).
 *
 * This file is deliberately almost empty. It reads the environment in exactly
 * one place, hands the result to `buildApp` in `src/app.js`, runs the loop and
 * maps the outcome to a process exit code. It constructs no adapter, opens no
 * session and issues no `bd` or `git` call — `test/loop.test.ts` greps this
 * file to keep it that way, because a second place that decides configuration
 * is a second place that can be wrong.
 */
import { pathToFileURL } from "node:url";

import { AgentError, parseThinkingLevel } from "./agent.ts";
import type { ThinkingLevel } from "./agent.ts";
import { runApp } from "./app.ts";
import type { AppConfig, KanbanSetting, NotifySetting } from "./app.ts";
import { LoopError } from "./loop.ts";
import type { LoopResult } from "./loop.ts";

/**
 * The one environment-reading site in this module.
 *
 * `.11` replaces this with the real config layer; everything below stays
 * untouched because nothing else reads the process environment here.
 *
 * `LOOP_WORK_THINKING` / `LOOP_SPLIT_THINKING` set the level of each pass and
 * are validated on the way in: unset means *unset* (the user's configured
 * default, then pi's own), never a silently-guessed level.
 */
export function readEnv(source: Readonly<Record<string, string | undefined>> = process.env): AppConfig {
  const number = (name: string): number | undefined => {
    const raw = source[name];
    if (raw === undefined || raw.trim() === "") return undefined;
    const parsed = Number(raw);
    return Number.isFinite(parsed) ? Math.trunc(parsed) : undefined;
  };
  const flag = (name: string): boolean | undefined => {
    const raw = source[name];
    if (raw === undefined) return undefined;
    return raw === "1" || raw.toLowerCase() === "true" || raw.toLowerCase() === "yes";
  };
  const cwd = source.LOOP_CWD ?? process.cwd();
  const provider = source.PI_PROVIDER;
  const model = source.PI_MODEL;
  /**
   * A thinking level, read where the environment is read and validated on the
   * way out. Unlike the timeout — which drops what it cannot parse — a level pi
   * would not accept is *refused*: a run at a level nobody asked for looks
   * exactly like one that was configured, from the outside.
   */
  const thinking = (name: string): ThinkingLevel | undefined =>
    parseThinkingLevel(source[name]);
  /**
   * Where the audited config goes. Anything truthy writes the patched config
   * next to `models.json`; `inplace` means the live file, with a `.bak` first.
   */
  const auditWriteMode = (): "none" | "proposed" | "inplace" => {
    const raw = source.LOOP_AUDIT_WRITE?.trim().toLowerCase();
    if (raw === undefined || raw === "" || raw === "0" || raw === "false") return "none";
    if (raw === "inplace" || raw === "in-place") return "inplace";
    return "proposed";
  };
  const auditTimeout = number("LOOP_AUDIT_TIMEOUT_MS");
  /**
   * The read-only server panel. On unless switched off: the numbers it shows are
   * the ones you wish you had looked at, and the cost of looking is four `GET`s
   * against a server that is already on the path of everything on the screen.
   *
   * `LOOP_MONITOR_AT=top` puts the panel at the top of the work surface instead
   * of above the footer. That scrolls with the transcript, so it is the option
   * for a surface known to stay short — not the default for a long-running one.
   */
  const monitorPlacement = (): "band" | "top" =>
    source.LOOP_MONITOR_AT?.trim().toLowerCase() === "top" ? "top" : "band";
  /**
   * The mini kanban, read as one knob with three answers: `0`/`off` hides it,
   * `row` and `board` pick the shape, and anything else — including unset —
   * leaves it on with the shape each surface prefers.
   *
   * The shape words are recognised here rather than left to fall through to the
   * boolean: a variable that switched the board *off* because its value was
   * `board` rather than `1` would be a trap that costs the first person who
   * tries it an hour of wondering.
   */
  /**
   * Split a tag list the way a person types one.
   *
   * Comma or whitespace both separate, because the natural thing to write into
   * an environment variable is `LOOP_NTFY_TAGS="+1 tada"` and the natural thing
   * *not* to notice is that one of them silently never arrived.
   */
  const tagList = (raw: string | undefined): string[] =>
    (raw ?? "")
      .split(/[,;\s]+/u)
      .map((entry) => entry.trim())
      .filter((entry) => entry !== "");

  /**
   * How many failed publishes in a row before the notice gives up, refused
   * rather than dropped.
   *
   * `number()` returns `undefined` for what it cannot parse, which for this knob
   * would be a trap: `LOOP_NTFY_MAX_FAILURES="three"` would fall back to the
   * default and look exactly like a setting that took. A value that is set has
   * to be a whole number of tries — and a *safely representable* one, since
   * `1e21` parses to an integer that is not the integer anybody meant — and
   * saying so is cheaper than a run that quietly gave up after three when it was
   * told to try nine.
   */
  const maxFailuresSetting = (): number | undefined => {
    const raw = source.LOOP_NTFY_MAX_FAILURES;
    if (raw === undefined || raw.trim() === "") return undefined;
    const parsed = Number(raw);
    if (!Number.isSafeInteger(parsed) || parsed < 1) {
      throw new LoopError(
        "notify-config",
        `LOOP_NTFY_MAX_FAILURES must be a whole number of tries, at least 1, not "${raw.trim()}"`,
      );
    }
    return parsed;
  };

  /**
   * The completion notice: one optional feature with the topic as its switch.
   *
   * `undefined` when nothing ntfy-related is set at all, which keeps "this run
   * was never asked to notify anybody" distinct in the transcript from "this run
   * was asked and cannot". Those two read very differently to whoever is looking
   * for a notice that did not arrive.
   *
   * `LOOP_NTFY_TOPIC` is the switch, and it takes either a bare topic name —
   * joined onto `LOOP_NTFY_URL`, which defaults to the hosted server — or a
   * complete URL, which is the form people copy out of the ntfy web UI. A
   * self-hosted server is a different value in the same variable, not a
   * different configuration: that is what makes local hosting a one-line change.
   */
  const notifySetting = (): NotifySetting | undefined => {
    const knobs: readonly (string | undefined)[] = [
      source.LOOP_NTFY_TOPIC,
      source.LOOP_NTFY_URL,
      source.LOOP_NTFY_TOKEN,
      source.LOOP_NTFY_PRIORITY,
      source.LOOP_NTFY_TAGS,
      source.LOOP_NTFY_CLICK,
      source.LOOP_NTFY_TITLE_PREFIX,
      source.LOOP_NTFY_TIMEOUT_MS,
      source.LOOP_NTFY_MAX_FAILURES,
    ];
    if (knobs.every((value) => value === undefined || value.trim() === "")) return undefined;
    const topic = (source.LOOP_NTFY_TOPIC ?? "").trim();
    const tags = tagList(source.LOOP_NTFY_TAGS);
    const maxFailures = maxFailuresSetting();
    return {
      // The topic is the switch. A server configured with nowhere to publish is
      // a server that will never be exercised, and saying so beats pretending.
      enabled: topic !== "",
      ...(topic === "" ? {} : { topic }),
      ...(source.LOOP_NTFY_URL === undefined ? {} : { url: source.LOOP_NTFY_URL.trim() }),
      ...(source.LOOP_NTFY_TOKEN === undefined ? {} : { token: source.LOOP_NTFY_TOKEN.trim() }),
      ...(source.LOOP_NTFY_PRIORITY === undefined
        ? {}
        : { priority: source.LOOP_NTFY_PRIORITY.trim() }),
      ...(tags.length === 0 ? {} : { tags }),
      ...(source.LOOP_NTFY_CLICK === undefined ? {} : { click: source.LOOP_NTFY_CLICK.trim() }),
      ...(source.LOOP_NTFY_TITLE_PREFIX === undefined
        ? {}
        : { titlePrefix: source.LOOP_NTFY_TITLE_PREFIX }),
      ...(number("LOOP_NTFY_TIMEOUT_MS") === undefined
        ? {}
        : { timeoutMs: number("LOOP_NTFY_TIMEOUT_MS") }),
      ...(maxFailures === undefined ? {} : { maxConsecutiveFailures: maxFailures }),
    };
  };

  const kanbanSetting = (): KanbanSetting => {
    const raw = source.LOOP_KANBAN?.trim().toLowerCase();
    const off = raw === "0" || raw === "off" || raw === "no" || raw === "false";
    const shape = raw === "row" || raw === "board" ? raw : undefined;
    const intervalMs = number("LOOP_KANBAN_MS");
    const lines = number("LOOP_KANBAN_LINES");
    const doneLimit = number("LOOP_KANBAN_DONE");
    return {
      enabled: !off,
      ...(shape === undefined ? {} : { mode: shape }),
      ...(intervalMs === undefined ? {} : { intervalMs }),
      ...(lines === undefined ? {} : { lines }),
      ...(doneLimit === undefined ? {} : { doneLimit }),
      placement: source.LOOP_KANBAN_AT?.trim().toLowerCase() === "top" ? "top" : "band",
      verbose: flag("LOOP_KANBAN_VERBOSE") ?? false,
    };
  };
  return {
    cwd,
    bdBin: source.LOOP_BD_BIN,
    gitBin: source.LOOP_GIT_BIN,
    /**
     * The index-lock policy. See
     * [ADR-006](../docs/ADR-006-index-lock.md): the wait covers contention
     * with something alive, the stale threshold decides when a lock looks like a
     * crashed process instead, and removal stays off until it is asked for.
     */
    gitLock: {
      ...(number("LOOP_GIT_LOCK_WAIT_MS") === undefined
        ? {}
        : { waitMs: number("LOOP_GIT_LOCK_WAIT_MS") }),
      ...(number("LOOP_GIT_KILL_GRACE_MS") === undefined
        ? {}
        : { killGraceMs: number("LOOP_GIT_KILL_GRACE_MS") }),
      ...(number("LOOP_GIT_STALE_LOCK_AFTER_MS") === undefined
        ? {}
        : { staleAfterMs: number("LOOP_GIT_STALE_LOCK_AFTER_MS") }),
      ...((source.LOOP_GIT_STALE_LOCK ?? "").trim().toLowerCase() === "remove"
        ? { stalePolicy: "remove" as const }
        : {}),
    },
    modelRef: provider !== undefined && model !== undefined ? { provider, id: model } : undefined,
    workTimeoutMs: number("LOOP_WORK_TIMEOUT_MS"),
    /** When to tell a run to land and report. Unset = budget minus the lead. */
    wrapUpMs: number("LOOP_WRAP_UP_MS"),
    /** Opt back up: retry a run that ran out of time or context in place. */
    retryUnfitWork: flag("LOOP_RETRY_UNFIT_WORK"),
    workThinkingLevel: thinking("LOOP_WORK_THINKING"),
    splitThinkingLevel: thinking("LOOP_SPLIT_THINKING"),
    /**
     * The startup provider comparison. Default on: the failures it catches cost
     * a whole work pass each and are free to see before one starts. It is a
     * warning, not a gate, unless `LOOP_AUDIT_STRICT` says otherwise.
     */
    providerAudit: {
      enabled: flag("LOOP_AUDIT") ?? true,
      strict: flag("LOOP_AUDIT_STRICT") ?? false,
      verbose: flag("LOOP_AUDIT_VERBOSE") ?? false,
      writeMode: auditWriteMode(),
      ...(source.LOOP_HEALTH_URL === undefined ? {} : { healthUrl: source.LOOP_HEALTH_URL }),
      ...(auditTimeout === undefined ? {} : { timeoutMs: auditTimeout }),
    },
    maxIterations: number("LOOP_MAX_ITERATIONS"),
    /** The read-only backend monitor. See `MonitorSetting` in `src/app.ts`. */
    monitor: {
      enabled: flag("LOOP_MONITOR") ?? true,
      ...(number("LOOP_MONITOR_MS") === undefined ? {} : { intervalMs: number("LOOP_MONITOR_MS") }),
      ...(number("LOOP_MONITOR_TIMEOUT_MS") === undefined
        ? {}
        : { timeoutMs: number("LOOP_MONITOR_TIMEOUT_MS") }),
      ...(number("LOOP_MONITOR_MODELS_MS") === undefined
        ? {}
        : { modelsEveryMs: number("LOOP_MONITOR_MODELS_MS") }),
      ...(source.LOOP_MONITOR_URL === undefined ? {} : { url: source.LOOP_MONITOR_URL }),
      ...(number("LOOP_MONITOR_LINES") === undefined ? {} : { lines: number("LOOP_MONITOR_LINES") }),
      placement: monitorPlacement(),
      verbose: flag("LOOP_MONITOR_VERBOSE") ?? false,
    },
    /** The mini kanban. See `KanbanSetting` in `src/app.ts`. */
    kanban: kanbanSetting(),
    /** The completion notice. See `NotifySetting` in `src/app.ts`. */
    notify: notifySetting(),
    themeName: source.PI_THEME,
    dryRun: flag("LOOP_DRY_RUN"),
    verbose: flag("LOOP_VERBOSE"),
    epicId: source.LOOP_EPIC_ID,
    epicTitle: source.LOOP_EPIC_TITLE,
  };
}

function summary(result: LoopResult): string {
  const closed = result.transcript.effects.filter((entry) => entry.kind === "beads.close_issue").length;
  const created = result.transcript.createdIssueIds.length;
  const counts = `created ${created} issue(s), closed ${closed} write(s), ` +
    `${result.transcript.transitions.length} transition(s)`;
  switch (result.kind) {
    case "done":
      return `Stopped cleanly after ${result.iterations} iteration(s); ${counts}.`;
    case "planned":
      return `Dry run: ${result.reason ?? "nothing executed"}.`;
    case "aborted":
      return `Aborted after ${result.iterations} iteration(s); ${counts}. ` +
        "An issue may still be in_progress and will be resumed by the next run.";
    case "blocked":
      return `Blocked: ${result.reason ?? "no pending effect"}`;
    case "iteration-limit":
      return `Stopped at the iteration limit: ${result.reason ?? ""}`;
    case "fatal":
      return `Fatal: ${result.reason ?? "unknown failure"}`;
  }
}

/** Map a loop outcome onto a process exit code. Exported so it is testable. */
export function exitCodeFor(result: LoopResult): number {
  return result.exitCode;
}

/**
 * The one mapping from a thrown error to the line the operator is told.
 *
 * A `LoopError` carries its own code. An `AgentError` is a refused value — a
 * configured model that is not in the catalog, a thinking level nothing
 * recognises — which is the operator's business rather than a crash, and wants
 * one plain line instead of a stack pointing at the place the string was
 * checked. `undefined` means "not one of ours", and leaves the caller to decide
 * whether that is a line to print or an error to rethrow.
 */
function fatalLine(error: unknown): string | undefined {
  if (LoopError.is(error)) return `Fatal: ${error.code}: ${error.message}`;
  if (AgentError.is(error)) return `Fatal: ${error.message}`;
  return undefined;
}

export async function cli(
  config: AppConfig,
  write: (line: string) => void = (line) => void process.stdout.write(`${line}\n`),
): Promise<number> {
  try {
    const result = await runApp(config);
    write(summary(result));
    return exitCodeFor(result);
  } catch (error) {
    // `runLoop` turns its own failures into a result, so this is the seam for
    // everything beneath it: a bad config, an adapter that could not be built.
    const fatal = fatalLine(error);
    if (fatal !== undefined) {
      write(fatal);
      return 2;
    }
    const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
    write(`Fatal: unexpected failure: ${message}`);
    return 2;
  }
}

/**
 * Read the config, then run it — with the refusal case handled before `cli` is
 * ever entered.
 *
 * `readEnv` *refuses* a thinking level pi has never heard of instead of rounding
 * it down, and that happens on the way in, outside `cli`'s try. Same line and the
 * same exit code as a fatal from the run, because to whoever is watching it is
 * the same event: the loop did not start.
 *
 * `read` is a thunk rather than a config because the refusal happens *while
 * reading*: a config already built cannot be the thing that failed.
 */
export async function runFromEnv(
  read: () => AppConfig = () => readEnv(),
  write: (line: string) => void = (line) => void process.stdout.write(`${line}\n`),
): Promise<number> {
  try {
    return await cli(await read(), write);
  } catch (error) {
    const fatal = fatalLine(error);
    if (fatal !== undefined) {
      write(fatal);
      return 2;
    }
    throw error;
  }
}

/**
 * Only run when this file is the entry point. Without this guard, a test that
 * imports `readEnv` to check the mapping would also start the loop.
 */
const isEntry =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(process.argv[1]).href;

if (isEntry) {
  process.exitCode = await runFromEnv();
}
