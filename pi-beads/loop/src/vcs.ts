/**
 * `src/vcs.ts` — the git writer.
 *
 * `src/repo.ts` reads git. This module writes to it, and the split is
 * deliberate: the read side must never surprise anyone, and the write side is
 * where every scary thing lives. Concentrating the scary things in one file means
 * there is one place to audit, one spawn allowlist entry, and one set of rules.
 *
 * The rules, and why each one is here:
 *
 * 1. **A plan is built before anything is mutated.** {@link GitWriter.planCommit}
 *    performs only reads, classifies every reported path, and returns the exact
 *    argv arrays the write phase will use. A dry run prints that plan and stops;
 *    a live run executes the very same arrays. The printed plan therefore cannot
 *    drift from the executed commands — there is one builder, not a print path
 *    and a run path.
 * 2. **Only reported paths are staged, one argv per path.** `git add -- <path>`,
 *    never `git add -A`, never `git add .`, never `git commit -a`. A pathspec
 *    that happens to glob across someone else's work is the classic way a
 *    loop eats a working tree, so each path is named explicitly after the `--`.
 * 3. **Deletions are staged by the same explicit `add`.** Verified against a
 *    real git: `git add -- <deleted tracked path>` records the removal. So no
 *    blanket flag is needed to cover the delete case.
 * 4. **Paths are treated as hostile input.** A reported path that is absolute
 *    outside the repo, that walks out with `..`, that escapes through a symlink,
 *    or that carries pathspec magic (`:(glob)`, `:!x`) is refused before any
 *    command is built. Refusal is loud: the path and the reason come back as a
 *    typed block, and nothing is staged to "compensate".
 * 5. **The index is checked before we touch it.** If paths we did not report are
 *    already staged, `strict` (the default) refuses the whole commit rather
 *    than folding a stranger's staged work into ours. `lenient` proceeds, but
 *    only by committing with an explicit pathspec, so the commit still holds
 *    exactly our paths and the stranger stays in the index where we found it.
 * 6. **Nothing is invented.** An empty stageable set is a *blocked* plan
 *    (`nothing-to-commit`), not an empty commit. A commit that says "done" over
 *    an empty diff is worse than no commit: it looks like work happened.
 * 7. **A failed write leaves no partial staging.** If any mutating command in
 *    the plan fails, the paths we staged are reset out of the index (best
 *    effort, and reported) and the worktree is never touched. Caveat stated in
 *    {@link GitWriter.execute} because it deserves to be read, not buried.
 * 8. **Identity is passed on the command line, never written to config.**
 *    `-c user.name=… -c user.email=…` in front of the subcommand, so a headless
 *    run with an empty `HOME` commits without inventing a global git config and
 *    without failing on "Please tell me who you are".
 * 9. **Verification reads after the commit.** The hash is read back with
 *    `rev-parse` and the committed path list with `show --name-only`; the
 *    returned hash is git's, never a caller's claim. A mismatch is an error
 *    that still reports the hash, because a commit that exists must never be
 *    reported as if it did not.
 *
 * What this module never does: push, rewrite history, `--no-verify`,
 * `--force`, `reset --hard`, or touch the worktree. There is a source test in
 * `test/finalize.test.ts` that fails if any of those strings appear here.
 */
import { existsSync, lstatSync, realpathSync } from "node:fs";
import { isAbsolute, relative, resolve } from "node:path";

import {
  backoffMs,
  indexLockPath,
  isIndexLockFailure,
  lockAgeMs,
  removeLockBestEffort,
  runGracefully,
  sleep,
} from "./gitlock.ts";

const DEFAULT_BIN = "git";
const DEFAULT_TIMEOUT_MS = 30_000;
const MAX_BUFFER_BYTES = 8 * 1024 * 1024;

/**
 * How long a write will wait out a locked index before giving up.Thirty seconds
 * covers an IDE refreshing a big repo, which is the contention this is for.
 */
const DEFAULT_LOCK_WAIT_MS = 30_000;
/** Grace after `SIGTERM` before `SIGKILL`. See {@link runGracefully}. */
const DEFAULT_KILL_GRACE_MS = 5_000;
/** A lock older than this *looks* stale. Looking is not the same as removing. */
const DEFAULT_STALE_LOCK_AFTER_MS = 60_000;
const DEFAULT_LOCK_RETRY_BASE_MS = 250;
const DEFAULT_LOCK_RETRY_CAP_MS = 4_000;
/** A ceiling on retries, so a permanently locked repo cannot spin here forever. */
const MAX_LOCK_ATTEMPTS = 20;

/**
 * Trailer written into every loop commit. It is the join between a commit and
 * the bd memory that explains it, and it is what makes a crash between the two
 * stages recoverable: {@link GitWriter.findCommitByTrailer} can tell whether
 * this issue was already committed.
 */
export const COMMIT_TRAILER = "Loop-Handoff";

const DEFAULT_AUTHOR_NAME = "pi-loop";
const DEFAULT_AUTHOR_EMAIL = "pi-loop@localhost";

// ── errors ──────────────────────────────────────────────────────────────────

