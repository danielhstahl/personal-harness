/**
 * Tests for `src/autoconfig.ts` — turning the server's report into the loop's
 * config.
 *
 * Two things are being pinned here, and the second matters more than the first.
 *
 * The first is the mapping: this field of the report becomes that field of the
 * model. Those assertions read like a table and are cheap.
 *
 * The second is the *contract*, against a captured payload. `test/fixtures/
 * health-halogen.json` is what a real box said on a real day. When that box is
 * upgraded and renames `thinking_answer_room`, or stops advertising
 * `reasoning_effort`, or starts offering `max_thinking_tokens` only, the pinned
 * tests fail. That failure is not an inconvenience to be regenerated away: it is
 * the sound this module makes when the ground moves under it, which is exactly
 * the sound nobody could hear before the probe existed — a reworded rule simply
 * moving the loop's stop point, unnoticed, weeks later, inside a ticket.
 *
 * The third thing, checked everywhere and stated once: unknown is never zero. A
 * report that says nothing produces no patch and a note, never a small number
 * that makes the guards pass.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  answerRoomFloor,
  applyModelPatch,
  deriveFromHealth,
  deriveThinkingLevelMap,
  formatResolvedProfile,
  parseAnswerRoom,
  pickThinkingBudgetField,
  readCapacity,
  unreachableLevels,
} from "../src/autoconfig.ts";
import { parseServerHealth, type ServerHealth } from "../src/health.ts";
import type { OpenAICompletionsCompat } from "@earendil-works/pi-ai";
import { UNWORKABLE_OUTPUT_TOKENS } from "../src/context.ts";

const fixture = JSON.parse(
  readFileSync(new URL("./fixtures/health-halogen.json", import.meta.url), "utf8"),
) as Record<string, unknown>;

function health(overrides: Record<string, unknown> = {}): ServerHealth {
  return parseServerHealth({ ...fixture, ...overrides });
}

/** A minimal model shaped like what `models.json` produces for this box. */
function declaredModel(overrides: Record<string, unknown> = {}): any {
  return {
    id: "halogen-2.2-27b-it-q8_0",
    api: "openai-completions",
    baseUrl: "http://box.test:8081/v1",
    contextWindow: 256_000,
    maxTokens: 16_384,
    compat: {
      supportsDeveloperRole: false,
      supportsReasoningEffort: false,
      thinkingFormat: "chat-template",
      chatTemplateKwargs: { preserve_thinking: true },
    },
    ...overrides,
  };
}

// ── the room ────────────────────────────────────────────────────────────────

test("the box's own window replaces the one that was claimed", () => {
  const derived = deriveFromHealth(health());
  assert.equal(derived.modelPatch.contextWindow, 262_144);
  assert.equal(derived.modelPatch.maxTokensCap, 65_536);
  assert.ok(
    derived.notes.some((note) => note.includes("context window 262144")),
    "every adoption names the field it came from",
  );
});

test("slots that share one KV pool are called out before anyone parallelises", () => {
  // 4 slots × 262144 would need 1048576 positions; the pool holds 262144. The
  // declared per-slot window is a lie the moment two passes run at once, and
  // nobody discovers that from the numbers alone.
  const derived = deriveFromHealth(health());
  assert.ok(
    derived.warnings.some((line) => /do not parallelise/.test(line) && line.includes("1048576")),
    derived.warnings.join("\n"),
  );
  // Same box, one pass at a time: nothing to warn about once the maths works.
  const oneSlot = deriveFromHealth(health({ slots: 1 }));
  assert.ok(!oneSlot.warnings.some((line) => /do not parallelise/.test(line)));
});

test("a report that says nothing about the room leaves the declared config alone", () => {
  const sparse = parseServerHealth({ status: "ok" });
  const derived = deriveFromHealth(sparse);
  assert.equal(derived.modelPatch.contextWindow, undefined, "absent, not 0");
  assert.equal(derived.modelPatch.maxTokensCap, undefined);
  assert.equal(derived.modelPatch.input, undefined, "vision unknown is not 'text only'");
  assert.ok(derived.notes.some((note) => note.includes("the declared one stands")));
  assert.deepEqual(derived.blockers, []);
});

// ── thinking ────────────────────────────────────────────────────────────────

test("the thinking level the loop asks for actually reaches this wire", () => {
  const derived = deriveFromHealth(health());
  const compat = derived.modelPatch.compat ?? {};
  assert.equal(compat.supportsReasoningEffort, true);
  assert.equal(compat.thinkingFormat, "chat-template");
  // `$var` indirection, not a literal: the value has to follow whatever level the
  // individual pass asked for.
  assert.deepEqual(compat.chatTemplateKwargs, {
    enable_thinking: { $var: "thinking.enabled" },
    reasoning_effort: { $var: "thinking.effort" },
  });
  assert.equal(compat.thinkingTokenBudgetField, "thinking_token_budget");
});

