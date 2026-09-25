/**
 * Agent runner — one `AgentSession` per iteration, created fresh, always disposed.
 *
 * This is where ADR-001's amnesia contract becomes real. Everything the loop
 * keeps across an iteration lives in beads or in git; everything the *model*
 * knows starts from zero each time, because each iteration builds a brand-new
 * in-memory session and throws it away afterwards. Not "compacted", not
 * "summarised", not "cleared" — a new object that never saw the last run.
 *
 * Three rules carry that, and each one is tested rather than asserted:
 *
 * 1. **Fresh session per run.** `SessionManager.inMemory()` per run, `compact()`
 *    disabled in settings. The alternative APIs — `SessionManager.create()`,
 *    `compact()`, `navigateTree()` — all retain model-summarised turns, which is
 *    precisely the breach this module exists to prevent. See ADR-001.
 * 2. **Dispose always.** Success, thrown error, refused verdict, or timeout: the
 *    session is disposed exactly once, in a `finally`. The disposal counter is
 *    exposed so a test can prove it rather than trust it.
 * 3. **Completion is structured.** Done is a `report_done` tool call (or, as a
 *    fallback, a fenced JSON verdict). A reply that merely *says* "done" is
 *    reported as `unstructured-verdict`, which the orchestrator treats as failed
 *    work — the loop never guesses.
 *
 * The context handed to the model is assembled by {@link buildWorkContext}, a
 * pure function of exactly these inputs: the issue payload, its dependency state
 * (read through {@link normaliseDependencies}, never a raw `depends_on_id`), the
 * `bd` memory of any prior attempt, and a {@link RepoSnapshot}. Nothing else. No
 * transcript, no scratch notes, no previous iteration's anything.
 *
 * Timeout is never ambiguous: the budget is explicit, `session.abort()` is called
 * and awaited with a grace period, and the outcome says so with a distinct kind
 * (`timeout`) rather than being folded into "error".
 *
 * This module never writes an issue's status and never closes anything. It returns
 * a verdict; {@link toWorkEvent} shows the one legal mapping onto the
 * orchestrator's `work_succeeded` / `work_failed` events, and workspace-5yn.9
 * owns the decision. Claims stay with the workgraph tooling.
 */
import {
  createAgentSession,
  DefaultResourceLoader,
  defineTool,
  getAgentDir,
  ModelRuntime,
  SessionManager,
  SettingsManager,
} from "@earendil-works/pi-coding-agent";
import type {
  AgentSession,
  CreateAgentSessionOptions,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

import type { BdClient, Issue, NewIssueSpec } from "./beads.ts";
import { BdError, normaliseDependencies } from "./beads.ts";
import { failureKeyFor, handoffKeyFor } from "./orchestrator.ts";
import type { OrchestratorEvent } from "./orchestrator.ts";
import { createRepoReader, formatRepoSnapshot, RepoError } from "./repo.ts";
import type { RepoSnapshot } from "./repo.ts";
import type { ContextBudget } from "./context.ts";
import { describeContextBudget, measureContextBudget } from "./context.ts";

/** Avoid the phantom-dependency import: borrow the SDK's own level type. */
type ThinkingLevel = NonNullable<CreateAgentSessionOptions["thinkingLevel"]>;

/**
 * Every thinking level pi accepts, declared as a table rather than a list so the
 * compiler keeps it honest: `Record<ThinkingLevel, true>` needs one key per
 * member of the union, so a level added upstream makes this object stop
 * compiling, and one invented here fails the same check from the other side.
 *
 * Insertion order is pi's own — cheapest first — and {@link THINKING_LEVELS}
 * reads it back for messages and for validating configuration.
 */
const THINKING_LEVEL_LOOKUP: Record<ThinkingLevel, true> = {
  off: true,
  minimal: true,
  low: true,
  medium: true,
  high: true,
  xhigh: true,
  max: true,
};

/** {@link THINKING_LEVEL_LOOKUP} as a list, in pi's order. */
export const THINKING_LEVELS: readonly ThinkingLevel[] = Object.keys(
  THINKING_LEVEL_LOOKUP,
) as ThinkingLevel[];

// ── verdicts ─────────────────────────────────────────────────────────────────

/** Where a verdict came from. Tool calls outrank prose. */
export const VERDICT_SOURCES = ["report_done_tool", "fenced_json", "none"] as const;
export type VerdictSource = (typeof VERDICT_SOURCES)[number];

export interface DoneVerdict {
  done: true;
  summary: string;
  changedFiles: string[];
  nextSteps: string[];
}

export interface IncompleteVerdict {
  done: false;
  /** Why it is not done. Required: a `done: false` with no reason tells the loop nothing. */
  reason: string;
  summary: string;
  changedFiles: string[];
  nextSteps: string[];
}

export type Verdict = DoneVerdict | IncompleteVerdict;

export type VerdictValidation =
  | { ok: true; verdict: Verdict }
  | { ok: false; problems: string[] };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim().length > 0 ? value.trim() : null;
}

function stringArray(value: unknown): string[] | null {
  if (!Array.isArray(value)) return null;
  const out: string[] = [];
  for (const item of value) {
    if (typeof item !== "string") return null;
    out.push(item);
  }
  return out;
}

/**
 * Dependency entries for a proposed split.
 *
 * Under the split protocol these are batch positions — integers, or numeric
 * strings (`"0"`, `"#2"`) — because no bd ids exist yet. This function does not
 * enforce that; it only refuses what cannot be a dependency at all (an object, a
 * boolean, `-1`, `1.5`) and passes strings through untouched.
 *
 * The division is deliberate: `.5` keeps the payload honest, `src/split.ts` is
 * the single authority on what a valid split *means*. Enforcing position
 * semantics here too would leave two places to keep in step, and the stricter one
 * would win silently.
 */
function depArray(value: unknown): string[] | null {
  if (!Array.isArray(value)) return null;
  const out: string[] = [];
  for (const entry of value) {
    if (typeof entry === "number") {
      if (!Number.isInteger(entry) || entry < 0) return null;
      out.push(String(entry));
    } else if (typeof entry === "string") {
      const text = entry.trim();
      if (text === "") return null;
      out.push(text);
    } else {
      return null;
    }
  }
  return out;
}

/**
 * Accept either spelling of the multi-word fields. Models drift between
 * `changed_files` and `changedFiles`; silently dropping one would lose the file
 * list from the commit, so both are read and the snake_case form wins.
 */
function pick(record: Record<string, unknown>, snake: string, camel: string): unknown {
  return record[snake] !== undefined ? record[snake] : record[camel];
}

/** Validate one verdict-shaped value. Pure; also used directly on fenced JSON. */
export function validateVerdict(raw: unknown): VerdictValidation {
  if (!isRecord(raw)) {
    return { ok: false, problems: ["the verdict must be a JSON object"] };
  }

  const problems: string[] = [];

  const done = raw["done"];
  if (typeof done !== "boolean") {
    problems.push("`done` must be a boolean (true or false)");
  }

  const summary = nonEmptyString(raw["summary"]);
  if (summary === null) {
    problems.push("`summary` is required and must be a non-empty string");
  }

  const changedRaw = pick(raw, "changed_files", "changedFiles");
  if (changedRaw === undefined) {
    problems.push("`changed_files` is required (an empty array is fine)");
  } else if (stringArray(changedRaw) === null) {
    problems.push("`changed_files` must be an array of strings");
  }

  const nextRaw = pick(raw, "next_steps", "nextSteps");
  if (nextRaw !== undefined && stringArray(nextRaw) === null) {
    problems.push("`next_steps` must be an array of strings");
  }

  const reason = nonEmptyString(raw["reason"]);
  if (done === false && reason === null) {
    problems.push("when `done` is false, `reason` is required and must be non-empty");
  }

  if (problems.length > 0) return { ok: false, problems };

  const changedFiles = stringArray(changedRaw) ?? [];
  const nextSteps = stringArray(nextRaw) ?? [];
  if (done === true) {
    return { ok: true, verdict: { done: true, summary: summary!, changedFiles, nextSteps } };
  }
  return {
    ok: true,
    verdict: {
      done: false,
      reason: reason!,
      summary: summary ?? "(no summary given)",
      changedFiles,
      nextSteps,
    },
  };
}