export type VcsErrorKind =
  /** The `git` binary could not be found. */
  | "missing-binary"
  /** git ran and failed. Read {@link VcsError.stderr}: usually a hook. */
  | "exit"
  /** The cwd is not inside a git work tree. */
  | "not-a-repo"
  /** Killed by our timeout (locked index, huge tree). */
  | "timeout"
  /**
   * The index was locked by something else for as long as we were willing to
   * wait. Nothing was staged and nothing was committed: this error is only ever
   * raised when git failed *before* it got the lock, so retrying is safe and
   * this run changed nothing. See [gitlock.ts](./gitlock.ts) and
   * [ADR-006](../docs/ADR-006-index-lock.md).
   */
  | "index-locked"
  /** A reported path was refused: outside the repo, symlinked out, pathspec magic. */
  | "unsafe-path"
  /** Nothing to stage. Refusing to make an empty commit. */
  | "nothing-to-commit"
  /** Unrelated paths were already staged and strictness forbids folding them in. */
  | "unrelated-staged"
  /** We staged something other than what was reported, or the commit disagrees. */
  | "verification"
  /** Rejected locally, before any child process was spawned. */
  | "invalid-arguments";

export interface VcsErrorInit {
  kind: VcsErrorKind;
  message: string;
  argv?: readonly string[];
  exitCode?: number | null;
  stderr?: string;
  cause?: unknown;
}

export class VcsError extends Error {
  readonly kind: VcsErrorKind;
  readonly argv: readonly string[];
  readonly exitCode: number | null;
  readonly stderr: string;

  constructor(init: VcsErrorInit) {
    super(init.message, init.cause !== undefined ? { cause: init.cause } : undefined);
    this.name = "VcsError";
    this.kind = init.kind;
    this.argv = init.argv ?? [];
    this.exitCode = init.exitCode ?? null;
    this.stderr = init.stderr ?? "";
  }

  static is(error: unknown): error is VcsError {
    return error instanceof VcsError ||
      (typeof error === "object" && error !== null && (error as { name?: string }).name === "VcsError");
  }
}

// ── commands and plans ──────────────────────────────────────────────────────

/**
 * One git invocation. `argv` deliberately excludes the binary: the plan and the
 * live spawn are then compared on identical terms, and the recorder in the test
 * suite (a fake `git` that writes argv to a file) matches the printed plan with
 * no normalising in between.
 */
export interface GitCommand {
  readonly phase: "read" | "write";
  readonly argv: readonly string[];
  readonly note: string;
}

export interface SkippedPath {
  readonly path: string;
  readonly reason: string;
}

export interface RefusedPath {
  readonly path: string;
  readonly reason: string;
}

/** Why a plan will not produce a commit. The plan is still returned, for the log. */
export type CommitBlock =
  | { kind: "nothing-to-commit"; message: string }
  | { kind: "unsafe-path"; message: string; paths: readonly string[] }
  | { kind: "unrelated-staged"; message: string; paths: readonly string[] };

export interface CommitPlan {
  /** Reads and writes in the order they run. The writes are what dry-run shows. */
  readonly commands: readonly GitCommand[];
  /** Paths that will actually be staged, in the order reported. */
  readonly stageable: readonly string[];
  readonly skipped: readonly SkippedPath[];
  readonly refused: readonly RefusedPath[];
  /** Paths already in the index when we looked, none of them ours. */
  readonly preexistingStaged: readonly string[];
  readonly blocking: CommitBlock | null;
  readonly strictness: Strictness;
  readonly repoRoot: string;
  /**
   * The exact message this plan will put in the commit. Carried on the plan so
   * the write phase can prove that HEAD is *this* commit and not somebody
   * else's that happened to land first: the paths alone cannot tell those apart.
   */
  readonly commitMessage: string;
}

export type Strictness = "strict" | "lenient";

export interface CommitResult {
  /** Read back from git after the commit, not handed to us. */
  readonly hash: string;
  /** The paths git says the commit contains. */
  readonly paths: readonly string[];
  /** True when git's committed list equals {@link CommitPlan.stageable}. */
  readonly exact: boolean;
}

// ── spawning ────────────────────────────────────────────────────────────────

export interface RawGitResult {
  readonly stdout: string;
  readonly stderr: string;
  readonly exitCode: number;
}

/**
 * Injectable spawn. The default implementation is `execFile` with `shell:false`;
 * tests inject a recorder or a script that returns a chosen exit code so a hook
 * rejection can be simulated without installing hooks in a temp repo.
 */
export type GitRunner = (
  bin: string,
  args: readonly string[],
  cwd: string,
  env: Readonly<Record<string, string>>,
) => Promise<RawGitResult>;

function trimTrailingNewline(text: string): string {
  return text.endsWith("\n") ? text.slice(0, -1) : text;
}

/** Truncate for the debug line only; the child always gets the full value. */
function logArg(value: string): string {
  return value.length > 72 ? `${value.slice(0, 69)}...` : value;
}

