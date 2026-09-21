/**
 * Tests for `src/beads.ts` against a fake `bd` shim (`test/fake-bin/bd`).
 *
 * The shim records the argv and environment of every invocation, so these tests
 * assert what the adapter *sent*, not only what it returned. Exit-code shapes are
 * the ones captured from real `bd 1.3.0`.
 */
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  BdError,
  assertClaimFreeArgs,
  createBdClient,
  dependsOn,
  isEpicIssue,
  normaliseDependencies,
  selectWorkable,
  type BdClient,
  type Issue,
} from "../src/beads.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC_DIR = join(HERE, "..", "src");
const FAKE_BD = join(HERE, "fake-bin", "bd");

interface ShimCall {
  scenario: string;
  argv: string[];
  bdLastTouchedFallback: string | null;
}

interface Harness {
  client: BdClient;
  logs: string[];
  dir: string;
  calls(): ShimCall[];
}

interface HarnessOverrides {
  bin?: string;
  timeoutMs?: number;
  env?: Record<string, string>;
}

/**
 * Each scenario gets its own record file, so `calls()` is exactly the set of
 * children this test caused.
 */
function harness(scenario: string, overrides: HarnessOverrides = {}): Harness {
  const dir = mkdtempSync(join(tmpdir(), "bd-adapter-test-"));
  const recordPath = join(dir, "calls.jsonl");
  process.env.FAKE_BD_SCENARIO = scenario;
  process.env.FAKE_BD_RECORD = recordPath;

  const logs: string[] = [];
  const client = createBdClient({
    bin: overrides.bin ?? FAKE_BD,
    timeoutMs: overrides.timeoutMs,
    env: overrides.env,
    debug: true,
    logger: (line) => logs.push(line),
  });

  return {
    client,
    logs,
    dir,
    calls(): ShimCall[] {
      if (!existsSync(recordPath)) return [];
      return readFileSync(recordPath, "utf8")
        .split("\n")
        .filter((line) => line.trim() !== "")
        .map((line) => JSON.parse(line) as ShimCall);
    },
  };
}

function isBdError(kind: BdError["kind"], extra?: (error: BdError) => void): (e: unknown) => boolean {
  return (error: unknown): boolean => {
    assert.ok(BdError.is(error), `expected a BdError, got ${String(error)}`);
    const bd = error as BdError;
    assert.equal(bd.kind, kind, `expected kind ${kind}, got ${bd.kind}: ${bd.message}`);
    extra?.(bd);
    return true;
  };
}

function slice(argv: string[], flag: string, count: number): string[] {
  const at = argv.indexOf(flag);
  assert.notEqual(at, -1, `argv must contain ${flag}; got ${JSON.stringify(argv)}`);
  return argv.slice(at, at + 1 + count);
}

// ── happy paths ────────────────────────────────────────────────────────

test("listReady returns typed issues and runs `bd ready --json` with the fallback disabled", async () => {
  const { client, calls } = harness("issues");

  const issues: Issue[] = await client.listReady({ labels: ["loop"] });

  assert.equal(issues.length, 2);
  assert.equal(issues[0]?.id, "fake-1");
  assert.equal(issues[1]?.status, "open");
  assert.equal(issues[0]?.acceptance_criteria, "ac for fake-1");

  const recorded = calls();
  assert.equal(recorded.length, 1);
  assert.deepEqual(recorded[0]?.argv, ["ready", "--json", "--label", "loop"]);
  assert.equal(recorded[0]?.bdLastTouchedFallback, "0");
});

test("listInProgress runs `bd list --status in_progress --json`", async () => {
  const { client, calls } = harness("issues");

  await client.listInProgress({ limit: 5 });

  assert.deepEqual(calls()[0]?.argv, [
    "list",
    "--status",
    "in_progress",
    "--json",
    "--limit",
    "5",
  ]);
});

test("an empty board is [] and not an error", async () => {
  const { client } = harness("empty");
  assert.deepEqual(await client.listReady(), []);
  assert.deepEqual(await client.listInProgress(), []);
});

