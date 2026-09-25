/**
 * The server profile: the loop's standing relationship with one model box.
 *
 * `src/health.ts` fetches a report, `src/autoconfig.ts` decides what it means.
 * This is the small stateful holder that makes that usable across a run: probe
 * once at startup, carry the decision, re-check capacity before each pass.
 *
 ## Why a holder instead of a call
 *
 * The derived limits are needed in three places that do not otherwise talk to
 * each other — the session factory (patch the model), the context guard (the
 * floor under "worth continuing"), and the interpreter (is the box free before
 * I start a pass whose clock I am about to arm). Threading a `ServerHealth`
 * through all three would spread the decision across the codebase. One object,
 * built in the composition root, keeps the decision in one place and lets each
 * consumer ask the question it has.
 *
 ## What it must never do
 *
 * Stop work because it could not reach `/health`. The probe is an improvement on
 * the declared config, not a dependency of it: every method degrades to "the
 * declared config stands", and says so. A loop that halts because a status
 * endpoint is down has made the tail wag the dog.
 *
 ## Why capacity is read *before* the pass, not during
 *
 * A request that sits in the server's queue is not work. If the pass clock is
 * already running, queue time is charged against the ticket's budget, and a
 * timeout note that says "timed out at 20:00 of a 20:00 budget" describes a
 * wait rather than a slow model — the single most misleading thing this loop can
 * report. Waiting first, and reporting the wait, keeps the number honest.
 */
import type { Model } from "@earendil-works/pi-ai";

import type { AnswerRoomRule, CapacityReading, DerivedConfig } from "./autoconfig.ts";
import {
  applyModelPatch,
  deriveFromHealth,
  formatResolvedProfile,
  readCapacity,
} from "./autoconfig.ts";
import { probeServerHealth, type FetchLike, type ProbeTarget, type ServerHealth } from "./health.ts";

/** What `main.ts` hands to the runner and the loop. */
export interface ServerProfile {
  /**
   * The startup decision — facts about the box, derived before any model was
   * resolved. Null when there is no report, which every consumer reads as "the
   * declared config stands".
   */
  derived(): DerivedConfig | null;
  /** The live report, or null when there is none. */
  health(): ServerHealth | null;
  /**
   * The decision re-derived against the model the run actually uses.
   *
   * The profile is built before the model is known — pi's settings can override
   * what the environment said — so the comparison against the declared limits
   * (is the budget inside the cap, is this even the same model) can only happen
   * here, where both sides are in hand. Pure, and cheap enough to call per pass.
   */
  resolveFor(model: Model<any> | undefined): DerivedConfig | null;
  /**
   * Apply the derived limits to a resolved model. Identity with no report: a
   * model that was never probed keeps exactly the numbers its operator wrote.
   */
  patchModel(model: Model<any>): Model<any>;
  /**
   * The server's answer-room rule, for the context guard. Undefined means "no
   * rule was readable", which the guard reads as its own constant.
   */
  answerRoom(): AnswerRoomRule | undefined;
  /**
   * Is the box free? Waits up to the configured window for room, then returns
   * whatever was last seen — never null when a probe ran, never a blocker.
   */
  awaitCapacity(): Promise<CapacityReading | null>;
  /** Lines for the startup surface: what was adopted, what was clamped, what is blocked. */
  describe(): readonly string[];
  /**
   * The full adoption trail — every value taken, with the report field it came
   * from. Longer than {@link ServerProfile.describe}, and worth its length when
   * something is wrong: this is the list that answers "why is the loop running
   * with that number?" without reading a line of code.
   */
  notes(): readonly string[];
  /** Reasons not to run at all. The loop refuses on these; nothing else does. */
  blockers(): readonly string[];
  /** True when a usable report is in hand. */
  live(): boolean;
}

export interface ServerProfileOptions {
  /**
   * What to probe, and where that address came from. Absent, or a target that
   * resolved to nothing, turns the profile into a no-op that prints the reason.
   *
   * The target is normally produced by {@link resolveProbeTarget} from the
   * provider's own `baseUrl` — not from a second URL that has to be kept in step
   * with the first by hand.
   */
  readonly target?: ProbeTarget;
  /** Per-probe timeout. Default 5s. */
  readonly timeoutMs?: number;
  /**
   * How long {@link ServerProfile.awaitCapacity} may wait for room before
   * starting anyway. Default 90s. `0` observes without waiting.
   */
  readonly waitMs?: number;
  /** Poll interval while waiting. Default 3s. */
  readonly pollMs?: number;
  readonly fetchImpl?: FetchLike;
  readonly now?: () => number;
  readonly sleep?: (ms: number) => Promise<void>;
}

const DEFAULT_WAIT_MS = 90_000;
const DEFAULT_POLL_MS = 3_000;

function defaultSleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    timer.unref?.();
  });
}

/**
 * Normalise the two arms of a {@link ProbeTarget} into the two things this
 * module actually needs: an address to GET, or one sentence saying why there is
 * none. Done once, here, so no later branch has to re-narrow the union.
 */
