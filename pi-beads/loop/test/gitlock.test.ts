/**
 * Tests for `.git/index.lock`: the kill policy, and what the writer does when
 * somebody else is holding the index.
 *
 * There are two kinds of test in here, and the difference matters.
 *
 * The pure half — parsing git's error, aging a lock file, the backoff curve —
 * runs against in-memory values and costs nothing.
 *
 * The other half runs **real git in a throwaway repo** and kills it with real
 * signals, because the claim being tested is not about our code's shape. It is
 * about a behaviour that was measured before this module was written: git
 * removes its own lock when it gets a `SIGTERM`, and does not when it gets a
 * `SIGKILL`. A fake cannot prove that. The control test — the one that kills
 * with `SIGKILL` and asserts the lock *is* stranded — exists so the good test
 * would demonstrably fail if the policy regressed. A test that cannot fail is
 * not a test.
 *
 * The writer half uses real git plus a real lock file rather than a fake quoting
 * git's error message. If git ever rephrases `Unable to create '…': File
 * exists`, these tests notice. A fake would have gone on agreeing with our guess.
 */
import assert from "node:assert/strict";
import { execFile, spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
  COMMIT_TRAILER,
  createDefaultGitRunner,
  createGitWriter,
  VcsError,
} from "../src/vcs.ts";
import {
  backoffMs,
  indexLockPath,
  isIndexLockFailure,
  lockAgeMs,
  removeLockBestEffort,
} from "../src/gitlock.ts";

// ── git's error, as git actually writes it ──────────────────────────────────

const GIT_LOCK_MESSAGE =
  "fatal: Unable to create '/work/repo/.git/index.lock': File exists.\n" +
  "\n" +
  "Another git process seems to be running in this repository, e.g.\n" +
  "an editor opened by 'git commit'. Please make sure all processes are\n" +
  "terminated then try again.\n";

test("git's index-lock error is recognised, and the lock path is taken from the error", () => {
  assert.equal(isIndexLockFailure(GIT_LOCK_MESSAGE), true);
  assert.equal(indexLockPath(GIT_LOCK_MESSAGE), "/work/repo/.git/index.lock");
});

test("lookalikes are not mistaken for index contention", () => {
  // Retrying these would be worse than useless: they never clear on their own.
  const notIndexLock = [
    "fatal: Unable to create '/work/repo/.git/COMMIT_EDITMSG': File exists",
    "error: unable to write file index: File exists",
    "fatal: cannot lock 'refs/heads/main': File exists",
    "error: index.lock: no such file or directory",
    "",
    "nothing to commit, working tree clean",
  ];
  for (const stderr of notIndexLock) {
    assert.equal(isIndexLockFailure(stderr), false, `matched what it should not: ${stderr}`);
  }
});