test("createIssue returns the created issue from a single JSON object", async () => {
  const { client, calls } = harness("create");

  const created = await client.createIssue({
    title: "Build the splitter",
    description: "desc",
    acceptance: "ac",
    priority: 1,
    type: "feature",
    labels: ["loop", "p1"],
    parent: "fake-parent",
    deps: ["fake-dep1", "fake-dep2"],
  });

  assert.equal(created.id, "fake-new1");
  const argv = calls()[0]?.argv ?? [];
  assert.ok(argv.includes("--json"));
  assert.deepEqual(slice(argv, "--title", 1), ["--title", "Build the splitter"]);
  assert.deepEqual(slice(argv, "--acceptance", 1), ["--acceptance", "ac"]);
  assert.deepEqual(slice(argv, "--priority", 1), ["--priority", "1"]);
  assert.deepEqual(slice(argv, "--type", 1), ["--type", "feature"]);
  assert.deepEqual(slice(argv, "--parent", 1), ["--parent", "fake-parent"]);
  assert.equal(argv.filter((a) => a === "--labels").length, 2);
  assert.ok(
    argv.includes("blocked-by:fake-dep1,blocked-by:fake-dep2"),
    `deps must be mapped to blocked-by: <${argv.join(" ")}>`,
  );
});

test("addDep, setStatus and closeIssue build the expected argv", async () => {
  const dep = harness("dep");
  await dep.client.addDep("fake-2", "fake-1");
  assert.deepEqual(dep.calls()[0]?.argv, ["dep", "add", "fake-2", "fake-1", "--json"]);

  const status = harness("updated");
  const updated = await status.client.setStatus("fake-1", "in_progress");
  assert.equal(updated.status, "in_progress");
  assert.deepEqual(status.calls()[0]?.argv, [
    "update",
    "fake-1",
    "--json",
    "--status",
    "in_progress",
  ]);

  const guarded = harness("updated");
  await guarded.client.setStatus("fake-1", "in_progress", { ifStatus: "open" });
  assert.deepEqual(guarded.calls()[0]?.argv, [
    "update",
    "fake-1",
    "--json",
    "--status",
    "in_progress",
    "--if-status",
    "open",
  ]);

  const closed = harness("closed");
  await closed.client.closeIssue("fake-1", "shipped");
  assert.deepEqual(closed.calls()[0]?.argv, [
    "close",
    "fake-1",
    "--json",
    "--reason",
    "shipped",
  ]);
});

test("appendNote appends to the notes field with `bd note`, never replacing it", async () => {
  const noted = harness("noted");
  const issue = await noted.client.appendNote("fake-1", "Human request, verbatim:\nship the loop");

  assert.deepEqual(noted.calls()[0]?.argv, [
    "note",
    "fake-1",
    "Human request, verbatim:\nship the loop",
    "--json",
  ]);
  // The echoed note is what real bd stores; nothing here rewrites it.
  assert.equal(issue.notes, "Human request, verbatim:\nship the loop");

  // An empty id or empty text is refused before a child is spawned: `bd note "" x`
  // is exactly the kind of call that lands on someone else's issue.
  const empty = harness("noted");
  await assert.rejects(() => empty.client.appendNote("", "text"), isBdError("invalid-arguments"));
  await assert.rejects(() => empty.client.appendNote("   ", "text"), isBdError("invalid-arguments"));
  await assert.rejects(() => empty.client.appendNote("fake-1", "  "), isBdError("invalid-arguments"));
  assert.equal(empty.calls().length, 0, "a refused note must not spawn bd");
});

test("remember and recall round-trip through --json", async () => {
  const store = harness("remembered");
  await store.client.remember("iteration 3 finished the adapter", "loop-handoff");
  assert.deepEqual(store.calls()[0]?.argv, [
    "remember",
    "iteration 3 finished the adapter",
    "--json",
    "--key",
    "loop-handoff",
  ]);

  const found = harness("recall-found");
  assert.equal(await found.client.recall("loop-handoff"), "remembered handoff text");
});

// ── not-found is an answer, not a fault ────────────────────────────────

test("getIssue returns null for a missing issue (bd exits 1 with a JSON error body)", async () => {
  const { client, calls } = harness("show-not-found");

  assert.equal(await client.getIssue("fake-gone"), null);
  assert.deepEqual(calls()[0]?.argv, ["show", "fake-gone", "--json"]);
});

test("recall returns null for a missing key (bd exits 1 with found:false)", async () => {
  const { client } = harness("recall-missing");
  assert.equal(await client.recall("nope"), null);
});

// ── failure taxonomy ───────────────────────────────────────────────────

test("exit 1 is a typed exit-1 error carrying stderr", async () => {
  const { client } = harness("exit1");

  await assert.rejects(
    () => client.listReady(),
    isBdError("exit-1", (bd) => {
      assert.equal(bd.exitCode, 1);
      assert.match(bd.stderr, /general failure/u);
    }),
  );
});

