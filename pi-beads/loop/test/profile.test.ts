/**
 * Tests for `src/profile.ts` — the loop's standing relationship with one box.
 *
 * What is under test is the *degradation*, because that is the part that runs in
 * production and the part a happy-path test never reaches. A probe that succeeds
 * at startup and fails before the third pass must leave the loop exactly where it
 * was before the probe existed: working, with the declared config, and a line in
 * the log saying what changed. The opposite — a status endpoint having a bad
 * afternoon turning into "no work happened today" — is the failure this module
 * is most likely to cause and the one its author would least expect.
 *
 * The capacity half is arithmetic about time, so the clock and the sleeper are
 * injected and driven by hand: waiting is asserted by where the fake clock ended
 * up, not by how long the test took.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { createServerProfile } from "../src/profile.ts";
import type { FetchLike, ProbeTarget } from "../src/health.ts";

const fixtureBody = readFileSync(new URL("./fixtures/health-halogen.json", import.meta.url), "utf8");

function jsonWith(overrides: Record<string, unknown>): string {
  return JSON.stringify({ ...JSON.parse(fixtureBody), ...overrides });
}

/** A fetch that plays a script, repeating its last entry once the script ends. */
function scriptFetch(script: { status?: number; body?: string; error?: Error }[]): {
  impl: FetchLike;
  callCount: () => number;
} {
  let calls = 0;
  const impl: FetchLike = (_url, _init) => {
    const step = script[Math.min(calls, script.length - 1)]!;
    calls += 1;
    if (step.error !== undefined) return Promise.reject(step.error);
    const status = step.status ?? 200;
    return Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      text: async () => step.body ?? "",
    });
  };
  return { impl, callCount: () => calls };
}

/** A clock the test drives: sleeping moves time, it does not pass it. */
function fakeClock(start = 0): { now: () => number; sleep: (ms: number) => Promise<void>; at: () => number } {
  let t = start;
  return {
    now: () => t,
    at: () => t,
    sleep: async (ms: number) => {
      t += Math.max(0, ms);
    },
  };
}

const URL_OK = "http://box.test:8081/health";
const CONFIGURED: ProbeTarget = { url: URL_OK, source: "configured" };
const PROVIDER: ProbeTarget = {
  url: "http://box.test:8081/health",
  source: "provider",
  from: "http://box.test:8081/v1",
};

test("the profile describes the box before any session exists", async () => {
  const fake = scriptFetch([{ body: fixtureBody }]);
  const profile = await createServerProfile({ target: CONFIGURED, fetchImpl: fake.impl });
  const lines = profile.describe().join("\n");
  assert.match(lines, /server probe: http:\/\/box\.test:8081\/health \(configured endpoint\) \d+ms/);
  assert.match(lines, /server profile: model halogen-2.2-27b-it-q8_0 · window 262144/);
  assert.match(lines, /server capacity: room to start/);
  assert.equal(profile.live(), true);
  assert.deepEqual(profile.blockers(), []);
});

