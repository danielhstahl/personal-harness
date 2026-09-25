#!/usr/bin/env node
/**
 * `npm run audit` — the startup comparison without the loop.
 *
 * The same comparison the loop runs before it claims a ticket, run on its own:
 * read `models.json`, resolve the provider and model this run would use, GET
 * the server's `/health` at its base URL, and print what disagrees and what to
 * change. Exit 0 when nothing errored and 1 when there is a finding that should
 * be fixed before a run — the same signal the loop acts on under
 * `LOOP_AUDIT_STRICT`.
 *
 * Configuration comes from {@link readEnv} rather than a second parse of the
 * environment: two places reading the same knobs is two places that can
 * disagree about what a run means.
 */
import { pathToFileURL } from "node:url";

import { harnessToolSchemas } from "./agent.ts";
import { readEnv } from "./main.ts";
import { runStartupAudit } from "./startup.ts";

export async function auditCli(
  env: Readonly<Record<string, string | undefined>> = process.env,
  write: (line: string) => void = (line) => void process.stdout.write(`${line}\n`),
): Promise<number> {
  const config = readEnv(env as Record<string, string | undefined>);
  const setting = config.providerAudit ?? { enabled: true };
  const result = await runStartupAudit({
    cwd: config.cwd,
    ...(config.modelRef === undefined ? {} : { modelRef: config.modelRef }),
    levels: {
      ...(config.workThinkingLevel === undefined ? {} : { work: config.workThinkingLevel }),
      ...(config.splitThinkingLevel === undefined ? {} : { split: config.splitThinkingLevel }),
    },
    toolSchemas: harnessToolSchemas(),
    verbose: setting.verbose ?? false,
    writeMode: setting.writeMode ?? "none",
    ...(setting.healthUrl === undefined ? {} : { healthUrl: setting.healthUrl }),
    ...(setting.timeoutMs === undefined ? {} : { timeoutMs: setting.timeoutMs }),
  });

  for (const line of result.lines) write(line);

  const report = result.report;
  if (report !== undefined && report.suggestions.length > 0 && result.proposedJson !== undefined) {
    write("");
    write("proposed config — printable form, secret values redacted:");
    write(result.proposedJson.trimEnd());
    write(
      setting.writeMode === "inplace"
        ? "already applied in place; the original is at models.json.bak"
        : "nothing was written — re-run with LOOP_AUDIT_WRITE=1 for a .proposed file, or LOOP_AUDIT_WRITE=inplace to patch models.json with a backup",
    );
  } else if (report !== undefined) {
    write("nothing to change: the report and the config agree everywhere this audit looks.");
  }

  return result.blocking ? 1 : 0;
}

const isEntry =
  process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href;

if (isEntry) {
  process.exitCode = await auditCli();
}