test("every pi level lands on something this box accepts, and the folding is stated", () => {
  const derived = deriveFromHealth(health());
  const map = derived.modelPatch.thinkingLevelMap ?? {};
  assert.equal(map.off, "none");
  for (const level of ["minimal", "low", "medium", "high", "xhigh"]) {
    assert.equal(map[level as keyof typeof map], level, `${level} is advertised as itself`);
  }
  // `max` is a pi level this server does not have. It folds to the top offered.
  assert.equal(map.max, "xhigh");
  assert.ok(
    derived.notes.some((note) => note.includes('no "max" effort') && note.includes("xhigh")),
    derived.notes.join("\n"),
  );
  assert.deepEqual(derived.modelPatch.availableLevels, [
    "off",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
  ]);
  assert.deepEqual(unreachableLevels(derived, ["max", "off"]), []);
});

test("an effort scale with gaps folds upward rather than downward", () => {
  const { map, available } = deriveThinkingLevelMap(["low"]);
  assert.equal(map?.low, "low");
  // Over-thinking a ticket is a slower run; under-thinking it is a worse one.
  assert.equal(map?.minimal, "low");
  assert.equal(map?.max, "low");
  assert.deepEqual(available, ["minimal", "low", "medium", "high", "xhigh", "max"]);
});

test("no offered off-name means thinking cannot be switched off, and that is said", () => {
  const { map } = deriveThinkingLevelMap(["low", "high"]);
  assert.equal(map?.off, undefined);
});

test("names the loop cannot rank are reported rather than silently placed", () => {
  // Placing an unseen name above `xhigh` because it came late in the array is a
  // guess with consequences. It gets a note instead.
  const { map, notes } = deriveThinkingLevelMap(["low", "ultrathink-9000"]);
  assert.equal(map?.low, "low");
  assert.ok(
    notes.some((note) => note.includes("ultrathink-9000") && note.includes("cannot rank")),
    notes.join("\n"),
  );
});

test("a server that does not advertise reasoning_effort makes the loop's knob a no-op, out loud", () => {
  const silent = health({
    supported: ["chat_completions"],
    chat_template_kwargs: ["enable_thinking"],
  });
  const derived = deriveFromHealth(silent);
  assert.equal(derived.modelPatch.availableLevels, undefined);
  assert.equal(derived.modelPatch.compat?.supportsReasoningEffort, undefined);
  const warning = derived.warnings.join("\n");
  assert.match(warning, /cannot reach the wire/);
  assert.match(warning, /LOOP_WORK_THINKING/);
  assert.match(warning, /xhigh/, "the server's own default is named so the gap is concrete");
});

test("a chat-template kwarg that is not honoured is not sent", () => {
  const partial = health({ chat_template_kwargs: ["reasoning_effort"] });
  const derived = deriveFromHealth(partial);
  assert.deepEqual(derived.modelPatch.compat?.chatTemplateKwargs, {
    reasoning_effort: { $var: "thinking.effort" },
  });
  assert.ok(
    derived.warnings.some((line) => line.includes("chat_template_kwargs.enable_thinking")),
    derived.warnings.join("\n"),
  );
});

test("a reasoning-budget field pi cannot name is named, not quietly ignored", () => {
  const onlyAlien = health({
    supported: ["reasoning_effort", "max_thinking_tokens"],
    thinking_control_aliases: ["max_thinking_tokens"],
  });
  const picked = pickThinkingBudgetField(onlyAlien);
  assert.equal(picked.field, undefined);
  assert.match(picked.notes.join("\n"), /cannot be capped from the client/);

  const onePiKnows = health({
    supported: ["reasoning_effort"],
    thinking_control_aliases: ["thinking_budget"],
  });
  assert.equal(pickThinkingBudgetField(onePiKnows).field, "thinking_budget");
});

// ── answer room ─────────────────────────────────────────────────────────────

test("the answer-room rule parses, and rewording it fails the parse rather than passing quietly", () => {
  assert.deepEqual(parseAnswerRoom("max(1024, 15% of max_tokens)"), {
    floorTokens: 1024,
    percentOfMaxTokens: 15,
  });
  assert.deepEqual(parseAnswerRoom("2048"), { floorTokens: 2048, percentOfMaxTokens: 0 });
  // Same meaning, different words, and this refuses it. That is deliberate: the
  // alternative is a default that silently moved the loop's stop point.
  assert.equal(parseAnswerRoom("at least 1024 tokens, plus 15%"), null);
  assert.equal(parseAnswerRoom(undefined), null);
});