/** Validate the payload of `report_split` / a fenced split block. */
export function validateSplitPayload(raw: unknown):
  | { ok: true; specs: NewIssueSpec[] }
  | { ok: false; problems: string[] } {
  const items = Array.isArray(raw)
    ? raw
    : isRecord(raw)
      ? (pick(raw, "issues", "issues") as unknown)
      : undefined;

  if (!Array.isArray(items)) {
    return {
      ok: false,
      problems: ['expected an object like {"issues": [...]} or a bare array of issues'],
    };
  }
  if (items.length === 0) {
    return { ok: false, problems: ["no issues were proposed"] };
  }

  const problems: string[] = [];
  const specs: NewIssueSpec[] = [];
  items.forEach((item, index) => {
    if (!isRecord(item)) {
      problems.push(`issue #${index + 1} must be an object`);
      return;
    }
    const title = nonEmptyString(item["title"]);
    if (title === null) {
      problems.push(`issue #${index + 1} needs a non-empty \`title\``);
      return;
    }
    const spec: NewIssueSpec = { title };

    const description = nonEmptyString(item["description"]);
    if (description !== null) spec.description = description;

    const acceptance = nonEmptyString(
      pick(item, "acceptance", "acceptance_criteria") as unknown,
    );
    if (acceptance !== null) spec.acceptance = acceptance;

    const type = nonEmptyString(item["type"]);
    if (type !== null) spec.type = type;

    const priorityRaw = item["priority"];
    if (priorityRaw !== undefined) {
      const priority = typeof priorityRaw === "string" ? Number(priorityRaw) : priorityRaw;
      if (typeof priority !== "number" || !Number.isInteger(priority) || priority < 0 || priority > 4) {
        problems.push(`issue #${index + 1} \`priority\` must be an integer 0..4`);
      } else {
        spec.priority = priority as 0 | 1 | 2 | 3 | 4;
      }
    }

    const depsValue = pick(item, "depends_on", "deps");
    if (depsValue !== undefined && depsValue !== null) {
      const deps = depArray(depsValue);
      if (deps === null) {
        // Present but unreadable is reported, not dropped: a silently missing
        // ordering constraint produces work in the wrong order, which looks like
        // the loop working correctly.
        problems.push(
          `issue #${index + 1} \`depends_on\` must be an array of batch positions ` +
            `(integers like 0, or strings like "0"/"#0")`,
        );
      } else if (deps.length > 0) {
        spec.deps = deps;
      }
    }

    specs.push(spec);
  });

  if (problems.length > 0) return { ok: false, problems };
  return { ok: true, specs };
}

// ── fenced JSON verdicts ────────────────────────────────────────────────────

export type FencedBlockExtraction =
  | { found: false }
  | { found: true; raw: string; json?: unknown; parseError?: string };

const FENCE_RE = /```[ \t]*(?:json|JSON)?[ \t]*\r?\n([\s\S]*?)\r?\n```/g;

/**
 * Pull the last fenced JSON block out of prose. The last one wins: a model that
 * drafts a verdict and then revises it leaves the revision at the end.
 */
export function extractLastFencedJson(text: string): FencedBlockExtraction {
  FENCE_RE.lastIndex = 0;
  let last: string | null = null;
  for (let match = FENCE_RE.exec(text); match !== null; match = FENCE_RE.exec(text)) {
    const body = match[1];
    if (typeof body === "string" && body.trim().length > 0) last = body;
  }
  if (last === null) return { found: false };

  const raw = last.trim();
  try {
    return { found: true, raw, json: JSON.parse(raw) };
  } catch (error) {
    return {
      found: true,
      raw,
      parseError: error instanceof Error ? error.message : String(error),
    };
  }
}

/** What a run's evidence adds up to, before it becomes an outcome. */
export type RunClassification =
  | { kind: "done"; verdict: DoneVerdict; source: VerdictSource }
  | { kind: "incomplete"; verdict: IncompleteVerdict; source: VerdictSource }
  | { kind: "unstructured-verdict"; rawText: string }
  | {
      kind: "malformed-verdict";
      rawBlock: string;
      problems: string[];
      source: VerdictSource;
    };

/**
 * Decide, from evidence only: a captured tool verdict, a rejected tool call, a
 * fenced JSON block, or nothing. Pure, so the whole "did the agent actually
 * report" question is testable without a model.
 */
export function classifyRunEvidence(input: {
  toolVerdicts: readonly unknown[];
  toolRejections: readonly { raw: string; problems: readonly string[] }[];
  assistantText: string;
}): RunClassification {
  const lastVerdict = input.toolVerdicts.at(-1);
  if (lastVerdict !== undefined) {
    const validated = validateVerdict(lastVerdict);
    if (validated.ok) {
      return validated.verdict.done
        ? { kind: "done", verdict: validated.verdict, source: "report_done_tool" }
        : { kind: "incomplete", verdict: validated.verdict, source: "report_done_tool" };
    }
    return {
      kind: "malformed-verdict",
      rawBlock: JSON.stringify(lastVerdict),
      problems: validated.problems,
      source: "report_done_tool",
    };
  }

  const lastRejection = input.toolRejections.at(-1);
  if (lastRejection !== undefined) {
    return {
      kind: "malformed-verdict",
      rawBlock: lastRejection.raw,
      problems: [...lastRejection.problems],
      source: "report_done_tool",
    };
  }

  const fenced = extractLastFencedJson(input.assistantText);
  if (!fenced.found) {
    return { kind: "unstructured-verdict", rawText: input.assistantText };
  }
  if (fenced.parseError !== undefined) {
    return {
      kind: "malformed-verdict",
      rawBlock: fenced.raw,
      problems: [`the fenced JSON block could not be parsed: ${fenced.parseError}`],
      source: "fenced_json",
    };
  }

  const validated = validateVerdict(fenced.json);
  if (!validated.ok) {
    return {
      kind: "malformed-verdict",
      rawBlock: fenced.raw,
      problems: validated.problems,
      source: "fenced_json",
    };
  }
  return validated.verdict.done
    ? { kind: "done", verdict: validated.verdict, source: "fenced_json" }
    : { kind: "incomplete", verdict: validated.verdict, source: "fenced_json" };
}

// ── outcomes ────────────────────────────────────────────────────────────────

export const WORK_OUTCOME_KINDS = [
  "done",
  "incomplete",
  "unstructured-verdict",
  "malformed-verdict",
  "context-exhausted",
  "timeout",
  "error",
] as const;

export type WorkOutcomeKind = (typeof WORK_OUTCOME_KINDS)[number];

/** Everything a run reports back, whatever it decided. */
export interface WorkOutcomeBase {
  readonly issueId: string;
  /** `null` only when the session could not be created at all. */
  readonly sessionId: string | null;
  readonly sessionFile: string | null;
  readonly verdictSource: VerdictSource;
  /** Final assistant prose, for the human watching the loop. */
  readonly assistantText: string;
  readonly elapsedMs: number;
  /** Context problems that degraded the prompt (e.g. a failed `recall`). */
  readonly contextNotes: readonly string[];
  /** How many times `report_done` was called. More than one is worth seeing. */
  readonly verdictToolCalls: number;
}

export type WorkOutcome =
  | (WorkOutcomeBase & { kind: "done"; verdict: DoneVerdict })
  | (WorkOutcomeBase & { kind: "incomplete"; verdict: IncompleteVerdict })
  | (WorkOutcomeBase & { kind: "unstructured-verdict"; rawText: string })
  | (WorkOutcomeBase & {
      kind: "malformed-verdict";
      rawBlock: string;
      problems: readonly string[];
    })
  | (WorkOutcomeBase & {
      kind: "context-exhausted";
      /** The measurement that stopped the run, as the provider reported it. */
      budget: ContextBudget;
      /** Assistant turns it took to get there. */
      turns: number;
      settledAfterAbort: boolean;
    })
  | (WorkOutcomeBase & {
      kind: "timeout";
      budgetMs: number;
      /** Did the session settle after `abort()`, or did we stop waiting? */
      settledAfterAbort: boolean;
    })
  | (WorkOutcomeBase & { kind: "error"; message: string; phase: "session" | "run" });

