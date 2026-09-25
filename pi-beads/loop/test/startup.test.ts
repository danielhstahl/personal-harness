/**
 * Tests for `src/startup.ts` — the comparison wired to files and a server.
 *
 * Nothing here touches a real home directory, a real `models.json`, or the
 * network: the reads, the writes and the fetch are all injected, which is the
 * only way to test the paths that matter — the file that is not there, the
 * server that will not answer, the two-providers-and-no-default case where
 * guessing would be worse than asking.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { buildApp } from "../src/app.ts";
import { createNullPresenter } from "../src/render.ts";
import { proposedPathFor, resolveTarget, runStartupAudit } from "../src/startup.ts";
import type { StartupAuditResult } from "../src/startup.ts";

const HEALTH = JSON.parse(
  readFileSync(new URL("./fixtures/health.json", import.meta.url), "utf8"),
) as Record<string, unknown>;

const CONFIG = {
  providers: {
    halogen: {
      baseUrl: "http://inference.test:8081/v1",
      api: "openai-completions",
      apiKey: "hello",
      compat: {
        supportsDeveloperRole: false,
        supportsReasoningEffort: false,
        thinkingFormat: "chat-template",
        chatTemplateKwargs: { preserve_thinking: true },
      },
      models: [
        {
          id: "halogen-qwen3.8-flash-next",
          reasoning: true,
          maxTokens: 16_384,
          contextWindow: 256_000,
          samplingParams: { temperature: 1.0 },
        },
      ],
    },
  },
};

interface Harness {
  readonly files: Map<string, string>;
  readonly written: Map<string, string>;
  readonly readFile: (path: string) => string;
  readonly writeFile: (path: string, contents: string) => void;
  readonly fileExists: (path: string) => boolean;
}

function harness(files: Record<string, string> = {}): Harness {
  const store = new Map(Object.entries(files));
  const written = new Map<string, string>();
  return {
    files: store,
    written,
    readFile: (path) => {
      const value = store.get(path);
      if (value === undefined) throw new Error(`ENOENT ${path}`);
      return value;
    },
    writeFile: (path, contents) => {
      written.set(path, contents);
    },
    fileExists: (path) => store.has(path),
  };
}

const MODELS = "/agent/models.json";

function responding(): typeof fetch {
  return (async () => new Response(JSON.stringify(HEALTH), { status: 200 })) as typeof fetch;
}

function silent(): typeof fetch {
  return (async () => new Response("nope", { status: 503 })) as typeof fetch;
}

async function runWith(
  overrides: Partial<Parameters<typeof runStartupAudit>[0]> = {},
): Promise<{ result: StartupAuditResult; fs: Harness }> {
  const fs = harness({ [MODELS]: JSON.stringify(CONFIG) });
  const result = await runStartupAudit({
    cwd: "/repo",
    modelsPath: MODELS,
    agentDir: "/agent",
    defaultRef: () => ({ provider: "halogen", id: "halogen-qwen3.8-flash-next" }),
    fetchImpl: responding(),
    readFile: fs.readFile,
    writeFile: fs.writeFile,
    fileExists: fs.fileExists,
    ...overrides,
  });
  return { result, fs };
}

// ── target resolution ────────────────────────────────────────────────────

test("an explicit provider and model wins over everything on disk", () => {
  const resolved = resolveTarget(CONFIG, {
    explicit: { provider: "halogen", id: "halogen-qwen3.8-flash-next" },
  });
  assert.equal(resolved.target?.provider, "halogen");
  assert.equal(resolved.target?.modelIndex, 0, "a raw entry is patched in place, not via overrides");
  assert.equal(resolved.view?.baseUrl, "http://inference.test:8081/v1");
});

test("one provider with one model needs no default to be configured", () => {
  const resolved = resolveTarget(CONFIG, {});
  assert.equal(resolved.target?.provider, "halogen");
  assert.equal(resolved.target?.modelId, "halogen-qwen3.8-flash-next");
});

test("two providers and no default is reported, not guessed", () => {
  const two = {
    providers: {
      a: { baseUrl: "http://a/v1", models: [{ id: "a1" }, { id: "a2" }] },
      b: { baseUrl: "http://b/v1", models: [{ id: "b1" }] },
    },
  };
  const resolved = resolveTarget(two, {});
  assert.equal(resolved.target, undefined);
  assert.match(resolved.note ?? "", /2 providers/);
  assert.deepEqual(resolved.candidates, ["a/a1", "a/a2", "b/b1"]);
});

test("the saved default id picks its provider when there is only one", () => {
  const resolved = resolveTarget(CONFIG, { fallback: { id: "halogen-qwen3.8-flash-next" } });
  assert.equal(resolved.target?.provider, "halogen");
});

test("an explicit target that is not declared falls back to the overrides path", () => {
  const resolved = resolveTarget(CONFIG, { explicit: { provider: "halogen", id: "a-catalog-model" } });
  assert.equal(resolved.target?.modelIndex, undefined, "no raw entry → no index");
  assert.equal(resolved.view?.hasRawEntry, false);
  assert.equal(resolved.view?.baseUrl, "http://inference.test:8081/v1", "the provider block still applies");
});

// ── the run ──────────────────────────────────────────────────────────────

test("a warm server gives a report and the gaps are named in the lines", async () => {
  const { result } = await runWith();
  assert.equal(result.status, "audited");
  assert.ok(result.report);
  assert.equal(result.report?.healthState, "ok");
  const text = result.lines.join("\n");
  assert.match(text, /provider audit — halogen\/halogen-qwen3\.8-flash-next/);
  assert.match(text, /context-window-undersized/);
  assert.match(text, /thinking-effort-never-sent/);
  assert.match(text, /suggested change/);
});

test("an unreachable server is a line, not a failure of the run", async () => {
  const { result } = await runWith({ fetchImpl: silent() });
  assert.equal(result.status, "unreachable");
  assert.match(result.lines.join("\n"), /probe failed: HTTP 503/);
  assert.equal(result.report?.healthState, "unread");
});

test("a config file that is not there says so and stops", async () => {
  const fs = harness({});
  const result = await runStartupAudit({
    cwd: "/repo",
    modelsPath: MODELS,
    agentDir: "/agent",
    readFile: fs.readFile,
    writeFile: fs.writeFile,
    fileExists: fs.fileExists,
    fetchImpl: responding(),
  });
  assert.equal(result.status, "no-config");
  assert.match(result.lines.join("\n"), /no models\.json/);
});

test("a file that is not JSON is a config problem, not a crash", async () => {
  const fs = harness({ [MODELS]: "{not json" });
  const result = await runStartupAudit({
    cwd: "/repo",
    modelsPath: MODELS,
    agentDir: "/agent",
    readFile: fs.readFile,
    writeFile: fs.writeFile,
    fileExists: fs.fileExists,
    fetchImpl: responding(),
  });
  assert.equal(result.status, "no-config");
  assert.match(result.lines.join("\n"), /could not read/);
});

test("no derivable health URL is reported with the fix, not a stack trace", async () => {
  const config = {
    providers: { weird: { baseUrl: "not-a-url", models: [{ id: "m" }] } },
  };
  const fs = harness({ [MODELS]: JSON.stringify(config) });
  const result = await runStartupAudit({
    cwd: "/repo",
    modelsPath: MODELS,
    agentDir: "/agent",
    defaultRef: () => ({ provider: "weird", id: "m" }),
    readFile: fs.readFile,
    writeFile: fs.writeFile,
    fileExists: fs.fileExists,
    fetchImpl: responding(),
  });
  assert.equal(result.status, "unreachable");
  assert.match(result.lines.join("\n"), /cannot derive a health URL/);
  assert.match(result.lines.join("\n"), /LOOP_HEALTH_URL/);
});

test("an explicit health URL override is used verbatim", async () => {
  let asked = "";
  const { result } = await runWith({
    healthUrl: "http://elsewhere.test/status",
    fetchImpl: (async (url: unknown) => {
      asked = String(url);
      return new Response(JSON.stringify(HEALTH), { status: 200 });
    }) as typeof fetch,
  });
  assert.equal(asked, "http://elsewhere.test/status");
  assert.equal(result.status, "audited");
});

// ── writing the suggestion ───────────────────────────────────────────────

test("nothing is written unless it was asked for", async () => {
  const { fs } = await runWith({ writeMode: "none" });
  assert.equal(fs.written.size, 0);
});

test("the proposed file is written beside models.json and is valid JSON", async () => {
  const { result, fs } = await runWith({ writeMode: "proposed" });
  assert.equal(result.writtenTo, `${MODELS}.proposed`);
  const text = fs.written.get(`${MODELS}.proposed`);
  assert.ok(text);
  const patched = JSON.parse(text) as typeof CONFIG;
  const model = patched.providers.halogen.models[0];
  assert.ok(model);
  assert.equal(model.contextWindow, 262_144);
  assert.equal((model.maxTokens as number) > 16_384, true);
  assert.equal(patched.providers.halogen.apiKey, "hello", "the file keeps the credential it already had");
  const kwargs = (patched.providers.halogen.compat as Record<string, unknown>).chatTemplateKwargs as Record<string, unknown>;
  assert.deepEqual(kwargs.reasoning_effort, { $var: "thinking.effort" });
});

test("the printable proposed form redacts the credential the file keeps", async () => {
  const { result } = await runWith({ writeMode: "proposed" });
  assert.ok(result.proposedJson);
  assert.match(result.proposedJson, /«redacted»/);
  assert.doesNotMatch(result.proposedJson, /hello/);
});

test("in-place mode backs the original up before it overwrites it", async () => {
  const { result, fs } = await runWith({ writeMode: "inplace" });
  assert.equal(result.writtenTo, MODELS);
  assert.ok(fs.written.get(`${MODELS}.bak`), "a backup of what was there before");
  assert.equal(JSON.parse(fs.written.get(`${MODELS}.bak`) ?? "{}").providers.halogen.models[0].contextWindow, 256_000);
  assert.equal(JSON.parse(fs.written.get(MODELS) ?? "{}").providers.halogen.models[0].contextWindow, 262_144);
});

test("proposedPathFor names the two destinations distinctly", () => {
  assert.equal(proposedPathFor("/a/models.json", "proposed"), "/a/models.json.proposed");
  assert.equal(proposedPathFor("/a/models.json", "inplace"), "/a/models.json");
  assert.equal(proposedPathFor("/a/models.json", "none"), "/a/models.json.proposed", "mode none never gets here");
});

test("a clean config writes nothing, because there is nothing to suggest", async () => {
  const aligned = {
    providers: {
      halogen: {
        baseUrl: "http://inference.test:8081/v1",
        api: "openai-completions",
        apiKey: "hello",
        compat: {
          thinkingFormat: "chat-template",
          supportsStrictMode: false,
          chatTemplateKwargs: {
            enable_thinking: { $var: "thinking.enabled" },
            reasoning_effort: { $var: "thinking.effort" },
            preserve_thinking: true,
          },
        },
        models: [
          {
            id: "halogen-qwen3.8-flash-next",
            reasoning: true,
            maxTokens: 32_768,
            contextWindow: 262_144,
            input: ["text"],
          },
        ],
      },
    },
  };
  const fs = harness({ [MODELS]: JSON.stringify(aligned) });
  const result = await runStartupAudit({
    cwd: "/repo",
    modelsPath: MODELS,
    agentDir: "/agent",
    defaultRef: () => ({ provider: "halogen", id: "halogen-qwen3.8-flash-next" }),
    fetchImpl: responding(),
    readFile: fs.readFile,
    writeFile: fs.writeFile,
    fileExists: fs.fileExists,
    writeMode: "proposed",
    levels: { work: "high", split: "low" },
  });
  assert.equal(result.report?.suggestions.length, 0);
  assert.equal(result.writtenTo, undefined, "no suggestion, no file");
  assert.equal(result.blocking, false);
});

// ── the loop's own use of it ─────────────────────────────────────────────

/**
 * Point pi's agent dir at a temp directory holding `CONFIG`.
 *
 * The app-level path reads pi's own `getAgentDir()`, which reads the
 * environment when it is called — so the audit's config source is controlled
 * here without threading a `modelsPath` through the app for testing alone.
 */
