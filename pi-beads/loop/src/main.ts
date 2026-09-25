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
import type { AppConfig } from "./app.ts";
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
  return {
    cwd,
    bdBin: source.LOOP_BD_BIN,
    gitBin: source.LOOP_GIT_BIN,
    modelRef: provider !== undefined && model !== undefined ? { provider, id: model } : undefined,
    workTimeoutMs: number("LOOP_WORK_TIMEOUT_MS"),
    /** When to tell a run to land and report. Unset = budget minus the lead. */
    wrapUpMs: number("LOOP_WRAP_UP_MS"),
    /** Opt back up: retry a run that ran out of time or context in place. */
    retryUnfitWork: flag("LOOP_RETRY_UNFIT_WORK"),
    workThinkingLevel: thinking("LOOP_WORK_THINKING"),
    splitThinkingLevel: thinking("LOOP_SPLIT_THINKING"),
    maxIterations: number("LOOP_MAX_ITERATIONS"),
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
