/**
 * Everything this loop knows about `.git/index.lock`.
 *
 * Two facts about git, both measured rather than assumed (git 2.47, throwaway
 * repos), and both are the whole reason this module exists.
 *
 * **1. `SIGKILL` leaves `.git/index.lock` behind. `SIGTERM` does not.**
 *
 * git removes its own lock on every exit path it gets to run. A process that is
 * killed with `SIGKILL` gets none of them, so the lock survives — and a surviving
 * lock is not one failed command, it is *every later write to that repository*,
 * forever, until a human deletes the file. A timeout that stops a child with
 * `SIGKILL` therefore converts a slow commit into an unusable repo. `SIGTERM`
 * costs the same and cleans up:
 *
 * ```text
 *   git add -- huge.bin        (500 MB, ~2.9s here, lock held throughout)
 *   kill -TERM during add  ->  lock removed by git
 *   kill -KILL during add  ->  LOCK LEFT BEHIND
 * ```
 *
 * So the kill policy is: **`SIGTERM` first, `SIGKILL` only as an escalation**
 * after a grace period. `exec`'s own `timeout` + `killSignal` pair can deliver
 * exactly one signal, which is why the timer lives here instead.
 *
 * **2. `git status` takes the index lock too.**
 *
 * A status run that has to refresh the stat cache *writes* the index, so a
 * reader and a writer in one repository genuinely collide — contention is the
 * normal condition of a repo with several things looking at it (an IDE, a second
 * run, the agent session's own git, this loop's reader and this loop's writer),
 * not a sign of a bug. `fatal: Unable to create '…/index.lock': File exists.` is
 * therefore something to *wait out* and re-check, not something to report as a
 * failure the first time it appears.
 *
 * What is deliberately **not** done: unilaterally deleting a lock. A lock held by
 * a live process is load-bearing, and ripping it out mid-write is how a repository
 * gets a corrupt index — a much worse injury than the one being cured. Removal is
 * offered as an explicit opt-in, gated on the lock being older than a stated age
 * (`staleLockPolicy: "remove"`), and the default is to report the age and the
 * remedy instead.
 */
import { execFile } from "node:child_process";
import { statSync, unlinkSync } from "node:fs";

/**
 * git's own words when the index is already locked. Matched on the pair rather
 * than on `File exists` alone, because that phrase appears in other git errors
 * about other files and a false positive would put a retry loop behind an error
 * that will never clear.
 */
const INDEX_LOCK_ERROR = /Unable to create '([^']*index\.lock)':\s*File exists/iu;

/** True when this stderr is the index-lock contention error. */
export function isIndexLockFailure(stderr: string): boolean {
  return INDEX_LOCK_ERROR.test(stderr ?? "");
}

/**
 * The lock file git was trying to create, as git named it.
 *
 * Taken from the error rather than guessed at, because it is the path git itself
 * tried — which is the path whose age matters. `null` when the message does not
 * name one.
 */
export function indexLockPath(stderr: string): string | null {
  const match = INDEX_LOCK_ERROR.exec(stderr ?? "");
  return match?.[1] ? match[1] : null;
}

/**
 * How long this lock has been sitting there, in ms, or `null` when there is no
 * lock to age (already gone, or unreadable).
 *
 * The mtime of a git lock is set when git creates it and never updated, so its
 * age *is* how long it has been held. `null` means "unknown", and unknown is
 * never treated as stale: not being able to prove a lock is old is a reason to
 * wait, not a reason to delete.
 */
export function lockAgeMs(path: string, now: () => number = Date.now): number | null {
  try {
    const stats = statSync(path);
    if (!stats.isFile()) return null;
    return Math.max(0, now() - stats.mtimeMs);
  } catch {
    return null;
  }
}

/** Best-effort removal. Returns whether the file is gone afterwards. */
export function removeLockBestEffort(path: string): boolean {
  try {
    unlinkSync(path);
    // True means *this call removed it*. "It is not there any more" would be a
    // different claim, and a misleading one in the log: a lock that vanished on
    // its own is somebody else's doing, and the line that says "removal
    // succeeded" would have taken credit for it.
    return true;
  } catch {
    // Already gone, or not ours to remove. Either way the caller retries and
    // finds out shortly after.
    return false;
  }
}