/** Only `done` means finished. Everything else is unfinished work, by kind. */
export function isDone(outcome: WorkOutcome): boolean {
  return outcome.kind === "done";
}

/**
 * The one legal mapping onto the orchestrator's events. `done` → succeeded; every
 * other kind → failed, with a reason that names what actually happened so the
 * failure note written by the finalize path is useful rather than generic.
 */
export function toWorkEvent(outcome: WorkOutcome): OrchestratorEvent {
  if (outcome.kind === "done") {
    return {
      type: "work_succeeded",
      summary: outcome.verdict.summary,
      changedFiles: outcome.verdict.changedFiles,
    };
  }
  return { type: "work_failed", reason: describeFailure(outcome) };
}

/** Pure failure text, shaped for `bd remember`. */
export function describeFailure(outcome: WorkOutcome): string {
  switch (outcome.kind) {
    case "done":
      return "not a failure";
    case "incomplete":
      return `agent reported incomplete: ${outcome.verdict.reason}`;
    case "unstructured-verdict":
      return (
        "no structured verdict: the agent stopped without calling report_done and " +
        "without a fenced JSON verdict"
      );
    case "malformed-verdict":
      return `malformed verdict (${outcome.verdictSource}): ${outcome.problems.join("; ")}`;
    case "timeout":
      return `timed out after ${outcome.budgetMs}ms ` +
        `(${outcome.settledAfterAbort ? "settled after abort" : "still running when the grace period ended"})`;
    case "context-exhausted":
      return (
        `context exhausted after ${outcome.turns} assistant turn(s): ` +
        `${describeContextBudget(outcome.budget)} — stopped instead of spending ` +
        `the run budget ` +
        `(${outcome.settledAfterAbort ? "settled after abort" : "still running when the grace period ended"})`
      );
    case "error":
      return `${outcome.phase} error: ${outcome.message}`;
  }
}

// ── errors ──────────────────────────────────────────────────────────────────

export type AgentErrorKind =
  | "invalid-arguments"
  | "issue-not-found"
  | "unstructured-verdict"
  | "malformed-verdict"
  | "timeout"
  | "session-failed";

export class AgentError extends Error {
  readonly kind: AgentErrorKind;
  readonly detail: string;

  constructor(kind: AgentErrorKind, message: string, detail = "") {
    super(message);
    this.name = "AgentError";
    this.kind = kind;
    this.detail = detail;
  }

  static is(error: unknown): error is AgentError {
    return error instanceof AgentError ||
      (typeof error === "object" && error !== null && (error as { name?: string }).name === "AgentError");
  }
}

// ── session abstraction ─────────────────────────────────────────────────────

/**
 * The slice of `AgentSession` this runner uses. `Pick` rather than a hand-written
 * interface on purpose: if the SDK renames or re-types one of these members the
 * build breaks here instead of at 2 a.m. inside a loop iteration.
 */
export type AgentSessionLike = Pick<
  AgentSession,
  "sessionId" | "sessionFile" | "messages" | "prompt" | "abort" | "dispose" | "subscribe"
> & {
  /**
   * Only the two numbers the context check needs, not the SDK's whole model
   * type. Optional on purpose: a session that reports no model is a session this
   * check has no opinion about, and a fake that says nothing stays a valid port.
   */
  readonly model?: { readonly contextWindow: number; readonly maxTokens: number };
};

export type RunnerSessionKind = "work" | "split";

export interface SessionSpec {
  readonly kind: RunnerSessionKind;
  readonly cwd: string;
  readonly customTools: readonly ToolDefinition[];
  /** Built-in allowlist. Omitted → pi's defaults (read, bash, edit, write). */
  readonly builtinTools?: readonly string[];
  /** True → no built-in tools at all, only {@link SessionSpec.customTools}. */
  readonly noBuiltinTools?: boolean;
  readonly systemPromptOverride?: string;
  readonly thinkingLevel?: ThinkingLevel;
  readonly modelRef?: { provider: string; id: string };
}

export type SessionFactory = (spec: SessionSpec) => Promise<AgentSessionLike>;

let sharedModelRuntime: Promise<ModelRuntime> | null = null;

/** One `ModelRuntime` per process; sessions stay per-iteration. */
async function getModelRuntime(): Promise<ModelRuntime> {
  if (sharedModelRuntime === null) {
    sharedModelRuntime = ModelRuntime.create();
  }
  try {
    return await sharedModelRuntime;
  } catch (error) {
    // A failed create must not poison the process forever.
    sharedModelRuntime = null;
    throw error;
  }
}

type SessionModel = NonNullable<CreateAgentSessionOptions["model"]>;

/** The bit of `ModelRuntime` model resolution needs; stubbable in tests. */
type ModelRuntimeSlice = Pick<ModelRuntime, "getModel" | "getModels">;

/**
 * Resolve the model for one run.
 *
 * Explicit `modelRef` wins; otherwise pi's own configured default is read from
 * the real settings file. Nothing here reads the ambient environment on purpose:
 * a loop that silently inherits whatever `PI_MODEL` some parent process happened
 * to export is a loop that runs a different model in CI than at the keyboard.
 */
export function resolveModelForRun(
  runtime: ModelRuntimeSlice,
  settings: { getDefaultProvider(): string | undefined; getDefaultModel(): string | undefined },
  ref?: { provider: string; id: string },
): SessionModel | undefined {
  if (ref !== undefined) {
    const exact = runtime.getModel(ref.provider, ref.id);
    if (exact === undefined) {
      throw new AgentError(
        "session-failed",
        `model ${ref.provider}/${ref.id} is not available in ModelRuntime`,
      );
    }
    return exact;
  }

  const provider = settings.getDefaultProvider();
  const id = settings.getDefaultModel();
  if (provider !== undefined && id !== undefined) {
    const configured = runtime.getModel(provider, id);
    if (configured === undefined) {
      throw new AgentError(
        "session-failed",
        `the configured default model ${provider}/${id} is not available — ` +
          "check the provider config and models.json",
      );
    }
    return configured;
  }

  if (runtime.getModels().length === 0) {
    throw new AgentError(
      "session-failed",
      "no models are configured; set a default model or pass modelRef",
    );
  }
  // No default named: let pi pick from what it has rather than guessing here.
  return undefined;
}

function isThinkingLevel(value: string): value is ThinkingLevel {
  return Object.prototype.hasOwnProperty.call(THINKING_LEVEL_LOOKUP, value);
}

/**
 * Read a thinking level out of configuration.
 *
 * Absent or blank is `undefined`: not "low", not "medium" — *not configured*, a
 * state the caller can tell apart from a choice. Anything unrecognised is refused
 * rather than rounded down, because a level asked for in one place and ignored in
 * another is invisible from outside: a run at the wrong level looks exactly like
 * a run that was configured correctly.
 */
export function parseThinkingLevel(raw: unknown): ThinkingLevel | undefined {
  if (raw === undefined || raw === null) return undefined;
  if (typeof raw !== "string") {
    throw new AgentError(
      "invalid-arguments",
      `a thinking level must be a string, got ${typeof raw}`,
    );
  }
  const text = raw.trim().toLowerCase();
  if (text === "") return undefined;
  if (!isThinkingLevel(text)) {
    throw new AgentError(
      "invalid-arguments",
      `unknown thinking level "${raw.trim()}"; expected one of: ` +
        `${THINKING_LEVELS.join(", ")}`,
    );
  }
  return text;
}

/**
 * The level one run gets: the caller's if it named one, else the user's
 * configured default, else nothing — which leaves pi to apply its own default
 * instead of this loop guessing on the user's behalf.
 *
 * Deliberately the same shape as {@link resolveModelForRun}. Model and level are
 * the two knobs that decide what a run actually is; they should resolve the same
 * way, from the same place, with the same precedence.
 */
export function resolveThinkingLevelForRun(
  settings: { getDefaultThinkingLevel(): ThinkingLevel | undefined },
  explicit?: ThinkingLevel,
): ThinkingLevel | undefined {
  if (explicit !== undefined) return explicit;
  return settings.getDefaultThinkingLevel() ?? undefined;
}

