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

import { runApp } from "./app.ts";
import type { AppConfig } from "./app.ts";
import { LoopError } from "./loop.ts";
import type { LoopResult } from "./loop.ts";

/**
 * The one environment-reading site in this module.
 *
 * `.11` replaces this with the real config layer; everything below stays
 * untouched because nothing else reads the process environment here.
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
  return {
    cwd,
    bdBin: source.LOOP_BD_BIN,
    gitBin: source.LOOP_GIT_BIN,
    modelRef: provider !== undefined && model !== undefined ? { provider, id: model } : undefined,
    workTimeoutMs: number("LOOP_WORK_TIMEOUT_MS"),
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
    if (LoopError.is(error)) {
      write(`Fatal: ${error.code}: ${error.message}`);
      return 2;
    }
    const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
    write(`Fatal: unexpected failure: ${message}`);
    return 2;
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
  process.exitCode = await cli(readEnv());
}
