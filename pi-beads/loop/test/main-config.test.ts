/**
 * Tests for `readEnv` in `src/main.ts` — the one place the loop reads the world.
 *
 * The rule under test is one sentence: **unset means unset, set means valid.**
 * A knob that is absent falls to the default its section documents; a knob that
 * is present has to be a value of the type that knob claims. Before this, the
 * reader coerced: `LOOP_KANBAN="bord"` was not one of the off-words so it
 * became `enabled: true` with no shape, `LOOP_KANBAN_AT="upper"` became
 * `"band"`, `flag("LOOP_VERBOSE")` read `"ture"` as `false`, and `number()`
 * dropped `LOOP_KANBAN_MS="5s"` on the floor. Each of those produced a run
 * configured to something nobody chose that looked, from the transcript,
 * exactly like a run configured on purpose.
 *
 * The house style being extended is the one `LOOP_NTFY_MAX_FAILURES` already
 * set: refuse, and name the key, the raw value and the accepted set in the
 * message, because whoever typed it is standing at the terminal they typed it
 * in.
 *
 * The last three tests are about *when* the refusal happens rather than what it
 * says. Validation must finish before `runApp` — before a bead is claimed, an
 * adapter is built or the terminal is taken — which is checked end to end by
 * running the real entry point in a subprocess, alongside a control run that
 * proves the harness would notice if it were not there.
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { readEnv, runFromEnv } from "../src/main.ts";
import { LoopError } from "../src/loop.ts";

type Env = Record<string, string | undefined>;

const SRC_DIR = fileURLToPath(new URL("../src/", import.meta.url));

function readSource(name: string): string {
  return readFileSync(join(SRC_DIR, name), "utf8");
}

/**
 * Read `env` and demand it refused, returning the error so the message can be
 * inspected. Passing through is a failure of the test, not a passing
 * assertion: these are the values that must never reach a config.
 */
function refused(env: Env): LoopError {
  let thrown: unknown = undefined;
  try {
    readEnv(env);
  } catch (error) {
    thrown = error;
  }
  assert.ok(
    LoopError.is(thrown) && thrown.code === "bad-config",
    `${JSON.stringify(env)} should have been refused as bad-config, got ${String(thrown)}`,
  );
  return thrown;
}

function includes(message: string, needle: string, what: string): void {
  assert.ok(message.includes(needle), `${what}: ${message} did not contain ${needle}`);
}

/** Every `LOOP_*` key offered as accepted in a refusal of an unknown key. */
function acceptedKnobs(message: string): string[] {
  const match = message.match(/Accepted LOOP_\* knobs: ([A-Z0-9_, ]+)/u);
  assert.ok(match, `no accepted-knob list in: ${message}`);
  return (match[1] ?? "").split(",").map((entry) => entry.trim()).filter((entry) => entry !== "");
}

// -- the kanban knob ---------------------------------------------------------

test("LOOP_KANBAN refuses a value it does not know instead of turning the board on", () => {
  // The original bug: "bord" is not one of the off-words, so the old reader
  // concluded `enabled: true` with `mode` left undefined. A typo that reads as
  // a decision is worse than a crash, because a crash gets read.
  for (const bad of ["bord", "borad", "broad", "b", "yes-but-no", "3"]) {
    const error = refused({ LOOP_KANBAN: bad });
    includes(error.message, "LOOP_KANBAN", "the refusal names the key");
    includes(error.message, bad, "the refusal quotes the value as it arrived");
    includes(error.message, "accepted: 0, off, no, false, row, board", "and the whole set");
  }

  // The words that *are* the vocabulary still answer, in any case, padded.
  for (const off of ["0", "off", "no", "false", " OFF "]) {
    assert.equal(readEnv({ LOOP_KANBAN: off }).kanban?.enabled, false, `"${off}" hides the board`);
  }
  assert.equal(readEnv({ LOOP_KANBAN: "board" }).kanban?.mode, "board");
  assert.equal(readEnv({ LOOP_KANBAN: "board" }).kanban?.enabled, true);
  assert.equal(readEnv({ LOOP_KANBAN: " ROW " }).kanban?.mode, "row");

  // Unset is a third answer, and not the same as any of the above.
  assert.equal(readEnv({}).kanban?.enabled, true, "unset is on: the board is a default");
  assert.equal(readEnv({}).kanban?.mode, undefined, "unset leaves the shape to each surface");
});