/**
 * The default spawn: `execFile` with `shell:false` and a two-stage kill.
 *
 * The signal order is not a detail. `SIGKILL` cannot be caught, so git never
 * runs the cleanup that removes `.git/index.lock`, and the repo is left with a
 * lock that fails every later write — including this loop's own. `SIGTERM` does
 * get caught, git cleans up, and the timeout costs one command instead of the
 * repository. Measured, not assumed:
 * [ADR-006](../docs/ADR-006-index-lock.md).
 */
async function defaultRunner(
  bin: string,
  args: readonly string[],
  cwd: string,
  env: Readonly<Record<string, string>>,
  timeoutMs: number,
  killGraceMs: number,
): Promise<RawGitResult> {
  const outcome = await runGracefully(bin, args, cwd, { ...process.env, ...env }, {
    timeoutMs,
    killGraceMs,
    maxBufferBytes: MAX_BUFFER_BYTES,
  });

  if (outcome.killedBy !== null) {
    throw new VcsError({
      kind: "timeout",
      message: `\`${bin} ${args.join(" ")}\` was killed after ${timeoutMs}ms ` +
        `(stopped with ${outcome.killedBy})`,
      argv: [bin, ...args],
      ...(outcome.spawnError === undefined ? {} : { cause: outcome.spawnError }),
    });
  }
  const spawnError = outcome.spawnError as NodeJS.ErrnoException | undefined;
  if (spawnError !== undefined) {
    if (spawnError.code === "ENOENT") {
      throw new VcsError({
        kind: "missing-binary",
        message: `\`${bin}\` was not found on PATH`,
        argv: [bin, ...args],
        cause: spawnError,
      });
    }
    // Non-zero exit is a *result*, not a spawn failure: callers classify it by
    // context (a rejected hook and an empty repo both exit non-zero and mean very
    // different things).
    const code = typeof spawnError.code === "number" ? spawnError.code : null;
    return { stdout: outcome.stdout, stderr: outcome.stderr, exitCode: code ?? 1 };
  }
  return { stdout: outcome.stdout, stderr: outcome.stderr, exitCode: outcome.exitCode ?? 0 };
}

// ── path safety ─────────────────────────────────────────────────────────────

/**
 * Normalise one reported path against the repo root.
 *
 * `null` means "refuse, with this reason". Every rejection here is a case where
 * a plausible-looking report could stage something its author did not mean:
 * `../` walks out, an absolute path can name another project, a symlink can
 * lead out at write time, and git's pathspec magic turns a filename into a
 * matcher that names other files.
 */
export function classifyPathSafety(root: string, reported: string): { path: string } | { refuse: string } {
  if (typeof reported !== "string" || reported.trim() === "") {
    return { refuse: "path is empty" };
  }
  const raw = reported.trim();
  if (raw.includes("\0")) return { refuse: "path contains a NUL byte" };
  if (raw.startsWith("-")) return { refuse: "path starts with '-' (option injection)" };
  // git's pathspec magic would turn an exact name into a matcher over others.
  if (raw.startsWith(":") || raw.includes(":(")) {
    return { refuse: "path carries git pathspec magic (e.g. ':(glob)', ':!exclude')" };
  }
  const windowsStyle = raw.includes("\\");
  const slashNormalised = windowsStyle ? raw.replace(/\\/g, "/") : raw;
  if (isAbsolute(slashNormalised)) {
    const rel = relative(resolve(root), resolve(slashNormalised));
    if (rel.startsWith("..") || isAbsolute(rel)) {
      return { refuse: `absolute path is outside the repository (${raw})` };
    }
  }
  let normalised = slashNormalised.replace(/^\.\/+/, "").replace(/\/+$/, "");
  if (isAbsolute(normalised)) {
    // An absolute path inside the repo is the same file, named in full. What
    // `git add` matches against and what `git status` reports are both
    // repo-root-relative, so translate it here. Without this the request stops
    // matching its own file and the plan comes back `nothing to commit` — a
    // wrong answer wearing an honest face.
    normalised = relative(resolve(root), resolve(normalised));
  }
  if (normalised === "" || normalised === ".") return { refuse: "path resolves to the repo root" };
  const parts = normalised.split("/").filter((part) => part !== "");
  if (parts.some((part) => part === "..")) {
    return { refuse: `path walks outside the repository (${raw})` };
  }
  if (parts[0] === ".git") return { refuse: "refusing to stage anything inside .git" };
  const insideRepo = resolve(root, normalised);
  const relCheck = relative(resolve(root), insideRepo);
  if (relCheck.startsWith("..") || isAbsolute(relCheck)) {
    return { refuse: `path resolves outside the repository (${raw})` };
  }
  // Symlink escape: if something exists at this path (or at its nearest
  // existing ancestor), its real location must still be inside the repo.
  const escape = symlinkEscape(root, insideRepo);
  if (escape !== null) return { refuse: escape };
  return { path: normalised };
}