function withAgentDir(contents: unknown, run: () => Promise<void>): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "loop-audit-"));
  writeFileSync(join(dir, "models.json"), JSON.stringify(contents, null, 2));
  const previous = process.env.PI_CODING_AGENT_DIR;
  process.env.PI_CODING_AGENT_DIR = dir;
  return run().finally(() => {
    if (previous === undefined) delete process.env.PI_CODING_AGENT_DIR;
    else process.env.PI_CODING_AGENT_DIR = previous;
    rmSync(dir, { recursive: true, force: true });
  });
}

test("buildApp runs the audit before the loop and survives an unfriendly server", async () => {
  const notices: string[] = [];
  await withAgentDir(CONFIG, async () => {
    const app = buildApp({
      cwd: "/repo",
      providerAudit: { enabled: true, timeoutMs: 200 },
      overrides: {
        presenter: {
          ...createNullPresenter(),
          notice: (level: string, text: string) => notices.push(`${level}:${text.split("\n")[0]}`),
        } as never,
        beads: { listReady: async () => [], listInProgress: async () => [] } as never,
        git: undefined as never,
        audit: { fetchImpl: silent() },
      },
      maxIterations: 0,
    });
    // The run itself has no bd and no runner here; what is under test is that the
    // audit ran first, printed, and did not take the process down with it.
    await app.run().catch(() => undefined);
  });
  assert.ok(
    notices.some((line) => line.includes("provider audit")),
    `expected an audit notice, got: ${notices.join(" | ")}`,
  );
});

