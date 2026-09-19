/**
 * Typed, side-effect-safe adapter around the `bd` (beads) CLI.
 *
 * **This is the only module in this app that shells out to `bd`.** Everything
 * else — orchestrator, split, finalize — goes through these functions, so the
 * safety rules below have exactly one place to live and one place to be audited.
 *
 * Enforced here, asserted in `test/beads.test.ts`:
 *
 * 1. **`execFile` + argv array.** Never `shell: true`, never a command string
 *    with interpolated text. Issue titles and reasons are argv elements.
 * 2. **`BD_LAST_TOUCHED_FALLBACK=0` is forced** into the child environment
 *    *after* the caller's env is merged, so nothing can raise it back to 1.
 * 3. **Ids are validated in-process before a child is spawned.** A command built
 *    from an empty variable is refused here, so it never reaches `bd` at all.
 * 4. **No claim path.** `--assignee`, `-a` and `--claim` are refused by
 *    {@link assertClaimFreeArgs}: this module cannot touch the assignee field and
 *    therefore cannot clobber workgraph lease fencing. Claim/release/close of
 *    claimed work belongs to the workgraph tools.
 * 5. **`--json` everywhere**, parsed defensively. Unparseable stdout is a typed
 *    error — never silently coerced into an empty list.
 * 6. **Exit codes are classified, not guessed.** `13` (stale `--if-status`
 *    guard) becomes `guard-mismatch` and is *never* retried here; a retry is a
 *    caller decision that must be made with fresh state.
 * 7. **Every command is logged at debug level before it runs.**
 */
import { execFile } from "node:child_process";

/** bd's built-in statuses (see `bd statuses`). Custom statuses are not modelled. */
export type IssueStatus =
  | "open"
  | "in_progress"
  | "blocked"
  | "deferred"
  | "closed"
  | "pinned"
  | "hooked";

const ISSUE_STATUSES: readonly IssueStatus[] = [
  "open",
  "in_progress",
  "blocked",
  "deferred",
  "closed",
  "pinned",
  "hooked",
];

/**
 * A dependency exactly as bd reports it. bd uses **two different shapes** and
 * both are real:
 *
 * - `bd list` / `bd ready` return an **edge**: `{issue_id, depends_on_id, type}`.
 * - `bd show` returns the **other issue itself**, with `dependency_type` on it
 *   (verified live against bd 1.3.0).
 *
 * Code that assumes only the edge shape will read `undefined` off a `show` result
 * and conclude "not blocked by X" — a fail-open bug. Consume dependencies
 * through {@link normaliseDependencies} instead.
 */
export interface DependencyEdge {
  issue_id?: string;
  /** Always present on the edge shape; required so the type guard can narrow. */
  depends_on_id: string;
  type?: string;
  created_at?: string;
  created_by?: string;
}

/** A dependency reported as the other issue, with `dependency_type` attached. */
export type DependentIssue = Partial<Issue> & { id: string; dependency_type?: string };

export type IssueDependency = DependencyEdge | DependentIssue;

/** Normalised dependency: id of the other issue + edge type, from either shape. */
export interface NormalisedDependency {
  /** The other issue's id (what this issue depends on, or who blocks it). */
  id: string;
  type: string | null;
  /** Present when bd inlined the whole issue, as `bd show` does. */
  issue?: Issue;
}

/**
 * An issue as returned by `bd ... --json`. Field names mirror bd's JSON keys so
 * no mapping layer can silently drop data.
 */
export interface Issue {
  id: string;
  title: string;
  status: IssueStatus;
  priority: number;
  issue_type: string;
  description?: string;
  acceptance_criteria?: string;
  /** `bd note` output. Read here; written only through {@link BdClient.appendNote}. */
  notes?: string;
  owner?: string;
  created_at?: string;
  updated_at?: string;
  created_by?: string;
  labels?: string[];
  dependencies?: IssueDependency[];
  dependency_count?: number;
  dependent_count?: number;
  comment_count?: number;
  started_at?: string;
  revision?: number | string;
  /** Read-only view. This module never writes it (see rule 4). */
  assignee?: string;
}