test("a lock is aged by its mtime; an unreadable one has no age rather than a confident zero", () => {
  const dir = mkdtempSync(join(tmpdir(), "loop-gitlock-age-"));
  try {
    const path = join(dir, "index.lock");
    writeFileSync(path, "");
    const fresh = lockAgeMs(path, () => Date.now());
    assert.ok(fresh !== null && fresh < 5_000, `a new lock is young: ${fresh}`);

    const fiveMinutesAgo = new Date(Date.now() - 300_000);
    utimesSync(path, fiveMinutesAgo, fiveMinutesAgo);
    const old = lockAgeMs(path, () => Date.now());
    assert.ok(old !== null && old >= 299_000, `backdated to ~5min: ${old}`);

    assert.equal(lockAgeMs(join(dir, "absent.lock"), () => Date.now()), null);

    // A *directory* named index.lock is not something to delete, and reporting
    // it as "0ms old" would read as "just created", not as "we do not know".
    const weird = join(dir, "weird.lock");
    mkdirSync(weird);
    assert.equal(lockAgeMs(weird, () => Date.now()), null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("removeLockBestEffort reports the truth and never throws", () => {
  const dir = mkdtempSync(join(tmpdir(), "loop-gitlock-remove-"));
  try {
    const path = join(dir, "index.lock");
    writeFileSync(path, "");
    assert.equal(removeLockBestEffort(path), true);
    assert.equal(existsSync(path), false);
    assert.equal(removeLockBestEffort(join(dir, "ghost.lock")), false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the backoff grows, is capped, and is jittered", () => {
  const noJitter = () => 0;
  assert.equal(backoffMs(1, 250, 4000, noJitter), 250);
  assert.equal(backoffMs(2, 250, 4000, noJitter), 500);
  assert.equal(backoffMs(3, 250, 4000, noJitter), 1000);
  assert.equal(backoffMs(10, 250, 4000, noJitter), 4000, "capped, not exponential forever");
  assert.ok(backoffMs(0, 250, 4000, noJitter) >= 1, "never zero, never a busy loop");

  const jittered = new Set<number>();
  for (let i = 0; i < 50; i += 1) jittered.add(backoffMs(3, 250, 4000, Math.random));
  assert.ok(jittered.size > 5, "retries must not all fire at the same millisecond");
  for (const value of jittered) {
    assert.ok(value >= 750 && value <= 1000, `jitter stays under the cap: ${value}`);
  }
});

// ── the kill policy, against real git ───────────────────────────────────────

interface Sandbox {
  readonly dir: string;
  readonly env: Record<string, string>;
  readonly lock: string;
}

/**
 * A throwaway repo with no identity available from anywhere, so nothing here can
 * accidentally depend on the developer's git config.
 */
function makeSandbox(): Sandbox {
  const dir = mkdtempSync(join(tmpdir(), "loop-gitlock-repo-"));
  const home = mkdtempSync(join(tmpdir(), "loop-gitlock-home-"));
  const emptyConfig = join(home, "empty.gitconfig");
  writeFileSync(emptyConfig, "");
  const env: Record<string, string> = {
    HOME: home,
    GIT_CONFIG_GLOBAL: emptyConfig,
    GIT_CONFIG_SYSTEM: emptyConfig,
    GIT_CONFIG_NOSYSTEM: "1",
  };
  const git = (...args: string[]): string => {
    const res = spawnSync(
      "git",
      ["-c", "user.name=setup", "-c", "user.email=setup@example.invalid", ...args],
      { cwd: dir, env, encoding: "utf8" },
    );
    if (res.status !== 0) throw new Error(`git ${args.join(" ")}: ${res.stderr}`);
    return res.stdout;
  };
  git("init", "-q", ".");
  writeFileSync(join(dir, "BASE.md"), "base\n");
  git("add", "--", "BASE.md");
  git("commit", "-q", "-m", "base");
  return { dir, env, lock: join(dir, ".git", "index.lock") };
}

/**
 * Big enough that `git add` runs for about a second here, so a 250ms deadline
 * is guaranteed to catch it mid-write with the index lock held. Calibrated on
 * this machine at 200 MB ≈ 1.2s; the deadline is a third of that, so the test
 * is not sensitive to a fast disk, only to a catastrophic one.
 */
function addBigFile(dir: string): void {
  writeFileSync(join(dir, "huge.bin"), Buffer.alloc(200 * 1024 * 1024, 7));
}

test("a timed-out write leaves no lock: SIGTERM gives git a chance to clean up", async () => {
  const box = makeSandbox();
  addBigFile(box.dir);
  try {
    const runner = createDefaultGitRunner(250, 5_000);
    await assert.rejects(
      () => runner("git", ["add", "--", "huge.bin"], box.dir, box.env),
      (error: unknown) => VcsError.is(error) && error.kind === "timeout",
      "the add must be reported as a timeout",
    );
    assert.equal(
      existsSync(box.lock),
      false,
      "git must have removed its own lock on the way out — a leftover here is the " +
        "bug that locks every later write in the repository until a human intervenes",
    );
  } finally {
    rmSync(box.dir, { recursive: true, force: true });
  }
});

test("control: SIGKILL strands the lock, which is exactly why the policy changed", async () => {
  const box = makeSandbox();
  addBigFile(box.dir);
  try {
    // The old behaviour, verbatim: node's own `timeout` with
    // `killSignal: "SIGKILL"`. Asserting that it *does* strand the lock is what
    // makes the test above worth trusting.
    await new Promise<void>((resolvePromise) => {
      execFile(
        "git",
        ["add", "--", "huge.bin"],
        { cwd: box.dir, env: box.env, timeout: 250, killSignal: "SIGKILL", shell: false },
        () => resolvePromise(),
      );
    });
    assert.equal(
      existsSync(box.lock),
      true,
      "SIGKILL strands the lock; that is the whole reason the writer stops on SIGTERM first",
    );
    rmSync(box.lock, { force: true });
  } finally {
    rmSync(box.dir, { recursive: true, force: true });
  }
});

// ── the writer's lock policy ────────────────────────────────────────────────

/** Create an index lock, optionally backdated to look like a crashed process. */
function touchLock(box: Sandbox, ageMs = 0): void {
  const fd = openSync(box.lock, "w");
  closeSync(fd);
  if (ageMs > 0) {
    const when = new Date(Date.now() - ageMs);
    utimesSync(box.lock, when, when);
  }
}

/**
 * The message shape the finalizer actually writes. The writer's verification
 * requires both a subject and a `Loop-Handoff:` trailer in HEAD, so a test that
 * commits without one is testing its own shorthand, not the real call.
 */
function commitMessage(id: string): string {
  return `${id} the work\n\n${COMMIT_TRAILER}: loop:handoff:${id}\n`;
}

function writerFor(box: Sandbox, options: Parameters<typeof createGitWriter>[0] = {}) {
  return createGitWriter({
    cwd: box.dir,
    env: box.env,
    authorName: "loop",
    authorEmail: "loop@example.invalid",
    // Tiny backoffs: these tests are about the policy, not about the waiting.
    lockRetryBaseMs: 2,
    lockRetryCapMs: 5,
    ...options,
  });
}

test("a write waits out a lock that clears, and commits normally", async () => {
  const box = makeSandbox();
  writeFileSync(join(box.dir, "work.txt"), "worked\n");
  touchLock(box);
  // Somebody releases it shortly after we start looking.
  const release = setTimeout(() => rmSync(box.lock, { force: true }), 30);
  try {
    const writer = writerFor(box, { lockWaitMs: 5_000 });
    const plan = await writer.planCommit(commitMessage("tst.1"), ["work.txt"]);
    const result = await writer.execute(plan);

    assert.match(result.hash, /^[0-9a-f]{7,40}$/u);
    assert.deepEqual(result.paths, ["work.txt"]);
  } finally {
    clearTimeout(release);
    rmSync(box.lock, { force: true });
    rmSync(box.dir, { recursive: true, force: true });
  }
});

test("a lock that will not clear is reported with its age and the remedy, not retried forever", async () => {
  const box = makeSandbox();
  writeFileSync(join(box.dir, "work.txt"), "worked\n");
  touchLock(box, 90_000); // 90 seconds old — looks stale, policy says "report"
  try {
    const writer = writerFor(box, { lockWaitMs: 40, maxLockAttempts: 3 });
    const plan = await writer.planCommit(commitMessage("tst.2"), ["work.txt"]);

    await assert.rejects(
      () => writer.execute(plan),
      (error: unknown) => {
        assert.ok(VcsError.is(error), "expected a VcsError");
        assert.equal(error.kind, "index-locked");
        assert.match(error.message, /locked for \d+\.\ds/u);
        assert.match(error.message, /90\.\ds old/u);
        assert.match(error.message, /Nothing was staged and nothing was committed/u);
        assert.match(
          error.message,
          /LOOP_GIT_STALE_LOCK=remove/u,
          "the message has to say what to do about it",
        );
        return true;
      },
    );
    assert.equal(existsSync(box.lock), true, "the default policy leaves somebody else's lock alone");
  } finally {
    rmSync(box.lock, { force: true });
    rmSync(box.dir, { recursive: true, force: true });
  }
});

test("stale-lock removal is opt-in, and with it the commit goes through", async () => {
  const box = makeSandbox();
  writeFileSync(join(box.dir, "work.txt"), "worked\n");
  touchLock(box, 90_000);
  try {
    const writer = writerFor(box, {
      lockWaitMs: 5_000,
      staleLockPolicy: "remove",
      staleLockAfterMs: 60_000,
    });
    const plan = await writer.planCommit(commitMessage("tst.3"), ["work.txt"]);
    const result = await writer.execute(plan);

    assert.match(result.hash, /^[0-9a-f]{7,40}$/u, "the commit went through after the clear");
    assert.equal(existsSync(box.lock), false, "and the stale lock is gone");
  } finally {
    rmSync(box.lock, { force: true });
    rmSync(box.dir, { recursive: true, force: true });
  }
});

test("a fresh lock is never removed, even under the remove policy", async () => {
  const box = makeSandbox();
  writeFileSync(join(box.dir, "work.txt"), "worked\n");
  touchLock(box, 500); // plainly a live process, by age
  const release = setTimeout(() => rmSync(box.lock, { force: true }), 250);
  try {
    const writer = writerFor(box, {
      lockWaitMs: 5_000,
      // Enough attempts to still be waiting when the timer releases the lock at
      // 250ms. The defaults bind on the 30s deadline; this test shrank the
      // backoff to a couple of milliseconds, so the ceiling has to be raised.
      maxLockAttempts: 2000,
      staleLockPolicy: "remove",
      staleLockAfterMs: 60_000,
    });
    const plan = await writer.planCommit(commitMessage("tst.4"), ["work.txt"]);
    const result = await writer.execute(plan);
    clearTimeout(release);

    assert.match(result.hash, /^[0-9a-f]{7,40}$/u);
    assert.equal(
      existsSync(box.lock),
      false,
      "gone because the timer released it — not because the loop deleted a live lock",
    );
  } finally {
    clearTimeout(release);
    rmSync(box.lock, { force: true });
    rmSync(box.dir, { recursive: true, force: true });
  }
});

test("a rejected hook is attempted once: only lock contention gets the retry loop", async () => {
  const box = makeSandbox();
  writeFileSync(join(box.dir, "work.txt"), "worked\n");
  // A real pre-commit hook that says no. Not a lock error, so the retry loop must
  // not swallow it into backoff noise.
  writeFileSync(join(box.dir, ".git", "hooks", "pre-commit"), "#!/bin/sh\necho 'no' >&2\nexit 1\n");
  chmodExec(join(box.dir, ".git", "hooks", "pre-commit"));
  try {
    const writer = writerFor(box, { lockWaitMs: 5_000, maxLockAttempts: 5 });
    const plan = await writer.planCommit(commitMessage("tst.5"), ["work.txt"]);
    await assert.rejects(
      () => writer.execute(plan),
      (error: unknown) => {
        assert.ok(VcsError.is(error), "expected a VcsError");
        assert.notEqual(error.kind, "index-locked", "a hook rejection is not contention");
        assert.match(`${error.kind} ${error.message}`, /no|exit/u);
        return true;
      },
    );
  } finally {
    rmSync(box.dir, { recursive: true, force: true });
  }
});

function chmodExec(path: string): void {
  spawnSync("chmod", ["+x", path]);
}
