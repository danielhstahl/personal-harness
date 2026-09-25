/**
 * Context accounting for a run in flight, in the provider's own units.
 *
 * Why this exists: the run budget is a wall clock. It can tell that a ticket ran
 * out of *time*; it cannot tell that a ticket ran out of *room* — and those two
 * want opposite responses. A ticket that is merely slow deserves its budget. A
 * ticket that has eaten the declared context window does not: pi clamps each
 * request's `max_completion_tokens` to whatever the window still has
 * (`min(cap, window - context - safety)`), so near the wall a turn can be
 * granted a single token of answer, and the turn after that may not fit at all.
 * Spending twenty minutes discovering that is the worst outcome available, and
 * it is indistinguishable from ordinary slowness in the logs.
 *
 * The provider reports how big every request it served actually was
 * (`usage.input + usage.cacheRead`), so the wall can be seen coming from the
 * same numbers the bill is cut from, rather than guessed at from character
 * counts.
 */

/**
 * The reserve pi keeps between the last context token and the end of the window.
 * Mirrors `CONTEXT_SAFETY_TOKENS` in
 * `@earendil-works/pi-ai/dist/api/simple-options.js`. The drift test pins it:
 * a silent change upstream moves the wall this module measures to.
 */
export const CONTEXT_SAFETY_TOKENS = 4_096;

/**
 * An answer budget too small to finish a ticket with. A structured verdict costs
 * a few hundred tokens; a turn that edits a file needs room to say what it is
 * changing. Below this the run is not slow, it is finished — stop and say so
 * instead of collecting another eleven minutes of it.
 */
export const UNWORKABLE_OUTPUT_TOKENS = 2_048;

export interface ContextBudgetInput {
  /** The model's declared context window. */
  readonly windowTokens: number;
  /** The model's own output cap (`maxTokens`). */
  readonly maxOutputTokens: number;
  /** What the last request actually cost: `usage.input + usage.cacheRead`. */
  readonly usedTokens: number;
  readonly safetyTokens?: number;
  readonly unworkableOutputTokens?: number;
}

export interface ContextBudget {
  readonly windowTokens: number;
  readonly usedTokens: number;
  /** Window minus what the last request used. Can go negative: already over. */
  readonly headroomTokens: number;
  /** What the next request would be granted as its answer budget. */
  readonly grantedOutputTokens: number;
  readonly percentUsed: number;
  readonly exhausted: boolean;
}

/**
 * What the next request looks like, given what the last one cost.
 *
 * An undeclared window (`<= 0`) is not a tiny one: there is nothing to be
 * exhausted against, so it is never exhausted rather than being always over.
 */
export function measureContextBudget(input: ContextBudgetInput): ContextBudget {
  const safety = input.safetyTokens ?? CONTEXT_SAFETY_TOKENS;
  const floor = input.unworkableOutputTokens ?? UNWORKABLE_OUTPUT_TOKENS;
  const { windowTokens, maxOutputTokens, usedTokens } = input;

  if (!Number.isFinite(windowTokens) || windowTokens <= 0) {
    return {
      windowTokens,
      usedTokens,
      headroomTokens: windowTokens - usedTokens,
      grantedOutputTokens: maxOutputTokens,
      percentUsed: 0,
      exhausted: false,
    };
  }

  const headroomTokens = windowTokens - usedTokens;
  const grantedOutputTokens = Math.min(
    maxOutputTokens,
    Math.max(1, headroomTokens - safety),
  );
  return {
    windowTokens,
    usedTokens,
    headroomTokens,
    grantedOutputTokens,
    percentUsed: Math.max(0, Math.round((usedTokens / windowTokens) * 100)),
    exhausted: grantedOutputTokens <= floor,
  };
}

/** 1_234_567 → "1234.6K"; 950 → "950". Enough for one line of terminal. */
export function formatTokenCount(tokens: number): string {
  if (!Number.isFinite(tokens)) return "?";
  if (Math.abs(tokens) >= 1_000) return `${(tokens / 1_000).toFixed(1)}K`;
  return String(Math.round(tokens));
}

/** One line that says where the run is, for the failure note and the surface. */
export function describeContextBudget(budget: ContextBudget): string {
  return (
    `${formatTokenCount(budget.usedTokens)}/${formatTokenCount(budget.windowTokens)} ` +
    `tokens used (${budget.percentUsed}%), ` +
    `${formatTokenCount(budget.grantedOutputTokens)} of answer budget left ` +
    `for the next request`
  );
}