test("LOOP_KANBAN=off is carried by `enabled`, not by a second way of saying it", () => {
  const off = readEnv({ LOOP_KANBAN: "off" }).kanban;
  assert.equal(off?.enabled, false);
  assert.equal(off?.mode, undefined, "no shape is claimed for a board that is not drawn");
});

// -- the placement knobs -----------------------------------------------------

test("LOOP_KANBAN_AT and LOOP_MONITOR_AT take two words and refuse everything else", () => {
  // Both keys, both mistake families. `"upper"`, `"topp"` and `"middle"` each
  // used to land on `"band"` without a word of complaint, and the only way to
  // find that out was to look at the screen and wonder.
  const sections = [
    { key: "LOOP_KANBAN_AT" as const, read: (env: Env) => readEnv(env).kanban?.placement },
    { key: "LOOP_MONITOR_AT" as const, read: (env: Env) => readEnv(env).monitor?.placement },
  ];
  for (const section of sections) {
    for (const bad of ["middle", "upper", "topp", "bottom", "footer", "up"]) {
      const error = refused({ [section.key]: bad });
      includes(error.message, section.key, "the refusal names which of the two keys");
      includes(error.message, bad, "and the value that was rejected");
      includes(error.message, "accepted: band, top", "and both allowed values");
    }
    assert.equal(section.read({ [section.key]: "top" }), "top");
    assert.equal(
      section.read({ [section.key]: " BAND " }),
      "band",
      "surrounding whitespace is a typing courtesy, not a wrong value",
    );
    assert.equal(
      section.read({}),
      "band",
      "unset is the fixed chrome above the footer",
    );
  }
});

// -- the switches ----------------------------------------------------------

test("every flag refuses what is not an on/off word instead of reading it as off", () => {
  // One table, because these were ten separate instances of the same slip:
  // `flag()` returned false for anything that was not literally on, so
  // `LOOP_AUDIT=ture` was a run that skipped the audit and looked like a run
  // somebody had configured to skip the audit.
  const switches: readonly {
    key: string;
    read: (env: Env) => boolean | undefined;
    unset: boolean | undefined;
  }[] = [
    // `unset` is what the *config* holds when the key is absent, which is not
    // always a boolean: three of these leave the key off the config entirely and
    // let the consumer's own default stand. That is the point of the split.
    { key: "LOOP_VERBOSE", read: (env) => readEnv(env).verbose, unset: undefined },
    { key: "LOOP_DRY_RUN", read: (env) => readEnv(env).dryRun, unset: undefined },
    { key: "LOOP_RETRY_UNFIT_WORK", read: (env) => readEnv(env).retryUnfitWork, unset: undefined },
    { key: "LOOP_KANBAN_VERBOSE", read: (env) => readEnv(env).kanban?.verbose, unset: false },
    { key: "LOOP_MONITOR", read: (env) => readEnv(env).monitor?.enabled, unset: true },
    { key: "LOOP_MONITOR_VERBOSE", read: (env) => readEnv(env).monitor?.verbose, unset: false },
    { key: "LOOP_AUDIT", read: (env) => readEnv(env).providerAudit?.enabled, unset: true },
    { key: "LOOP_AUDIT_STRICT", read: (env) => readEnv(env).providerAudit?.strict, unset: false },
    { key: "LOOP_AUDIT_VERBOSE", read: (env) => readEnv(env).providerAudit?.verbose, unset: false },
    { key: "LOOP_SPLIT_REPO_ACCESS", read: (env) => readEnv(env).splitRepoAccess, unset: true },
  ];

  for (const knob of switches) {
    for (const bad of ["ture", "ONN", "maybe", "yess", "enabled", "  ", "2", "-1"]) {
      const error = refused({ [knob.key]: bad });
      includes(error.message, knob.key, `the refusal names ${knob.key}`);
      includes(error.message, bad, "and the value that was rejected");
      includes(
        error.message,
        "accepted: 1, true, yes, on (on) or 0, false, no, off (off)",
        "and both halves of the vocabulary",
      );
    }

    for (const on of ["1", "true", "yes", "on", " TRUE ", "On"]) {
      assert.equal(knob.read({ [knob.key]: on }), true, `${knob.key}="${on}" is on`);
    }
    for (const off of ["0", "false", "no", "off", " OFF "]) {
      assert.equal(knob.read({ [knob.key]: off }), false, `${knob.key}="${off}" is off`);
    }

    assert.equal(
      knob.read({}),
      knob.unset,
      `${knob.key} unset is its documented default, reached only by the key being absent`,
    );
  }
});

