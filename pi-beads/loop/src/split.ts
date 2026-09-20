/**
 * `src/split.ts` — the splitter: one human sentence in, N well-formed beads out.
 *
 * This is the SPLIT node of the loop. Everything about it is designed around one
 * asymmetry: creating issues is cheap and irreversible, while *not* creating them
 * costs nothing. So every path that is not provably safe stops before the first
 * write, and every path that does write says exactly what it wrote and what
 * failed.
 *
 * The contract, in the order it is enforced:
 *
 *  1. **Empty input never reaches the model.** A blank request is not work, it is
 *     the absence of work. `split("")` returns before the port is touched, so a
 *     stray Enter press cannot invent tickets.
 *  2. **The batch is validated before the first `bd create`.** Titles,
 *     descriptions, acceptance criteria, priority, dependency indexes and issue
 *     types are checked here, with problems that name the offending index and
 *     field. A failing batch is rejected whole — never repaired field by field.
 *  3. **Local indexes become real `bd` ids during creation.** The model proposes
 *     `depends_on: [0, 2]` because it cannot know ids that do not exist yet. The
 *     ledger binds each index to the id `bd` actually returned, creating
 *     dependencies before dependents. Nothing is ever created pointing at an id
 *     that does not exist.
 *  4. **A bad payload is retried exactly once, with the error fed back.** The
 *     retry request quotes the problems, the previous output, and the original
 *     ask. Two strikes and it surfaces as a typed error carrying the raw model
 *     output — never a silent drop, never an endless retry loop.
 *  5. **A transport failure is not a malformed answer.** "The session died" and
 *     "the JSON was bad" are different facts with different remedies, so only the
 *     second is retried.
 *  6. **A partial creation reports itself.** Which issues exist, which failed,
 *     which were never attempted and why. A human can finish it by hand and a
 *     loop can decide; it cannot come out looking like nothing happened.
 *  7. **The human's words are recorded verbatim on the epic before any child is
 *     created.** If that record cannot be written, the split aborts. A board full
 *     of tickets nobody can trace back to a request is worse than no board.
 *
 * Boundaries held by this module (enforced by tests in `test/split.test.ts`):
 *
 * - Model access is only through {@link SplitAgentPort}. No provider, no HTTP
 *   client, no `pi` session — that is `src/agent.ts`'s job.
 * - Every board write goes through the injected `BdClient` from `src/beads.js`,
 *   which is where `BD_LAST_TOUCHED_FALLBACK=0`, argv arrays and `--json` live.
 * - No claim path: no assignee flag, no claim flag, no write to the assignee
 *   field. Claiming belongs to pi-workgraph's lease fencing, not to the splitter.
 * - No `bd remember` here. The handoff memory is written by the finalize path,
 *   after the work actually exists.
 *
 * Re-running a split is **not** idempotent: a second call creates a second set of
 * issues. Nothing here dedupes, because silently deciding "this batch already
 * exists" is a worse failure than a visible duplicate. Callers must not retry a
 * split blindly.
 */
import { BdError } from "./beads.ts";
import type { BdClient, NewIssueSpec } from "./beads.ts";
import type { OrchestratorEvent } from "./orchestrator.ts";

// ── the wire shape ──────────────────────────────────────────────────────────

/** bd's priority scale: 0 is most urgent, 4 is "someday". */
export const PRIORITY_VALUES = [0, 1, 2, 3, 4] as const;
export type SplitPriority = (typeof PRIORITY_VALUES)[number];

/** Issue types `bd create --type` accepts (canonical names; aliases are not). */
export const SPLIT_ISSUE_TYPES = [
  "task",
  "feature",
  "bug",
  "epic",
  "chore",
  "decision",
  "spike",
  "story",
  "milestone",
] as const;
export type SplitIssueType = (typeof SPLIT_ISSUE_TYPES)[number];

/** What one proposed issue must carry. */
export const SPLIT_ITEM_FIELDS = [
  "title",
  "description",
  "acceptance_criteria",
  "priority",
  "depends_on",
] as const;
export type SplitItemField = (typeof SPLIT_ITEM_FIELDS)[number];

/**
 * One validated issue in a proposed batch.
 *
 * Index-free on purpose: `dependsOn` holds positions in the *same batch*, because
 * real ids do not exist until the batch is created. {@link SplitLedger} is what
 * turns those positions into ids.
 */
export interface SplitItem {
  readonly title: string;
  readonly description: string;
  readonly acceptanceCriteria: string;
  readonly priority: SplitPriority;
  /** 0-based positions in the same batch. Never bd ids. */
  readonly dependsOn: readonly number[];
  readonly type: SplitIssueType;
  /** Fields the batch config filled in because the model omitted them. */
  readonly filledFromDefaults: readonly SplitItemField[];
}

/** One thing wrong with a proposed batch. `index: -1` means the batch as a whole. */
export interface SplitProblem {
  readonly index: number;
  readonly field: string;
  readonly message: string;
}

export function formatSplitProblem(problem: SplitProblem): string {
  const where = problem.index < 0 ? "the batch" : `issue #${problem.index + 1}`;
  return `${where} \`${problem.field}\`: ${problem.message}`;
}

export type SplitBatchValidation =
  | { ok: true; items: readonly SplitItem[] }
  | { ok: false; problems: readonly SplitProblem[] };