function symlinkEscape(root: string, insideRepo: string): string | null {
  const realRoot = safeRealpath(root);
  if (realRoot === null) return null;
  let probe = insideRepo;
  for (;;) {
    const realProbe = safeRealpath(probe);
    if (realProbe !== null) {
      const rel = relative(realRoot, realProbe);
      if (rel.startsWith("..") || isAbsolute(rel)) {
        return `path resolves outside the repository through a symlink (${probe} -> ${realProbe})`;
      }
      return null;
    }
    const parent = resolve(probe, "..");
    if (parent === probe) return null;
    probe = parent;
  }
}

function safeRealpath(target: string): string | null {
  try {
    // lstat first: a symlink's own realpath would resolve the link, which is
    // exactly the thing we are checking for.
    if (!existsSync(target) && lstatSyncSafe(target) === null) return null;
    return realpathSync(target);
  } catch {
    return null;
  }
}

function lstatSyncSafe(target: string) {
  try {
    return lstatSync(target);
  } catch {
    return null;
  }
}

// ── the writer ──────────────────────────────────────────────────────────────

export interface GitWriterOptions {
  /** Directory inside the repository. Default `process.cwd()`. */
  readonly cwd?: string;
  readonly bin?: string;
  readonly timeoutMs?: number;
  /**
   * How long the child gets to stop after `SIGTERM` before it is killed.
   * Default 5s. This is the window in which git writes the index and removes its
   * own lock; shrinking it is what leaves a stale `.git/index.lock` behind.
   */
  readonly killGraceMs?: number;
  /** How long a write will wait out somebody else's index lock. Default 30s. */
  readonly lockWaitMs?: number;
  readonly lockRetryBaseMs?: number;
  readonly lockRetryCapMs?: number;
  /** Ceiling on lock retries, so a permanently locked repo cannot spin here. */
  readonly maxLockAttempts?: number;
  /** A lock older than this *looks* stale. Default 60s. */
  readonly staleLockAfterMs?: number;
  /**
   * What to do about a lock that looks stale.
   *
   * `"report"` (the default) — wait out the deadline, then fail with an error
   * naming the lock's age and the one-line fix. `"remove"` — delete it once and
   * retry, and say so in the log. Removal is opt-in because a lock held by a
   * live process is load-bearing and pulling it mid-write corrupts the index,
   * which is a far worse injury than the one being cured.
   */
  readonly staleLockPolicy?: "report" | "remove";
  readonly env?: Readonly<Record<string, string>>;
  /** Committer identity, passed as `-c` flags. Never written to config. */
  readonly authorName?: string;
  readonly authorEmail?: string;
  /** Default `strict`: unrelated staged paths block the commit outright. */
  readonly strictness?: Strictness;
  /** Injected for tests. Defaults to `execFile` with `shell: false`. */
  readonly run?: GitRunner;
  readonly debug?: boolean;
  readonly logger?: (line: string) => void;
}

export interface GitWriter {
  /** Repo root. Throws {@link VcsError} kind `not-a-repo` when there is none. */
  repoRoot(): Promise<string>;
  /**
   * Read the tree and build the plan. Performs NO mutations. The returned
   * {@link CommitPlan.blocking} says whether the write phase would be refused,
   * and why — so a caller can decide before anything changes.
   */
  planCommit(message: string, reportedPaths: readonly string[]): Promise<CommitPlan>;
  /**
   * Run the write phase of a plan, then read the result back.
   *
   * On failure, the paths this plan staged are reset out of the index (best
   * effort) and the error says so. Caveat worth reading twice: that reset moves
   * those index entries back to HEAD. The worktree is never touched, so no file
   * content is lost — but if a human had hand-staged a *different* version of
   * one of the reported paths before we ran, that staged variant is unstaged.
   * Strict mode makes this almost impossible by refusing to run at all when the
   * index holds anything unexpected, which is why it is the default.
   */
  execute(plan: CommitPlan): Promise<CommitResult>;
  /** Find an earlier commit carrying `Loop-Handoff: <value>`. `null` when absent. */
  findCommitByTrailer(value: string): Promise<string | null>;
  /**
   * Paths a given commit contains, as git reports them (repo-root-relative).
   * `[]` when the hash is unreadable — an absent answer, never a guess.
   */
  commitPaths(hash: string): Promise<string[]>;
  /** `git rev-parse --verify HEAD`, or `null` on a repo with no commits. */
  headHash(): Promise<string | null>;
}

function splitLines(stdout: string): string[] {
  return trimTrailingNewline(stdout)
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
}

const SHA_RE = /^[0-9a-f]{7,40}$/;

/**
 * Build a git writer bound to one repository.
 *
 * The plan/execute split is the whole point: {@link GitWriter.planCommit} can be
 * called by a dry run and by the live run alike, so the printed plan is not a
 * description of the work but the work's own argv.
 */
