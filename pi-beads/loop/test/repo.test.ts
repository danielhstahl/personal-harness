/**
 * Tests for `src/repo.ts` — the read-only git snapshot the context builder reads.
 *
 * These run against a real `git` in a throwaway directory: the point of this
 * module is that it reports the tree truthfully, and a fake would prove nothing
 * about exit codes, porcelain formats, or detached HEAD.
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import { createRepoReader, formatRepoSnapshot, RepoError } from "../src/repo.js";

const tempDirs: string[] = [];

function makeTempRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "loop-repo-"));
  tempDirs.push(dir);
  return dir;
}

function git(cwd: string, ...args: string[]): string {
  return execFileSync(
    "git",
    ["-c", "user.email=loop@test", "-c", "user.name=Loop Test", "-c", "commit.gpgsign=false", ...args],
    { cwd, encoding: "utf8" },
  );
}

function seedRepo(withCommit: boolean): string {
  const dir = makeTempRepo();
  git(dir, "init", "-q", "--initial-branch=main");
  if (withCommit) {
    writeFileSync(join(dir, "seed.txt"), "seed\n");
    git(dir, "add", "seed.txt");
    git(dir, "commit", "-q", "-m", "chore: seed the repo");
  }
  return dir;
}

after(() => {
  for (const dir of tempDirs) rmSync(dir, { recursive: true, force: true });
});

test("a snapshot reports branch, head, recent commits and dirty files", async () => {
  const dir = seedRepo(true);
  writeFileSync(join(dir, "dirty.txt"), "uncommitted\n");

  const reader = createRepoReader({ cwd: dir });
  const snapshot = await reader.snapshot();

  assert.equal(snapshot.branch, "main");
  assert.equal(snapshot.detached, false);
  assert.match(snapshot.head, /^[0-9a-f]{7,}$/u);
  assert.equal(snapshot.emptyRepo, false);
  assert.equal(snapshot.hasUncommittedChanges, true);
  assert.equal(snapshot.recentCommits.length, 1);
  assert.equal(snapshot.recentCommits[0]?.subject, "chore: seed the repo");
  assert.equal(snapshot.recentCommits[0]?.author, "Loop Test");
  assert.ok((snapshot.recentCommits[0]?.age ?? "").length > 0, "git's own age string is kept");

  const paths = snapshot.dirtyFiles.map((file) => file.path);
  assert.ok(paths.includes("dirty.txt"), `dirty.txt missing from ${paths.join(", ")}`);
});

test("a clean tree says so, in the snapshot and in the formatted text", async () => {
  const dir = seedRepo(true);
  const snapshot = await createRepoReader({ cwd: dir }).snapshot();

  assert.equal(snapshot.hasUncommittedChanges, false);
  assert.deepEqual(snapshot.dirtyFiles, []);
  assert.match(formatRepoSnapshot(snapshot), /Working tree: clean/u);
});

test("a detached HEAD is reported as detached, not as an error", async () => {
  const dir = seedRepo(true);
  const sha = git(dir, "rev-parse", "HEAD").trim();
  git(dir, "checkout", "-q", "--detach", sha);

  const snapshot = await createRepoReader({ cwd: dir }).snapshot();
  assert.equal(snapshot.detached, true);
  assert.equal(snapshot.branch, "HEAD");
  assert.match(formatRepoSnapshot(snapshot), /detached HEAD/u);
});

test("the dirty list truncates instead of flooding the prompt", async () => {
  const dir = seedRepo(true);
  for (let index = 0; index < 6; index += 1) {
    writeFileSync(join(dir, `file-${index}.txt`), `content ${index}\n`);
  }

  const reader = createRepoReader({ cwd: dir, maxDirtyFiles: 2 });
  const snapshot = await reader.snapshot();

  assert.equal(snapshot.dirtyFiles.length, 2, "the cap is honoured");
  assert.equal(snapshot.truncated, true);
  assert.equal(snapshot.hasUncommittedChanges, true);
  assert.match(formatRepoSnapshot(snapshot), /truncated/u);
});

test("a fresh repo with no commits is a state, not a failure", async () => {
  const dir = seedRepo(false);
  writeFileSync(join(dir, "untracked.txt"), "nothing committed yet\n");

  const snapshot = await createRepoReader({ cwd: dir }).snapshot();
  assert.equal(snapshot.emptyRepo, true);
  assert.deepEqual(snapshot.recentCommits, []);
  assert.equal(snapshot.hasUncommittedChanges, true);
  assert.match(formatRepoSnapshot(snapshot), /none yet/u);
});

test("outside a work tree: describe() is null and snapshot() is typed", async () => {
  const dir = mkdtempSync(join(tmpdir(), "loop-not-a-repo-"));
  tempDirs.push(dir);

  const reader = createRepoReader({ cwd: dir });
  assert.equal(await reader.describe(), null, "not a repo must be answerable, not fatal");

  await assert.rejects(
    () => reader.snapshot(),
    (error: unknown) => RepoError.is(error) && error.kind === "not-a-repo",
  );
});

test("a missing git binary is a typed error, never an empty snapshot", async () => {
  const dir = seedRepo(true);
  const reader = createRepoReader({ cwd: dir, bin: "definitely-not-a-git-binary" });

  await assert.rejects(
    () => reader.snapshot(),
    (error: unknown) => RepoError.is(error) && error.kind === "missing-binary",
  );
  // `describe` must not swallow a missing binary: that is a broken install, not
  // "we are not in a repo".
  await assert.rejects(
    () => reader.describe(),
    (error: unknown) => RepoError.is(error) && error.kind === "missing-binary",
  );
});

test("from a subdirectory the root reported is the repo top level", async () => {
  const dir = seedRepo(true);
  const nested = join(dir, "src", "deep");
  const { mkdirSync } = await import("node:fs");
  mkdirSync(nested, { recursive: true });
  writeFileSync(join(nested, "note.txt"), "deep\n");

  const snapshot = await createRepoReader({ cwd: nested }).snapshot();
  assert.equal(snapshot.root, git(dir, "rev-parse", "--show-toplevel").trim());
  assert.ok(snapshot.dirtyFiles.map((file) => file.path).includes("src/deep/note.txt"));
});

test("taking a snapshot never changes the repository", async () => {
  const dir = seedRepo(true);
  writeFileSync(join(dir, "before.txt"), "dirty before\n");
  const before = git(dir, "status", "--porcelain=v1", "--untracked-files=all");
  const beforeLog = git(dir, "log", "--oneline");

  await createRepoReader({ cwd: dir }).snapshot();
  await createRepoReader({ cwd: dir }).describe();

  assert.equal(git(dir, "status", "--porcelain=v1", "--untracked-files=all"), before);
  assert.equal(git(dir, "log", "--oneline"), beforeLog);
});

test("recentCommits=0 asks git for no log at all", async () => {
  const dir = seedRepo(true);
  const snapshot = await createRepoReader({ cwd: dir, recentCommits: 0 }).snapshot();
  assert.deepEqual(snapshot.recentCommits, []);
  assert.equal(snapshot.emptyRepo, false, "an explicit zero must not read as an empty repo");
});

test("the formatted snapshot is one block of plain text a prompt can carry", async () => {
  const dir = seedRepo(true);
  writeFileSync(join(dir, "changed.txt"), "x\n");
  const formatted = formatRepoSnapshot(await createRepoReader({ cwd: dir }).snapshot());

  assert.match(formatted, /Repository: /u);
  assert.match(formatted, /Branch: main @ /u);
  assert.match(formatted, /Working tree: 1 changed file\(s\)/u);
  assert.match(formatted, /Recent commits:/u);
  assert.match(formatted, /chore: seed the repo/u);
  assert.equal(formatted.includes("undefined"), false);
  assert.equal(formatted.includes("null"), false);
});