/**
 * The production session factory.
 *
 * `SessionManager.inMemory()` + `SettingsManager.inMemory({compaction:{enabled:false}})`
 * is the whole amnesia story: nothing is written to a session file and nothing is
 * summarised forward, so when `dispose()` runs the iteration's context is gone
 * rather than consolidated into the next one.
 */
export const defaultSessionFactory: SessionFactory = async (spec) => {
  const runtime = await getModelRuntime();
  const diskSettings = SettingsManager.create(spec.cwd, getAgentDir());
  const model = resolveModelForRun(runtime, diskSettings, spec.modelRef);
  // Resolved, never assigned: the caller's level if it named one, else the user's
  // configured default, else pi's own. A hard-coded level here made every ticket
  // run at whatever this file said, with nothing in the config able to overrule
  // it — and neither work nor split could be pitched differently from the other.
  const thinkingLevel = resolveThinkingLevelForRun(diskSettings, spec.thinkingLevel);
  const settingsManager = SettingsManager.inMemory({ compaction: { enabled: false } });

  const options: CreateAgentSessionOptions = {
    cwd: spec.cwd,
    modelRuntime: runtime,
    thinkingLevel,
    sessionManager: SessionManager.inMemory(spec.cwd),
    settingsManager,
    customTools: [...spec.customTools],
  };
  if (model !== undefined) {
    options.model = model;
  }
  if (spec.noBuiltinTools === true) {
    // The default prompt would advertise tools this session does not have. Rather
    // than trust the caller to remember that, refuse the combination.
    const gap = toolInventoryGap(spec);
    if (gap !== null) throw new AgentError("invalid-arguments", gap);
    options.noTools = "builtin";
  } else if (spec.builtinTools !== undefined) {
    options.tools = [...spec.builtinTools];
  }
  if (spec.systemPromptOverride !== undefined) {
    const loader = new DefaultResourceLoader({
      cwd: spec.cwd,
      agentDir: getAgentDir(),
      settingsManager,
      systemPrompt: spec.systemPromptOverride,
    });
    await loader.reload();
    options.resourceLoader = loader;
  }

  const { session } = await createAgentSession(options);
  return session;
};

/** Defensive text extraction: works for user, assistant and tool-result shapes. */
export function messageText(message: unknown): string {
  if (!isRecord(message)) return "";
  const content = message["content"];
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";

  const parts: string[] = [];
  for (const part of content) {
    if (!isRecord(part)) continue;
    if (part["type"] === "text" && typeof part["text"] === "string") parts.push(part["text"]);
  }
  return parts.join("");
}

/** Concatenated text of every assistant message, oldest first. */
export function collectAssistantText(messages: readonly unknown[]): string {
  const chunks: string[] = [];
  for (const message of messages) {
    if (isRecord(message) && message["role"] === "assistant") {
      const text = messageText(message);
      if (text.length > 0) chunks.push(text);
    }
  }
  return chunks.join("\n");
}

/** The text of the last assistant message — where a prose verdict would live. */
export function lastAssistantText(messages: readonly unknown[]): string {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (isRecord(message) && message["role"] === "assistant") {
      const text = messageText(message);
      if (text.trim().length > 0) return text;
    }
  }
  return "";
}

// ── context builder (pure) ──────────────────────────────────────────────────

/** Fixed section names, in the order they are assembled. */
export const WORK_CONTEXT_SECTIONS = [
  "task",
  "description",
  "acceptance_criteria",
  "dependencies",
  "prior_attempt",
  "repo",
  "memories",
  "instructions",
] as const;

export type WorkContextSectionName = (typeof WORK_CONTEXT_SECTIONS)[number];

export interface WorkContextSection {
  readonly name: WorkContextSectionName;
  readonly body: string;
}

export interface WorkContext {
  readonly sections: readonly WorkContextSection[];
  readonly prompt: string;
}

export interface WorkContextInput {
  readonly issue: Issue;
  /** `recall(failureKeyFor(id))` — what went wrong last time, if anything. */
  readonly priorFailure?: string | null;
  /** `recall(handoffKeyFor(id))` — the previous finalize note, if any. */
  readonly handoff?: string | null;
  /** Repo state at iteration start, or `null` when not inside a git tree. */
  readonly repo?: RepoSnapshot | null;
  /** Extra `bd` memories the caller judged relevant. */
  readonly memories?: readonly { key: string; text: string }[];
  /** Per-field cap so one bloated description cannot crowd out the rest. */
  readonly maxFieldChars?: number;
}

const DEFAULT_MAX_FIELD_CHARS = 12_000;

function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max)}\n… truncated ${text.length - max} chars`;
}

function metaLine(issue: Issue): string {
  const parts: string[] = [];
  if (typeof issue.issue_type === "string" && issue.issue_type.length > 0) {
    parts.push(`type: ${issue.issue_type}`);
  }
  if (typeof issue.priority === "number") parts.push(`priority: P${issue.priority}`);
  if (typeof issue.status === "string" && issue.status.length > 0) parts.push(`status: ${issue.status}`);
  return parts.join(" · ");
}

/**
 * Assemble the work prompt. Pure: same input, byte-identical output, and a
 * section is emitted only when there is something to put in it — which is why a
 * fixture with no prior failure contains no failure text and no literal
 * "undefined"/"null" anywhere.
 */
export function buildWorkContext(input: WorkContextInput): WorkContext {
  const max = input.maxFieldChars ?? DEFAULT_MAX_FIELD_CHARS;
  const issue = input.issue;
  const sections: WorkContextSection[] = [];

  const meta = metaLine(issue);
  sections.push({
    name: "task",
    body: `# Work item ${issue.id}: ${issue.title}${meta.length > 0 ? `\n${meta}` : ""}`,
  });

  const description = nonEmptyString(issue.description);
  if (description !== null) {
    sections.push({ name: "description", body: `## What this issue asks\n${truncate(description, max)}` });
  }

  const acceptance = nonEmptyString(issue.acceptance_criteria);
  if (acceptance !== null) {
    sections.push({
      name: "acceptance_criteria",
      body: `## Acceptance criteria\n${truncate(acceptance, max)}`,
    });
  }

  // Normalised on purpose: `bd show` inlines the other issue and carries no
  // `depends_on_id`, so a naive read reports "not blocked".
  const dependencies = normaliseDependencies(issue);
  if (dependencies.length > 0) {
    const lines = dependencies.map(
      (dep) => `- ${dep.id}${dep.type ? ` (${dep.type})` : ""}${dep.issue ? `: ${dep.issue.title}` : ""}`,
    );
    sections.push({ name: "dependencies", body: `## Dependencies\n${lines.join("\n")}` });
  }

  const priorLines: string[] = [];
  const prior = nonEmptyString(input.priorFailure);
  if (prior !== null) priorLines.push(`### Why the last attempt stopped\n${truncate(prior, max)}`);

  const handoff = nonEmptyString(input.handoff);
  if (handoff !== null) priorLines.push(`### Handoff from the previous run\n${truncate(handoff, max)}`);

  if (priorLines.length > 0) {
    sections.push({
      name: "prior_attempt",
      body: `## Prior attempts on this issue\n${priorLines.join("\n\n")}`,
    });
  }

  if (input.repo) {
    sections.push({ name: "repo", body: `## Repository state when this run started\n${formatRepoSnapshot(input.repo)}` });
  }

  const memories = (input.memories ?? []).filter((m) => nonEmptyString(m.text) !== null);
  if (memories.length > 0) {
    const lines = memories.map((m) => `### ${m.key}\n${truncate(m.text.trim(), max)}`);
    sections.push({ name: "memories", body: `## Notes from beads memory\n${lines.join("\n")}` });
  }

  sections.push({ name: "instructions", body: REPORTING_INSTRUCTIONS });

  return { sections, prompt: sections.map((section) => section.body).join("\n\n") };
}