test("the floor is the largest of the server's terms and this loop's own", () => {
  // 15% of the 16384 this box grants is 2457.6, which rounds up to 2458 — and
  // that is above UNWORKABLE_OUTPUT_TOKENS, so the server's rule binds.
  assert.equal(answerRoomFloor({ floorTokens: 1024, percentOfMaxTokens: 15 }, 16_384), 2_458);
  // A grant so small that neither term reaches this loop's own floor: ours binds.
  assert.equal(
    answerRoomFloor({ floorTokens: 1024, percentOfMaxTokens: 15 }, 1_000),
    UNWORKABLE_OUTPUT_TOKENS,
  );
  // The floor is never below UNWORKABLE_OUTPUT_TOKENS whatever the server says.
  assert.equal(answerRoomFloor({ floorTokens: 1, percentOfMaxTokens: 0 }, 4_096), 2_048);
});

// ── capacity ────────────────────────────────────────────────────────────────

test("a queued request is not room, and the reason is printed", () => {
  const reading = readCapacity(health({ busy: true, queued: 2, in_flight: 4 }));
  assert.equal(reading.open, false);
  assert.match(reading.why, /2 request\(s\) already queued/);
  assert.match(reading.why, /4 in flight against 4 slot/);
});

test("room to join a busy box is stated as such", () => {
  const reading = readCapacity(health({ busy: true, busy_for_s: 12, in_flight: 1, queued: 0 }));
  assert.equal(reading.open, true);
  assert.match(reading.why, /room to join/);
  assert.match(reading.why, /busy for 12s/);
});

test("a report with no load fields reads as open, and admits it is guessing", () => {
  const reading = readCapacity(parseServerHealth({ status: "ok", context: 4096 }));
  assert.equal(reading.open, true);
  assert.equal(reading.unknown, true);
  assert.match(reading.why, /says nothing about load/);
});

// ── blockers ────────────────────────────────────────────────────────────────

test("an API that answers while its engine does not is a reason not to start", () => {
  const derived = deriveFromHealth(health({ engine: { responds: false, probe_s: 30 } }));
  assert.equal(derived.blockers.length, 1);
  assert.match(derived.blockers[0] ?? "", /engine behind it does not/);
});

test("version skew and a degraded status are warnings, not a stopped run", () => {
  const derived = deriveFromHealth(
    health({ status: "degraded", version: { api: "1.4.0", engine: "1.3.9", match: false } }),
  );
  assert.deepEqual(derived.blockers, []);
  const warnings = derived.warnings.join("\n");
  assert.match(warnings, /different versions/);
  assert.match(warnings, /status: degraded/);
});

// ── applying it ─────────────────────────────────────────────────────────────

test("patching returns a new model and keeps the operator's choices", () => {
  const model = declaredModel();
  const before = structuredClone(model);
  const patched = applyModelPatch(model, deriveFromHealth(health(), {
    modelId: model.id,
    contextWindow: model.contextWindow,
    maxTokens: model.maxTokens,
  }));

  assert.deepEqual(model, before, "the input model is untouched");
  assert.equal(patched.contextWindow, 262_144, "the box's window wins");
  assert.equal(patched.maxTokens, 16_384, "inside the cap, the declared budget stands");
  assert.deepEqual(patched.input, ["text"]);
  // `Model.compat` is a union across API families; this box is openai-completions.
  const compat = (patched.compat ?? {}) as OpenAICompletionsCompat;
  assert.equal(compat.supportsReasoningEffort, true, "a wire fact overrides the claim");
  // The one thing that must never be lost: an operator's deliberate replay choice.
  assert.deepEqual(compat.chatTemplateKwargs, {
    enable_thinking: { $var: "thinking.enabled" },
    reasoning_effort: { $var: "thinking.effort" },
    preserve_thinking: true,
  });
});

