/**
 * Spike 5: the splitter, live.
 *
 * Fakes prove the contract; this proves it against a real model writing into a
 * scratch database. Nothing here touches the project board: `BEADS_DIR` points at
 * a temporary directory that is deleted at the end, and the model is asked only
 * to plan — never to implement.
 *
 * What we look for in the output:
 *   - the request lands on an epic, verbatim;
 *   - more than one issue when the request has an ordering in it;
 *   - `depends_on` positions that came from the model become real bd ids;
 *   - every issue has acceptance criteria and a priority.
 *
 * Run: `npx tsx spikes/5-split-live.ts`
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { createBdClient, dependsOn, normaliseDependencies } from "../src/beads.js";
import { createAgentRunner } from "../src/agent.js";
import { createSplitter, portFromAgentRunner, describeSplitFailure, createdIds } from "../src/split.js";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = process.env.LOOP_SPIKE_OUT ?? join(here, "out");
mkdirSync(outDir, { recursive: true });
const outPath = join(outDir, "5-split-live.txt");

const lines: string[] = [];
const log = (text: string): void => {
  lines.push(text);
  process.stdout.write(`${text}\n`);
};
const flush = (): void => {
  writeFileSync(outPath, `${lines.join("\n")}\n`);
};

const REQUEST =
  "Add a retry wrapper for the flaky integration tests, then write a short doc page explaining how to use it.";

function main(): void {
  const root = mkdtempSync(join(tmpdir(), "loop-split-live-"));
  const beadsDir = join(root, ".beads");
  const env = { ...process.env, BEADS_DIR: beadsDir };

  log(`scratch db: ${beadsDir}`);
  const init = spawnSync("bd", ["init", "--prefix", "live", "--non-interactive"], {
    cwd: root,
    env,
    encoding: "utf8",
  });
  if (init.status !== 0) {
    log(`bd init failed: ${init.stderr}`);
    flush();
    process.exit(1);
  }
  log("bd init ok");

  const beads = createBdClient({ cwd: root, env: { BEADS_DIR: beadsDir } });
  const runner = createAgentRunner({
    beads,
    cwd: root,
    timeoutMs: 5 * 60 * 1000,
    includeRepoSnapshot: false,
    onEvent: (event) => log(`  [runner] ${event.type}`),
  });
  const splitter = createSplitter(
    { agent: portFromAgentRunner(runner), beads },
    { epicPriority: 1 },
  );

  log(`\nrequest: ${REQUEST}\n`);

  void (async () => {
    let exitCode = 0;
    try {
      const outcome = await splitter.split(REQUEST);
      log(`outcome: ${outcome.kind}`);
      log(`attempts: ${outcome.attempts}`);

      if (outcome.kind !== "created" && outcome.kind !== "partial") {
        log(describeSplitFailure(outcome));
        if (outcome.kind === "invalid-batch") {
          for (const problem of outcome.problems) log(`  problem: ${problem.field} @ ${problem.index}`);
          for (const raw of outcome.rawOutputs) log(`  raw: ${raw.slice(0, 400)}`);
        }
        exitCode = 1;
      }

      if (outcome.kind === "created" || outcome.kind === "partial") {
        log(`epic: ${outcome.epic.mode} (${outcome.epic.epicId}) landed in ${outcome.epic.landedIn}`);
        const epic = await beads.getIssue(outcome.epic.epicId);
        const carried = outcome.kind === "created" ? epic?.description ?? epic?.notes ?? "" : "";
        log(`epic record ends with the request: ${carried.endsWith(REQUEST)}`);

        for (const entry of outcome.created) {
          log(`created #${entry.index} -> ${entry.id}: ${entry.title}`);
        }
        for (const failure of outcome.kind === "partial" ? outcome.failed : []) {
          log(`FAILED  #${failure.index} (${failure.errorKind}): ${failure.message}`);
        }
        for (const skipped of outcome.kind === "partial" ? outcome.skipped : []) {
          log(`SKIPPED #${skipped.index}: needs ${skipped.missing.join(", ")}`);
        }

        // Read every created issue back and show its real dependencies.
        log("\nread-back:");
        const ids = createdIds(outcome);
        for (const id of ids) {
          const issue = await beads.getIssue(id);
          if (!issue) {
            log(`  ${id}: MISSING`);
            continue;
          }
          const deps = normaliseDependencies(issue).map((dep) => dep.id);
          log(
            `  ${id} [p${issue.priority}] acceptance=${(issue.acceptance_criteria ?? "").slice(0, 60)}... deps=${JSON.stringify(deps)}`,
          );
        }
        // Which of the created issues depend on which, stated with real ids only.
        if (ids.length >= 2) {
          log("\nedges among created issues:");
          for (const id of ids) {
            for (const other of ids) {
              if (id === other) continue;
              const issue = await beads.getIssue(id);
              if (issue && dependsOn(issue, other)) log(`  ${id} depends on ${other}`);
            }
          }
        }
      }

      const listed = spawnSync("bd", ["list", "--json"], { cwd: root, env, encoding: "utf8" });
      log(`\nboard now holds ${JSON.parse(listed.stdout).length} issue(s)`);
    } catch (error) {
      log(`threw: ${error instanceof Error ? error.stack ?? error.message : String(error)}`);
      exitCode = 1;
    } finally {
      const disposed = await runner.dispose();
      log(`sessions disposed: ${disposed}`);
      spawnSync("rm", ["-rf", root]);
      log(`scratch db removed: ${root}`);
      flush();
      process.exit(exitCode);
    }
  })();
}

main();