test("a probe named where it came from, so the wrong box cannot pass for the right one", async () => {
  // A probe of the wrong box is worse than no probe: it prints real numbers
  // about the wrong machine. So the line carries the address and the base URL it
  // was derived from, and they can be checked against each other at a glance.
  const profile = await createServerProfile({
    target: PROVIDER,
    fetchImpl: scriptFetch([{ body: fixtureBody }]).impl,
  });
  assert.match(
    profile.describe().join("\n"),
    /server probe: http:\/\/box\.test:8081\/health \(from the provider's baseUrl http:\/\/box\.test:8081\/v1\)/,
  );
});

test("with no health endpoint configured, the profile is a no-op that says why", async () => {
  const profile = await createServerProfile({});
  assert.equal(profile.derived(), null);
  assert.equal(profile.answerRoom(), undefined);
  const model = { id: "x", maxTokens: 16_384, contextWindow: 256_000 } as any;
  assert.equal(profile.patchModel(model), model, "no probe means the declared model passes through");
  assert.equal(await profile.awaitCapacity(), null, "nothing to ask, nothing to wait on");
  assert.match(profile.describe().join("\n"), /no probe target was given/);
});

test("a failed startup probe degrades to the declared config, with the reason printed", async () => {
  const fake = scriptFetch([{ error: new Error("connect ECONNREFUSED") }]);
  const profile = await createServerProfile({ target: CONFIGURED, fetchImpl: fake.impl });
  assert.equal(profile.derived(), null);
  assert.equal(profile.live(), false);
  assert.deepEqual(profile.blockers(), [], "unreachable is not a refusal to run");
  assert.match(profile.describe().join("\n"), /ECONNREFUSED/);
  const model = { id: "x", maxTokens: 16_384, contextWindow: 256_000 } as any;
  assert.equal(profile.patchModel(model), model);
});

test("the answer-room rule survives to the context guard", async () => {
  const profile = await createServerProfile({
    target: CONFIGURED,
    fetchImpl: scriptFetch([{ body: jsonWith({ thinking_answer_room: "max(2048, 20% of max_tokens)" }) }])
      .impl,
  });
  assert.deepEqual(profile.answerRoom(), { floorTokens: 2_048, percentOfMaxTokens: 20 });
});

test("an unreadable answer-room rule leaves the loop's own floor and says so", async () => {
  const profile = await createServerProfile({
    target: CONFIGURED,
    fetchImpl: scriptFetch([{ body: jsonWith({ thinking_answer_room: "roughly fifteen percent" }) }])
      .impl,
  });
  assert.equal(profile.answerRoom(), undefined);
  assert.ok(
    profile.notes().some((note) => /not a shape this loop can read/.test(note)),
    profile.notes().join("\n"),
  );
});

test("a model-name mismatch is reported once, not once per pass", async () => {
  const fake = scriptFetch([{ body: fixtureBody }]);
  const profile = await createServerProfile({ target: CONFIGURED, fetchImpl: fake.impl });
  const model = { id: "not-what-the-box-said", maxTokens: 16_384, contextWindow: 256_000 } as any;
  profile.patchModel(model);
  profile.patchModel(model);
  profile.patchModel(model);
  const described = profile.describe().join("\n");
  const hits = described.match(/not the same model/g) ?? [];
  assert.equal(hits.length, 1, `expected one mismatch line, got ${hits.length}`);
});

test("capacity waits for room instead of charging the wait to the ticket", async () => {
  const clock = fakeClock();
  // Script index 0 is the startup probe; the capacity probes start at index 1.
  const fake = scriptFetch([
    { body: jsonWith({ busy: false, queued: 0, in_flight: 0 }) },
    { body: jsonWith({ busy: true, queued: 1, in_flight: 4 }) },
    { body: jsonWith({ busy: false, queued: 0, in_flight: 0 }) },
  ]);
  const profile = await createServerProfile({
    target: CONFIGURED,
    fetchImpl: fake.impl,
    now: clock.now,
    sleep: clock.sleep,
    waitMs: 60_000,
    pollMs: 1_000,
  });
  const reading = await profile.awaitCapacity();
  assert.equal(reading?.open, true, "it waited and got in");
  assert.equal(
    clock.at(),
    1_000,
    "one poll interval was spent waiting, and it is accounted for here rather than inside the pass budget",
  );
  assert.equal(fake.callCount(), 3, "startup probe, one busy capacity probe, one clear");
});

test("waiting has a deadline, and gives up with the reason rather than forever", async () => {
  const clock = fakeClock();
  const fake = scriptFetch([{ body: jsonWith({ busy: true, queued: 3, in_flight: 4 }) }]);
  const profile = await createServerProfile({
    target: CONFIGURED,
    fetchImpl: fake.impl,
    now: clock.now,
    sleep: clock.sleep,
    waitMs: 1_000,
    pollMs: 250,
  });
  const reading = await profile.awaitCapacity();
  assert.equal(reading?.open, false, "the box never freed, and that is what gets reported");
  assert.match(reading?.why ?? "", /3 request\(s\) already queued/);
  assert.ok(clock.at() >= 1_000 && clock.at() <= 1_300, `waited about the budget, ended at ${clock.at()}`);
  assert.ok(fake.callCount() >= 4, "it polled rather than giving up immediately");
});

test("a box that goes away between passes keeps the last known reading instead of stopping", async () => {
  const clock = fakeClock();
  // Clear at startup, busy on the first capacity probe, then the probe itself
  // dies. The loop must still get a usable answer.
  const fake = scriptFetch([
    { body: jsonWith({ busy: false, queued: 0, in_flight: 0 }) },
    { body: jsonWith({ busy: true, queued: 1, in_flight: 4 }) },
    { error: new Error("socket hang up") },
  ]);
  const profile = await createServerProfile({
    target: CONFIGURED,
    fetchImpl: fake.impl,
    now: clock.now,
    sleep: clock.sleep,
    waitMs: 10_000,
    pollMs: 250,
  });
  const reading = await profile.awaitCapacity();
  assert.equal(reading?.open, false, "the last thing we knew was: busy");
  assert.match(reading?.why ?? "", /1 request\(s\) already queued/);
  assert.equal(profile.live(), false, "and the profile knows the probe is now failing");
});

test("blockers reach the profile so preflight can refuse the run", async () => {
  const profile = await createServerProfile({
    target: CONFIGURED,
    fetchImpl: scriptFetch([{ body: jsonWith({ engine: { responds: false, probe_s: 31 } }) }]).impl,
  });
  const blockers = profile.blockers();
  assert.equal(blockers.length, 1);
  assert.match(blockers[0] ?? "", /engine behind it does not/);
  assert.match(profile.describe().join("\n"), /x the API answers but the engine/);
});
