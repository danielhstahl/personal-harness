/**
 * Repo snapshot adapter — the second (and last) process-spawning module in `src/`.
 *
 * The loop needs one thing from git: a short, factual description of the working
 * tree at the moment an iteration starts, so the agent's context says what branch
 * it is on, what is already dirty, and what landed recently. That is `git status`,
 * `git log` and `git rev-parse` — nothing else, and never a write.
 *
 * The discipline is copied deliberately from `src/beads.ts`, because every place
 * that spawns a child process is a place that can go wrong in the same four ways:
 *
 * 1. **argv arrays only, `shell: false`.** No string is ever interpolated into a
 *    shell, so a branch named `x; rm -rf /` is a branch name, not a command.
 * 2. **Timeouts.** A hung `git` (locked index, stat storm on a huge tree) must
 *    not hang the loop; it becomes a typed `timeout` error.
 * 3. **Typed errors.** `RepoError.kind` distinguishes "this is not a git repo"
 *    (a legitimate answer the caller can degrade on) from "git is not installed"
 *    (fatal) from "git said something we could not parse" (a bug worth seeing).
 * 4. **Debug log before the spawn**, so a hung call is still visible in the log.
 *
 * This module never commits, never stages, never touches remotes. The commit that
 * finalizes an iteration is workspace-5yn.9's decision; this module only reads.
 */
import { execFile } from "node:child_process";

const DEFAULT_BIN = "git";
const DEFAULT_TIMEOUT_MS = 15_000;
const DEFAULT_RECENT_COMMITS = 8;
const DEFAULT_MAX_DIRTY_FILES = 60;
const MAX_BUFFER_BYTES = 8 * 1024 * 1024;

/** Field separator for `--pretty=format:%x1f…` — cannot occur in a commit message. */
const FS = "\u001f";

export type RepoErrorKind =
  /** The `git` binary could not be found. */
  | "missing-binary"
  /** git ran and failed with an unexpected exit code. */
  | "exit"
  /** The cwd is not inside a git working tree (git exit 128). */
  | "not-a-repo"
  /** The child was killed by our timeout. */
  | "timeout"
  /** git printed something we could not parse — a bug, not a state of the world. */
  | "unparseable"
  /** Rejected locally, before any child process was spawned. */
  | "invalid-arguments";

export interface RepoErrorInit {
  kind: RepoErrorKind;
  message: string;
  argv?: readonly string[];
  exitCode?: number | null;
  stderr?: string;
  cause?: unknown;
}

export class RepoError extends Error {
  readonly kind: RepoErrorKind;
  readonly argv: readonly string[];
  readonly exitCode: number | null;
  readonly stderr: string;

  constructor(init: RepoErrorInit) {
    super(init.message, init.cause !== undefined ? { cause: init.cause } : undefined);
    this.name = "RepoError";
    this.kind = init.kind;
    this.argv = init.argv ?? [];
    this.exitCode = init.exitCode ?? null;
    this.stderr = init.stderr ?? "";
  }

  static is(error: unknown): error is RepoError {
    return error instanceof RepoError ||
      (typeof error === "object" && error !== null && (error as { name?: string }).name === "RepoError");
  }
}

export interface RecentCommit {
  readonly sha: string;
  readonly subject: string;
  readonly author: string;
  /** git's own relative age string, e.g. "3 hours ago". */
  readonly age: string;
}

export interface DirtyFile {
  /** Path relative to the repo root. */
  readonly path: string;
  /** git's two-character status, e.g. " M", "??", "A ". */
  readonly status: string;
}

export interface RepoSnapshot {
  /** Repo top level, as git reports it. */
  readonly root: string;
  /** Branch name, or "HEAD" when detached. */
  readonly branch: string;
  readonly detached: boolean;
  readonly head: string;
  readonly recentCommits: readonly RecentCommit[];
  readonly dirtyFiles: readonly DirtyFile[];
  /** True when the dirty list was cut off at `maxDirtyFiles`. */
  readonly truncated: boolean;
  readonly hasUncommittedChanges: boolean;
  /** Set when the tree has no commits at all — a fresh repo is not an error. */
  readonly emptyRepo: boolean;
}

export interface RepoReaderOptions {
  /** Directory inside the repo. Default `process.cwd()`. */
  readonly cwd?: string;
  /** git executable. Default `"git"`. */
  readonly bin?: string;
  readonly timeoutMs?: number;
  /** How many commits to include. Default 8. */
  readonly recentCommits?: number;
  readonly maxDirtyFiles?: number;
  readonly env?: Readonly<Record<string, string>>;
  readonly debug?: boolean;
  readonly logger?: (line: string) => void;
}