const REPORTING_INSTRUCTIONS = `## How to report this run

You are working in a session that is thrown away when this turn ends. Nobody
remembers it except what you report, so report precisely.

When the work is finished, call the \`report_done\` tool with:
- \`summary\`: what you actually did, in one paragraph. This becomes the commit
  message and the handoff note, so it has to make sense to someone who never saw
  this conversation.
- \`changed_files\`: every path you modified, created or deleted, written
  **relative to the repository root** — \`src/parser.ts\`, not
  \`/home/someone/repo/src/parser.ts\`. The loop stages exactly this list
  and nothing else, so a path that matches no real change is dropped and
  the commit ends up thinner than the work.
- \`next_steps\`: anything you deliberately left for later.

If you cannot finish, call \`report_done\` with \`done: false\` and a \`reason\` that
says what stopped you. That is a useful answer; the issue stays open and the
reason is saved for the next attempt.

Stopping with a prose "done" and no tool call is treated as **no verdict at all**
and the work counts as incomplete. Same for a JSON block that does not validate.
Do not commit or close the issue yourself — the loop does that after you report.`;

/** Prompt for the split pass. Deliberately lean; it plans, it does not implement. */
export function buildSplitPrompt(text: string): string {
  return `# Split this request into work items

A human asked for the following. Turn it into a small number of issues that can be
worked one at a time, in a fresh session each, with nothing carried between them.

## The request

${text.trim()}

## Report the split with the \`report_split\` tool

Each issue needs:
- \`title\`: a specific, actionable title.
- \`description\`: what to do, self-contained. Someone with no other context has
  to be able to start from this alone.
- \`acceptance\`: how to tell it is done.
- \`priority\`: integer 0 (urgent) to 4 (later).
- \`type\`: task, feature, bug, spike or decision.
- \`depends_on\`: the 0-based **positions** of other issues in this same array
  that must be finished before this one can start, e.g. \`[0]\` or \`[0, 1]\`.
  Positions, never ids — no ids exist yet. Omit it when nothing is needed, and
  never list an issue's own position. Two issues that can be worked in parallel
  have no dependency between them.

Keep it to the smallest number of issues that is still honest about the work. Do
not start implementing any of it here.`;
}

// ── report tools ────────────────────────────────────────────────────────────

/**
 * What a session with no built-in tools is told it can do.
 *
 * pi's default system prompt is written for a coding agent holding bash, read,
 * edit and write. A split session holds none of those — it is a planner with one
 * reporting tool. Left on the default prompt the model reaches for `bash`, gets
 * `Tool "bash" not found` back, and reads *that* as a flaky harness. The failure
 * was ours, not the model's, and it costs a turn every time it happens.
 *
 * The inventory is generated from the tools actually handed to the session, so a
 * tool cannot be advertised without existing, and one that exists cannot be left
 * out of what the model is told.
 */
export function bareToolsetSystemPrompt(
  customTools: readonly string[],
): string {
  if (customTools.length === 0) {
    throw new AgentError(
      "invalid-arguments",
      "a no-builtin-tools session needs at least one custom tool to be worth opening",
    );
  }
  const inventory = customTools.map((name) => `\`${name}\``).join(", ");
  const report = customTools[0] as string;
  return `# You are the planner for a beads-driven agent loop

You do one thing: turn one human sentence into a batch of board issues. You do
not implement any of them, and you could not if you tried.

## Your tool inventory is exactly: ${inventory}

That is the whole list. This session has **no built-in tools**: no shell, no
\`bash\`, no \`read\`, no \`edit\`, no \`write\`, no filesystem access, no web
access, no sub-agents. Nothing in here can look at the repository, run a
command, or confirm that a file exists. Plan from the words you were given.

A call to anything outside the list above fails with
\`Tool "<name>" not found\`. It tells you nothing and shortens the run. When you
want to check something, resolve instead to what the request implies and report
that.

## Report once, then stop

Call \`${report}\` one time, with the whole batch, and stop there. The loop
creates the issues from that single call and stops listening to you as soon as it
has it, so anything said afterwards is never read.`;
}

/**
 * The invariant behind {@link bareToolsetSystemPrompt}, as a function so it can
 * be tested instead of trusted: a session that was denied the built-in tools has
 * to be told, in its own system prompt, about every tool it *does* have. The
 * answer is `null` when there is no gap, or the sentence that explains one.
 */
export function toolInventoryGap(spec: {
  readonly customTools: readonly { name: string }[];
  readonly noBuiltinTools?: boolean;
  readonly systemPromptOverride?: string;
}): string | null {
  if (spec.noBuiltinTools !== true) return null;
  if (spec.customTools.length === 0) {
    return "a session with no built-in tools and no custom tool has nothing to do; do not open one";
  }
  if (spec.systemPromptOverride === undefined) {
    return (
      "a session with no built-in tools must set systemPromptOverride: pi's " +
      "default prompt promises bash/read/edit/write, and a model holding it will " +
      "call tools that are not there"
    );
  }
  const missing = spec.customTools
    .map((tool) => tool.name)
    .filter((name) => !spec.systemPromptOverride?.includes(name));
  if (missing.length > 0) {
    return (
      `the system prompt does not name every tool the session has: ${missing.join(", ")}`
    );
  }
  return null;
}

/**
 * What one session's report tools write into, and what the runner reads back out.
 *
 * Exported because `createReportSplitTool` is exported: a caller that can hand
 * over a tool has to be able to hand over the thing it fills, otherwise the
 * only way to test the tool's contract is to run a whole session through it.
 */
export interface Capture<T> {
  accepted: T[];
  rejected: { raw: string; problems: string[] }[];
  /**
   * Proposals that arrived *after* a valid one. Recorded, never applied: a batch
   * created twice is the one thing this path must not do, and silently picking
   * between two answers is worse than keeping the one that landed first.
   */
  duplicates: T[];
  /** Set once a tool accepted a payload and asked for the turn to end. */
  stopRequested: boolean;
  /**
   * Installed by the runner before the prompt starts, called by a tool right
   * after it accepts. The runner ends the turn because the answer is already in
   * hand; every further model turn is a chance to reach for something that is
   * not there.
   */
  onAccepted?: () => void;
}

/** A capture in its starting state: nothing accepted, nothing rejected. */
export function createCapture<T>(): Capture<T> {
  return {
    accepted: [],
    rejected: [],
    duplicates: [],
    stopRequested: false,
  };
}

/**
 * What one session run leaves behind. Read out of the session before it is
 * disposed, so nothing downstream has to touch a dead object — or worse, keep one
 * alive because it still needs to ask a question.
 */
export interface RunEvidence<T> {
  readonly sessionId: string;
  readonly sessionFile: string | null;
  readonly assistantText: string;
  readonly classification: RunClassification | null;
  /** Set when the wall-clock budget ran out; `null` otherwise. */
  readonly timeout: { readonly settledAfterAbort: boolean } | null;
  /**
   * Set when the *context* ran out before the clock did; `null` otherwise. The
   * two are kept apart because the fixes are opposite: raise the budget for one,
   * split the ticket for the other.
   */
  readonly context: {
    readonly budget: ContextBudget;
    readonly turns: number;
    readonly settledAfterAbort: boolean;
  } | null;
  /** Set when the prompt itself was rejected; `null` otherwise. */
  readonly promptError: string | null;
  readonly accepted: readonly T[];
  /** Proposals made after the accepted one. Never applied, always reported. */
  readonly duplicates: readonly T[];
  readonly verdictToolCalls: number;
}

const REPORT_DONE_PARAMS = Type.Object({
  done: Type.Boolean({
    description: "True only if the issue is fully worked. false means not finished.",
  }),
  summary: Type.String({
    description: "What you actually did. Becomes the commit message and handoff note.",
  }),
  changed_files: Type.Array(Type.String(), {
    description: "Every file path you created, modified or deleted. Empty if none.",
  }),
  next_steps: Type.Optional(
    Type.Array(Type.String(), { description: "Anything intentionally left for later." }),
  ),
  reason: Type.Optional(
    Type.String({ description: "Required when done is false: what stopped you." }),
  ),
});

const REPORT_SPLIT_PARAMS = Type.Object({
  issues: Type.Array(
    Type.Object({
      title: Type.String({ description: "Specific, actionable title." }),
      description: Type.Optional(Type.String({ description: "Self-contained description." })),
      acceptance: Type.Optional(Type.String({ description: "How to tell it is done." })),
      priority: Type.Optional(Type.Integer({ minimum: 0, maximum: 4 })),
      type: Type.Optional(Type.String({ description: "task|feature|bug|spike|decision." })),
      depends_on: Type.Optional(
        Type.Array(Type.Union([Type.Integer({ minimum: 0 }), Type.String()]), {
          description:
            "0-based positions of OTHER issues in this same array that this one " +
            "needs finished first, e.g. [0] or [0, 1]. Positions, not bd ids — " +
            "no ids exist yet. Omit when nothing is needed.",
        }),
      ),
    }),
    { minItems: 1, description: "The proposed work items." },
  ),
});