// -- the numbers -----------------------------------------------------------

test("every LOOP_MONITOR_* duration refuses a value that is not a whole number", () => {
  // The acceptance case, spelled out: `""` is a key somebody set with nothing
  // in it and `"1x"` is a number with a unit typed into it. Neither is a
  // duration, and neither gets to run at the default as if it were.
  const durations = [
    { key: "LOOP_MONITOR_MS", read: (env: Env) => readEnv(env).monitor?.intervalMs },
    { key: "LOOP_MONITOR_TIMEOUT_MS", read: (env: Env) => readEnv(env).monitor?.timeoutMs },
    { key: "LOOP_MONITOR_MODELS_MS", read: (env: Env) => readEnv(env).monitor?.modelsEveryMs },
  ];
  for (const knob of durations) {
    for (const bad of ["", "1x", "  ", "1.5", "two", "1,5", "0", "-1", "1e3.5"]) {
      const error = refused({ [knob.key]: bad });
      includes(error.message, knob.key, "the refusal names the key");
      includes(error.message, "is not a whole number of milliseconds", "and what it wanted");
      includes(error.message, "accepted: a whole number, at least 1", "and the legal range");
    }
    assert.equal(knob.read({ [knob.key]: "2500" }), 2_500, `${knob.key} still reaches the monitor`);
  }
});

test("the rest of the numeric knobs are held to the same standard", () => {
  const numeric: readonly (readonly [string, (env: Env) => number | undefined])[] = [
    ["LOOP_AUDIT_TIMEOUT_MS", (env) => readEnv(env).providerAudit?.timeoutMs],
    ["LOOP_KANBAN_MS", (env) => readEnv(env).kanban?.intervalMs],
    ["LOOP_KANBAN_LINES", (env) => readEnv(env).kanban?.lines],
    ["LOOP_KANBAN_DONE", (env) => readEnv(env).kanban?.doneLimit],
    ["LOOP_GIT_LOCK_WAIT_MS", (env) => readEnv(env).gitLock?.waitMs],
    ["LOOP_GIT_KILL_GRACE_MS", (env) => readEnv(env).gitLock?.killGraceMs],
    ["LOOP_GIT_STALE_LOCK_AFTER_MS", (env) => readEnv(env).gitLock?.staleAfterMs],
    ["LOOP_WORK_TIMEOUT_MS", (env) => readEnv(env).workTimeoutMs],
    ["LOOP_WRAP_UP_MS", (env) => readEnv(env).wrapUpMs],
    ["LOOP_MAX_ITERATIONS", (env) => readEnv(env).maxIterations],
    ["LOOP_MONITOR_LINES", (env) => readEnv(env).monitor?.lines],
    ["LOOP_NTFY_TIMEOUT_MS", (env) => readEnv(env).notify?.timeoutMs],
  ];

  for (const [key, read] of numeric) {
    // A section with no topic has no timeout to be wrong about, so give the
    // ntfy key a section to live in.
    const knobs: Env = key.startsWith("LOOP_NTFY") ? { LOOP_NTFY_TOPIC: "t" } : {};
    for (const bad of ["", "1x", "NaN", "Infinity", "1.5", "0x10"]) {
      const error = refused({ ...knobs, [key]: bad });
      includes(error.message, key, "the refusal names the key it could not read");
    }
    // A real number still gets where it was going.
    assert.equal(read({ ...knobs, [key]: "7" }), 7, `${key}="7" reaches the config`);
  }

  // Where zero means something, zero is allowed. Where it means "a poll that
  // never polls" or "a loop that never loops", it is refused like any other
  // nonsense value — with the range in the message.
  assert.equal(readEnv({ LOOP_KANBAN_DONE: "0" }).kanban?.doneLimit, 0, "keep no closed tickets");
  assert.equal(readEnv({ LOOP_GIT_LOCK_WAIT_MS: "0" }).gitLock?.waitMs, 0, "do not wait at all");
  refused({ LOOP_MONITOR_MS: "0" });
  refused({ LOOP_MAX_ITERATIONS: "0" });
  refused({ LOOP_WORK_TIMEOUT_MS: "0" });
});