test("exit 13 is guard-mismatch and is never retried", async () => {
  const { client, calls } = harness("guard13");

  await assert.rejects(
    () => client.setStatus("fake-1", "blocked", { ifStatus: "closed" }),
    isBdError("guard-mismatch", (bd) => {
      assert.equal(bd.exitCode, 13);
      assert.match(bd.message, /[Nn]ot retried/u);
    }),
  );

  assert.equal(calls().length, 1, "a guard mismatch must not be retried blindly");
});

test("non-JSON stdout on exit 0 is an error, never an empty list", async () => {
  const { client } = harness("nonjson");

  await assert.rejects(
    () => client.listReady(),
    isBdError("non-json", (bd) => assert.match(bd.stdoutSnippet, /definitely not JSON/u)),
  );
});

test("exit 0 with no stdout at all is an error, never an empty list", async () => {
  const { client } = harness("empty-output");
  await assert.rejects(() => client.listReady(), isBdError("non-json"));
});

test("a missing bd binary is an actionable typed error", async () => {
  const { client } = harness("issues", { bin: join(HERE, "fake-bin", "no-such-bd") });

  await assert.rejects(
    () => client.getIssue("fake-1"),
    isBdError("missing-binary", (bd) => {
      assert.match(bd.message, /not found/u);
      assert.match(bd.message, /PATH/u);
      assert.equal(bd.exitCode, null);
    }),
  );
});

test("a hung bd is a timeout error", async () => {
  const { client } = harness("slow", { timeoutMs: 300 });
  await assert.rejects(() => client.listReady(), isBdError("timeout"));
});

test("an unknown bd status is refused locally", async () => {
  const { client, calls } = harness("updated");

  await assert.rejects(
    () => client.setStatus("fake-1", "wonky" as never),
    isBdError("invalid-arguments"),
  );
  assert.equal(calls().length, 0, "nothing may spawn for an invalid status");
});

// ── the empty-variable protection ──────────────────────────────────────

test("blank ids are refused before any child is spawned", async () => {
  for (const blank of ["", "   ", "\t"]) {
    const { client, calls } = harness("updated");

    await assert.rejects(() => client.getIssue(blank), isBdError("invalid-arguments"));
    await assert.rejects(() => client.setStatus(blank, "in_progress"), isBdError("invalid-arguments"));
    await assert.rejects(() => client.closeIssue(blank, "why"), isBdError("invalid-arguments"));
    await assert.rejects(() => client.addDep(blank, "fake-1"), isBdError("invalid-arguments"));
    await assert.rejects(() => client.addDep("fake-1", blank), isBdError("invalid-arguments"));
    await assert.rejects(() => client.remember(blank), isBdError("invalid-arguments"));
    await assert.rejects(() => client.recall(blank), isBdError("invalid-arguments"));
    await assert.rejects(
      () => client.createIssue({ title: blank }),
      isBdError("invalid-arguments"),
    );
    await assert.rejects(
      () => client.createIssue({ title: "ok", parent: blank }),
      isBdError("invalid-arguments"),
    );
    await assert.rejects(
      () => client.createIssue({ title: "ok", deps: ["good", blank] }),
      isBdError("invalid-arguments"),
    );

    assert.equal(calls().length, 0, "an empty id must never reach bd");
  }
});

test("every mutating method still forces BD_LAST_TOUCHED_FALLBACK=0", async () => {
  const mutations: Array<{ name: string; run: (c: BdClient) => Promise<unknown> }> = [
    { name: "listReady", run: (c) => c.listReady() },
    { name: "listInProgress", run: (c) => c.listInProgress() },
    { name: "getIssue", run: (c) => c.getIssue("fake-1") },
    { name: "createIssue", run: (c) => c.createIssue({ title: "t" }) },
    { name: "addDep", run: (c) => c.addDep("fake-2", "fake-1") },
    { name: "appendNote", run: (c) => c.appendNote("fake-1", "a note") },
    { name: "setStatus", run: (c) => c.setStatus("fake-1", "in_progress") },
    { name: "closeIssue", run: (c) => c.closeIssue("fake-1", "done") },
    { name: "remember", run: (c) => c.remember("text", "k") },
    { name: "recall", run: (c) => c.recall("k") },
  ];

  for (const mutation of mutations) {
    // Both a hostile option and a hostile ambient env must lose to the adapter.
    const { client, calls } = harness("issues", {
      env: { BD_LAST_TOUCHED_FALLBACK: "1" },
    });
    process.env.BD_LAST_TOUCHED_FALLBACK = "1";
    try {
      await mutation.run(client);
    } catch (error) {
      // Scenario payloads differ per command; only the env assertion matters here.
      assert.ok(BdError.is(error), `${mutation.name}: unexpected error ${String(error)}`);
    }
    const recorded = calls();
    assert.ok(recorded.length >= 1, `${mutation.name} must spawn exactly one child`);
    for (const call of recorded) {
      assert.equal(
        call.bdLastTouchedFallback,
        "0",
        `${mutation.name} must run with BD_LAST_TOUCHED_FALLBACK=0`,
      );
    }
  }

  delete process.env.BD_LAST_TOUCHED_FALLBACK;
});

