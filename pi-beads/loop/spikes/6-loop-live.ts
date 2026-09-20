/**
 * Spike 6: the whole loop, live. (workspace-5yn.9)
 *
 * `.1`–`.8` were each proved against their own live surface. This is the one that
 * asks a different question: **does the composed program actually walk**
 * `idle → SPLIT → work → FINALIZE → idle` **with a real model, real `git` and a
 * real `bd`, with nothing carried between iterations?**
 *
 * Real here:
 *   - the real model, through `.5`'s `defaultSessionFactory` (wrapped, not
 *     replaced — the wrapper only copies the prompt out on its way past);
 *   - real `git` in a throwaway repo, with the identity set explicitly so the
 *     commit is attributable to the loop and not to whoever ran the spike;
 *   - real `bd` in a scratch `BEADS_DIR` under that temp dir, `init`ed fresh;
 *   - `buildApp` — the same composition root `src/main.ts` uses — with only the
 *     idle surface and the prompt spy overridden.
 *
 * Scripted, deliberately:
 *   - the human. `.6` proved the real TUI input path (`spikes/4-idle-pty.txt`);
 *     here a queue stands in for a person typing two lines, so the run is
 *     repeatable without a keyboard.
 *
 * What we look for in the output:
 *   - the request becomes an epic plus children, and the children are worked;
 *   - a commit exists per worked issue whose message carries the issue id and a
 *     `Loop-Handoff:` trailer, and the files in it are the ones the model says
 *     it touched;
 *   - the handoff memory is recallable by `loop:handoff:<id>`;
 *   - the bead is closed;
 *   - **no prompt contains any part of an earlier session's answer** — the
 *     amnesia contract, measured rather than asserted;
 *   - the run ends back at idle, exits clean, disposes every session.
 *
 * Run: `npx tsx spikes/6-loop-live.ts`   (needs a working model credential)
 * Env:  LOOP_LIVE_REQUEST to change what the scripted human asks for.
 */

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { createAgentRunner, defaultSessionFactory, messageText } from "../src/agent.js";
import type { AgentSessionLike, SessionSpec } from "../src/agent.js";
import { createBdClient } from "../src/beads.js";
import { buildApp } from "../src/app.js";
import { createFinalizer } from "../src/finalize.js";
import type { FinalizeOutcome, FinalizeRequest } from "../src/finalize.js";
import type { IdleOutcome } from "../src/idle.js";
import { createGitWriter } from "../src/vcs.js";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = process.env.LOOP_SPIKE_OUT ?? join(here, "out");
mkdirSync(outDir, { recursive: true });
const outPath = join(outDir, "6-loop-live.txt");

const lines: string[] = [];
const log = (text: string): void => {
  lines.push(text);
  process.stdout.write(`${text}\n`);
};
const flush = (): void => {
  writeFileSync(outPath, `${lines.join("\n")}\n`);
};

const REQUEST =
  process.env.LOOP_LIVE_REQUEST ??
  "Write a file named hello.txt containing exactly: hello from the loop. " +
  "Then write docs/hello.md explaining what hello.txt is for.";

/** Two lines from a scripted human: one request, then `/exit`. */
const HUMAN: IdleOutcome[] = [
  { kind: "input", text: REQUEST },
  { kind: "exit", reason: "command" },
];

function sh(cmd: string, args: string[], cwd: string, env: NodeJS.ProcessEnv = {}) {
  return spawnSync(cmd, args, { cwd, env, encoding: "utf8" });
}

const sha = (text: string): string => createHash("sha256").update(text).digest("hex").slice(0, 12);

/** The last assistant utterance in a session, flattened to plain text. */
function lastAssistantText(session: AgentSessionLike): string {
  for (let i = session.messages.length - 1; i >= 0; i -= 1) {
    const message = session.messages[i] as { role?: string } | undefined;
    if (message?.role === "assistant") return messageText(message).trim();
  }
  return "";
}