test("unset is still unset: no numeric knob appears when the key is absent", () => {
  const bare = readEnv({});
  assert.equal(bare.workTimeoutMs, undefined, "unset means the derived rule, not zero");
  assert.equal(bare.wrapUpMs, undefined);
  assert.equal(bare.maxIterations, undefined);
  assert.equal(bare.kanban?.intervalMs, undefined, "unset means the source's own default");
  assert.equal(bare.gitLock?.waitMs, undefined);
  assert.equal(bare.monitor?.intervalMs, undefined);
});

// -- the two remaining keyword knobs ----------------------------------------

test("LOOP_GIT_STALE_LOCK takes report or remove, not anything-that-is-not-remove", () => {
  assert.equal(readEnv({ LOOP_GIT_STALE_LOCK: "remove" }).gitLock?.stalePolicy, "remove");
  assert.equal(readEnv({ LOOP_GIT_STALE_LOCK: " Report " }).gitLock?.stalePolicy, "report");
  assert.equal(readEnv({}).gitLock?.stalePolicy, undefined, "unset: the writer's own default");

  // The old rule was `=== "remove"`, so anything else took the default. A
  // policy about deleting other processes' locks is not a thing to arrive at
  // by not-typing-something-else.
  for (const bad of ["delete", "clear", "remov", "dropped", "clear-it"]) {
    const error = refused({ LOOP_GIT_STALE_LOCK: bad });
    includes(error.message, "LOOP_GIT_STALE_LOCK", "the refusal names the key");
    includes(error.message, "accepted: report, remove", "and both policies");
  }
});

test("LOOP_AUDIT_WRITE is a declared mode, not a truthiness test", () => {
  assert.equal(readEnv({}).providerAudit?.writeMode, "none");
  assert.equal(readEnv({ LOOP_AUDIT_WRITE: "0" }).providerAudit?.writeMode, "none");
  assert.equal(readEnv({ LOOP_AUDIT_WRITE: "1" }).providerAudit?.writeMode, "proposed");
  assert.equal(readEnv({ LOOP_AUDIT_WRITE: "inplace" }).providerAudit?.writeMode, "inplace");
  assert.equal(readEnv({ LOOP_AUDIT_WRITE: "in-place" }).providerAudit?.writeMode, "inplace");

  // `in-plce` used to fall through to `proposed`: a request to patch the
  // live file that quietly became a request to write a side file, and no
  // difference anybody would notice until the side file was needed.
  for (const bad of ["proopsed", "in-plce", "in place", "live", "yes please"]) {
    const error = refused({ LOOP_AUDIT_WRITE: bad });
    includes(error.message, "LOOP_AUDIT_WRITE", "the refusal names the key");
    includes(error.message, "inplace", "and the accepted set, including the dangerous word");
  }
});

// -- unknown keys ----------------------------------------------------------

test("an unknown LOOP_* key stops the run instead of being silently ignored", () => {
  // A typo'd key is the worst-shaped mistake of all: it is *set* as far as the
  // shell is concerned and *unset* as far as the loop is concerned, so every
  // default stays default and the run reads as configured. `LOOP_KANBAN_MODE`
  // is the canonical one — a real word, in a real knob's shape, that nothing
  // on the other end reads.
  const error = refused({ LOOP_KANBAN_MODE: "board" });
  includes(error.message, "LOOP_KANBAN_MODE", "the refusal names the offending key");
  includes(error.message, "did you mean LOOP_KANBAN", "and the knob it was reaching for");

  for (const bad of ["LOOP_MONITR", "LOOP_KANBANLINES", "LOOP_WORK_TIMEOUTS_MS", "LOOP_KANB AN"]) {
    const thrown = refused({ [bad]: "1" });
    includes(thrown.message, bad, "each unknown key is named");
  }

  // The whole accepted surface is listed, because "which names are right?" is
  // the question the operator is left with and it should be answered where the
  // error was printed.
  assert.ok(acceptedKnobs(error.message).length > 40, "the accepted list is the whole surface");
});

test("non-LOOP keys are pi's business and are not the unknown-knob rule's", () => {
  const config = readEnv({ PI_MODEL: "some-model", PI_THEME: "dark", PATH: "/usr/bin" });
  assert.equal(config.themeName, "dark");
  assert.equal(config.modelRef, undefined, "one of two keys missing is unrelated to this rule");
});