export interface RepoReader {
  /** Read the whole snapshot. Throws {@link RepoError}. */
  snapshot(): Promise<RepoSnapshot>;
  /** Cheap existence check: is the cwd inside a work tree? `null` when it is not. */
  describe(): Promise<RepoSnapshot | null>;
}

interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

function trimTrailingNewline(text: string): string {
  return text.endsWith("\n") ? text.slice(0, -1) : text;
}

/** Truncate a long argv value for the debug line only; the child gets the full one. */
function logArg(value: string): string {
  return value.length > 60 ? `${value.slice(0, 57)}...` : value;
}

function run(
  bin: string,
  args: readonly string[],
  options: RepoReaderOptions,
): Promise<RunResult> {
  const argv = [bin, ...args];
  const log = options.logger ?? ((line: string) => console.error(line));
  const wantDebug = options.debug === true || process.env.LOOP_DEBUG !== undefined;
  // Logged BEFORE the spawn: a call that hangs forever must still be visible.
  if (wantDebug) {
    log(`[repo] $ ${argv.map(logArg).join(" ")}`);
  }

  return new Promise<RunResult>((resolve, reject) => {
    execFile(
      bin,
      [...args],
      {
        cwd: options.cwd ?? process.cwd(),
        env: { ...process.env, ...(options.env ?? {}) },
        maxBuffer: MAX_BUFFER_BYTES,
        shell: false,
        timeout: options.timeoutMs ?? DEFAULT_TIMEOUT_MS,
        killSignal: "SIGKILL",
      },
      (error, stdout, stderr) => {
        if (error) {
          const anyError = error as NodeJS.ErrnoException & {
            killed?: boolean;
            code?: string | number;
            signal?: string | null;
          };
          if (anyError.code === "ENOENT") {
            reject(new RepoError({
              kind: "missing-binary",
              message: `\`${bin}\` was not found on PATH`,
              argv,
              cause: error,
            }));
            return;
          }
          const timedOut = anyError.killed === true ||
            anyError.signal === "SIGKILL" ||
            anyError.code === "ETIMEDOUT";
          if (timedOut) {
            reject(new RepoError({
              kind: "timeout",
              message: `\`${bin} ${args.join(" ")}\` was killed after ` +
                `${options.timeoutMs ?? DEFAULT_TIMEOUT_MS}ms`,
              argv,
              cause: error,
            }));
            return;
          }
          const code = typeof anyError.code === "number" ? anyError.code : null;
          if (code === 128) {
            // git's generic "wrong place / bad rev" code; classified by the caller
            // because an empty repo also lands here and is not an error.
            reject(new RepoError({
              kind: "not-a-repo",
              message: trimTrailingNewline(stderr) || `git exited 128: ${args.join(" ")}`,
              argv,
              exitCode: 128,
              stderr,
              cause: error,
            }));
            return;
          }
          reject(new RepoError({
            kind: "exit",
            message: trimTrailingNewline(stderr) ||
              `\`${bin} ${args.join(" ")}\` failed with exit ${code ?? "?"}`,
            argv,
            exitCode: code,
            stderr,
            cause: error,
          }));
          return;
        }
        resolve({ stdout, stderr, exitCode: 0 });
      },
    );
  });
}

/** True when git's 128 means "repo exists, no commits yet" rather than "no repo". */
function isEmptyRepoMessage(stderr: string): boolean {
  const text = stderr.toLowerCase();
  // git phrases "this repo has no commits" several ways depending on the
  // subcommand: `log` says "does not have any commits yet", `rev-parse` says
  // "needed a single revision" or "unknown revision". All of them mean the same
  // thing to us; "not a git repository" must never be mistaken for it.
  return (text.includes("does not have any commits yet") ||
      text.includes("unknown revision") ||
      text.includes("needed a single revision") ||
      text.includes("ambiguous argument") ||
      text.includes("bad default revision")) &&
    !text.includes("not a git repository");
}

function parseRecentCommits(stdout: string, limit: number): RecentCommit[] {
  const commits: RecentCommit[] = [];
  for (const line of trimTrailingNewline(stdout).split("\n")) {
    if (line.length === 0) continue;
    const fields = line.split(FS);
    if (fields.length !== 4) {
      throw new RepoError({
        kind: "unparseable",
        message: `unexpected \`git log\` record: ${line.slice(0, 120)}`,
      });
    }
    const [sha, author, age, subject] = fields as [string, string, string, string];
    commits.push({ sha, subject, author, age });
    if (commits.length >= limit) break;
  }
  return commits;
}

function parseStatus(stdout: string, maxFiles: number): { files: DirtyFile[]; truncated: boolean } {
  const files: DirtyFile[] = [];
  let truncated = false;
  for (const line of trimTrailingNewline(stdout).split("\n")) {
    if (line.length < 4) continue;
    const status = line.slice(0, 2);
    let path = line.slice(3);
    // Renames: `R  old -> new` — the working file is the right-hand side.
    const arrow = path.indexOf(" -> ");
    if (arrow >= 0) path = path.slice(arrow + 4);
    if (files.length >= maxFiles) {
      truncated = true;
      break;
    }
    files.push({ path, status });
  }
  return { files, truncated };
}