test("a declared budget over the box's cap is clamped, and complains about it", () => {
  const model = declaredModel({ maxTokens: 99_999 });
  const derived = deriveFromHealth(health(), {
    modelId: model.id,
    contextWindow: model.contextWindow,
    maxTokens: model.maxTokens,
  });
  assert.ok(
    derived.warnings.some((line) => /over the server's cap/.test(line) && /65536/.test(line)),
    derived.warnings.join("\n"),
  );
  assert.equal(applyModelPatch(model, derived).maxTokens, 65_536);
});

test("a hand-mapped level outranks the derived one", () => {
  const model = declaredModel({ thinkingLevelMap: { max: "medium" } });
  const patched = applyModelPatch(model, deriveFromHealth(health()));
  assert.equal(patched.thinkingLevelMap?.max, "medium");
  assert.equal(patched.thinkingLevelMap?.high, "high", " unmapped levels still come from the derive");
});

test("a report about a different model than the one configured is refused loudly", () => {
  const derived = deriveFromHealth(health(), { modelId: "some-other-model" });
  const warning = derived.warnings.join("\n");
  assert.match(warning, /configured model is "some-other-model"/);
  assert.match(warning, /box reports "halogen-2.2-27b-it-q8_0"/);
  assert.deepEqual(derived.blockers, [], "a naming mismatch warns; it does not stop the run");
});

// ── the startup line, and the whole pinned contract ──────────────────────────

test("the startup print names every adopted value", () => {
  const lines = formatResolvedProfile(deriveFromHealth(health())).join("\n");
  assert.match(lines, /window 262144/);
  assert.match(lines, /out cap 65536/);
  assert.match(lines, /input text/);
  assert.match(lines, /levels off\/minimal\/low\/medium\/high\/xhigh\/max/);
  assert.match(lines, /thinking as chat-template/);
  assert.match(lines, /budget via thinking_token_budget/);
  assert.match(lines, /answer room: max\(1024, 15% of granted\)/);
  assert.match(lines, /server capacity: room to start/);
});

/**
 * The drift pin. The whole derived shape for the captured payload, in one
 * assertion.
 *
 * If the server changes *anything* this module reads, this fails. Read the diff,
 * decide what the new contract means for the loop, update the fixture and this
 * expectation together. Regenerating the fixture and deleting the assertion is
 * how a renamed field becomes an unplanned behaviour change.
 */
test("DRIFT: the derived contract for the captured payload is exactly this", () => {
  const derived = deriveFromHealth(health());
  assert.deepEqual(
    {
      contextWindow: derived.modelPatch.contextWindow,
      maxTokensCap: derived.modelPatch.maxTokensCap,
      input: derived.modelPatch.input,
      availableLevels: derived.modelPatch.availableLevels,
      thinkingLevelMap: derived.modelPatch.thinkingLevelMap,
      compat: derived.modelPatch.compat,
      answerRoom: derived.answerRoom,
      answerRoomText: derived.answerRoomText,
      capacityOpen: derived.capacity.open,
      blockers: derived.blockers,
    },
    {
      contextWindow: 262_144,
      maxTokensCap: 65_536,
      input: ["text"],
      availableLevels: ["off", "minimal", "low", "medium", "high", "xhigh", "max"],
      thinkingLevelMap: {
        off: "none",
        minimal: "minimal",
        low: "low",
        medium: "medium",
        high: "high",
        xhigh: "xhigh",
        max: "xhigh",
      },
      compat: {
        supportsReasoningEffort: true,
        thinkingFormat: "chat-template",
        chatTemplateKwargs: {
          enable_thinking: { $var: "thinking.enabled" },
          reasoning_effort: { $var: "thinking.effort" },
        },
        thinkingTokenBudgetField: "thinking_token_budget",
      },
      answerRoom: { floorTokens: 1_024, percentOfMaxTokens: 15 },
      answerRoomText: "max(1024, 15% of granted)",
      capacityOpen: true,
      blockers: [],
    },
    `derived changed:\n${JSON.stringify(derived, null, 2)}`,
  );
});

/**
 * The other half of the drift guard: every field named in the code's read list
 * must exist in the fixture. This catches a rename in the *code* — the case
 * where someone fixes a typo here and leaves the fixture describing a field
 * nobody reads, which would otherwise pass every test while the derive read
 * `undefined`.
 */
test("DRIFT: every health field this module reads exists in the capture", () => {
  const READ_FIELDS = [
    "status",
    "model",
    "context",
    "slot_ctx",
    "kv_pool_positions",
    "max_tokens_cap",
    "reasoning_effort_values",
    "reasoning_effort_default",
    "thinking_answer_room",
    "thinking_control_aliases",
    "chat_template_kwargs",
    "supported",
    "accepted_but_ignored",
    "tool_calls.forced_call_disables_thinking",
    "engine.responds",
    "version.match",
    "version.api",
    "version.engine",
    "busy",
    "slots",
    "queued",
    "in_flight",
    "busy_for_s",
    "vision.enabled",
  ] as const;
  const read = (path: string): unknown =>
    path.split(".").reduce<unknown>((value, key) => {
      if (value === null || typeof value !== "object") return undefined;
      return (value as Record<string, unknown>)[key];
    }, fixture);
  for (const path of READ_FIELDS) {
    assert.notEqual(read(path), undefined, `the capture does not carry ${path}`);
  }
});