/**
 * A jittered backoff: `base * 2^(attempt-1)`, capped, with up to 25% of it
 * removed at random.
 *
 * The jitter is the point. Two processes that start retrying at the same instant
 * with the same fixed schedule keep colliding on exactly the same milliseconds
 * forever; a little randomness breaks the lockstep. It is also why the rng is a
 * parameter: a test that wants deterministic numbers can pass a stub.
 */
export function backoffMs(
  attempt: number,
  baseMs: number,
  capMs: number,
  random: () => number = Math.random,
): number {
  const exponential = baseMs * Math.pow(2, Math.max(0, attempt - 1));
  const capped = Math.min(capMs, exponential);
  const jitter = capped * 0.25 * random();
  return Math.max(1, Math.round(capped - jitter));
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, Math.max(0, ms));
  });
}

// ── the spawn policy ────────────────────────────────────────────────────────

/**
 * What came back from one child process, before any module decides what it means.
 *
 * Deliberately neutral between `VcsError` and `RepoError`: `src/vcs.ts` and
 * `src/repo.ts` classify the same bytes into different types, and a shared
 * helper that picked one of them would make the other one translate.
 */
export interface ChildOutcome {
  readonly stdout: string;
  readonly stderr: string;
  /** The child's exit code, or `null` if a signal or a failed spawn ended it. */
  readonly exitCode: number | null;
  /** The signal that ended it, when a signal ended it. */
  readonly signal: NodeJS.Signals | null;
  /**
   * The signal *we* sent, when we sent one. This is what makes "timed out" a
   * reported fact rather than an inference from `killed === true`, which node
   * also sets for other reasons.
   */
  readonly killedBy: NodeJS.Signals | null;
  /** The spawn error, when the child never ran (`ENOENT` and friends). */
  readonly spawnError?: unknown;
}

export interface GracefulRunOptions {
  /** How long the child may run before it is asked politely to stop. */
  readonly timeoutMs: number;
  /**
   * How long it gets to stop after `SIGTERM` before it is killed outright.
   *
   * Five seconds is generous on purpose. This is the window in which git runs
   * its cleanup and writes the index out; making it tight is what reintroduces
   * the stale lock this exists to prevent.
   */
  readonly killGraceMs: number;
  readonly maxBufferBytes?: number;
  readonly onTimeout?: (signal: NodeJS.Signals, waitedMs: number) => void;
}

/**
 * Run a child process with a two-stage kill.
 *
 * `SIGTERM` at the deadline; `SIGKILL` only if the child is still there
 * `killGraceMs` later. Nothing else about the child is unusual: no shell, no
 * `timeout` option of node's own (it can send one signal, and the whole point
 * here is to send two in sequence).
 */
export function runGracefully(
  bin: string,
  args: readonly string[],
  cwd: string,
  env: NodeJS.ProcessEnv,
  options: GracefulRunOptions,
): Promise<ChildOutcome> {
  return new Promise<ChildOutcome>((resolvePromise) => {
    let killedBy: NodeJS.Signals | null = null;
    const startedAt = Date.now();
    let killTimer: ReturnType<typeof setTimeout> | undefined;

    const termTimer = setTimeout(() => {
      killedBy = "SIGTERM";
      options.onTimeout?.("SIGTERM", Date.now() - startedAt);
      child.kill("SIGTERM");
      killTimer = setTimeout(() => {
        if (child.exitCode === null && child.signalCode === null) {
          killedBy = "SIGKILL";
          options.onTimeout?.("SIGKILL", Date.now() - startedAt);
          child.kill("SIGKILL");
        } else {
          clearTimeout(killTimer);
          killTimer = undefined;
        }
      }, Math.max(0, options.killGraceMs));
    }, Math.max(1, options.timeoutMs));

    const child = execFile(
      bin,
      [...args],
      {
        cwd,
        env,
        maxBuffer: options.maxBufferBytes,
        shell: false,
      },
      (error, stdout, stderr) => {
        clearTimeout(termTimer);
        if (killTimer !== undefined) clearTimeout(killTimer);
        const anyError = error as
          | (NodeJS.ErrnoException & { killed?: boolean; signal?: string | null })
          | null;
        resolvePromise({
          stdout: stdout ?? "",
          stderr: stderr ?? "",
          exitCode: anyError === null ? 0 : typeof anyError?.code === "number" ? anyError.code : null,
          signal: (anyError?.signal as NodeJS.Signals | null | undefined) ?? child.signalCode ?? null,
          killedBy,
          ...(anyError !== null && anyError !== undefined ? { spawnError: anyError } : {}),
        });
      },
    );
  });
}