/**
 * Format a snapshot for the agent's context. Deliberately terse and factual: the
 * loop tells the agent what the tree looks like and lets it decide what that
 * means. Pure string function — no I/O, so the context builder stays testable.
 */
export function formatRepoSnapshot(snapshot: RepoSnapshot): string {
  const lines: string[] = [
    `Repository: ${snapshot.root}`,
    `Branch: ${snapshot.branch}${snapshot.detached ? " (detached HEAD)" : ""} @ ${snapshot.head}`,
  ];

  if (snapshot.dirtyFiles.length === 0) {
    lines.push("Working tree: clean");
  } else {
    const shown = snapshot.dirtyFiles.map((f) => `  ${f.status} ${f.path}`).join("\n");
    const more = snapshot.truncated
      ? `\n  … (list truncated; more paths are dirty)`
      : "";
    lines.push(`Working tree: ${snapshot.dirtyFiles.length} changed file(s)\n${shown}${more}`);
  }

  if (snapshot.emptyRepo) {
    lines.push("Recent commits: none yet (no commits in this repo)");
  } else if (snapshot.recentCommits.length === 0) {
    lines.push("Recent commits: (none reported)");
  } else {
    lines.push(
      "Recent commits:\n" +
        snapshot.recentCommits
          .map((c) => `  ${c.sha.slice(0, 8)} ${c.subject} (${c.author}, ${c.age})`)
          .join("\n"),
    );
  }

  return lines.join("\n");
}

export function createRepoReader(options: RepoReaderOptions = {}): RepoReader {
  const bin = options.bin ?? DEFAULT_BIN;
  const recent = Math.max(0, options.recentCommits ?? DEFAULT_RECENT_COMMITS);
  const maxDirty = Math.max(1, options.maxDirtyFiles ?? DEFAULT_MAX_DIRTY_FILES);
  const call = (args: readonly string[]) => run(bin, args, { ...options, bin });

  async function snapshot(): Promise<RepoSnapshot> {
    let root: string;
    try {
      const result = await call(["rev-parse", "--show-toplevel"]);
      root = trimTrailingNewline(result.stdout);
      if (root.length === 0) {
        throw new RepoError({ kind: "unparseable", message: "`git rev-parse --show-toplevel` was empty" });
      }
    } catch (error) {
      if (RepoError.is(error) && error.kind === "not-a-repo") {
        throw new RepoError({
          kind: "not-a-repo",
          message: `no git working tree for the current directory: ${error.message}`,
          argv: error.argv,
          exitCode: error.exitCode,
          stderr: error.stderr,
        });
      }
      throw error;
    }

    // Branch is best-effort: a detached HEAD is a real state, not a failure.
    let branch = "HEAD";
    let detached = true;
    try {
      const result = await call(["rev-parse", "--abbrev-ref", "HEAD"]);
      const name = trimTrailingNewline(result.stdout);
      if (name.length > 0) {
        branch = name;
        detached = name === "HEAD";
      }
    } catch (error) {
      if (!RepoError.is(error) || error.kind !== "not-a-repo") throw error;
      // No commits yet: HEAD has no name. Fall through with the empty-repo shape.
    }

    let head = "";
    let emptyRepo = false;
    try {
      const result = await call(["rev-parse", "--short", "HEAD"]);
      head = trimTrailingNewline(result.stdout);
    } catch (error) {
      if (RepoError.is(error) && error.kind === "not-a-repo" && isEmptyRepoMessage(error.stderr)) {
        emptyRepo = true;
      } else {
        throw error;
      }
    }

    const status = await call(["status", "--porcelain=v1", "--untracked-files=all"]);
    const { files: dirtyFiles, truncated } = parseStatus(status.stdout, maxDirty);

    let recentCommits: readonly RecentCommit[] = [];
    if (!emptyRepo && recent > 0) {
      const log = await call([
        "log",
        `-n${recent}`,
        `--pretty=format:%H${FS}%an${FS}%ar${FS}%s`,
      ]);
      recentCommits = parseRecentCommits(log.stdout, recent);
    }

    return {
      root,
      branch,
      detached,
      head,
      recentCommits,
      dirtyFiles,
      truncated,
      hasUncommittedChanges: dirtyFiles.length > 0,
      emptyRepo,
    };
  }

  return {
    snapshot,
    async describe() {
      try {
        return await snapshot();
      } catch (error) {
        if (RepoError.is(error) && error.kind === "not-a-repo") return null;
        throw error;
      }
    },
  };
}