export function createGitWriter(options: GitWriterOptions = {}): GitWriter {
  const bin = options.bin ?? DEFAULT_BIN;
  const cwd = options.cwd ?? process.cwd();
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const killGraceMs = options.killGraceMs ?? DEFAULT_KILL_GRACE_MS;
  const lockWaitMs = options.lockWaitMs ?? DEFAULT_LOCK_WAIT_MS;
  const lockRetryBaseMs = options.lockRetryBaseMs ?? DEFAULT_LOCK_RETRY_BASE_MS;
  const lockRetryCapMs = options.lockRetryCapMs ?? DEFAULT_LOCK_RETRY_CAP_MS;
  const maxLockAttempts = options.maxLockAttempts ?? MAX_LOCK_ATTEMPTS;
  const staleLockAfterMs = options.staleLockAfterMs ?? DEFAULT_STALE_LOCK_AFTER_MS;
  const staleLockPolicy = options.staleLockPolicy ?? "report";
  const env = options.env ?? {};
  const authorName = options.authorName ?? DEFAULT_AUTHOR_NAME;
  const authorEmail = options.authorEmail ?? DEFAULT_AUTHOR_EMAIL;
  const strictness: Strictness = options.strictness ?? "strict";
  const log = options.logger ?? ((line: string) => console.error(line));
  const wantDebug = options.debug === true || process.env.LOOP_DEBUG !== undefined;
  const injected = options.run;

  const runner: GitRunner = injected
    ? injected
    : (b, args, dir, e) => defaultRunner(b, args, dir, e, timeoutMs, killGraceMs);

  async function git(args: readonly string[]): Promise<RawGitResult> {
    // Logged BEFORE the spawn: a call that never returns is still visible.
    if (wantDebug) log(`[vcs] $ ${[bin, ...args].map(logArg).join(" ")}`);
    return runner(bin, [...args], cwd, { ...env });
  }

  /**
   * A git **write** command, retried while somebody else holds the index lock.
   *
   * Lock contention is ordinary: `git status` takes the lock to refresh the stat
   * cache, an IDE refreshes the repo, the agent session runs its own git, a
   * second run of this loop is looking at the same tree. Failing the first time
   * git says "File exists" treats a two-hundred-millisecond collision as a
   * broken build.
   *
   * Only two things are retried, and both are safe by construction:
   *
   * - the index-lock error, which by definition means git did not get the lock
   *   and therefore changed nothing — so retrying the same command cannot
   *   double-apply anything; and
   * - nothing else. A rejected hook, an unsafe path, a corrupt object comes
   *   back exactly as it was, once, for the caller to classify.
   *
   * A lock older than `staleLockAfterMs` *looks* abandoned. By default that is
   * reported, not acted on; `staleLockPolicy: "remove"` clears it once and
   * retries, loudly in the log.
   */
  async function runWrite(argv: readonly string[]): Promise<RawGitResult> {
    const startedAt = Date.now();
    let lockAttempts = 0;
    let removedStaleLock = false;

    for (;;) {
      const result = await git(argv);
      if (result.exitCode === 0) return result;
      if (!isIndexLockFailure(result.stderr)) return result;

      lockAttempts += 1;
      const lockPath = indexLockPath(result.stderr);
      const ageMs = lockPath === null ? null : lockAgeMs(lockPath);
      const age = ageMs === null ? "unknown age" : `${(ageMs / 1000).toFixed(1)}s old`;

      if (
        staleLockPolicy === "remove" &&
        lockPath !== null &&
        !removedStaleLock &&
        ageMs !== null &&
        ageMs >= staleLockAfterMs
      ) {
        removedStaleLock = true;
        const removed = removeLockBestEffort(lockPath);
        log(
          `[vcs] ${lockPath} was ${age}, which looks like a crashed process rather than a ` +
            `running one; removal ${removed ? "succeeded" : "failed"} and the write is retried once. ` +
            `If a git process really was still running, this is the line to look at — ` +
            `LOOP_GIT_STALE_LOCK=report is the default and would have waited instead.`,
        );
        continue;
      }

      const waitedMs = Date.now() - startedAt;
      if (lockAttempts >= maxLockAttempts || waitedMs >= lockWaitMs) {
        throw new VcsError({
          kind: "index-locked",
          message:
            `the git index has been locked for ${(waitedMs / 1000).toFixed(1)}s ` +
            `(${lockAttempts} retries; the lock is ${age}). Nothing was staged and nothing was ` +
            `committed. ` +
            (
              ageMs !== null && ageMs >= staleLockAfterMs
                ? `A lock that old usually means a git process was killed partway through: if no ` +
                  `git is running, remove ${lockPath ?? ".git/index.lock"} and run again — or set ` +
                  `LOOP_GIT_STALE_LOCK=remove to let the loop clear locks older than ` +
                  `${(staleLockAfterMs / 1000).toFixed(0)}s by itself.`
                : `Something is holding it right now. Wait for it, or raise ` +
                  `LOOP_GIT_LOCK_WAIT_MS if that thing is legitimately slow.`
            ),
          argv: [bin, ...argv],
          stderr: result.stderr,
        });
      }

      const delayMs = backoffMs(lockAttempts, lockRetryBaseMs, lockRetryCapMs);
      log(
        `[vcs] the git index is locked (${age}); retrying in ${delayMs}ms ` +
          `(${lockAttempts}/${maxLockAttempts}, up to ${(lockWaitMs / 1000).toFixed(0)}s total)`,
      );
      await sleep(delayMs);
    }
  }

  function identityArgs(): string[] {
    // `-c` must precede the subcommand. This is how a headless run with an empty
    // HOME commits without inventing config on disk.
    return ["-c", `user.name=${authorName}`, "-c", `user.email=${authorEmail}`];
  }

  async function repoRoot(): Promise<string> {
    const res = await git(["rev-parse", "--show-toplevel"]);
    if (res.exitCode !== 0) {
      throw new VcsError({
        kind: "not-a-repo",
        message: trimTrailingNewline(res.stderr) || `\`git rev-parse\` failed in ${cwd}`,
        argv: [bin, "rev-parse", "--show-toplevel"],
        exitCode: res.exitCode,
        stderr: res.stderr,
      });
    }
    const root = trimTrailingNewline(res.stdout).trim();
    if (root === "") {
      throw new VcsError({
        kind: "not-a-repo",
        message: `\`git rev-parse --show-toplevel\` printed nothing in ${cwd}`,
        argv: [bin, "rev-parse", "--show-toplevel"],
      });
    }
    return root;
  }

  async function trackedPaths(root: string, paths: readonly string[]): Promise<Set<string>> {
    if (paths.length === 0) return new Set();
    const res = await git(["ls-files", "--", ...paths]);
    if (res.exitCode !== 0) return new Set();
    const found = new Set(splitLines(res.stdout).map((line) => relativePath(root, line)));
    return found;
  }

  async function stagedPaths(): Promise<string[]> {
    const res = await git(["diff", "--cached", "--name-only"]);
    if (res.exitCode !== 0) return [];
    return splitLines(res.stdout);
  }

  /**
   * Which reported paths git considers changed: staged, unstaged, deleted, or
   * untracked. Read with `-z` so a path with unusual bytes comes back verbatim
   * instead of octal-escaped.
   *
   * This read exists because `existsSync` is not an answer to "is there anything
   * to commit?". A tracked path whose content already equals HEAD is not dirty:
   * `git add` on it succeeds while changing nothing, and the commit then dies
   * with "nothing to commit" — which, after a run that committed and then lost
   * the handoff write, would turn a recoverable state into a failure. Git's own
   * answer is the only honest one.
   *
   * `git status --porcelain` reports from the repository root, so entries are
   * folded back into this writer's directory to line up with the reported paths.
   */
  async function dirtyPaths(argv: readonly string[], root: string): Promise<Set<string>> {
    const res = await git(argv);
    if (res.exitCode !== 0) return new Set();
    const found = new Set<string>();
    for (const record of res.stdout.split("\0")) {
      // Each record is "XY <path>"; anything shorter carried no path.
      if (record.length < 4) continue;
      const reported = record.slice(3);
      if (reported === "") continue;
      const folded = relative(resolve(cwd), resolve(root, reported));
      found.add(folded === "" || folded.startsWith("..") ? reported : folded);
    }
    return found;
  }

  function relativePath(root: string, value: string): string {
    if (!isAbsolute(value)) return value;
    const rel = relative(resolve(root), resolve(value));
    return rel.startsWith("..") && !isAbsolute(rel) ? value : rel;
  }

  async function planCommit(
    message: string,
    reportedPaths: readonly string[],
  ): Promise<CommitPlan> {
    const commandList: GitCommand[] = [];
    const skipped: SkippedPath[] = [];
    const refused: RefusedPath[] = [];

    if (typeof message !== "string" || message.trim() === "") {
      throw new VcsError({
        kind: "invalid-arguments",
        message: "commit message must be a non-empty string",
        argv: [],
      });
    }

    commandList.push({
      phase: "read",
      argv: ["rev-parse", "--show-toplevel"],
      note: "find the repository root",
    });
    const root = await repoRoot();

    const safe: string[] = [];
    for (const reported of reportedPaths) {
      const verdict = classifyPathSafety(root, reported);
      if ("refuse" in verdict) {
        refused.push({ path: String(reported), reason: verdict.refuse });
      } else {
        safe.push(verdict.path);
      }
    }

    commandList.push({
      phase: "read",
      argv: ["diff", "--cached", "--name-only"],
      note: "look at the index before touching it",
    });
    const preexisting = await stagedPaths();
    const reportedSet = new Set(safe);
    const foreign = preexisting.filter((path) => !reportedSet.has(path));

    const tracked = await trackedPaths(root, safe);
    if (safe.length > 0) {
      commandList.push({
        phase: "read",
        argv: ["ls-files", "--", ...safe],
        note: "which reported paths git already tracks (a deleted one is still tracked)",
      });
    }

    // The dirty read is recorded before it runs, so a dry run prints the exact
    // argv the live run uses, and the classification below is driven by git
    // rather than by a look at the filesystem.
    const statusArgv: string[] = [
      "status",
      "--porcelain",
      "--untracked-files=all",
      "--no-renames",
      "-z",
      ...(safe.length > 0 ? ["--", ...safe] : []),
    ];
    if (safe.length > 0) {
      commandList.push({
        phase: "read",
        argv: statusArgv,
        note: "which reported paths differ from HEAD — staged, unstaged, deleted, or new",
      });
    }
    const dirty = safe.length > 0 ? await dirtyPaths(statusArgv, root) : new Set<string>();

    const stageable: string[] = [];
    const seen = new Set<string>();
    for (const path of safe) {
      if (seen.has(path)) continue;
      seen.add(path);
      if (dirty.has(path)) {
        stageable.push(path);
      } else if (tracked.has(path)) {
        skipped.push({
          path,
          reason: "tracked but identical to HEAD — nothing to commit for it",
        });
      } else {
        skipped.push({
          path,
          reason: "not in the working tree, not tracked, and no change to stage",
        });
      }
    }

    let blocking: CommitBlock | null = null;
    if (refused.length > 0) {
      blocking = {
        kind: "unsafe-path",
        message: `refused ${refused.length} unsafe path(s): ` +
          refused.map((r) => `${r.path} (${r.reason})`).join("; "),
        paths: refused.map((r) => r.path),
      };
    } else if (stageable.length === 0) {
      blocking = {
        kind: "nothing-to-commit",
        message: "no reported path produced anything to stage — refusing to make an empty commit",
      };
    } else if (strictness === "strict" && foreign.length > 0) {
      blocking = {
        kind: "unrelated-staged",
        message: `${foreign.length} path(s) were already staged and were not reported: ` +
          `${foreign.join(", ")}. Nothing was staged; unsettle the index or run lenient.`,
        paths: foreign,
      };
    }

    if (blocking === null) {
      for (const path of stageable) {
        commandList.push({
          phase: "write",
          argv: ["add", "--", path],
          note: existsSync(resolve(root, path)) ? "stage" : "stage deletion",
        });
      }      const commitArgv: string[] = [...identityArgs(), "commit", "-m", message];
      if (strictness === "lenient") {
        // Narrow the commit by pathspec so pre-existing staged work stays out of
        // it. Under strict this is unnecessary: nothing foreign is in the index.
        commitArgv.push("--", ...stageable);
      }
      commandList.push({
        phase: "write",
        argv: commitArgv,
        note: strictness === "lenient"
          ? "commit only the reported paths"
          : "commit the index, which holds only the reported paths",
      });
      commandList.push({
        phase: "read",
        argv: ["rev-parse", "HEAD"],
        note: "read the new hash back instead of trusting a claim",
      });
      commandList.push({
        phase: "read",
        argv: ["show", "--name-only", "--format=", "HEAD"],
        note: "read the committed path list back",
      });
    }

    return {
      commands: commandList,
      stageable,
      skipped,
      refused,
      preexistingStaged: foreign,
      blocking,
      strictness,
      repoRoot: root,
      commitMessage: message,
    };
  }

  async function execute(plan: CommitPlan): Promise<CommitResult> {
    if (plan.blocking !== null) {
      throw new VcsError({
        kind: plan.blocking.kind,
        message: plan.blocking.message,
        argv: [],
      });
    }

    const writes = plan.commands.filter((command) => command.phase === "write");
    let ranWrites = 0;
    try {
      for (const command of writes) {
        const res = await runWrite(command.argv);
        ranWrites += 1;
        if (res.exitCode !== 0) {
          throw new VcsError({
            kind: "exit",
            message: trimTrailingNewline(res.stderr) ||
              `\`git ${command.argv.join(" ")}\` failed with exit ${res.exitCode}`,
            argv: [bin, ...command.argv],
            exitCode: res.exitCode,
            stderr: res.stderr,
          });
        }
      }
    } catch (error) {
      // Only unwind what we actually did. If the very first write failed there
      // is nothing of ours in the index and resetting would be theatre.
      if (ranWrites > 0 && VcsError.is(error) && error.kind === "exit") {
        await unstage(plan.stageable);
        error.message = `${error.message} (staged paths were reset out of the index: ` +
          `${plan.stageable.join(", ")}; the working tree was not touched)`;
      }
      throw error;
    }

    const hashRes = await git(["rev-parse", "HEAD"]);
    const hash = trimTrailingNewline(hashRes.stdout).trim();
    if (hashRes.exitCode !== 0 || !SHA_RE.test(hash)) {
      throw new VcsError({
        kind: "verification",
        message: `committed but could not read the new hash back ` +
          `(exit ${hashRes.exitCode}, stdout ${JSON.stringify(hash.slice(0, 40))})`,
        argv: [bin, "rev-parse", "HEAD"],
        exitCode: hashRes.exitCode,
        stderr: hashRes.stderr,
      });
    }

    const shownRes = await git(["show", "--name-only", "--format=", "HEAD"]);
    const committed = splitLines(shownRes.stdout).map((line) => relativePath(plan.repoRoot, line));
    const expected = [...plan.stageable].sort();
    const actual = [...committed].sort();
    const exact = expected.length === actual.length && expected.every((p, i) => actual[i] === p);
    if (!exact) {
      // The commit exists. Say so, with the hash, and refuse to pretend it is
      // the one that was asked for.
      throw new VcsError({
        kind: "verification",
        message: `commit ${hash} does not contain exactly the reported paths — ` +
          `expected [${expected.join(", ")}], got [${actual.join(", ")}]`,
        argv: [bin, "show", "--name-only", "--format=", "HEAD"],
        stderr: shownRes.stderr,
      });
    }

    // HEAD must be *our* commit. Two commits can hold the same path list; only
    // the message identifies who made it. Read it back from `git log -1` and
    // require both the subject and the handoff trailer.
    const bodyRes = await git(["log", "-1", "--pretty=%B"]);
    const subject = plan.commitMessage.split("\n")[0]?.trim() ?? "";
    const trailerLine = `${COMMIT_TRAILER}: `;
    if (
      bodyRes.exitCode !== 0 ||
      subject === "" ||
      !bodyRes.stdout.includes(subject) ||
      !bodyRes.stdout.includes(trailerLine)
    ) {
      throw new VcsError({
        kind: "verification",
        message:
          `commit ${hash} is not the commit this plan made: HEAD's message carries neither ` +
          `the subject ${JSON.stringify(subject)} nor a ${JSON.stringify(trailerLine)} trailer. ` +
          `The commit exists — do not assume it was someone else's.`,
        argv: [bin, "log", "-1", "--pretty=%B"],
        exitCode: bodyRes.exitCode,
        stderr: bodyRes.stderr,
      });
    }

    return { hash, paths: committed, exact: true };
  }

  async function unstage(paths: readonly string[]): Promise<void> {
    if (paths.length === 0) return;
    try {
      // `-q` because the output is noise here; `--` so a path can never be read
      // as an option. This is a soft unstage: the worktree is left alone.
      // Went through `runWrite` rather than `git` so an unstage that hits a
      // contended index waits for it too: leaving our paths in the index because
      // an IDE was refreshing is exactly the state the next commit would trip on.
      await runWrite(["reset", "-q", "--", ...paths]);
    } catch {
      // Best effort. The original failure is the one worth reporting; the caller
      // is told we tried.
    }
  }

  async function findCommitByTrailer(value: string): Promise<string | null> {
    if (typeof value !== "string" || value.trim() === "") {
      throw new VcsError({
        kind: "invalid-arguments",
        message: "trailer value must be a non-empty string",
        argv: [],
      });
    }
    const res = await git([
      "log",
      "--fixed-strings",
      `--grep=${COMMIT_TRAILER}: ${value}`,
      "--format=%H",
      "-n",
      "1",
    ]);
    if (res.exitCode !== 0) return null; // including "no commits yet"
    const line = splitLines(res.stdout)[0] ?? "";
    return SHA_RE.test(line) ? line : null;
  }

  async function commitPaths(hash: string): Promise<string[]> {
    if (!SHA_RE.test(hash)) return [];
    const res = await git(["show", "--name-only", "--format=", hash]);
    if (res.exitCode !== 0) return [];
    // git prints these relative to the repo root whatever the cwd is, so no
    // remapping happens here and nothing is invented when the read fails.
    return splitLines(res.stdout);
  }

  async function headHash(): Promise<string | null> {
    const res = await git(["rev-parse", "--verify", "--quiet", "HEAD"]);
    if (res.exitCode !== 0) return null;
    const line = trimTrailingNewline(res.stdout).trim();
    return SHA_RE.test(line) ? line : null;
  }

  return {
    repoRoot,
    planCommit,
    execute,
    findCommitByTrailer,
    commitPaths,
    headHash,
  };
}