// ── no shell, no claim path ────────────────────────────────────────────

test("argv is never shell-interpreted: a shell payload in a title is inert", async () => {
  const marker = join(mkdtempSync(join(tmpdir(), "bd-shell-canary-")), "pwned");
  const { client, calls } = harness("create");

  await client.createIssue({ title: `nope; touch ${marker}` });

  assert.equal(existsSync(marker), false, "a shell metacharacter must not be executed");
  assert.deepEqual(
    slice(calls()[0]?.argv ?? [], "--title", 1),
    ["--title", `nope; touch ${marker}`],
    "the payload must travel as a single argv element",
  );
});

test("assertClaimFreeArgs refuses claim and assignee flags", () => {
  assert.throws(
    () => assertClaimFreeArgs(["update", "--claim", "fake-1"]),
    (error: unknown) => BdError.is(error) && error.kind === "invalid-arguments",
  );
  assert.throws(() => assertClaimFreeArgs(["update", "--assignee", "someone"]));
  assert.throws(() => assertClaimFreeArgs(["list", "-a", "someone"]));
  assert.doesNotThrow(() => assertClaimFreeArgs(["list", "--json", "--status", "in_progress"]));
});

/**
 * Only these modules may spawn anything, each for one named purpose. The
 * invariant is not "beads.ts is special" — it is "process creation is confined
 * to named adapters, so there is a short list to audit and everything else has
 * to go through them". Adding an entry is a deliberate act; the test below also
 * fails if an entry stops spawning, so the list cannot go stale.
 */
const SPAWN_ALLOWLIST: Readonly<Record<string, string>> = {
  "beads.ts": "bd, the issue tracker",
  "repo.ts": "git, read-only snapshotting",
  "vcs.ts": "git, staging and committing for finalize",
};

