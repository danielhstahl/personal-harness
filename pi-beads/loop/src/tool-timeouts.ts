/**
 * The per-tool-call timeout — [ADR-010](../docs/ADR-010-tool-call-timeout.md).
 *
 * One module owns it, the way `src/gitlock.ts` owns spawn-and-kill: everything
 * that caps, kills or reports a wedged tool call comes through here, so there is
 * exactly one place to read when a timeout needs changing.
 *
 * The problem this solves is narrower than "a run was slow" and wider than
 * "bash took too long": a tool call that never returns holds the *whole* run.
 * The run budget (`LOOP_WORK_TIMEOUT_MS`) does bound it, but it pays for one
 * wedged child with every model turn already spent, because its only lever over
 * a live session is `abort()`. Capping the call instead of the run keeps the
 * iteration: the model gets a result that says *this one call was killed*, and
 * goes on to do something else with its remaining budget.
 *
 * Three properties, each of which the shape below exists to keep:
 *
 * - **The wrapper returns; it does not throw.** `AgentToolResult` has no
 *   `isError` field (ADR-010 §5): the error flag comes from throwing, and the
 *   throw path *discards* `details`. `details.timedOut` is the one fact that
 *   separates "the harness capped this call" from "the tool failed by itself",
 *   so the result is returned and the renderer is taught to read it (see
 *   {@link isTimedOutToolResult}).
 * - **The wrapper owns the `AbortController`.** pi kills the whole process tree
 *   of a shell child whenever the signal it was handed fires
 *   (`core/tools/bash.js`: `onAbort → killProcessTree`), whoever owns that
 *   signal. Substituting the wrapper's controller is therefore enough to kill
 *   the hung child — and *forwarding* the inbound signal is what keeps
 *   `session.abort()` / Ctrl-C working after the substitution, because the
 *   session's chain now ends at this wrapper instead of at the child.
 * - **The race is not an await.** A wrapper that only awaits the inner call is
 *   as cooperative as the tool it wraps. `Promise.race` with the timer is what
 *   makes a tool that ignores abort irrelevant to the run's clock. The residual
 *   is honest: an uncooperative *in-process* tool keeps running, because JS has
 *   no preemption. What the wrapper buys is that it can no longer hold the run,
 *   plus the tree kill for everything that spawned a process.
 *
 * Nothing here reads the environment. `LOOP_TOOL_TIMEOUT_MS` is read in
 * `src/main.ts` — the one place this harness reads config — and arrives as an
 * argument.
 */