/**
 * `report_done` — the completion contract.
 *
 * A rejected call throws on purpose: the agent sees the reason and can fix it,
 * and the rejection is recorded so that if the run ends without a valid verdict
 * the outcome is `malformed-verdict` with the actual problems, not a shrug.
 */
export function createReportDoneTool(capture: Capture<unknown>): ToolDefinition {
  return defineTool({
    name: "report_done",
    label: "Report verdict",
    description:
      "Report the structured verdict for this issue. This is the only accepted " +
      "completion signal: do not just say you are finished.",
    promptSnippet: "Report the structured verdict for the current issue",
    promptGuidelines: [
      "Call report_done exactly once when the issue is worked, with a summary that " +
        "makes sense without this conversation.",
      "If you cannot finish, call report_done with done:false and a reason — that " +
        "is a valid, useful outcome.",
    ],
    parameters: REPORT_DONE_PARAMS,
    execute: async (_toolCallId, params) => {
      const raw = JSON.stringify(params);
      const validated = validateVerdict(params);
      if (!validated.ok) {
        const problems = validated.problems.join("; ");
        capture.rejected.push({ raw, problems: validated.problems });
        throw new Error(`report_done was rejected: ${problems}`);
      }
      capture.accepted.push(params);
      return {
        content: [
          {
            type: "text",
            text: validated.verdict.done
              ? "Verdict recorded: done. The loop will now commit, save the handoff and close the issue."
              : `Verdict recorded: not done (${validated.verdict.reason}). The loop will keep the issue open.`,
          },
        ],
        details: { verdict: validated.verdict },
      };
    },
  });
}

export function createReportSplitTool(capture: Capture<NewIssueSpec[]>): ToolDefinition {
  return defineTool({
    name: "report_split",
    label: "Report split",
    description: "Report the proposed child issues for the human's request.",
    promptSnippet: "Report the proposed child issues",
    parameters: REPORT_SPLIT_PARAMS,
    execute: async (_toolCallId, params) => {
      const raw = JSON.stringify(params);
      const first = capture.accepted[0] ?? [];
      if (capture.accepted.length > 0) {
        // A split already landed. This is not a second batch, and it must not
        // become one. It is recorded so the run can say so out loud, and it
        // does not throw: failing a run that already holds a good answer would
        // be the worse outcome of the two.
        const again = validateSplitPayload(params);
        if (again.ok) capture.duplicates.push(again.specs);
        return {
          content: [
            {
              type: "text",
              text:
                `A split was already recorded in this session (${first.length} ` +
                `issue(s)); that is the batch that will be created. This second ` +
                `proposal was NOT added` +
                `${again.ok ? "." : " — and it did not validate anyway."} ` +
                "Nothing more is needed: stop here.",
            },
          ],
          details: {
            duplicate: true,
            problems: again.ok ? [] : again.problems,
            specs: again.ok ? again.specs : undefined,
          },
        };
      }
      const validated = validateSplitPayload(params);
      if (!validated.ok) {
        const problems = validated.problems.join("; ");
        capture.rejected.push({ raw, problems: validated.problems });
        throw new Error(`report_split was rejected: ${problems}`);
      }
      capture.accepted.push(validated.specs);
      capture.stopRequested = true;
      capture.onAccepted?.();
      return {
        content: [
          {
            type: "text",
            text: `Split recorded: ${validated.specs.length} issue(s) will be created. ` +
              "Do not implement them here.",
          },
        ],
        details: {
          duplicate: false,
          problems: [],
          specs: validated.specs,
        },
      };
    },
  });
}

// ── runner ──────────────────────────────────────────────────────────────────

export interface RunnerEvent {
  readonly type:
    | "session_created"
    | "session_disposed"
    | "tool_call"
    | "agent_event"
    | "timeout"
    | "context_note";
  readonly sessionId?: string;
  readonly kind?: RunnerSessionKind;
  readonly detail?: string;
  readonly raw?: unknown;
}

export interface RepoReaderLike {
  /** `null` when the cwd is not inside a work tree — a state, not a failure. */
  describe(): Promise<RepoSnapshot | null>;
}

export interface AgentRunnerOptions {
  readonly beads: BdClient;
  /** Injected so the suite runs with no model, no network and no pi session. */
  readonly sessionFactory?: SessionFactory;
  readonly repo?: RepoReaderLike;
  /** Wall-clock budget per run. Default 20 minutes — real work is slow. */
  readonly timeoutMs?: number;
  /** How long to wait for the session to settle after abort(). Default 5s. */
  readonly abortGraceMs?: number;
  readonly cwd?: string;
  /** Explicit model; otherwise pi's configured default is used. Never env. */
  readonly modelRef?: { provider: string; id: string };
  readonly workThinkingLevel?: ThinkingLevel;
  readonly splitThinkingLevel?: ThinkingLevel;
  /** Injectable clock, so elapsed time is testable. */
  readonly now?: () => number;
  readonly onEvent?: (event: RunnerEvent) => void;
  /** Extra bd memory keys folded into the work context. */
  readonly extraMemoryKeys?: readonly string[];
  readonly includeRepoSnapshot?: boolean;
}

export interface RunnerStats {
  readonly created: number;
  readonly disposed: number;
  readonly live: number;
}

export interface AgentRunner {
  /** Work one issue in a brand-new session. Never throws for a normal failure. */
  run(issueId: string): Promise<WorkOutcome>;
  /** Split human text into specs. Throws {@link AgentError} if it could not. */
  split(text: string): Promise<NewIssueSpec[]>;
  /** The `session` port: dispose whatever is still live. Idempotent. */
  dispose(): Promise<number>;
  liveSessionIds(): readonly string[];
  stats(): RunnerStats;
}

interface LiveSession {
  readonly session: AgentSessionLike;
  readonly kind: RunnerSessionKind;
  disposed: boolean;
  disposeCount: number;
  /**
   * The in-flight `prompt()` promise, or undefined once it has settled. Tracked so
   * disposal can do the one thing `dispose()` alone cannot: stop a running turn.
   */
  running?: Promise<unknown>;
}

const DEFAULT_TIMEOUT_MS = 20 * 60_000;
const DEFAULT_GRACE_MS = 5_000;

/**
 * What the context check hands the run loop when it stops one early, as a tag so
 * it sits beside the prompt-error arm of the same race instead of being
 * second-guessed from its shape.
 */
interface ContextStopped {
  readonly kind: "context";
  readonly budget: ContextBudget;
  readonly turns: number;
}

/**
 * Where the model's declared window leaves the run, given the token counts the
 * provider reported for the turn that just landed. `null` when there is nothing
 * to measure — no model on the session, or a turn with no counts. A `null` is
 * never treated as "fine": the check simply has no opinion.
 */
function contextBudgetFor(
  session: AgentSessionLike,
  usage: { input?: number; cacheRead?: number } | undefined,
): ContextBudget | null {
  const model = session.model;
  if (model === undefined || usage === undefined) return null;
  const usedTokens = (usage.input ?? 0) + (usage.cacheRead ?? 0);
  if (usedTokens <= 0) return null;
  return measureContextBudget({
    windowTokens: model.contextWindow,
    maxOutputTokens: model.maxTokens,
    usedTokens,
  });
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    timer.unref?.();
  });
}