test("strict mode stops the run before the loop starts", async () => {
  let loopStarted = false;
  // Strict only bites on an `error`-severity finding, so the config here has one:
  // a window bigger than the server's KV pool can hold.
  const broken = JSON.parse(JSON.stringify(CONFIG)) as Record<string, unknown>;
  const provider = ((broken.providers as Record<string, unknown>).halogen as Record<string, unknown>);
  const models = provider.models as Record<string, unknown>[];
  models[0]!.contextWindow = 300_000;
  await withAgentDir(broken, async () => {
    const app = buildApp({
      cwd: "/repo",
      providerAudit: { enabled: true, strict: true },
      overrides: {
        presenter: createNullPresenter(),
        beads: {
          listReady: async () => {
            loopStarted = true;
            return [];
          },
          listInProgress: async () => [],
        } as never,
        git: undefined as never,
        audit: { fetchImpl: responding() },
      },
    });
    await assert.rejects(() => app.run(), /provider-audit|startup audit stopped the run/);
  });
  assert.equal(loopStarted, false, "strict means the loop never got to the board");
});

test("disabling the audit means no request is made at all", async () => {
  let asked = 0;
  await withAgentDir(CONFIG, async () => {
    const app = buildApp({
      cwd: "/repo",
      providerAudit: { enabled: false },
      overrides: {
        presenter: createNullPresenter(),
        beads: { listReady: async () => [], listInProgress: async () => [] } as never,
        git: undefined as never,
        audit: {
          fetchImpl: (async () => {
            asked += 1;
            return new Response("{}", { status: 200 });
          }) as typeof fetch,
        },
      },
      maxIterations: 0,
    });
    await app.run().catch(() => undefined);
  });
  assert.equal(asked, 0);
});

test("skipAudit turns it off even when the setting says otherwise", async () => {
  let asked = 0;
  await withAgentDir(CONFIG, async () => {
    const app = buildApp({
      cwd: "/repo",
      providerAudit: { enabled: true },
      overrides: {
        presenter: createNullPresenter(),
        beads: { listReady: async () => [], listInProgress: async () => [] } as never,
        git: undefined as never,
        skipAudit: true,
        audit: {
          fetchImpl: (async () => {
            asked += 1;
            return new Response("{}", { status: 200 });
          }) as typeof fetch,
        },
      },
      maxIterations: 0,
    });
    await app.run().catch(() => undefined);
  });
  assert.equal(asked, 0);
});