/** Everything `bd create` can be told. */
export interface NewIssueSpec {
  title: string;
  description?: string;
  acceptance?: string;
  priority?: 0 | 1 | 2 | 3 | 4;
  /** bd issue type, e.g. task|feature|bug|epic|spike|decision. */
  type?: string;
  labels?: string[];
  parent?: string;
  /** Ids this new issue is blocked by. */
  deps?: string[];
}

/** Why a bd call failed. */
export type BdErrorKind =
  /** The `bd` binary could not be found (ENOENT). */
  | "missing-binary"
  /** stdout was not valid JSON, or not the JSON shape we required. */
  | "non-json"
  /** bd reported a general failure (any non-zero exit other than 0/13). */
  | "exit-1"
  /** Exit 13: `--if-status` precondition no longer held. Nothing was written. */
  | "guard-mismatch"
  /** The referenced issue/memory does not exist (a normal answer, not a fault). */
  | "not-found"
  /** The child was killed by our timeout. */
  | "timeout"
  /** Rejected locally, before any child process was spawned. */
  | "invalid-arguments";

export interface BdErrorInit {
  kind: BdErrorKind;
  message: string;
  argv?: readonly string[];
  exitCode?: number | null;
  stderr?: string;
  stdoutSnippet?: string;
  cause?: unknown;
}

/** Every failure from this module is a `BdError` with a machine-readable `kind`. */
export class BdError extends Error {
  readonly kind: BdErrorKind;
  readonly argv: readonly string[];
  readonly exitCode: number | null;
  readonly stderr: string;
  readonly stdoutSnippet: string;

  constructor(init: BdErrorInit) {
    super(init.message, init.cause !== undefined ? { cause: init.cause } : undefined);
    this.name = "BdError";
    this.kind = init.kind;
    this.argv = init.argv ?? [];
    this.exitCode = init.exitCode ?? null;
    this.stderr = init.stderr ?? "";
    this.stdoutSnippet = init.stdoutSnippet ?? "";
  }

  static is(error: unknown): error is BdError {
    return error instanceof BdError ||
      (typeof error === "object" && error !== null && (error as { name?: string }).name === "BdError");
  }
}

export interface BdClientOptions {
  /** bd executable. Default `"bd"` (resolved via PATH). */
  readonly bin?: string;
  readonly cwd?: string;
  /** Extra env for every call. Cannot override {@link DISABLE_LAST_TOUCHED}. */
  readonly env?: Readonly<Record<string, string>>;
  /** Per-call timeout. Default 60s. */
  readonly timeoutMs?: number;
  /** Enable debug logging even if `LOOP_DEBUG` is unset. */
  readonly debug?: boolean;
  /** Debug sink. Defaults to `console.error`. */
  readonly logger?: (line: string) => void;
}

export interface ListOptions {
  readonly labels?: readonly string[];
  readonly limit?: number;
}

export interface SetStatusOptions {
  /**
   * Optimistic precondition. If the issue is not currently in this status bd
   * refuses the write and exits 13, surfaced as `kind: "guard-mismatch"`.
   */
  readonly ifStatus?: IssueStatus;
}