function main(): void {
  const root = mkdtempSync(join(tmpdir(), "loop-live-6-"));
  const beadsDir = join(root, ".beads");
  const env: NodeJS.ProcessEnv = { ...process.env, BEADS_DIR: beadsDir };

  log(`repo + scratch board: ${root}`);

  // ── a real repo with a real first commit ────────────────────────────────
  const gitInit = sh("git", ["init", "-q", "."], root, env);
  if (gitInit.status !== 0) {
    log(`git init failed: ${gitInit.stderr}`);
    flush();
    process.exit(1);
  }
  writeFileSync(join(root, "README.md"), "# loop live spike\n\nNothing here outlives the run.\n");
  sh("git", ["add", "README.md"], root, env);
  sh(
    "git",
    ["commit", "-q", "-m", "base commit", "--author", "spike <spike@localhost>"],
    root,
    { ...env, GIT_AUTHOR_NAME: "spike", GIT_AUTHOR_EMAIL: "spike@localhost", GIT_COMMITTER_NAME: "spike", GIT_COMMITTER_EMAIL: "spike@localhost" },
  );

  const bdInit = sh("bd", ["init", "--prefix", "live", "--non-interactive"], root, env);
  if (bdInit.status !== 0) {
    log(`bd init failed: ${bdInit.stderr}`);
    flush();
    process.exit(1);
  }
  log("git + bd initialised");

  // ── the prompt spy: wrap the real factory, replace nothing ──────────────
  const prompts: { index: number; kind: string; text: string }[] = [];
  const sessions: { kind: string; session: AgentSessionLike }[] = [];
  const sessionFactory = async (spec: SessionSpec): Promise<AgentSessionLike> => {
    const real = await defaultSessionFactory(spec);
    const wrapped: AgentSessionLike = {
      get sessionId() {
        return real.sessionId;
      },
      get sessionFile() {
        return real.sessionFile;
      },
      get messages() {
        return real.messages;
      },
      prompt: (async (...args: Parameters<AgentSessionLike["prompt"]>) => {
        prompts.push({ index: prompts.length, kind: spec.kind, text: args[0] });
        return real.prompt(...args);
      }) as AgentSessionLike["prompt"],
      abort: () => real.abort(),
      dispose: () => real.dispose(),
      subscribe: (listener: Parameters<AgentSessionLike["subscribe"]>[0]) => real.subscribe(listener),
    };
    sessions.push({ kind: spec.kind, session: wrapped });
    return wrapped;
  };

  const beads = createBdClient({ cwd: root, env: { BEADS_DIR: beadsDir } });
  const eventCounts = new Map<string, number>();
  const runner = createAgentRunner({
    beads,
    cwd: root,
    sessionFactory,
    timeoutMs: 8 * 60_000,
    onEvent: (event) => {
      eventCounts.set(event.type, (eventCounts.get(event.type) ?? 0) + 1);
      // `agent_event` fires for every streaming update; counting it instead of
      // printing it keeps this artifact readable without hiding that it ran.
      if (event.type !== "agent_event") log(`  [runner] ${event.type}`);
    },
  });

  let humanIndex = 0;
  const gitWriter = createGitWriter({
    cwd: root,
    authorName: "pi-loop",
    authorEmail: "pi-loop@localhost",
  });
  const realFinalizer = createFinalizer({ vcs: gitWriter, beads }, {});
  const finalizer = {
    async finalize(request: FinalizeRequest): Promise<FinalizeOutcome> {
      log(
        `  [finalize] issue=${request.issueId} summary=${JSON.stringify(request.summary.slice(0, 80))} ` +
          `changedFiles=${JSON.stringify(request.changedFiles)}`,
      );
      const outcome = await realFinalizer.finalize(request);
      log(
        `  [finalize] -> ${outcome.kind}` +
          ("commitHash" in outcome && outcome.commitHash ? ` hash=${outcome.commitHash.slice(0, 8)}` : "") +
          ("message" in outcome && outcome.message ? ` ${outcome.message}` : ""),
      );
      return outcome;
    },
  };
  const app = buildApp({
    cwd: root,
    maxIterations: 8,
    authorName: "pi-loop",
    authorEmail: "pi-loop@localhost",
    overrides: {
      beads,
      runner,
      git: gitWriter,
      finalizer,
      idle: {
        next: async (): Promise<IdleOutcome> =>
          HUMAN[Math.min(humanIndex++, HUMAN.length - 1)] ?? { kind: "exit", reason: "command" },
      },
      ui: {
        say: (text: string) => log(`  [ui] ${text}`),
        warn: (text: string) => log(`  [ui warn] ${text}`),
      },
    },
  });

  void (async () => {
    let exitCode = 0;
    try {
      log(`\nhuman says: ${REQUEST}\n`);
      const result = await app.run();

      log(`\n── run ended: ${result.kind} (exit ${result.exitCode}) ──`);
      log(`reason: ${result.reason ?? "(none)"}`);

      log("\ntransitions:");
      for (const t of result.transcript.transitions) {
        log(`  #${t.seq} ${t.from} --${t.event}--> ${t.to}`);
      }

      log("\nlog timeline (iteration / phase):");
      for (const entry of result.transcript.logEntries) {
        log(`  it${entry.iteration} [${entry.phase}] ${entry.message ?? ""}`);
      }

      log("\nnote trail:");
      for (const note of result.transcript.notes) log(`  - ${note}`);
      log("\neffects:");
      for (const effect of result.transcript.effects) log(`  ${effect.kind}: ${effect.detail}`);

      log("\nprompts sent to the model:");
      for (const p of prompts) {
        const ids = Array.from(new Set(p.text.match(/\blive\.\d+/g) ?? []));
        log(`  #${p.index} [${p.kind}] ${p.text.length} chars sha=${sha(p.text)} ids=${JSON.stringify(ids)}`);
      }

      // The no-carry-over claim is only worth what a reader can see, so the
      // second work prompt is reproduced here in full.
      const workPrompts = prompts.filter((p) => p.kind === "work");
      const laterWork = workPrompts.slice(1);
      if (laterWork.length === 0) {
        log("\n!! only one work prompt was issued; the second iteration is not visible\n");
      }
      for (const prompt of laterWork) {
        log(
          `\n── work session #${workPrompts.indexOf(prompt) + 1}'s prompt, verbatim ` +
            `(prompt #${prompt.index} of this process) ──`,
        );
        for (const line of prompt.text.split("\n")) log(`  | ${line}`);
        log("── end of prompt ──\n");
        const carriesHandoff = /Notes from beads memory|loop:handoff/i.test(prompt.text);
        log(`  recalled-handoff content present in this prompt: ${carriesHandoff}`);
        if (!carriesHandoff) {
          log(
            "    (expected here: handoffs are keyed per issue, loop:handoff:<id>, and this issue was " +
              "worked for the first time, so there was nothing of its own to recall. The recall path " +
              "itself is proved in test/loop.test.ts — a prior note under that key does arrive in the " +
              "next prompt — and the keys written by this run are listed under 'handoff memories' below.)",
          );
        }
      }

      // ── the amnesia contract, measured ─────────────────────────────────
      log("\nrunner event counts:");
      for (const [type, count] of eventCounts) log(`  ${type}: ${count}`);

      log("\ncarry-over check:");
      const answers = sessions
        .map((entry, i) => ({ i, kind: entry.kind, answer: lastAssistantText(entry.session) }))
        .filter((entry) => entry.answer.length > 24);
      let breaches = 0;
      for (const earlier of answers) {
        const probe = earlier.answer.slice(0, 60).replace(/\s+/g, " ");
        for (const p of prompts) {
          if (p.index <= earlier.i) continue;
          if (p.text.replace(/\s+/g, " ").includes(probe)) {
            breaches += 1;
            log(`  BREACH: prompt #${p.index} [${p.kind}] contains answer text from session #${earlier.i}`);
          }
        }
      }
      log(
        breaches === 0
          ? `  clean: ${answers.length} earlier answer(s), none of them present in any later prompt`
          : `  ${breaches} BREACH(ES) — context carried over`,
      );
      if (breaches > 0) exitCode = 1;

      // ── what the board and the repo ended up holding ────────────────────
      const listed = sh("bd", ["list", "--all", "--json"], root, env);
      let board: { id?: string; status?: string; title?: string }[] = [];
      try {
        board = JSON.parse(listed.stdout) as typeof board;
      } catch {
        log(`\nbd list --all --json did not parse: ${listed.stdout.slice(0, 200)}`);
      }
      log("\nboard:");
      for (const issue of board) {
        log(`  ${issue.id} [${issue.status}] ${issue.title ?? ""}`);
      }

      log("\ngit log:");
      const gitLog = sh("git", ["log", "--pretty=%h %s"], root, env);
      for (const line of gitLog.stdout.trim().split("\n")) log(`  ${line}`);

      const head = sh("git", ["log", "-1", "--pretty=%B"], root, env);
      log("\nHEAD message:");
      for (const line of head.stdout.trim().split("\n")) log(`  ${line}`);
      const trailer = /Loop-Handoff:\s*\S+/.test(head.stdout);
      log(`  Loop-Handoff trailer present: ${trailer}`);

      log("\nfiles in HEAD:");
      const files = sh("git", ["show", "--stat", "--oneline", "HEAD"], root, env);
      for (const line of files.stdout.trim().split("\n").slice(1)) log(`  ${line}`);

      log("\nhandoff memories on the board:");
      for (const issue of board.filter((i) => i.status === "closed")) {
        const recall = sh("bd", ["recall", `loop:handoff:${issue.id}`, "--json"], root, env);
        const ok = recall.status === 0;
        log(`  loop:handoff:${issue.id} recallable=${ok}`);
        if (ok) log(`    ${recall.stdout.trim().replace(/\s+/g, " ").slice(0, 220)}`);
      }

      log(`\nsessions created: ${sessions.length}; live now: ${runner.liveSessionIds().length}`);
      const closed = board.filter((i) => i.status === "closed").map((i) => i.id);
      log(`closed issue ids: ${JSON.stringify(closed)}`);
      if (!trailer) {
        log("FAIL: the loop's commit does not carry the handoff trailer");
        exitCode = 1;
      }
      if (closed.length === 0) {
        log("FAIL: nothing was closed — the walk did not complete");
        exitCode = 1;
      }
    } catch (error) {
      log(`threw: ${error instanceof Error ? error.stack ?? error.message : String(error)}`);
      exitCode = 1;
    } finally {
      const disposed = await runner.dispose();
      log(`\nrunner.dispose() closed ${disposed} session(s)`);
      if (exitCode !== 0 || process.env.LOOP_LIVE_KEEP === "1") {
        log(`KEEPING scratch dir for inspection: ${root}`);
      } else {
        rmSync(root, { recursive: true, force: true });
        log(`scratch removed: ${root}`);
      }
      flush();
      process.exit(exitCode);
    }
  })();
}

main();