function describeMissingTarget(target: ProbeTarget | undefined): string {
  if (target === undefined) {
    return "no probe target was given; the declared model config stands";
  }
  return target.url === null ? target.reason : "the probe target was unusable";
}

function targetAddress(target: ProbeTarget | undefined): { url: string; whence: string } | null {
  if (target === undefined || target.url === null) return null;
  return {
    url: target.url,
    whence: target.source === "provider" ? `from the provider's baseUrl ${target.from}` : "configured endpoint",
  };
}

/**
 * Build the profile and take the startup reading.
 *
 * The startup probe happens here rather than lazily because the answer belongs in
 * the first thing the operator sees: which model this is, how big the room is,
 * whether the thinking knob is wired at all. A failure is a printed line, not an
 * exception.
 */
export async function createServerProfile(
  options: ServerProfileOptions = {},
): Promise<ServerProfile> {
  const now = options.now ?? Date.now;
  const sleep = options.sleep ?? defaultSleep;
  const waitMs = Math.max(0, options.waitMs ?? DEFAULT_WAIT_MS);
  const pollMs = Math.max(250, options.pollMs ?? DEFAULT_POLL_MS);
  const address = targetAddress(options.target);

  const lines: string[] = [];
  const allNotes: string[] = [];
  let derived: DerivedConfig | null = null;
  let health: ServerHealth | null = null;
  let lastCapacity: CapacityReading | null = null;
  let probeFailed = false;

  const probe = async (): Promise<ServerHealth | null> => {
    if (address === null) return null;
    const result = await probeServerHealth({
      url: address.url,
      timeoutMs: options.timeoutMs,
      fetchImpl: options.fetchImpl,
      now,
    });
    if (result.ok) {
      health = result.health;
      probeFailed = false;
      return result.health;
    }
    probeFailed = true;
    lines.push(`server probe: ${result.error.message}`);
    return null;
  };

  if (address === null) {
    // Nothing to ask. Said once, in the target's own words, and the profile
    // stays a no-op for the whole run.
    lines.push(`server probe: ${describeMissingTarget(options.target)}`);
  }

  const started = now();
  const report = address === null ? null : await probe();
  const tookMs = Math.max(0, now() - started);
  if (report !== null && address !== null) {
    // Naming the address and where it came from: a probe of the wrong box is
    // worse than no probe, so the line has to be checkable at a glance.
    lines.push(`server probe: ${address.url} (${address.whence}) ${tookMs}ms`);
    derived = deriveFromHealth(report);
  }
  // Warnings and blockers belong in the first thing the operator reads. Printing
  // them once is enough: a "thinking cannot reach the wire" line repeated per
  // ticket is noise, so per-model extras are diffed against this set.
  const reportedWarnings = new Set<string>(derived?.warnings ?? []);
  allNotes.push(...(derived?.notes ?? []));
  for (const warning of derived?.warnings ?? []) lines.push(`  ! ${warning}`);
  for (const blocker of derived?.blockers ?? []) lines.push(`  x ${blocker}`);

  return {
    derived: () => derived,
    health: () => health,
    resolveFor(model) {
      if (health === null) return null;
      return deriveFromHealth(health, {
        modelId: model?.id,
        contextWindow: model?.contextWindow,
        maxTokens: model?.maxTokens,
      });
    },
    patchModel(model) {
      if (health === null) return model;
      const resolved = deriveFromHealth(health, {
        modelId: model.id,
        contextWindow: model.contextWindow,
        maxTokens: model.maxTokens,
      });
      for (const warning of resolved.warnings) {
        if (reportedWarnings.has(warning)) continue;
        reportedWarnings.add(warning);
        lines.push(`  ! ${warning}`);
      }
      for (const note of resolved.notes) {
        if (allNotes.includes(note)) continue;
        allNotes.push(note);
      }
      return applyModelPatch(model, resolved);
    },
    answerRoom() {
      return derived?.answerRoom;
    },
    async awaitCapacity() {
      if (address === null) return null;
      const deadline = now() + waitMs;
      // eslint-disable-next-line no-constant-condition
      for (;;) {
        const fresh = await probe();
        const reading = fresh !== null ? readCapacity(fresh) : lastCapacity;
        if (reading === null) return null;
        lastCapacity = reading;
        if (reading.open) return reading;
        if (now() >= deadline) {
          // Starting anyway is the right call: the alternative is an unbounded
          // wait on a box that may never free, and the operator is watching.
          return reading;
        }
        await sleep(Math.min(pollMs, Math.max(0, deadline - now())));
      }
    },
    describe() {
      if (derived !== null) {
        return [...lines, ...formatResolvedProfile(derived)];
      }
      return lines;
    },
    notes() {
      return allNotes;
    },
    blockers() {
      return derived?.blockers ?? [];
    },
    live() {
      return health !== null && !probeFailed;
    },
  };
}