export interface BdClient {
  /** Blocker-aware ready work: open, unassigned-claimable, no active blockers. */
  listReady(options?: ListOptions): Promise<Issue[]>;
  /** Issues currently `in_progress`. */
  listInProgress(options?: ListOptions): Promise<Issue[]>;
  /** `null` when the issue does not exist (not an error). */
  getIssue(id: string): Promise<Issue | null>;
  createIssue(spec: NewIssueSpec): Promise<Issue>;
  /** Make `id` depend on `dependsOnId` (default edge type `blocks`). */
  addDep(id: string, dependsOnId: string, type?: string): Promise<void>;
  /**
   * `bd note <id> <text>` — append to the issue's notes field.
   *
   * Append-only on purpose: the splitter records the human's original wording
   * with it, and an append cannot clobber a note that is already there (a
   * replacement would make "what did the human actually ask for?" depend on
   * which run wrote last).
   */
  appendNote(id: string, text: string): Promise<Issue>;
  setStatus(id: string, status: IssueStatus, options?: SetStatusOptions): Promise<Issue>;
  closeIssue(id: string, reason?: string): Promise<Issue>;
  remember(text: string, key?: string): Promise<void>;
  /** `null` when no memory exists under `key` (not an error). */
  recall(key: string): Promise<string | null>;
}

/** Value that disables bd's last-touched fallback. Forced on every call. */
const DISABLE_LAST_TOUCHED = "0";

const DEFAULT_TIMEOUT_MS = 60_000;
const MAX_BUFFER_BYTES = 32 * 1024 * 1024;

/** Flags that would create a claim or write an assignee. Refused on sight. */
const FORBIDDEN_ARGS: readonly string[] = ["--assignee", "--claim", "-a"];

type Json = string | number | boolean | null | Json[] | { [key: string]: Json };

/**
 * Rule 4: no claim path, ever — checked on the assembled argv.
 *
 * @internal exported for tests; not part of the app-facing surface.
 */
export function assertClaimFreeArgs(argv: readonly string[]): void {
  const offending = argv.filter((arg) => FORBIDDEN_ARGS.includes(arg));
  if (offending.length > 0) {
    throw new BdError({
      kind: "invalid-arguments",
      message:
        `refusing to run bd with claim/assignee flags [${offending.join(", ")}]: ` +
        `this adapter must not be able to write the assignee field`,
      argv,
    });
  }
}

/** Rule 3: a mutation never runs with a missing/empty/blank identifier. */
function requireId(value: string, label: string, argv: readonly string[]): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new BdError({
      kind: "invalid-arguments",
      message: `${label} must be a non-empty string (got ${JSON.stringify(value)}); ` +
        `refusing to spawn bd where an empty variable could resolve to someone else's issue`,
      argv,
    });
  }
  return value.trim();
}