export function createAgentRunner(options: AgentRunnerOptions): AgentRunner {
  const sessionFactory = options.sessionFactory ?? defaultSessionFactory;
  const repo = options.repo ?? createRepoReader({ cwd: options.cwd });
  const budgetMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const graceMs = options.abortGraceMs ?? DEFAULT_GRACE_MS;
  const cwd = options.cwd ?? process.cwd();
  const now = options.now ?? ((): number => Date.now());
  const emit = options.onEvent ?? ((): void => {});
  const includeRepo = options.includeRepoSnapshot ?? true;

  const live = new Map<string, LiveSession>();
  let created = 0;
  let disposed = 0;

  async function disposeSession(entry: LiveSession): Promise<boolean> {
    if (entry.disposed) return false;
    entry.disposed = true;
    entry.disposeCount += 1;
    disposed += 1;
    live.delete(entry.session.sessionId);
    try {
      // Disposing a session that is still mid-turn has to abort it first.
      // `session.dispose()` releases listeners and resources, but it does not
      // settle the `prompt()` someone is awaiting — so Ctrl-C would leave the
      // loop parked on a promise that never resolves. Abort, give the turn the
      // grace period to unwind on its own, then dispose.
      if (entry.running !== undefined) {
        try {
          await entry.session.abort();
        } catch {
          // An abort that throws is still an abort; whether the turn settled is
          // what the race below decides.
        }
        await Promise.race([
          Promise.resolve(entry.running).catch(() => undefined),
          delay(graceMs),
        ]);
      }
      await entry.session.dispose();
      emit({ type: "session_disposed", sessionId: entry.session.sessionId, kind: entry.kind });
      return true;
    } catch (error) {
      emit({
        type: "session_disposed",
        sessionId: entry.session.sessionId,
        kind: entry.kind,
        detail: `dispose threw: ${error instanceof Error ? error.message : String(error)}`,
      });
      throw error;
    }
  }

  async function openAndRun<T>(
    kind: RunnerSessionKind,
    prompt: string,
    tools: readonly ToolDefinition[],
    thinkingLevel: ThinkingLevel | undefined,
    capture: Capture<T>,
  ): Promise<RunEvidence<T>> {
    const session = await sessionFactory({
      kind,
      cwd,
      customTools: tools,
      thinkingLevel,
      modelRef: options.modelRef,
      noBuiltinTools: kind === "split",
      // A session with nothing but its report tool has to be told so; see
      // {@link bareToolsetSystemPrompt}.
      systemPromptOverride:
        kind === "split"
          ? bareToolsetSystemPrompt(tools.map((tool) => tool.name))
          : undefined,
    });
    created += 1;
    const entry: LiveSession = { session, kind, disposed: false, disposeCount: 0 };
    live.set(session.sessionId, entry);
    emit({ type: "session_created", sessionId: session.sessionId, kind });

    // Everything we need is read out of the session *before* it is disposed, so
    // no caller ever has to touch a disposed session to find out what happened.
    let assistantText = "";
    let classification: RunClassification | null = null;
    let timeoutInfo: { settledAfterAbort: boolean } | null = null;
    let promptError: string | null = null;
    let contextInfo: {
      budget: ContextBudget;
      turns: number;
      settledAfterAbort: boolean;
    } | null = null;

    // Every assistant turn is weighed against the model's declared window as it
    // lands, on the provider's own count of the request that produced it. Two
    // turns of a work session are ordinary; a turn that leaves the *next* one a
    // couple of thousand tokens of answer is not a slow run but a dead one,
    // since the clamp in pi leaves nothing to say with. Stopping there costs the
    // seconds since the last turn instead of the rest of the budget.
    let assistantTurns = 0;
    let contextStopped = false;
    let stopForContext: ((stopped: ContextStopped) => void) | undefined;
    const contextExhausted = new Promise<ContextStopped>((resolve) => {
      stopForContext = (stopped: ContextStopped) => resolve(stopped);
    });

    const unsubscribe = session.subscribe((event) => {
      emit({ type: "agent_event", sessionId: session.sessionId, kind, raw: event });
      if (contextStopped || event.type !== "message_end") return;
      const message = event.message;
      if (message === undefined || message.role !== "assistant") return;
      assistantTurns += 1;
      const budget = contextBudgetFor(session, message.usage);
      if (budget === null || !budget.exhausted) return;
      contextStopped = true;
      stopForContext?.({ kind: "context", budget, turns: assistantTurns });
    });

    // The turn ends when a tool says the answer is in. Deferred by a macrotask so
    // the tool's own result is delivered first and the session history stays
    // readable; the alternative — letting the model keep generating — is what
    // produced duplicate proposals and phantom tool calls.
    capture.onAccepted = () => {
      setTimeout(() => {
        void Promise.resolve(session.abort()).catch(() => undefined);
      }, 0);
    };

    try {
      let timer: NodeJS.Timeout | undefined;
      const budget = new Promise<"timeout">((resolve) => {
        timer = setTimeout(() => resolve("timeout"), budgetMs);
        timer.unref?.();
      });
      const running = session
        .prompt(prompt)
        .then(() => "settled" as const)
        .catch((error: unknown) => ({
          kind: "prompt-error" as const,
          message: error instanceof Error ? error.message : String(error),
        }));
      entry.running = running;

      const first = await Promise.race([budget, running, contextExhausted]);
      if (timer !== undefined) clearTimeout(timer);

      if (first === "timeout") {
        emit({
          type: "timeout",
          sessionId: session.sessionId,
          kind,
          detail: `budget ${budgetMs}ms exceeded; aborting`,
        });
        try {
          await session.abort();
        } catch {
          // Whether abort throws does not decide the outcome: whether the prompt
          // settled after we asked it to stop does.
        }
        const settled = await Promise.race([
          running.then(() => "settled" as const),
          delay(graceMs).then(() => "grace-expired" as const),
        ]);
        timeoutInfo = { settledAfterAbort: settled === "settled" };
      } else if (typeof first === "object" && first.kind === "context") {
        // The window ended this run, not the clock. Which one it was is the whole
        // message: one wants a bigger budget, the other a smaller ticket.
        emit({
          type: "agent_event",
          sessionId: session.sessionId,
          kind,
          detail:
            `context exhausted after ${first.turns} assistant turn(s): ` +
            `${describeContextBudget(first.budget)}; aborting`,
        });
        try {
          await session.abort();
        } catch {
          // As above: the abort throwing changes nothing about what we report.
        }
        const settled = await Promise.race([
          running.then(() => "settled" as const),
          delay(graceMs).then(() => "grace-expired" as const),
        ]);
        contextInfo = {
          budget: first.budget,
          turns: first.turns,
          settledAfterAbort: settled === "settled",
        };
      } else if (typeof first === "object" && first.kind === "prompt-error") {
        if (capture.stopRequested && capture.accepted.length > 0) {
          // The turn ended because we ended it, with the answer already in hand.
          // Reporting that as a failure would be a lie with an error colour on it.
          emit({
            type: "agent_event",
            sessionId: session.sessionId,
            kind,
            detail: `turn stopped once the proposal was recorded (${first.message})`,
          });
        } else {
          promptError = first.message;
          emit({
            type: "agent_event",
            sessionId: session.sessionId,
            kind,
            detail: `prompt rejected: ${promptError}`,
          });
        }
      } else {
        assistantText = lastAssistantText(session.messages);
        classification = classifyRunEvidence({
          toolVerdicts: capture.accepted,
          toolRejections: capture.rejected,
          assistantText,
        });
      }

      return {
        sessionId: session.sessionId,
        sessionFile: session.sessionFile ?? null,
        assistantText,
        classification,
        timeout: timeoutInfo,
        context: contextInfo,
        promptError,
        accepted: capture.accepted,
        duplicates: capture.duplicates,
        verdictToolCalls: capture.accepted.length,
      };
    } finally {
      capture.onAccepted = undefined;
      unsubscribe();
      entry.running = undefined;
      await disposeSession(entry);
    }
  }

  async function readContextFor(issue: Issue): Promise<{
    priorFailure: string | null;
    handoff: string | null;
    memories: { key: string; text: string }[];
    notes: string[];
    repoSnapshot: RepoSnapshot | null;
  }> {
    const notes: string[] = [];
    const memories: { key: string; text: string }[] = [];

    async function safeRecall(key: string, label: string): Promise<string | null> {
      try {
        return await options.beads.recall(key);
      } catch (error) {
        // "No such memory" is a normal answer — a first attempt has none. Only a
        // real fault is worth a context note.
        if (BdError.is(error) && error.kind === "not-found") return null;
        const message = error instanceof Error ? error.message : String(error);
        notes.push(`${label} recall failed (${key}): ${message}`);
        emit({ type: "context_note", detail: `${label} recall failed for ${key}` });
        return null;
      }
    }

    const [priorFailure, handoff, ...extras] = await Promise.all([
      safeRecall(failureKeyFor(issue.id), "prior failure"),
      safeRecall(handoffKeyFor(issue.id), "handoff"),
      ...(options.extraMemoryKeys ?? []).map((key) => safeRecall(key, "memory")),
    ]);

    (options.extraMemoryKeys ?? []).forEach((key, index) => {
      const text = extras[index];
      if (text !== null && text !== undefined) memories.push({ key, text });
    });

    let repoSnapshot: RepoSnapshot | null = null;
    if (includeRepo) {
      try {
        repoSnapshot = await repo.describe();
        if (repoSnapshot === null) {
          notes.push("not inside a git working tree: no repo snapshot in context");
        }
      } catch (error) {
        // A git that cannot answer is a degraded prompt, not a reason to abandon
        // the issue. Anything other than RepoError is a bug and must surface.
        if (RepoError.is(error)) {
          notes.push(`repo snapshot unavailable (${error.kind}): ${error.message}`);
        } else {
          throw error;
        }
      }
    }

    return { priorFailure, handoff, memories, notes, repoSnapshot };
  }

  function baseFields(
    issueId: string,
    evidence: Pick<RunEvidence<unknown>, "sessionId" | "sessionFile" | "assistantText"> | null,
    started: number,
    contextNotes: readonly string[],
    verdictToolCalls: number,
  ): WorkOutcomeBase {
    return {
      issueId,
      sessionId: evidence?.sessionId ?? null,
      sessionFile: evidence?.sessionFile ?? null,
      verdictSource: "none",
      assistantText: evidence?.assistantText ?? "",
      elapsedMs: Math.max(0, now() - started),
      contextNotes,
      verdictToolCalls,
    };
  }

  async function run(issueId: string): Promise<WorkOutcome> {
    const id = issueId?.trim();
    if (!id) {
      throw new AgentError("invalid-arguments", "run() needs a non-empty issue id", String(issueId));
    }
    const started = now();

    const issue = await options.beads.getIssue(id);
    if (issue === null) {
      throw new AgentError("issue-not-found", `issue ${id} does not exist`);
    }

    const context = await readContextFor(issue);
    const workContext = buildWorkContext({
      issue,
      priorFailure: context.priorFailure,
      handoff: context.handoff,
      repo: context.repoSnapshot,
      memories: context.memories,
    });

    const capture: Capture<unknown> = createCapture<unknown>();
    const tools = [createReportDoneTool(capture)];

    let core: RunEvidence<unknown>;
    try {
      core = await openAndRun("work", workContext.prompt, tools, options.workThinkingLevel, capture);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return {
        ...baseFields(id, null, started, context.notes, capture.accepted.length),
        kind: "error",
        message: `could not run the session: ${message}`,
        phase: "session",
      };
    }

    const base = baseFields(id, core, started, context.notes, core.verdictToolCalls);

    if (core.context !== null) {
      return {
        ...base,
        kind: "context-exhausted",
        budget: core.context.budget,
        turns: core.context.turns,
        settledAfterAbort: core.context.settledAfterAbort,
      };
    }
    if (core.timeout !== null) {
      return { ...base, kind: "timeout", budgetMs, settledAfterAbort: core.timeout.settledAfterAbort };
    }
    if (core.promptError !== null) {
      return { ...base, kind: "error", message: core.promptError, phase: "run" };
    }

    const classification = core.classification;
    if (classification === null) {
      // Unreachable by construction, but a silent "done" is the last thing this
      // loop should ever produce, so it is an explicit error instead.
      return {
        ...base,
        kind: "error",
        message: "run finished with neither a verdict nor a timeout",
        phase: "run",
      };
    }

    switch (classification.kind) {
      case "done":
        return { ...base, kind: "done", verdict: classification.verdict, verdictSource: classification.source };
      case "incomplete":
        return {
          ...base,
          kind: "incomplete",
          verdict: classification.verdict,
          verdictSource: classification.source,
        };
      case "unstructured-verdict":
        return {
          ...base,
          kind: "unstructured-verdict",
          rawText: classification.rawText,
          verdictSource: "none",
        };
      case "malformed-verdict":
        return {
          ...base,
          kind: "malformed-verdict",
          rawBlock: classification.rawBlock,
          problems: classification.problems,
          verdictSource: classification.source,
        };
      default:
        return {
          ...base,
          kind: "error",
          message: `unhandled classification ${(classification as { kind: string }).kind}`,
          phase: "run",
        };
    }
  }

  async function split(text: string): Promise<NewIssueSpec[]> {
    const trimmed = typeof text === "string" ? text.trim() : "";
    if (!trimmed) {
      throw new AgentError("invalid-arguments", "split() needs non-empty text", String(text));
    }

    const capture = createCapture<NewIssueSpec[]>();
    const tools = [createReportSplitTool(capture)];

    const core = await openAndRun(
      "split",
      buildSplitPrompt(trimmed),
      tools,
      options.splitThinkingLevel,
      capture,
    );

    if (core.context !== null) {
      throw new AgentError(
        "timeout",
        `split ran out of context after ${core.context.turns} assistant turn(s): ` +
          describeContextBudget(core.context.budget),
      );
    }
    if (core.timeout !== null) {
      throw new AgentError(
        "timeout",
        `split timed out after ${budgetMs}ms and was aborted`,
      );
    }
    if (core.promptError !== null) {
      throw new AgentError("session-failed", `split session failed: ${core.promptError}`);
    }

    // The FIRST accepted batch is the batch. `at(-1)` here would be the last-one
    // wins behaviour this issue exists to remove: the tool refuses to append a
    // second batch, so both read the same value today — but the day something
    // does append, `at(-1)` silently changes which proposal becomes issues.
    const accepted = core.accepted[0];
    if (accepted !== undefined) {
      if (core.duplicates.length > 0) {
        // Said out loud rather than swallowed: two proposals in one session means
        // the model was unsure, and a human reading the split should know that
        // only the first was taken.
        emit({
          type: "context_note",
          detail:
            `report_split was called ${core.duplicates.length + 1} times in one ` +
            `split. Only the first proposal (${accepted.length} issue(s)) is ` +
            `created; the ${core.duplicates.length} later proposal(s) were ` +
            `ignored, because a second batch cannot be told apart from the first ` +
            `and creating both would double the board.`,
        });
      }
      return accepted;
    }

    const classification = core.classification;
    if (classification !== null && classification.kind === "malformed-verdict") {
      throw new AgentError(
        "malformed-verdict",
        `split produced an invalid payload: ${classification.problems.join("; ")}`,
        classification.rawBlock,
      );
    }
    const rawText =
      classification !== null && classification.kind === "unstructured-verdict"
        ? classification.rawText
        : "";
    throw new AgentError(
      "unstructured-verdict",
      "split produced no structured proposal: report_split was never called and " +
        "no fenced JSON array was found",
      rawText,
    );
  }

  return {
    run,
    split,
    async dispose() {
      const entries = [...live.values()];
      let count = 0;
      for (const entry of entries) {
        if (await disposeSession(entry)) count += 1;
      }
      return count;
    },
    liveSessionIds() {
      return [...live.keys()];
    },
    stats() {
      return { created, disposed, live: live.size };
    },
  };
}

/**
 * Compile-time conformance to the ports the orchestrator expects. These are not
 * casts: `tsc` checks the shapes, so a drift between the runner and
 * `OrchestratorPorts` stops the build instead of surfacing mid-loop.
 */
export function asAgentPort(runner: AgentRunner): OrchestratorPortsAgent {
  return runner;
}

export function asSessionPort(runner: AgentRunner): OrchestratorPortsSession {
  return {
    async dispose() {
      await runner.dispose();
    },
  };
}

type OrchestratorPortsAgent = import("./orchestrator.ts").OrchestratorPorts["agent"];
type OrchestratorPortsSession = import("./orchestrator.ts").OrchestratorPorts["session"];

export type { BdClient, Issue, NewIssueSpec, RepoSnapshot };
export type { ThinkingLevel };