export interface SplitValidationConfig {
  /**
   * Ceiling on how many issues one split pass may propose — the over-split guard.
   * A model that turns "fix the typo in the README" into nine tickets has
   * misunderstood the request, and the loop's unit of work is a session, not a
   * programme. Default {@link DEFAULT_MAX_ITEMS}.
   */
  readonly maxItems?: number;
  /** Filled in when an item omits `priority`. Left unset, priority is required. */
  readonly defaultPriority?: SplitPriority;
  /**
   * Filled in when an item omits `acceptance_criteria`. Left unset, acceptance
   * criteria are required.
   *
   * Deliberately opt-in. A fabricated acceptance line *looks* like a specified
   * ticket and is not one, which is worse than a rejected batch: the rejection
   * gets another attempt with a real answer, the fabrication gets worked. If a
   * fallback is wanted at all, configure something honest that points back at the
   * record — "Confirm with the human; the original request is on the epic."
   */
  readonly defaultAcceptance?: string;
  /** Issue type used when an item omits `type`. Default `"task"`. */
  readonly defaultType?: SplitIssueType;
}

export const DEFAULT_MAX_ITEMS = 12;
export const DEFAULT_RETRY_BUDGET = 1;
export const DEFAULT_EPIC_TITLE_MAX = 80;

/**
 * The line the verbatim record starts with. The human's text follows it
 * unchanged, byte for byte — so `record.endsWith(input)` holds, and the marker
 * keeps the quote findable later without editing the quote.
 */
export const VERBATIM_MARKER = "Human request, verbatim:";

// ── errors ──────────────────────────────────────────────────────────────────

export type SplitErrorKind =
  /** No usable text: nothing proposed, nothing written. */
  | "empty-input"
  /** The model's batch failed validation and the retry did not fix it. */
  | "invalid-batch"
  /** The port itself failed (session died, timed out, was never wired). */
  | "model-error"
  /** The epic / verbatim record could not be written, so no children were made. */
  | "epic-failed"
  /** A board write failed; see `created`/`failed`/`skipped` on the outcome. */
  | "creation-failed"
  /** A spec handed to the ledger that the ledger did not produce. */
  | "unknown-spec";

/** Every thrown failure from this module is a `SplitError` with a `kind`. */
export class SplitError extends Error {
  readonly kind: SplitErrorKind;
  readonly detail: string;
  readonly problems: readonly SplitProblem[];

  constructor(
    kind: SplitErrorKind,
    message: string,
    detail = "",
    problems: readonly SplitProblem[] = [],
  ) {
    super(message);
    this.name = "SplitError";
    this.kind = kind;
    this.detail = detail;
    this.problems = problems;
  }

  static is(error: unknown): error is SplitError {
    return (
      error instanceof SplitError ||
      (typeof error === "object" &&
        error !== null &&
        (error as { name?: string }).name === "SplitError")
    );
  }
}

// ── small readers ───────────────────────────────────────────────────────────

function isRecord(value: unknown): value is { [key: string]: unknown } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

/** First key present among `keys`; bd-style JSON arrives in either casing. */
function pick(item: { [key: string]: unknown }, ...keys: string[]): unknown {
  for (const key of keys) {
    if (item[key] !== undefined) return item[key];
  }
  return undefined;
}

function describeShape(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "an array";
  return `a ${typeof value}`;
}

/**
 * A dependency entry from the model, normalised to a batch index.
 *
 * Integers are the contract. Strings are accepted here (`"2"`, `"#2"`) for one
 * structural reason rather than out of leniency: the same dependency list has to
 * survive a round trip through `NewIssueSpec.deps`, which is `string[]` because
 * bd ids are strings. So an index that left here as the number `2` may come back
 * as `"2"`, and both must mean the same thing. `"urgent"` still fails.
 */