function renderArgv(argv: readonly string[]): string {
  return argv
    .map((arg) => (/[\s"]/u.test(arg) ? JSON.stringify(truncate(arg, 120)) : truncate(arg, 120)))
    .join(" ");
}

function truncate(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max)}…(+${text.length - max} chars)`;
}

interface RawRun {
  readonly stdout: string;
  readonly stderr: string;
  readonly exitCode: number;
}

/**
 * Run `bd` without a shell. Resolves for any exit code the child actually
 * reported; rejects only when the process could not run (missing binary) or was
 * killed by our timeout.
 */
function spawnBd(
  bin: string,
  argv: readonly string[],
  options: BdClientOptions,
): Promise<RawRun> {
  return new Promise<RawRun>((resolve, reject) => {
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      ...(options.env ?? {}),
      // Last so that neither process.env nor options.env can re-enable it.
      BD_LAST_TOUCHED_FALLBACK: DISABLE_LAST_TOUCHED,
    };

    execFile(
      bin,
      [...argv],
      {
        cwd: options.cwd,
        env,
        timeout: options.timeoutMs ?? DEFAULT_TIMEOUT_MS,
        killSignal: "SIGTERM",
        maxBuffer: MAX_BUFFER_BYTES,
        encoding: "utf8",
        shell: false,
      },
      (error, stdout, stderr) => {
        if (!error) {
          resolve({ stdout, stderr, exitCode: 0 });
          return;
        }
        const err = error as NodeJS.ErrnoException & {
          killed?: boolean;
          code?: string | number;
          signal?: NodeJS.Signals | null;
        };
        if (err.code === "ENOENT") {
          reject(
            new BdError({
              kind: "missing-binary",
              message:
                `bd executable "${bin}" not found. Install beads (bd) and make sure it is on PATH; ` +
                `no state was changed.`,
              argv,
              cause: error,
            }),
          );
          return;
        }
        if (err.killed || err.signal === "SIGTERM" || err.code === "ETIMEDOUT") {
          reject(
            new BdError({
              kind: "timeout",
              message: `bd timed out after ${options.timeoutMs ?? DEFAULT_TIMEOUT_MS}ms and was killed`,
              argv,
              stderr,
              cause: error,
            }),
          );
          return;
        }
        resolve({ stdout, stderr, exitCode: typeof err.code === "number" ? err.code : 1 });
      },
    );
  });
}

function isRecord(value: Json): value is { [key: string]: Json } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Edge shape (list/ready) as opposed to the inlined-issue shape (show). */
function isDependencyEdge(dep: IssueDependency): dep is DependencyEdge {
  return "depends_on_id" in dep && typeof dep.depends_on_id === "string" && dep.depends_on_id !== "";
}

/**
 * Normalise an issue's dependencies to `{id, type}` regardless of which bd
 * command produced them. Use this instead of poking at `depends_on_id`, which is
 * absent on the `bd show` shape.
 */
export function normaliseDependencies(issue: Issue): NormalisedDependency[] {
  const normalised: NormalisedDependency[] = [];
  for (const dep of issue.dependencies ?? []) {
    if (isDependencyEdge(dep)) {
      normalised.push({ id: dep.depends_on_id, type: dep.type ?? null });
      continue;
    }
    const inlined = dep as DependentIssue;
    if (typeof inlined.id === "string" && inlined.id !== "") {
      normalised.push({
        id: inlined.id,
        type: inlined.dependency_type ?? null,
        issue: inlined as unknown as Issue,
      });
    }
  }
  return normalised;
}

/** True when `issue` depends on `otherId`, in either dependency shape. */
export function dependsOn(issue: Issue, otherId: string): boolean {
  return normaliseDependencies(issue).some((dep) => dep.id === otherId);
}

/** bd returns one object for `create` and an array for most other reads. */
function asIssues(parsed: Json): Issue[] {
  const rows = Array.isArray(parsed) ? parsed : [parsed];
  return rows.filter(isRecord).map((row) => row as unknown as Issue);
}

/** bd's `show` on a missing id exits 1 with a JSON error body on stdout. */
function isIssueNotFound(parsed: Json): boolean {
  if (!isRecord(parsed)) return false;
  const error = parsed.error;
  return typeof error === "string" &&
    /not found|no issues found|does not exist/iu.test(error);
}

/** `bd recall --json` reports absence as `{"found": false}` with exit 1. */
function isMemoryNotFound(parsed: Json): boolean {
  return isRecord(parsed) && parsed.found === false;
}

/**
 * Build a client. All nine methods share one code path: assemble argv, assert,
 * log, spawn, classify, parse.
 */
export function createBdClient(options: BdClientOptions = {}): BdClient {
  const bin = options.bin ?? "bd";
  const logger = options.logger ?? ((line: string) => console.error(line));
  const debugEnabled = () => options.debug === true || process.env.LOOP_DEBUG === "1";

  function debug(message: string): void {
    if (debugEnabled()) logger(`bd[debug] ${message}`);
  }

  interface CallOptions {
    /** Predicate marking a normal "does not exist" answer. */
    readonly notFoundWhen?: (parsed: Json) => boolean;
  }

  async function call(argv: readonly string[], callOptions?: CallOptions): Promise<Json | null> {
    assertClaimFreeArgs(argv);
    // Rule 7: log before execution, not after.
    debug(`$ bd ${renderArgv(argv)}`);

    const run = await spawnBd(bin, argv, options);
    debug(`→ exit ${run.exitCode} (stdout ${run.stdout.length}B, stderr ${run.stderr.length}B)`);

    if (run.exitCode === 13) {
      throw new BdError({
        kind: "guard-mismatch",
        message:
          `bd refused [${renderArgv(argv)}]: the --if-status precondition no longer holds ` +
          `(exit 13, nothing written). Not retried — refetch state before deciding.`,
        argv,
        exitCode: 13,
        stderr: run.stderr,
      });
    }

    const mayParse = run.stdout.trim() !== "";
    const parsed = mayParse ? safeParse(run.stdout) : null;

    if (callOptions?.notFoundWhen && parsed !== null && callOptions.notFoundWhen(parsed)) {
      debug("→ treated as not-found (normal answer)");
      return null;
    }

    if (run.exitCode !== 0) {
      throw new BdError({
        kind: "exit-1",
        message: `bd failed [${renderArgv(argv)}] with exit ${run.exitCode}: ${truncate(run.stderr.trim() || run.stdout.trim() || "(no output)", 400)}`,
        argv,
        exitCode: run.exitCode,
        stderr: run.stderr,
        stdoutSnippet: truncate(run.stdout, 400),
      });
    }

    if (parsed === null) {
      throw new BdError({
        kind: "non-json",
        message: `bd exited 0 for [${renderArgv(argv)}] but wrote no parseable stdout`,
        argv,
        exitCode: 0,
        stderr: run.stderr,
        stdoutSnippet: truncate(run.stdout, 400),
      });
    }

    return parsed;
  }

  return {
    async listReady(listOptions: ListOptions = {}): Promise<Issue[]> {
      const argv: string[] = ["ready", "--json", ...listFlags(listOptions)];
      const parsed = await call(argv);
      return parsed === null ? [] : asIssues(parsed);
    },

    async listInProgress(listOptions: ListOptions = {}): Promise<Issue[]> {
      const argv: string[] = ["list", "--status", "in_progress", "--json", ...listFlags(listOptions)];
      const parsed = await call(argv);
      return parsed === null ? [] : asIssues(parsed);
    },

    async getIssue(id: string): Promise<Issue | null> {
      const issueId = requireId(id, "issue id", ["show"]);
      const argv = ["show", issueId, "--json"];
      const parsed = await call(argv, { notFoundWhen: isIssueNotFound });
      if (parsed === null) return null;
      const [first] = asIssues(parsed);
      return first ?? null;
    },

    async createIssue(spec: NewIssueSpec): Promise<Issue> {
      const argv: string[] = ["create", "--json", "--title", requireId(spec.title, "issue title", ["create"])];
      if (spec.description !== undefined) argv.push("--description", spec.description);
      if (spec.acceptance !== undefined) argv.push("--acceptance", spec.acceptance);
      if (spec.priority !== undefined) argv.push("--priority", String(spec.priority));
      if (spec.type !== undefined) argv.push("--type", spec.type);
      for (const label of spec.labels ?? []) argv.push("--labels", label);
      if (spec.parent !== undefined) argv.push("--parent", requireId(spec.parent, "parent id", argv));
      const deps = (spec.deps ?? []).map((dep) => `blocked-by:${requireId(dep, "dep id", argv)}`);
      if (deps.length > 0) argv.push("--deps", deps.join(","));

      const parsed = await call(argv);
      const [issue] = asIssues(parsed ?? []);
      if (!issue || typeof issue.id !== "string") {
        throw new BdError({
          kind: "non-json",
          message: "bd create returned no parsable issue; refusing to guess the new id",
          argv,
        });
      }
      return issue;
    },

    async addDep(id: string, dependsOnId: string, type = "blocks"): Promise<void> {
      const argv = ["dep", "add", requireId(id, "issue id", ["dep", "add"]), requireId(dependsOnId, "depends-on id", ["dep", "add"]), "--json"];
      if (type !== "blocks") argv.push("--type", type);
      await call(argv);
    },

    async appendNote(id: string, text: string): Promise<Issue> {
      const argv = [
        "note",
        requireId(id, "issue id", ["note"]),
        requireId(text, "note text", ["note"]),
        "--json",
      ];
      const parsed = await call(argv);
      const [issue] = asIssues(parsed ?? []);
      if (!issue) {
        throw new BdError({
          kind: "non-json",
          message: `bd note for ${id} returned no parsable issue`,
          argv,
        });
      }
      return issue;
    },

    async setStatus(id: string, status: IssueStatus, setStatusOptions: SetStatusOptions = {}): Promise<Issue> {
      if (!ISSUE_STATUSES.includes(status)) {
        throw new BdError({
          kind: "invalid-arguments",
          message: `"${status}" is not a bd status (valid: ${ISSUE_STATUSES.join(", ")})`,
          argv: ["update"],
        });
      }
      const argv = ["update", requireId(id, "issue id", ["update"]), "--json", "--status", status];
      if (setStatusOptions.ifStatus !== undefined) {
        if (!ISSUE_STATUSES.includes(setStatusOptions.ifStatus)) {
          throw new BdError({
            kind: "invalid-arguments",
            message: `"${setStatusOptions.ifStatus}" is not a bd status (valid: ${ISSUE_STATUSES.join(", ")})`,
            argv,
          });
        }
        argv.push("--if-status", setStatusOptions.ifStatus);
      }
      const parsed = await call(argv);
      const [issue] = asIssues(parsed ?? []);
      if (!issue) {
        throw new BdError({
          kind: "non-json",
          message: `bd update for ${id} returned no parsable issue`,
          argv,
        });
      }
      return issue;
    },

    async closeIssue(id: string, reason?: string): Promise<Issue> {
      const argv = ["close", requireId(id, "issue id", ["close"]), "--json"];
      if (reason !== undefined) argv.push("--reason", reason);
      const parsed = await call(argv);
      const [issue] = asIssues(parsed ?? []);
      if (!issue) {
        throw new BdError({
          kind: "non-json",
          message: `bd close for ${id} returned no parsable issue`,
          argv,
        });
      }
      return issue;
    },

    async remember(text: string, key?: string): Promise<void> {
      const argv = ["remember", requireId(text, "memory text", ["remember"]), "--json"];
      if (key !== undefined) argv.push("--key", requireId(key, "memory key", argv));
      await call(argv);
    },

    async recall(key: string): Promise<string | null> {
      const argv = ["recall", requireId(key, "memory key", ["recall"]), "--json"];
      const parsed = await call(argv, { notFoundWhen: isMemoryNotFound });
      if (parsed === null || !isRecord(parsed)) return null;
      const value = parsed.value;
      return typeof value === "string" && value !== "" ? value : null;
    },
  };
}

/** Shared `--label`/`-n` flags for the list-shaped reads. */
function listFlags(listOptions: ListOptions): string[] {
  const flags: string[] = [];
  for (const label of listOptions.labels ?? []) flags.push("--label", label);
  if (listOptions.limit !== undefined) flags.push("--limit", String(listOptions.limit));
  return flags;
}

/**
 * Parse that returns null instead of throwing, so a not-found body can still be
 * read. Callers must turn a `null` on a successful exit into a `non-json` error —
 * an unparseable answer is never treated as an empty list.
 */
function safeParse(stdout: string): Json | null {
  try {
    return JSON.parse(stdout.trim()) as Json;
  } catch {
    return null;
  }
}

/** Default client for callers that do not need custom options. */
export const defaultBdClient: BdClient = createBdClient();

export const listReady = defaultBdClient.listReady;
export const listInProgress = defaultBdClient.listInProgress;
export const getIssue = defaultBdClient.getIssue;
export const createIssue = defaultBdClient.createIssue;
export const addDep = defaultBdClient.addDep;
export const appendNote = defaultBdClient.appendNote;
export const setStatus = defaultBdClient.setStatus;
export const closeIssue = defaultBdClient.closeIssue;
export const remember = defaultBdClient.remember;
export const recall = defaultBdClient.recall;