test("only the named adapter modules may spawn processes anywhere in src/", () => {
  for (const file of readdirSync(SRC_DIR).filter((f) => f.endsWith(".ts"))) {
    const text = readFileSync(join(SRC_DIR, file), "utf8");
    const allowed = SPAWN_ALLOWLIST[file] !== undefined;
    const usesChildProcess = /child_process/.test(text);
    const spawns = /\b(execFile|spawnSync|spawn)\s*\(/.test(text);

    if (!allowed) {
      assert.equal(
        usesChildProcess,
        false,
        `${file} must not import child_process — process access is confined to ${Object.keys(SPAWN_ALLOWLIST).join(", ")}`,
      );
      assert.equal(spawns, false, `${file} must not spawn processes directly`);
      continue;
    }
    // An allowlisted file must actually still be an adapter, not a stale entry.
    assert.equal(
      usesChildProcess || spawns,
      true,
      `${file} is allowlisted (${SPAWN_ALLOWLIST[file]}) but spawns nothing — drop it from the allowlist`,
    );
  }
});

test("claim/assignee flags appear in src/ only inside beads.ts's denylist and prose", () => {
  const banned = ["--assignee", "--claim"];
  for (const file of readdirSync(SRC_DIR).filter((f) => f.endsWith(".ts"))) {
    const text = readFileSync(join(SRC_DIR, file), "utf8");
    if (file !== "beads.ts") {
      for (const flag of banned) {
        assert.equal(text.includes(flag), false, `${file} must not mention ${flag}`);
      }
      continue;
    }
    text.split("\n").forEach((line, index) => {
      if (!banned.some((flag) => line.includes(flag))) return;
      const allowed =
        /FORBIDDEN_ARGS/.test(line) || // the denylist itself
        /^\s*(\*|\/\/)/.test(line); // documentation
      assert.equal(allowed, true, `beads.ts:${index + 1} must not carry a claim flag: ${line}`);
    });
  }
});

// ── debug logging ──────────────────────────────────────────────────────

test("the command is logged at debug level before it executes", async () => {
  const { client, logs, calls } = harness("issues");

  const pending = client.listReady();
  // Asserted before awaiting: the log line must already exist while the child is
  // still running, i.e. it is written prior to execution, not after.
  assert.equal(logs.length, 1, `expected one pre-execution log line, got: ${logs.join(" | ")}`);
  assert.match(logs[0] ?? "", /^bd\[debug\] \$ bd ready --json$/u);

  await pending;
  assert.equal(calls().length, 1);
  assert.ok(
    logs.some((line) => /→ exit 0/u.test(line)),
    "the exit code is logged after the run too",
  );
});

test("long arguments are truncated in the debug log but sent in full", async () => {
  const long = "x".repeat(500);
  const { client, logs } = harness("create");

  await client.createIssue({ title: long });

  const logged = logs.find((line) => line.startsWith("bd[debug] $ bd "));
  assert.ok(logged !== undefined);
  assert.ok((logged?.length ?? 0) < 200, `log line should be truncated, was ${logged?.length}`);
  assert.match(logged ?? "", /\(\+380 chars\)/u);
});

test("debug output is off unless LOOP_DEBUG or debug:true", async () => {
  const quietLogs: string[] = [];
  const quiet = createBdClient({
    bin: FAKE_BD,
    debug: false,
    logger: (line) => quietLogs.push(line),
  });
  delete process.env.LOOP_DEBUG;

  await quiet.listReady();

  assert.deepEqual(quietLogs, []);
});

// ── the two dependency shapes bd actually emits ────────────────────────

test("list/ready report dependencies as edges; normaliseDependencies reads them", async () => {
  const { client } = harness("list-deps");
  const [child] = await client.listReady();
  assert.ok(child, "expected one issue");

  assert.deepEqual(normaliseDependencies(child), [{ id: "fake-blocker", type: "blocks" }]);
  assert.equal(dependsOn(child, "fake-blocker"), true);
  assert.equal(dependsOn(child, "unrelated"), false);
});

test("bd show inlines the other issue; normaliseDependencies still finds it", async () => {
  const { client } = harness("show-deps");
  const shown = await client.getIssue("fake-child");
  assert.ok(shown);

  const deps = normaliseDependencies(shown);
  assert.equal(deps.length, 1);
  assert.equal(deps[0]?.id, "fake-blocker");
  assert.equal(deps[0]?.type, "blocks");
  assert.equal(deps[0]?.issue?.status, "in_progress", "show inlines the blocker issue itself");
  assert.equal(dependsOn(shown, "fake-blocker"), true);

  // The fail-open trap, pinned: on the `show` shape a naive `depends_on_id` read
  // is undefined, so "is this blocked by X?" would wrongly answer "no".
  const raw = (shown.dependencies?.[0] ?? {}) as Record<string, unknown>;
  assert.equal(raw.depends_on_id, undefined);
  assert.equal(raw.dependency_type, "blocks");
});

test("an issue with no dependencies normalises to []", async () => {
  const { client } = harness("issues");
  const [one] = await client.listReady();
  assert.ok(one);

  assert.deepEqual(normaliseDependencies(one), []);
  assert.equal(dependsOn(one, "anything"), false);
});

// ── the one definition of pickable work ───────────────────────────────────

test("an epic is a container, not work", () => {
  assert.equal(isEpicIssue({ issue_type: "epic" }), true);
  assert.equal(isEpicIssue({ issue_type: "EPIC" }), true, "bd's casing is not our business");
  assert.equal(isEpicIssue({ issue_type: "task" }), false);
  assert.equal(isEpicIssue({}), false);
  assert.equal(isEpicIssue({ issue_type: "epiclogue" }), false, "prefix matching is not matching");
});

test("selectWorkable returns both halves so the difference is sayable", () => {
  const issues = [
    { id: "p.1", issue_type: "task" },
    { id: "p.2", issue_type: "epic" },
    { id: "p.3", issue_type: "bug" },
  ];

  const held = selectWorkable(issues);
  assert.deepEqual(held.pickable.map((i) => i.id), ["p.1", "p.3"]);
  assert.deepEqual(held.heldOut.map((i) => i.id), ["p.2"]);

  const everything = selectWorkable(issues, { workEpics: true });
  assert.deepEqual(everything.pickable.map((i) => i.id), ["p.1", "p.2", "p.3"]);
  assert.deepEqual(everything.heldOut, []);

  assert.deepEqual(selectWorkable([]), { pickable: [], heldOut: [] });
  assert.equal(issues.length, 3, "the input is never mutated");
});