export function normaliseDepIndex(value: unknown): number | null {
  if (typeof value === "number") {
    return Number.isInteger(value) && value >= 0 ? value : null;
  }
  if (typeof value === "string") {
    const text = value.trim().replace(/^#/, "");
    if (!/^\d+$/.test(text)) return null;
    const parsed = Number(text);
    return Number.isSafeInteger(parsed) ? parsed : null;
  }
  return null;
}

// ── validation ──────────────────────────────────────────────────────────────

type FieldOk<T> = { value: T; filled: boolean };
type FieldBad = { problem: string };

function parsePriority(value: unknown, config: SplitValidationConfig): FieldOk<SplitPriority> | FieldBad {
  if (value === undefined || value === null) {
    if (config.defaultPriority !== undefined) {
      return { value: config.defaultPriority, filled: true };
    }
    return {
      problem:
        "is required. Every issue the loop picks up needs a priority; set a " +
        "`defaultPriority` in the splitter config rather than leaving it out.",
    };
  }
  if (typeof value === "number") {
    if (!Number.isInteger(value)) {
      return { problem: `must be an integer 0..4, got the non-integer ${value}` };
    }
    if (value < 0 || value > 4) {
      return { problem: `must be within 0..4, got ${value}` };
    }
    return { value: value as SplitPriority, filled: false };
  }
  return {
    problem:
      `must be the integer 0..4, got ${JSON.stringify(value)}. bd takes a string ` +
      `flag, but the batch keeps it a number so "high" and "urgent" cannot pass ` +
      `for a priority.`,
  };
}

function parseType(value: unknown, config: SplitValidationConfig): FieldOk<SplitIssueType> | FieldBad {
  if (value === undefined || value === null) {
    return { value: config.defaultType ?? "task", filled: false };
  }
  const text = nonEmptyString(value);
  if (text === null) return { problem: "must be a non-empty string when present" };
  if ((SPLIT_ISSUE_TYPES as readonly string[]).includes(text)) {
    return { value: text as SplitIssueType, filled: false };
  }
  return {
    problem:
      `"${text}" is not a bd issue type (valid: ${SPLIT_ISSUE_TYPES.join(", ")}). ` +
      `Checked before creation because a bad type would fail mid-batch and leave a ` +
      `split half-done for a reason the batch could have said up front.`,
  };
}

function parseDependsOn(value: unknown, itemCount: number, index: number): { value: number[] } | FieldBad {
  if (value === undefined || value === null) return { value: [] };
  if (!Array.isArray(value)) {
    return { problem: `must be an array of batch indexes, got ${describeShape(value)}` };
  }
  const seen = new Set<number>();
  const deps: number[] = [];
  for (const [position, entry] of (value as readonly unknown[]).entries()) {
    const resolved = normaliseDepIndex(entry);
    if (resolved === null) {
      return {
        problem:
          `entry ${position} (${JSON.stringify(entry)}) is not a batch index. Use ` +
          `the 0-based position of another issue in this same array.`,
      };
    }
    if (resolved === index) {
      return {
        problem:
          "lists itself. An issue cannot block itself: it would never become " +
          "ready, and the loop would never pick it.",
      };
    }
    if (resolved >= itemCount) {
      return {
        problem:
          `entry ${position} points at #${resolved}, but this batch has only ` +
          `${itemCount} issue(s) (#0..#${itemCount - 1}). A dependency on a ` +
          `nonexistent issue is never created.`,
      };
    }
    if (seen.has(resolved)) continue; // the same index twice denotes the same edge
    seen.add(resolved);
    deps.push(resolved);
  }
  return { value: deps };
}

interface ParsedItem {
  item?: SplitItem;
  problems: SplitProblem[];
}

function parseSplitItem(
  raw: unknown,
  index: number,
  itemCount: number,
  config: SplitValidationConfig,
): ParsedItem {
  const problems: SplitProblem[] = [];
  if (!isRecord(raw)) {
    return {
      problems: [{ index, field: "issue", message: `must be an object, got ${describeShape(raw)}` }],
    };
  }

  const title = nonEmptyString(pick(raw, "title", "name"));
  if (title === null) {
    problems.push({
      index,
      field: "title",
      message: "must be a non-empty string. A title the loop can show a human is not optional.",
    });
  }

  const description = nonEmptyString(pick(raw, "description", "detail", "body"));
  if (description === null) {
    problems.push({
      index,
      field: "description",
      message:
        "must be a non-empty string, self-contained enough that someone with no " +
        "other context could start from it alone.",
    });
  }

  let acceptance = nonEmptyString(
    pick(raw, "acceptance_criteria", "acceptance", "acceptanceCriteria"),
  );
  let acceptanceFilled = false;
  if (acceptance === null) {
    const fallback = nonEmptyString(config.defaultAcceptance);
    if (fallback === null) {
      problems.push({
        index,
        field: "acceptance_criteria",
        message:
          "must be a non-empty string. Without it the judgment gate has nothing " +
          "to check the work against. Set `defaultAcceptance` for a standing " +
          "fallback instead of leaving it blank.",
      });
    } else {
      acceptance = fallback;
      acceptanceFilled = true;
    }
  }

  const priority = parsePriority(pick(raw, "priority"), config);
  if ("problem" in priority) {
    problems.push({ index, field: "priority", message: priority.problem });
  }

  const type = parseType(pick(raw, "type", "issue_type"), config);
  if ("problem" in type) {
    problems.push({ index, field: "type", message: type.problem });
  }

  const depends = parseDependsOn(pick(raw, "depends_on", "deps", "dependsOn"), itemCount, index);
  if ("problem" in depends) {
    problems.push({ index, field: "depends_on", message: depends.problem });
  }

  if (problems.length > 0) return { problems };

  const filled: SplitItemField[] = [];
  if (acceptanceFilled) filled.push("acceptance_criteria");
  if ("filled" in priority && priority.filled) filled.push("priority");

  return {
    item: {
      title: title as string,
      description: description as string,
      acceptanceCriteria: acceptance as string,
      priority: (priority as FieldOk<SplitPriority>).value,
      dependsOn: (depends as { value: number[] }).value,
      type: (type as FieldOk<SplitIssueType>).value,
      filledFromDefaults: filled,
    },
    problems: [],
  };
}

/**
 * Validate a proposed batch before anything is written.
 *
 * Accepts a bare array of issues or the `{ "issues": [...] }` wrapper that
 * `report_split`'s parameters use. Anything else is a batch-level problem — it is
 * *not* treated as "zero issues", because an unreadable answer and an empty
 * answer must not arrive at the same place.
 */
export function validateSplitBatch(
  raw: unknown,
  config: SplitValidationConfig = {},
): SplitBatchValidation {
  const items = Array.isArray(raw) ? raw : isRecord(raw) ? raw["issues"] : undefined;

  if (!Array.isArray(items)) {
    return {
      ok: false,
      problems: [
        {
          index: -1,
          field: "batch",
          message:
            `expected an array of issues (or {"issues": [...]}), got ${describeShape(raw)}. ` +
            `An unreadable answer is not an empty answer.`,
        },
      ],
    };
  }

  if (items.length === 0) {
    return {
      ok: false,
      problems: [
        {
          index: -1,
          field: "batch",
          message:
            "is empty. A request that breaks down into zero issues is not a split; " +
            "say so to the human rather than proposing nothing.",
        },
      ],
    };
  }

  const maxItems = config.maxItems ?? DEFAULT_MAX_ITEMS;
  if (items.length > maxItems) {
    return {
      ok: false,
      problems: [
        {
          index: -1,
          field: "batch",
          message:
            `proposes ${items.length} issue(s) for one request, over the limit of ` +
            `${maxItems}. Over-splitting is a failure, not thoroughness: fold the ` +
            `work back together or ask the human for a smaller ask.`,
        },
      ],
    };
  }

  const problems: SplitProblem[] = [];
  const parsed: SplitItem[] = [];
  for (const [index, entry] of (items as readonly unknown[]).entries()) {
    const result = parseSplitItem(entry, index, items.length, config);
    if (result.item) parsed.push(result.item);
    problems.push(...result.problems);
  }
  if (problems.length > 0) return { ok: false, problems };

  const order = planSplitOrder(parsed);
  if (!order.ok) {
    return {
      ok: false,
      problems: [
        {
          index: -1,
          field: "depends_on",
          message:
            `has a cycle: ${order.cycle.map((step) => `#${step}`).join(" → ")}. ` +
            "A cycle cannot be created, cannot be worked, and cannot be untangled " +
            "by the loop.",
        },
      ],
    };
  }

  return { ok: true, items: parsed };
}

// ── ordering ────────────────────────────────────────────────────────────────

export type SplitOrder =
  | { ok: true; order: readonly number[] }
  | { ok: false; cycle: readonly number[] };

/**
 * Topological order for a batch: every issue comes after the ones it depends on,
 * so a dependency always exists by the time something points at it.
 *
 * Deterministic — the ready set is drained in ascending order, so the same batch
 * always yields the same creation sequence. Cycles are reported, never broken by
 * quietly dropping an edge.
 */
export function planSplitOrder(items: readonly SplitItem[]): SplitOrder {
  const remaining = new Set<number>(items.map((_, index) => index));
  const blocking = new Map<number, Set<number>>();
  const dependents = new Map<number, number[]>();

  items.forEach((_, index) => {
    blocking.set(index, new Set<number>());
    dependents.set(index, []);
  });
  items.forEach((item, index) => {
    for (const dep of item.dependsOn) {
      blocking.get(index)?.add(dep);
      dependents.get(dep)?.push(index);
    }
  });

  const order: number[] = [];
  while (remaining.size > 0) {
    const ready = [...remaining]
      .filter((index) => (blocking.get(index)?.size ?? 0) === 0)
      .sort((a, b) => a - b);
    if (ready.length === 0) break;
    for (const index of ready) {
      remaining.delete(index);
      order.push(index);
      for (const dependent of dependents.get(index) ?? []) {
        blocking.get(dependent)?.delete(index);
      }
    }
  }

  if (remaining.size > 0) return { ok: false, cycle: findCycle([...remaining], blocking) };
  return { ok: true, order };
}

/** Walk the blocked subgraph to name a cycle, so the error is specific. */
function findCycle(
  nodes: readonly number[],
  blocking: ReadonlyMap<number, Set<number>>,
): readonly number[] {
  const stack: number[] = [];
  const onStack = new Set<number>();
  const visited = new Set<number>();

  function walk(node: number): readonly number[] | null {
    if (onStack.has(node)) return [...stack.slice(stack.indexOf(node)), node];
    if (visited.has(node)) return null;
    visited.add(node);
    onStack.add(node);
    stack.push(node);
    for (const dep of blocking.get(node) ?? []) {
      const found = walk(dep);
      if (found) return found;
    }
    stack.pop();
    onStack.delete(node);
    return null;
  }

  for (const node of nodes) {
    const cycle = walk(node);
    if (cycle) return cycle;
  }
  return nodes; // unreachable: a blocked subgraph with no cycle drains
}

// ── the verbatim record ────────────────────────────────────────────────────

/**
 * The string written to the epic: the marker, then the human's request with its
 * bytes intact. No paraphrase, no truncation, no re-wrap.
 */
export function verbatimRecord(input: string): string {
  return `${VERBATIM_MARKER}\n${input}`;
}

/**
 * Title for an epic the splitter has to create.
 *
 * The title is derived (first line, whitespace collapsed, capped) precisely
 * *because* the description holds the text unchanged: the title is a handle, the
 * description is the record. Nothing is lost by shortening a handle.
 */
export function epicTitleFor(input: string, config: { epicTitle?: string } = {}): string {
  const explicit = nonEmptyString(config.epicTitle);
  if (explicit !== null) return explicit;

  const firstLine = input
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find((line) => line !== "");
  if (!firstLine) return "Human request";

  const collapsed = firstLine.replace(/\s+/g, " ");
  if (collapsed.length <= DEFAULT_EPIC_TITLE_MAX) return collapsed;
  return `${collapsed.slice(0, DEFAULT_EPIC_TITLE_MAX - 1).trimEnd()}…`;
}

export interface EpicHandling {
  /** `created` when there was no epic and the splitter had to make one. */
  readonly mode: "existing" | "created";
  readonly epicId: string;
  readonly title: string;
  /** The exact string written; ends with the human's input unchanged. */
  readonly record: string;
  /** Where the record landed, so a human can go read it. */
  readonly landedIn: "notes" | "description";
}

export interface EpicConfig {
  readonly epicId?: string | null;
  readonly epicTitle?: string;
  readonly epicPriority?: SplitPriority;
}

/**
 * Write the human's words down before making anything from them.
 *
 * With `epicId` supplied the record is *appended* to that epic's notes — `bd
 * note` is append-only, where `bd update --notes` would let a later run overwrite
 * what an earlier human said. Without one, an epic is created whose description is
 * the record itself, and the children hang under it.
 *
 * Throws whatever the bd client throws. The caller treats that as fatal, which is
 * the point: no record, no children.
 */
export async function recordHumanRequest(
  beads: BdClient,
  input: string,
  config: EpicConfig = {},
): Promise<EpicHandling> {
  if (nonEmptyString(typeof input === "string" ? input : String(input ?? "")) === null) {
    // An epic whose record is blank is worse than no epic: it looks like the
    // request was captured when nothing was.
    throw new SplitError("empty-input", "there is no request to record, so nothing is written");
  }
  const record = verbatimRecord(input);
  const existing = nonEmptyString(config.epicId ?? undefined);

  if (existing !== null) {
    const issue = await beads.appendNote(existing, record);
    return {
      mode: "existing",
      epicId: existing,
      title: issue.title ?? existing,
      record,
      landedIn: "notes",
    };
  }

  const title = epicTitleFor(input, config);
  const issue = await beads.createIssue({
    title,
    description: record,
    type: "epic",
    priority: config.epicPriority ?? 2,
  });
  return { mode: "created", epicId: issue.id, title, record, landedIn: "description" };
}

// ── spec mapping ────────────────────────────────────────────────────────────

/**
 * Which batch position a handed-out spec came from. Module-level so identity
 * survives however far the spec travels: the ledger recognises a spec by the
 * object it minted, never by matching titles (two issues can legitimately share
 * one).
 */
const SPEC_TO_INDEX = new WeakMap<NewIssueSpec, number>();

/**
 * A validated item as a `bd create` spec.
 *
 * @param deps resolved dependency values — real bd ids when creating. When a
 *   spec is handed *out* before creation, unresolved positions travel as
 *   `#index` tokens: a token cannot be mistaken for an id, and if one ever
 *   reaches `bd` the reference fails loudly instead of silently binding to the
 *   wrong issue.
 * @param index the batch position, recorded so {@link indexForSpec} can find it.
 */
export function toNewIssueSpec(
  item: SplitItem,
  epicId: string | null,
  deps: readonly string[] = item.dependsOn.map((position) => `#${position}`),
  index?: number,
): NewIssueSpec {
  const spec: NewIssueSpec = {
    title: item.title,
    description: item.description,
    acceptance: item.acceptanceCriteria,
    priority: item.priority,
    type: item.type,
  };
  if (epicId !== null) spec.parent = epicId;
  if (deps.length > 0) spec.deps = [...deps];
  if (index !== undefined) SPEC_TO_INDEX.set(spec, index);
  return spec;
}

export function toNewIssueSpecs(items: readonly SplitItem[], epicId: string | null): NewIssueSpec[] {
  return items.map((item, index) => toNewIssueSpec(item, epicId, undefined, index));
}

export function indexForSpec(spec: NewIssueSpec): number | null {
  return SPEC_TO_INDEX.get(spec) ?? null;
}

// ── the ledger: local indexes to real ids ───────────────────────────────────

export interface CreatedIssue {
  /** Position in the proposed batch. */
  readonly index: number;
  /** The id `bd` actually returned. */
  readonly id: string;
  readonly title: string;
}

export interface FailedCreate {
  readonly index: number;
  readonly title: string;
  readonly message: string;
  /** `BdError.kind` when a bd call is what failed. */
  readonly errorKind: string;
}

export interface SkippedItem {
  readonly index: number;
  readonly title: string;
  /** Batch indexes this item needed that do not exist. */
  readonly missing: readonly number[];
  readonly reason: string;
}

export interface SplitCreationReport {
  readonly created: readonly CreatedIssue[];
  readonly failed: readonly FailedCreate[];
  readonly skipped: readonly SkippedItem[];
}

export type SplitLedgerEvent =
  | { type: "create_start"; index: number; title: string; deps: readonly string[] }
  | { type: "created"; index: number; id: string; title: string }
  | { type: "failed"; index: number; title: string; message: string; errorKind: string }
  | { type: "skipped"; index: number; title: string; missing: readonly number[] };

export interface SplitLedgerOptions {
  readonly items: readonly SplitItem[];
  readonly beads: BdClient;
  readonly epicId?: string | null;
  readonly onEvent?: (event: SplitLedgerEvent) => void;
}

export interface SplitLedger {
  /** The bd id created for a batch index, or `null` if it does not exist. */
  idOf(index: number): string | null;
  /**
   * Make sure batch `index` exists, creating whatever it depends on first.
   * Returns `null` when it could not be created; the caller decides whether that
   * is fatal.
   */
  ensure(index: number): Promise<CreatedIssue | null>;
  /**
   * A `beads.create_issue` effect entry point: hand back the exact spec
   * {@link toNewIssueSpecs} produced and the ledger recognises it by identity.
   */
  createForSpec(spec: NewIssueSpec): Promise<CreatedIssue | null>;
  /** Create the whole batch in topological order. */
  createAll(): Promise<SplitCreationReport>;
  report(): SplitCreationReport;
}

/**
 * The ledger turns a validated proposal into real issues, and remembers what it
 * did.
 *
 * It exists because of one hard rule: **nothing may depend on an id that does not
 * exist.** The model cannot know ids, so it proposes positions, and the only safe
 * moment to bind them is during creation. The lazy ({@link SplitLedger.ensure})
 * and eager (`createAll`) entry points go through the same resolution, so the
 * caller's arrival order does not matter — a dependent asked for first drags its
 * dependencies in ahead of it.
 *
 * No retries. A `bd` failure is a fact to report, not a condition to hammer on:
 * a guard mismatch means the world moved underneath the command, and re-issuing
 * it is how a loop writes the wrong thing twice.
 */
export function createSplitLedger(options: SplitLedgerOptions): SplitLedger {
  const { items, beads } = options;
  const emit = options.onEvent ?? ((): void => {});
  const epicId = options.epicId ?? null;

  const byId = new Map<number, string>();
  const created: CreatedIssue[] = [];
  const failed = new Map<number, FailedCreate>();
  const skipped = new Map<number, SkippedItem>();
  const pending = new Map<number, Promise<CreatedIssue | null>>();
  const visiting = new Set<number>();

  function report(): SplitCreationReport {
    return { created: [...created], failed: [...failed.values()], skipped: [...skipped.values()] };
  }

  function skip(index: number, missing: readonly number[]): null {
    const title = items[index]?.title ?? `(unparsed #${index})`;
    skipped.set(index, {
      index,
      title,
      missing,
      reason:
        `not attempted: it needs ${missing.map((step) => `#${step}`).join(", ")}, ` +
        `${missing.length === 1 ? "which was" : "all of which were"} created ` +
        "unsuccessfully, and a dependency on a nonexistent issue is never created.",
    });
    emit({ type: "skipped", index, title, missing });
    return null;
  }

  async function ensure(index: number): Promise<CreatedIssue | null> {
    const known = byId.get(index);
    if (known !== undefined) return { index, id: known, title: items[index]?.title ?? "" };
    if (failed.has(index) || skipped.has(index)) return null;

    const inFlight = pending.get(index);
    if (inFlight !== undefined) return inFlight;

    const item = items[index];
    if (!item) return skip(index, []);
    if (visiting.has(index)) {
      // Validation rules cycles out. Arriving here means a caller skipped it:
      // stop rather than recurse forever.
      return skip(index, item.dependsOn);
    }
    visiting.add(index);

    const promise = (async (): Promise<CreatedIssue | null> => {
      const resolvedDeps: string[] = [];
      const missing: number[] = [];
      for (const dep of item.dependsOn) {
        const dependency = await ensure(dep);
        if (dependency === null) missing.push(dep);
        else resolvedDeps.push(dependency.id);
      }
      if (missing.length > 0) return skip(index, missing);

      const spec = toNewIssueSpec(item, epicId, resolvedDeps);
      emit({ type: "create_start", index, title: item.title, deps: resolvedDeps });
      try {
        const issue = await beads.createIssue(spec);
        if (typeof issue.id !== "string" || issue.id.trim() === "") {
          throw new BdError({
            kind: "non-json",
            message: "bd create returned an issue with no usable id",
          });
        }
        const record: CreatedIssue = { index, id: issue.id, title: issue.title ?? item.title };
        byId.set(index, issue.id);
        created.push(record);
        emit({ type: "created", index, id: issue.id, title: record.title });
        return record;
      } catch (error) {
        const kind = BdError.is(error) ? error.kind : error instanceof Error ? error.name : "unknown";
        const message = error instanceof Error ? error.message : String(error);
        failed.set(index, { index, title: item.title, message, errorKind: kind });
        emit({ type: "failed", index, title: item.title, message, errorKind: kind });
        return null;
      }
    })();

    pending.set(index, promise);
    try {
      return await promise;
    } finally {
      visiting.delete(index);
    }
  }

  async function createForSpec(spec: NewIssueSpec): Promise<CreatedIssue | null> {
    const index = indexForSpec(spec);
    if (index === null || index >= items.length) {
      throw new SplitError(
        "unknown-spec",
        "the ledger was handed a spec it did not mint, so it cannot resolve that " +
          "batch's dependency indexes; it refuses to guess which issue this is",
        spec.title,
      );
    }
    return ensure(index);
  }

  async function createAll(): Promise<SplitCreationReport> {
    const order = planSplitOrder(items);
    // A cycle cannot reach here through `validateSplitBatch`; if one somehow
    // does, walk the given order and let the ledger's own guards report it.
    const sequence = order.ok ? order.order : items.map((_, index) => index);
    for (const index of sequence) {
      await ensure(index);
    }
    return report();
  }

  return { idOf: (index) => byId.get(index) ?? null, ensure, createForSpec, createAll, report };
}

// ── the agent port ────────────────────────────────────────────────────────

/**
 * How the splitter reaches a model: one call, one attempt, whatever came back.
 *
 * Returning `unknown` rather than a parsed batch is deliberate — the validator
 * stays the single source of truth about what a valid split is, whichever model or
 * harness sits behind the port. `src/agent.ts`'s runner satisfies this shape
 * (`propose: (text) => runner.split(text)`) and a test fake satisfies it with a
 * canned value, which is why nothing in here needs a network.
 */
export interface SplitAgentPort {
  propose(request: string): Promise<unknown>;
}

/** Structural view of `.5`'s runner, so this module need not import it. */
export interface AgentSplitLike {
  split(text: string): Promise<readonly NewIssueSpec[]>;
}

export function portFromAgentRunner(runner: AgentSplitLike): SplitAgentPort {
  return {
    async propose(request: string): Promise<unknown> {
      return await runner.split(request);
    },
  };
}

/**
 * A port that is not wired. Fails as a model error rather than inventing a batch:
 * a missing splitter must never look like a request that produced nothing.
 */
export function unavailableSplitPort(reason = "no split agent is wired"): SplitAgentPort {
  return {
    async propose(): Promise<unknown> {
      throw new SplitError("model-error", reason);
    },
  };
}

// ── retry request ─────────────────────────────────────────────────────────

/**
 * The second attempt's text: what was wrong, what came back last time, and the
 * original request unchanged. The model is being corrected, not re-asked, so it
 * gets the evidence.
 */
export function buildRetryRequest(
  input: string,
  problems: readonly SplitProblem[],
  previousOutput?: string,
): string {
  const lines: string[] = [
    "Your previous split was rejected. Why:",
    ...problems.map((problem) => `- ${formatSplitProblem(problem)}`),
    "",
    "Send it again, valid this time: a JSON array where every issue has a non-empty",
    "`title`, a self-contained non-empty `description`, a non-empty",
    "`acceptance_criteria`, an integer `priority` from 0 to 4, and `depends_on`",
    "listing the 0-based index of another issue in the same array.",
    "Do not invent work the request did not ask for.",
    "",
    "The original request, unchanged:",
    input,
  ];
  if (previousOutput !== undefined && previousOutput.trim() !== "") {
    lines.push("", "Exactly what came back last time:", previousOutput);
  }
  return lines.join("\n");
}

/** Pull the raw text a port failure carried, if it carried any. */
function rawFromError(error: unknown): string {
  if (error === null || error === undefined) return "";
  if (typeof error === "string") return error;
  if (typeof error === "object") {
    const record = error as { detail?: unknown; stdoutSnippet?: unknown; raw?: unknown };
    for (const candidate of [record.detail, record.stdoutSnippet, record.raw]) {
      if (typeof candidate === "string" && candidate.trim() !== "") return candidate;
    }
  }
  return "";
}

/** An unreadable answer, described as batch problems where it can be. */
function describeUnreadable(raw: string, message: string): readonly SplitProblem[] {
  try {
    const parsed: unknown = JSON.parse(raw.trim());
    const validated = validateSplitBatch(parsed);
    if (!validated.ok) return validated.problems;
  } catch {
    // fall through to the parse-error problem
  }
  return [{ index: -1, field: "json", message: `could not be read as JSON: ${message}` }];
}

/**
 * Classify a port failure.
 *
 * A malformed *answer* is retryable: the model said something wrong, and saying
 * it back is a fair second chance. A broken *channel* is not: a session dying,
 * timing out, or being absent is not improved by immediate repetition, and
 * retrying it turns one failure into a stall. The distinction stays explicit
 * because collapsing it is how a loop hammers a dead provider.
 */
function classifyPortFailure(error: unknown): {
  retryable: boolean;
  problems: readonly SplitProblem[];
  message: string;
  raw: string;
} {
  const raw = rawFromError(error);
  const message = error instanceof Error ? error.message : String(error);
  const name = error instanceof Error ? error.name : "";
  const kind =
    typeof error === "object" && error !== null
      ? String((error as { kind?: unknown }).kind ?? "")
      : "";

  // `.5` reports a bad split payload by throwing `AgentError` with the raw block
  // in `detail`; that is an unreadable answer, not a dead channel.
  const malformedAnswer =
    name === "AgentError" && (kind === "malformed-verdict" || kind === "unstructured-verdict");

  if (malformedAnswer || raw.trim() !== "") {
    return { retryable: true, problems: describeUnreadable(raw, message), message, raw };
  }
  return { retryable: false, problems: [], message, raw: "" };
}

// ── outcomes ──────────────────────────────────────────────────────────────

export interface SplitOutcomeBase {
  /** The human's request, unchanged, whatever happened to it. */
  readonly input: string;
  /** How many times the port was asked. 0 for empty input. */
  readonly attempts: number;
}

export type SplitOutcome =
  | (SplitOutcomeBase & { kind: "empty-input" })
  | (SplitOutcomeBase & {
      kind: "invalid-batch";
      problems: readonly SplitProblem[];
      /** Every raw answer the model gave, oldest first. */
      rawOutputs: readonly string[];
    })
  | (SplitOutcomeBase & {
      kind: "model-error";
      message: string;
      rawOutputs: readonly string[];
    })
  | (SplitOutcomeBase & {
      kind: "epic-failed";
      items: readonly SplitItem[];
      message: string;
      errorKind: string;
    })
  | (SplitOutcomeBase & {
      kind: "created";
      items: readonly SplitItem[];
      epic: EpicHandling;
      created: readonly CreatedIssue[];
    })
  | (SplitOutcomeBase & {
      kind: "partial";
      items: readonly SplitItem[];
      epic: EpicHandling;
      created: readonly CreatedIssue[];
      failed: readonly FailedCreate[];
      skipped: readonly SkippedItem[];
    });

export function isSplitSuccess(outcome: SplitOutcome): boolean {
  return outcome.kind === "created";
}

export function createdIds(outcome: SplitOutcome): readonly string[] {
  return outcome.kind === "created" || outcome.kind === "partial"
    ? outcome.created.map((issue) => issue.id)
    : [];
}

/** Human-readable failure text, shaped for a warning line or a board note. */
export function describeSplitFailure(outcome: SplitOutcome): string {
  switch (outcome.kind) {
    case "empty-input":
      return "nothing to split: the request was empty";
    case "invalid-batch":
      return `the split was rejected: ${outcome.problems.map(formatSplitProblem).join("; ")}`;
    case "model-error":
      return `the split agent failed: ${outcome.message}`;
    case "epic-failed":
      return (
        `the request could not be recorded on the epic (${outcome.errorKind}): ` +
        `${outcome.message}. No child issues were created.`
      );
    case "created":
      return "not a failure";
    case "partial":
      return (
        `the split only partly landed: ${outcome.created.length} created, ` +
        `${outcome.failed.length} failed` +
        (outcome.skipped.length > 0 ? `, ${outcome.skipped.length} not attempted` : "")
      );
  }
}

/**
 * The legal mapping onto the state machine's events (mirrors `toWorkEvent`).
 *
 * `created` is the only success. Everything else — including a partial, which has
 * real issues on the board — arrives as `split_failed` carrying the ids that do
 * exist, because "the machine kept going and looks fine" is the last thing a
 * partial should communicate.
 */
export function toSplitEvent(outcome: SplitOutcome): OrchestratorEvent {
  const ids = createdIds(outcome);
  if (outcome.kind === "created") return { type: "split_created", createdIds: ids };
  return { type: "split_failed", reason: describeSplitFailure(outcome), createdIds: ids };
}

// ── the splitter ──────────────────────────────────────────────────────────

export interface SplitterConfig extends SplitValidationConfig, EpicConfig {
  /** Retries after the first attempt, for a malformed answer. Default 1. */
  readonly retryBudget?: number;
  /** Ledger events, for a caller that wants to watch creation happen. */
  readonly onEvent?: (event: SplitLedgerEvent) => void;
}

export interface SplitProposal {
  readonly ok: boolean;
  readonly items: readonly SplitItem[];
  /** Specs carrying `#index` dep tokens, minted for this proposal. */
  readonly specs: readonly NewIssueSpec[];
  readonly attempts: number;
  readonly problems: readonly SplitProblem[];
  readonly rawOutputs: readonly string[];
  /** Set when the port itself failed, as opposed to answering badly. */
  readonly modelMessage: string | null;
}

export interface Splitter {
  /** Ask, validate, retry once. Performs no board writes. */
  propose(text: string): Promise<SplitProposal>;
  /** The full transaction: record the request, then create the batch. */
  split(text: string): Promise<SplitOutcome>;
  lastOutcome(): SplitOutcome | null;
  /** Everything this splitter has created, across calls — the receipts. */
  created(): readonly CreatedIssue[];
}

const NO_PROBLEMS: readonly SplitProblem[] = [];
const NO_OUTPUTS: readonly string[] = [];

/**
 * Build a splitter over a model port and a bd client.
 *
 * `split()` is the whole SPLIT step, and the unit the interpreter wires.
 * `propose()` is the same first half without writes, for a human-in-the-loop
 * preview or for tests that only care about the contract with the model.
 */
export function createSplitter(
  ports: { agent: SplitAgentPort; beads: BdClient },
  config: SplitterConfig = {},
): Splitter {
  const retryBudget = Math.max(0, config.retryBudget ?? DEFAULT_RETRY_BUDGET);
  const maxAttempts = retryBudget + 1;
  const receipts: CreatedIssue[] = [];
  let last: SplitOutcome | null = null;

  async function propose(text: string): Promise<SplitProposal> {
    const input = typeof text === "string" ? text : String(text ?? "");
    if (input.trim() === "") {
      // Guarded here as well as in `split`, so a caller that goes straight for
      // `propose` cannot reach the model with nothing either.
      return {
        ok: false,
        items: [],
        specs: [],
        attempts: 0,
        problems: [
          { index: -1, field: "text", message: "is empty; there is nothing to split" },
        ],
        rawOutputs: NO_OUTPUTS,
        modelMessage: null,
      };
    }

    const rawOutputs: string[] = [];
    let problems: readonly SplitProblem[] = NO_PROBLEMS;
    let modelMessage: string | null = null;
    let attempts = 0;

    for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
      const request =
        attempt === 1
          ? input
          : buildRetryRequest(input, problems, rawOutputs.at(-1));
      attempts = attempt;

      let raw: unknown;
      try {
        raw = await ports.agent.propose(request);
      } catch (error) {
        const failure = classifyPortFailure(error);
        if (failure.raw !== "") rawOutputs.push(failure.raw);
        if (!failure.retryable) {
          return {
            ok: false,
            items: [],
            specs: [],
            attempts,
            problems: NO_PROBLEMS,
            rawOutputs,
            modelMessage: failure.message,
          };
        }
        problems = failure.problems;
        modelMessage = failure.message;
        continue;
      }

      // The port may hand back a parsed value or the model's raw string; both are
      // read here so the validator sees one thing.
      let payload: unknown = raw;
      if (typeof raw === "string") {
        if (raw.trim() === "") {
          rawOutputs.push(raw);
          problems = [{ index: -1, field: "json", message: "the model returned nothing at all" }];
          modelMessage = "empty response";
          continue;
        }
        rawOutputs.push(raw);
        try {
          payload = JSON.parse(raw.trim());
        } catch (parseError) {
          const message = parseError instanceof Error ? parseError.message : String(parseError);
          problems = [{ index: -1, field: "json", message: `could not be read as JSON: ${message}` }];
          modelMessage = message;
          continue;
        }
      }

      const validated = validateSplitBatch(payload, config);
      if (validated.ok) {
        return {
          ok: true,
          items: validated.items,
          specs: toNewIssueSpecs(validated.items, config.epicId ?? null),
          attempts,
          problems: NO_PROBLEMS,
          rawOutputs,
          modelMessage: null,
        };
      }
      problems = validated.problems;
      modelMessage = null;
    }

    return {
      ok: false,
      items: [],
      specs: [],
      attempts,
      problems,
      rawOutputs,
      modelMessage,
    };
  }

  async function splitOnce(text: string): Promise<SplitOutcome> {
    const input = typeof text === "string" ? text : String(text ?? "");

    if (input.trim() === "") {
      return { kind: "empty-input", input, attempts: 0 };
    }

    const proposal = await propose(input);

    if (!proposal.ok) {
      if (proposal.modelMessage !== null && proposal.problems.length === 0) {
        return {
          kind: "model-error",
          input,
          attempts: proposal.attempts,
          message: proposal.modelMessage,
          rawOutputs: proposal.rawOutputs,
        };
      }
      return {
        kind: "invalid-batch",
        input,
        attempts: proposal.attempts,
        problems: proposal.problems,
        rawOutputs: proposal.rawOutputs,
      };
    }

    let epic: EpicHandling;
    try {
      epic = await recordHumanRequest(ports.beads, input, config);
    } catch (error) {
      const kind = BdError.is(error) ? error.kind : error instanceof Error ? error.name : "unknown";
      return {
        kind: "epic-failed",
        input,
        attempts: proposal.attempts,
        items: proposal.items,
        message: error instanceof Error ? error.message : String(error),
        errorKind: kind,
      };
    }

    const ledger = createSplitLedger({
      items: proposal.items,
      beads: ports.beads,
      epicId: epic.epicId,
      onEvent: config.onEvent,
    });
    const result = await ledger.createAll();
    receipts.push(...result.created);

    if (result.failed.length === 0 && result.skipped.length === 0) {
      return {
        kind: "created",
        input,
        attempts: proposal.attempts,
        items: proposal.items,
        epic,
        created: result.created,
      };
    }

    return {
      kind: "partial",
      input,
      attempts: proposal.attempts,
      items: proposal.items,
      epic,
      created: result.created,
      failed: result.failed,
      skipped: result.skipped,
    };
  }

  async function split(text: string): Promise<SplitOutcome> {
    const outcome = await splitOnce(text);
    last = outcome;
    return outcome;
  }

  return {
    propose,
    split,
    lastOutcome: () => last,
    created: () => [...receipts],
  };
}

