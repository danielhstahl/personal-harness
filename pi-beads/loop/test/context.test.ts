/**
 * Tests for `src/context.ts` — reading the context wall before hitting it.
 *
 * The numbers here are the subject, not the scenery: what the loop decides to do
 * with a run is decided by these six lines of arithmetic, so the boundary is
 * pinned at the exact token rather than "roughly a big number". The last test
 * pins the one number this module borrowed from pi, because a silent change to it
 * upstream moves the wall this measures to.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  CONTEXT_SAFETY_TOKENS,
  describeContextBudget,
  formatTokenCount,
  measureContextBudget,
  UNWORKABLE_OUTPUT_TOKENS,
} from "../src/context.ts";

const WINDOW = 128_000;
const CAP = 16_384;

function at(usedTokens: number, extra: object = {}) {
  return measureContextBudget({
    windowTokens: WINDOW,
    maxOutputTokens: CAP,
    usedTokens,
    ...extra,
  });
}

test("with room to spare the grant is the model's own output cap", () => {
  const budget = at(40_000);
  assert.equal(budget.grantedOutputTokens, CAP);
  assert.equal(budget.exhausted, false);
  assert.equal(budget.headroomTokens, 88_000);
});

test("the cap stops binding exactly where pi's clamp starts", () => {
  // The last usage that still gets the full cap, and the first that does not.
  const stillFull = WINDOW - CONTEXT_SAFETY_TOKENS - CAP;
  assert.equal(at(stillFull).grantedOutputTokens, CAP);
  assert.equal(at(stillFull + 1).grantedOutputTokens, CAP - 1);
});

test("the grant shrinks by exactly what the last request took", () => {
  for (const used of [110_000, 115_000, 120_000]) {
    assert.equal(
      at(used).grantedOutputTokens,
      WINDOW - used - CONTEXT_SAFETY_TOKENS,
      `used ${used}`,
    );
  }
});

test("a grant at one token is exhausted", () => {
  const budget = at(WINDOW - CONTEXT_SAFETY_TOKENS - 1);
  assert.equal(budget.grantedOutputTokens, 1);
  assert.equal(budget.exhausted, true, "one token is not an answer");
});

test("the working edge is the floor, inclusive", () => {
  // Exactly at the floor: unworkable. One token above it: still workable.
  assert.equal(at(WINDOW - CONTEXT_SAFETY_TOKENS - UNWORKABLE_OUTPUT_TOKENS).exhausted, true);
  assert.equal(
    at(WINDOW - CONTEXT_SAFETY_TOKENS - UNWORKABLE_OUTPUT_TOKENS - 1).exhausted,
    false,
  );
});

test("past the wall the headroom is negative and it is exhausted", () => {
  const budget = at(160_000);
  assert.equal(budget.headroomTokens, -32_000);
  assert.equal(budget.grantedOutputTokens, 1);
  assert.equal(budget.exhausted, true);
  assert.equal(budget.percentUsed, 125, "overdrawn reads as over 100%");
});

test("an undeclared window is never exhausted, because nothing was declared", () => {
  for (const windowTokens of [0, -1, Number.NaN]) {
    const budget = measureContextBudget({
      windowTokens,
      maxOutputTokens: CAP,
      usedTokens: 999_999,
    });
    assert.equal(budget.exhausted, false, `window ${windowTokens}`);
    assert.equal(budget.grantedOutputTokens, CAP);
  }
});

test("both thresholds are the caller's to move", () => {
  const generous = at(WINDOW - 100, { safetyTokens: 0, unworkableOutputTokens: 1 });
  assert.equal(generous.grantedOutputTokens, 100);
  assert.equal(generous.exhausted, false);

  const strict = at(0, { unworkableOutputTokens: 16_385 });
  assert.equal(strict.exhausted, true, "a floor above the cap stops the run at once");
});

test("percent used is rounded and never negative", () => {
  assert.equal(at(64_000).percentUsed, 50);
  assert.equal(at(WINDOW * 2).percentUsed, 200);
  assert.equal(at(-1_000).percentUsed, 0);
});

test("token counts read as thousands and small ones read as themselves", () => {
  assert.equal(formatTokenCount(128_000), "128.0K");
  assert.equal(formatTokenCount(1_234), "1.2K");
  assert.equal(formatTokenCount(950), "950");
  assert.equal(formatTokenCount(0), "0");
  assert.equal(formatTokenCount(Number.NaN), "?");
});

test("the one-line description carries used, window and grant", () => {
  const line = describeContextBudget(at(WINDOW - 512));
  assert.match(line, /128\.0K/);
  assert.match(line, /100%/u);
  assert.match(line, /1 of answer budget left/u);
});

test("the borrowed safety constant still matches pi's (drift guard)", () => {
  const source = readFileSync(
    fileURLToPath(import.meta.resolve("@earendil-works/pi-ai/api/simple-options")),
    "utf8",
  );
  const declared = source.match(/const CONTEXT_SAFETY_TOKENS = (\d+);/u);
  assert.ok(declared, "pi's safety constant was not found where this module says it lives");
  assert.equal(
    CONTEXT_SAFETY_TOKENS,
    Number(declared[1]),
    "CONTEXT_SAFETY_TOKENS no longer mirrors pi's; the wall this module measures moved",
  );
});