test("the accepted-knob list is not a lie: every key in it is one the loop reads", () => {
  const listed = acceptedKnobs(refused({ LOOP_NOPE: "1" }).message);
  const tree = ["main.ts", "beads.ts", "repo.ts", "vcs.ts", "idle.ts"].map(readSource).join("\n");
  for (const key of listed) {
    assert.ok(
      tree.includes(key),
      `${key} is offered as an accepted knob but no source file reads it — the list drifted`,
    );
  }
});

// -- failing before anything happens ---------------------------------------

test("readEnv refuses synchronously, so nothing downstream can have started", () => {
  // `readEnv` is synchronous and throws. That is the whole guarantee its
  // callers get: there is no half-built config, no promise that rejects after
  // a bead has been claimed, nothing to unwind.
  let returnedAConfig = false;
  try {
    readEnv({ LOOP_KANBAN: "bord" });
    returnedAConfig = true;
  } catch (error) {
    assert.ok(LoopError.is(error), "the throw is one of ours");
  }
  assert.equal(returnedAConfig, false, "the throw came from the read, not from anything after it");
});

test("a bad knob is one fatal line and exit 2, before the run is entered", async () => {
  const lines: string[] = [];
  const code = await runFromEnv(() => readEnv({ LOOP_KANBAN_AT: "middle" }), (line) => {
    lines.push(line);
  });
  assert.equal(code, 2);
  assert.equal(lines.length, 1, `one line, not a stack: ${lines.join("\n")}`);
  includes(lines[0] ?? "", 'Fatal: bad-config: LOOP_KANBAN_AT="middle"', "the fatal line names the knob");
});

/**
 * The environment for a subprocess run: everything this shell has, minus every
 * `LOOP_*` key, so the knobs under test are the only ones the run can see.
 */
function cleanEnv(knobs: Env): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined && !key.startsWith("LOOP_")) out[key] = value;
  }
  for (const [key, value] of Object.entries(knobs)) {
    if (value !== undefined) out[key] = value;
  }
  return out;
}

/**
 * The real entry point, as a subprocess, in a directory that is **not** a git
 * work tree.
 *
 * The missing work tree is the instrument. A run that gets as far as building
 * the app finds out there is nowhere to commit and says so — so "the git
 * message is absent" is evidence the run never reached `buildApp`: no `bd`
 * client, no git writer, no session, no terminal taken. The control run below
 * proves the harness can tell the difference rather than simply being unable
 * to see either case.
 */
function runEntry(knobs: Env, dir: string) {
  return spawnSync(process.execPath, [join(SRC_DIR, "main.ts")], {
    cwd: dir,
    env: cleanEnv(knobs),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 60_000,
  });
}

const WORK_TREE_MESSAGE = /git work tree|not a git repository/iu;

test("a typo'd knob aborts before any session, adapter or terminal surface exists", () => {
  const dir = mkdtempSync(join(tmpdir(), "loop-knobs-"));
  try {
    // Control: a valid value gets all the way to the work-tree check inside
    // the composition root. The entry point *can* reach that far from this
    // harness, so the next two runs not reaching it means something.
    const control = runEntry({ LOOP_KANBAN: "board", LOOP_MONITOR: "0", LOOP_AUDIT: "0" }, dir);
    assert.match(
      `${control.stdout}${control.stderr}`,
      WORK_TREE_MESSAGE,
      `control run should have reached the work-tree check: ${control.stdout} | ${control.stderr}`,
    );

    const bad = runEntry({ LOOP_KANBAN: "bord" }, dir);
    assert.equal(bad.status, 2, "the run exits 2, the same code as any other fatal");
    includes(bad.stdout, 'LOOP_KANBAN="bord"', "one line naming the knob and its value");
    includes(bad.stdout, "accepted: 0, off, no, false, row, board", "and what it accepts");
    assert.doesNotMatch(
      `${bad.stdout}${bad.stderr}`,
      WORK_TREE_MESSAGE,
      "never reached the point where a writer, a board client or a session exists",
    );

    // Same for an unknown key: it never gets the chance to become somebody's
    // unnoticed default.
    const unknown = runEntry({ LOOP_KANBAN_MODE: "board" }, dir);
    assert.equal(unknown.status, 2);
    includes(unknown.stdout, "LOOP_KANBAN_MODE", "the unknown key is named");
    assert.doesNotMatch(`${unknown.stdout}${unknown.stderr}`, WORK_TREE_MESSAGE);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