/**
 * Render a plan the way a dry run prints it: `git <argv>`, one per line, with
 * reads marked so a human can tell what would change from what would only be
 * looked at. Uses the configured binary so the printed line is runnable.
 */
export function renderPlan(
  commands: readonly GitCommand[],
  options: { readonly bin?: string } = {},
): string[] {
  const binName = options.bin ?? DEFAULT_BIN;
  return commands.map((command) => {
    const line = `${binName} ${command.argv.join(" ")}`;
    return command.phase === "read" ? `read  ${line}` : `WRITE ${line}`;
  });
}

/** Every mutating command in a plan, as `bin argv…` strings. */
export function renderWrites(commands: readonly GitCommand[], binName = DEFAULT_BIN): string[] {
  return commands
    .filter((command) => command.phase === "write")
    .map((command) => `${binName} ${command.argv.join(" ")}`);
}

/** Where the loop puts its identity when nothing else does. */
export const LOOP_AUTHOR = { name: DEFAULT_AUTHOR_NAME, email: DEFAULT_AUTHOR_EMAIL } as const;

/**
 * The production spawn, exported so a test can wrap it: a recorder that
 * delegates to the real runner is the only way to prove the plan a dry run
 * printed and the process a live run spawned are the same bytes.
 */
export function createDefaultGitRunner(
  timeoutMs: number = DEFAULT_TIMEOUT_MS,
  killGraceMs: number = DEFAULT_KILL_GRACE_MS,
): GitRunner {
  return (bin, args, cwd, env) => defaultRunner(bin, args, cwd, env, timeoutMs, killGraceMs);
}