import type {
  AgentToolResult,
  AgentToolUpdateCallback,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";

/**
 * A tool definition with its generics opened. This module never inspects params,
 * details or render state — it only re-wraps `execute` and spreads everything
 * else through untouched, which is what keeps the prompt, the label, the
 * parameter schema and the renderers exactly as the wrapped tool wrote them.
 */
export type AnyToolDefinition = ToolDefinition<any, any, any>;

/** What a timeout knows about the call it killed. */
export interface ToolTimeoutInfo {
  /** The tool whose `execute` did not come back. */
  readonly toolName: string;
  /** The cap that was applied to this call. */
  readonly timeoutMs: number;
  /** How long the call had actually been running when it was cut off. */
  readonly elapsedMs: number;
  /** Whatever the tool had streamed through `onUpdate` before the kill. */
  readonly partialOutput: string;
}

/**
 * The timeout branch's own error: raised by the timer, caught by the wrapper.
 *
 * It is an error rather than a flag because that is how it travels out of the
 * race, and it carries a model-visible message because a message that would be
 * useful to a human is equally useful to the model reading the transcript
 * three turns later. The message never claims the *tool* is broken: it says the
 * call was killed and did not run to completion, which is the whole truth and
 * the thing that must not be confused with an ordinary tool failure.
 */
export class ToolTimeoutError extends Error {
  /** Always `true`, so a timeout is greppable in an error-shaped value too. */
  readonly timedOut = true;
  readonly toolName: string;
  readonly timeoutMs: number;
  readonly elapsedMs: number;
  readonly partialOutput: string;

  constructor(
    toolName: string,
    timeoutMs: number,
    elapsedMs: number,
    partialOutput = "",
  ) {
    super(
      `tool "${toolName}" timed out after ${timeoutMs}ms and was killed — ` +
        "it did not run to completion",
    );
    this.name = "ToolTimeoutError";
    this.toolName = toolName;
    this.timeoutMs = timeoutMs;
    this.elapsedMs = elapsedMs;
    this.partialOutput = partialOutput;
  }

  static is(error: unknown): error is ToolTimeoutError {
    return (
      error instanceof ToolTimeoutError ||
      (typeof error === "object" &&
        error !== null &&
        (error as { name?: string }).name === "ToolTimeoutError")
    );
  }
}

/** `isAgentError`'s counterpart, for code that only has an unknown value. */
export function isToolTimeoutError(error: unknown): error is ToolTimeoutError {
  return ToolTimeoutError.is(error);
}

/** The `details` a timed-out call returns. */
export interface ToolTimeoutDetails {
  readonly timedOut: true;
  /** ADR-010 §5's name for the tool; kept because the transcript uses it. */
  readonly tool: string;
  readonly toolName: string;
  readonly timeoutMs: number;
  readonly elapsedMs: number;
  readonly partialChars: number;
}

/**
 * The result handed back to the model in place of the call that never finished.
 *
 * The partial output is the debuggable half of a kill: bash streams snapshots
 * through `onUpdate`, so a command that printed three pages before wedging on a
 * `docker push` says what it got to. Its limits are the honest ones — the tee
 * only sees what the tool pushed (for bash, throttled to ~100 ms), and the
 * final flush a tool does on a clean return never happens here, because the
 * kill arrives first. A partial tail beats a blank one.
 */
export function timedOutToolResult(info: ToolTimeoutInfo): AgentToolResult<ToolTimeoutDetails> {
  const partial = info.partialOutput;
  const text =
    `tool "${info.toolName}" timed out after ${info.timeoutMs}ms and was killed — ` +
    "it did not run to completion. " +
    (partial === ""
      ? "(no output had been streamed before the kill) "
      : `Partial output before the kill:\n${partial}\n`) +
    "Anything it had started was terminated with its process tree. Nothing is " +
    "known to be half-written; re-run the piece you need as a smaller command, " +
    "or pass an explicit `timeout` if it is expected to be slow.";
  return {
    content: [{ type: "text", text }],
    details: {
      timedOut: true,
      tool: info.toolName,
      toolName: info.toolName,
      timeoutMs: info.timeoutMs,
      elapsedMs: info.elapsedMs,
      partialChars: partial.length,
    },
  };
}

/**
 * "Did this tool result come from a killed call?" — the one definition.
 *
 * Needed outside the wrapper because the wrapper *returns* rather than throws
 * (ADR-010 §5), so the transcript's `isError` stays `false` and a renderer
 * that derives status from that flag alone would paint a killed call `✓ ok`.
 * Deliberately structural rather than `instanceof`: it has to work on a
 * transcript read back, where there is no class left to compare against.
 */
export function isTimedOutToolResult(result: unknown): boolean {
  if (typeof result !== "object" || result === null) return false;
  const details = (result as { details?: unknown }).details;
  if (typeof details !== "object" || details === null) return false;
  return (details as { timedOut?: unknown }).timedOut === true;
}

/** A running cap: the promise that fires it, and the way to stop it. */
export interface ToolTimeoutDeadline {
  readonly expired: Promise<void>;
  cancel(): void;
}

export interface ToolTimeoutHooks {
  /** Injectable clock, so elapsed time is asserted rather than slept. */
  readonly now?: () => number;
  /**
   * Injectable scheduler for the cap.
   *
   * Injectable rather than a bare `setTimeout` so a test can drive the timeout
   * off the same fake clock it uses for everything else instead of paying real
   * seconds for it — see the wedged-tool tests in `test/agent.test.ts`.
   */
  readonly deadline?: (timeoutMs: number) => ToolTimeoutDeadline;
  /** Called on the timeout branch, before the timed-out result is returned. */
  readonly onTimeout?: (info: ToolTimeoutInfo) => void;
}

/** Definitions already carrying a cap. First wrap wins; see below. */
const capped = new WeakSet<object>();

/** Has this definition already got a per-call cap? */
export function isToolTimeoutWrapped(tool: unknown): boolean {
  return typeof tool === "object" && tool !== null && capped.has(tool);
}

/** The default cap: a real timer, `unref`'d so it never holds the process open. */
function wallClockDeadline(timeoutMs: number): ToolTimeoutDeadline {
  let handle: NodeJS.Timeout | undefined;
  const expired = new Promise<void>((resolve) => {
    handle = setTimeout(resolve, timeoutMs);
    handle.unref?.();
  });
  return {
    expired,
    cancel: () => {
      if (handle !== undefined) clearTimeout(handle);
    },
  };
}

/**
 * Wrap {@link ToolDefinition.execute} so the call must come back inside
 * `timeoutMs` or is cut off, killed and reported.
 *
 * Everything but `execute` is spread through unchanged — name, label,
 * description, parameter schema, `promptSnippet`, `promptGuidelines`,
 * renderers, `executionMode`. ADR-010 probe P1 measured the system prompt of a
 * session whose four built-ins were shadowed this way as byte-identical to the
 * unshadowed baseline; a wrapper that rebuilt the definition instead would
 * silently delete the tool's line from the prompt.
 *
 * `timeoutMs === undefined` (or anything below 1 ms) returns the definition
 * untouched: no cap is today's behaviour exactly, and the default is off rather
 * than a number chosen here, because no value written in a file knows how long a
 * real `npm ci` takes in the repository it is pointed at.
 *
 * Wrapping is idempotent: a definition that already carries a cap is returned
 * as it is. Two sites wrap (the runner, for the tools handed to every session;
 * the session factory, for the built-ins it shadows), so without this the
 * second site would replace the first one's `onTimeout` sink and the timeout
 * would be reported to whichever module happened to run later.
 */
export function withToolTimeout<T extends ToolDefinition>(
  definition: T,
  timeoutMs: number | undefined,
  hooks: ToolTimeoutHooks = {},
): T {
  if (timeoutMs === undefined || !Number.isFinite(timeoutMs) || timeoutMs < 1) {
    return definition;
  }
  if (isToolTimeoutWrapped(definition)) return definition;

  const now = hooks.now ?? ((): number => Date.now());
  const deadlineFor = hooks.deadline ?? wallClockDeadline;
  // Clamped like every other cap in this harness (`src/health.ts`,
  // `src/monitor.ts`): a sub-millisecond cap is a cap that fires immediately.
  const cap = Math.max(1, Math.round(timeoutMs));

  const execute = async (
    toolCallId: string,
    params: any,
    inbound: AbortSignal | undefined,
    onUpdate: AgentToolUpdateCallback<any> | undefined,
    ctx: ExtensionContext,
  ): Promise<AgentToolResult<unknown>> => {
    const controller = new AbortController();
    const streamed: string[] = [];

    // Tee before forwarding: the model keeps seeing what the tool streams, and
    // the timeout branch keeps a copy of it to show.
    const tee: AgentToolUpdateCallback<any> = (partialResult) => {
      const content = (partialResult as AgentToolResult<unknown> | undefined)?.content;
      if (Array.isArray(content)) {
        for (const part of content) {
          if (
            typeof part === "object" &&
            part !== null &&
            (part as { type?: unknown }).type === "text" &&
            typeof (part as { text?: unknown }).text === "string"
          ) {
            streamed.push((part as { text: string }).text);
          }
        }
      }
      onUpdate?.(partialResult);
    };

    // The substitution above ends the session's abort chain here, so the chain
    // has to be re-joined or Ctrl-C would stop reaching the child altogether.
    const forward = (): void => {
      controller.abort(inbound?.reason ?? new Error("aborted"));
    };
    if (inbound !== undefined) {
      if (inbound.aborted) forward();
      else inbound.addEventListener("abort", forward, { once: true });
    }

    const startedAt = now();
    const inner = (definition as unknown as AnyToolDefinition).execute(
      toolCallId,
      params,
      controller.signal,
      tee,
      ctx,
    );
    // A late rejection from the losing side must not arrive as an unhandled
    // rejection at 04:00 and take the process down for an event nobody is
    // waiting on any more.
    void Promise.resolve(inner).catch(() => undefined);

    const deadline = deadlineFor(cap);
    try {
      // The timer wins by *throwing*, which is what makes `ToolTimeoutError`
      // the thing carried out of the race rather than a flag guessed at
      // afterwards. Anything else that comes out of here is the tool's own
      // failure and goes back out exactly as it came in.
      const raced = await Promise.race([
        Promise.resolve(inner).then((result) => result),
        deadline.expired.then(() => {
          throw new ToolTimeoutError(
            definition.name,
            cap,
            Math.max(0, now() - startedAt),
            streamed.join(""),
          );
        }),
      ]);
      return raced;
    } catch (error) {
      if (!ToolTimeoutError.is(error)) throw error;
      const info: ToolTimeoutInfo = {
        toolName: error.toolName,
        timeoutMs: error.timeoutMs,
        elapsedMs: error.elapsedMs,
        partialOutput: error.partialOutput,
      };
      try {
        hooks.onTimeout?.(info);
      } catch {
        // A listener that throws is a listener's problem. It must not turn a
        // reported timeout into an unreported crash of the run.
      }
      // Returned, not rethrown: see the module header. The run keeps going.
      return timedOutToolResult(info);
    } finally {
      deadline.cancel();
      if (inbound !== undefined) inbound.removeEventListener("abort", forward);
    }
  };

  const wrapper = { ...definition, execute } as unknown as T;
  capped.add(wrapper);
  return wrapper;
}

/**
 * Cap a whole list of definitions, which is what both wrap sites do.
 *
 * With no cap configured this is the identity — no wrapper objects, no
 * allocation, and `execute` is the tool's own.
 */
export function withToolTimeouts(
  definitions: readonly ToolDefinition[],
  timeoutMs: number | undefined,
  hooks: ToolTimeoutHooks = {},
): readonly ToolDefinition[] {
  if (timeoutMs === undefined) return definitions;
  return definitions.map((definition) => withToolTimeout(definition, timeoutMs, hooks));
}
