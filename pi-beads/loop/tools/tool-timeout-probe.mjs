#!/usr/bin/env node
/**
 * The ADR-010 probe: what pi 0.85.1 really does when a harness wraps a tool
 * call, asked of the SDK rather than read off it.
 *
 *   node tools/tool-timeout-probe.mjs
 *
 * Sections, each printing observed output and nothing else:
 *
 *   Q1  does a `customTools` entry named `bash` shadow the built-in?
 *   Q2  does `noTools:"builtin"` + pi's own factories stand in for the built-ins?
 *   K1  does a wrapper-owned AbortController actually kill the child process tree?
 *   K2  does forwarding the inbound (session.abort) signal still kill it?
 *   K3  does the wrapper still return when the inner tool ignores abort?
 *   K4  is the wrapper the thing a real session actually runs?
 *   K5  is the partial output before the kill recoverable?
 *   P0  what settings does the session read its built-in definitions from?
 *   P1  is the system prompt byte-identical when the shadow spreads the real definition?
 *   P2  what does a shadow with no `promptSnippet` do to the tool list?
 *   P3  what does a duplicate tool name do — diagnostic, or silent last-wins?
 *   P4  does a shadow compose with `tools` / `excludeTools`?
 *
 * Not a test — the assertions in
 * docs/ADR-010-tool-call-timeout.md were read off this output. No prompt is
 * sent and no network is needed: sessions are constructed for their tool
 * registry, and `execute` is called directly. Requires a resolvable model in
 * the ModelRuntime (any configured provider; nothing is asked of it).
 */
import { execFileSync } from "node:child_process";
import {
  createAgentSession,
  createBashToolDefinition,
  createEditToolDefinition,
  createReadToolDefinition,
  createWriteToolDefinition,
  defineTool,
  ModelRuntime,
  SessionManager,
  SettingsManager,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const CWD = process.cwd();
const banner = (t) => console.log(`\n===== ${t} =====`);

/** Surviving `sleep` processes — the orphan this whole exercise is about. */
function sleepProcs() {
  try {
    const out = execFileSync("bash", ["-lc", "ps -o pid=,etime=,args= -C sleep 2>/dev/null || true"], {
      encoding: "utf8",
    }).trim();
    return out.length === 0 ? [] : out.split("\n");
  } catch (e) {
    return ["ERR " + e.message];
  }
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

/** A ctx that satisfies the bash tool's `ctx.sessionManager` read (bash.js:128). */
const ctxStub = {
  cwd: CWD,
  sessionManager: { getSessionId: () => "probe-session", getSessionFile: () => undefined },
  model: undefined,
  thinkingLevel: undefined,
};

let runtimePromise = null;
function runtime() {
  runtimePromise ??= ModelRuntime.create();
  return runtimePromise;
}

async function openSession(options) {
  const rt = await runtime();
  const model = rt.getModel("llamacpp", "halogen-qwen3.8-flash-next") ?? rt.getModels()[0];
  const { session } = await createAgentSession({
    cwd: CWD,
    modelRuntime: rt,
    model,
    sessionManager: SessionManager.inMemory(CWD),
    settingsManager: SettingsManager.inMemory({ compaction: { enabled: false } }),
    ...options,
  });
  return session;
}

function markerTool(name, text) {
  return defineTool({
    name,
    label: name,
    description: `marker tool ${name}`,
    promptSnippet: `marker snippet for ${name}`,
    parameters: Type.Object({}),
    execute: async () => ({ content: [{ type: "text", text }], details: { marker: text } }),
  });
}

/**
 * The wrapper shape under test (ADR-010 §3, §5): wrapper-owned controller,
 * tee of onUpdate, inbound forwarding, race rather than await, a returned
 * result carrying `details.timedOut` rather than a thrown one.
 */
function withToolTimeout(inner, timeoutMs) {
  return {
    ...inner,
    execute: async (toolCallId, params, inbound, onUpdate, ctx) => {
      const controller = new AbortController();
      const stream = [];
      const tee = (partial) => {
        for (const c of partial?.content ?? []) if (c?.type === "text") stream.push(c.text);
        onUpdate?.(partial);
      };
      const forward = () => controller.abort(inbound?.reason ?? new Error("aborted"));
      if (inbound) {
        if (inbound.aborted) forward();
        else inbound.addEventListener("abort", forward, { once: true });
      }
      const started = Date.now();
      const run = inner.execute(toolCallId, params, controller.signal, tee, ctx);
      // A late rejection after we have already returned must not be unhandled.
      run.catch(() => undefined);
      const settled = await new Promise((resolve) => {
        const timer = setTimeout(() => {
          controller.abort(new Error(`tool timeout after ${timeoutMs}ms`));
          resolve({ timedOut: true });
        }, timeoutMs);
        run.then(
          (result) => {
            clearTimeout(timer);
            resolve({ timedOut: false, result });
          },
          (error) => {
            clearTimeout(timer);
            resolve({ timedOut: false, error });
          },
        );
      });
      const elapsedMs = Date.now() - started;
      if (settled.timedOut) {
        const partial = stream.join("");
        return {
          content: [
            {
              type: "text",
              text:
                `Tool call \`${inner.name}\` timed out after ${timeoutMs}ms and was killed. ` +
                (partial ? `Partial output:\n${partial}` : "(no output captured)"),
            },
          ],
          details: {
            timedOut: true,
            tool: inner.name,
            timeoutMs,
            elapsedMs,
            partialChars: partial.length,
          },
        };
      }
      if (settled.error) throw settled.error;
      return settled.result;
    },
  };
}

/** Pass-through wrapper: same metadata, different execute identity. */
const spreadThrough = (def) => ({
  ...def,
  execute: async (id, params, signal, onUpdate, ctx) => def.execute(id, params, signal, onUpdate, ctx),
});

// ─── Q1 ────────────────────────────────────────────────────────────────────
banner('Q1. customTools: [{name:"bash"}] — shadow, rejection, or ignored?');
{
  const session = await openSession({ customTools: [markerTool("bash", "SHADOW-BASH")] });
  const all = session.getAllTools();
  const bash = all.filter((t) => t.name === "bash");
  console.log("getAllTools names:", all.map((t) => t.name).join(","), `(${bash.length} \`bash\` entries)`);
  console.log("bash source:", bash[0]?.sourceInfo?.source, "| path:", bash[0]?.sourceInfo?.path);
  console.log("active tools:", session.agent.state.tools.map((t) => t.name).join(","));
  const res = await session.agent.state.tools
    .find((t) => t.name === "bash")
    .execute("q1", {}, undefined, undefined);
  console.log("active bash execute ->", JSON.stringify(res.content));
  console.log("SHADOW WINS?", res.content[0]?.text === "SHADOW-BASH");
  console.log(
    "bash lines in system prompt:",
    JSON.stringify(session.agent.state.systemPrompt.split("\n").filter((l) => /bash/.test(l))),
  );
  session.dispose?.();
}

// ─── Q2 ────────────────────────────────────────────────────────────────────
banner('Q2. noTools:"builtin" + builtins rebuilt from pi factories as customs');
{
  const session = await openSession({
    noTools: "builtin",
    customTools: [
      createReadToolDefinition(CWD),
      createBashToolDefinition(CWD),
      markerTool("report_done", "DONE"),
      markerTool("report_split", "SPLIT"),
    ],
  });
  const all = session.getAllTools();
  console.log("getAllTools names:", all.map((t) => t.name).join(","));
  for (const n of ["bash", "read", "report_done"]) {
    const e = all.find((t) => t.name === n);
    console.log(`  ${n}: source=${e?.sourceInfo?.source} descLen=${e?.description?.length}`);
  }
  console.log("active tools:", session.agent.state.tools.map((t) => t.name).join(","));
  console.log(
    "tool-list lines in prompt:",
    JSON.stringify(session.agent.state.systemPrompt.split("\n").filter((l) => /^- (read|bash):/.test(l))),
  );
  session.dispose?.();
}

// ─── K1 ────────────────────────────────────────────────────────────────────
banner("K1. wrapper timer around a hung bash: is the process tree actually killed?");
console.log("sleep procs before:", JSON.stringify(sleepProcs()));
{
  const wrapped = withToolTimeout(createBashToolDefinition(CWD), 1000);
  const t0 = Date.now();
  const res = await wrapped.execute("k1", { command: "sleep 45 && echo never" }, undefined, undefined, ctxStub);
  console.log("elapsed ms:", Date.now() - t0, "| details:", JSON.stringify(res.details));
  console.log("immediately after return:", JSON.stringify(sleepProcs()));
  await wait(2500);
  console.log("2.5 s later:", JSON.stringify(sleepProcs()));
}

// ─── K2 ────────────────────────────────────────────────────────────────────
banner("K2. inbound abort forwarded into the wrapper controller (Ctrl-C / session.abort)");
{
  const wrapped = withToolTimeout(createBashToolDefinition(CWD), 60_000); // cap never fires
  const inbound = new AbortController();
  setTimeout(() => inbound.abort(new Error("Ctrl-C")), 800);
  const t0 = Date.now();
  try {
    const res = await wrapped.execute("k2", { command: "sleep 45 && echo never" }, inbound.signal, undefined, ctxStub);
    console.log("returned without an abort — forwarding FAILED:", JSON.stringify(res.details));
  } catch (e) {
    console.log("threw", JSON.stringify(e.message), "at", Date.now() - t0, "ms (abort path preserved)");
  }
  await wait(800);
  console.log("sleep procs after inbound abort:", JSON.stringify(sleepProcs()));
}

// ─── K3 ────────────────────────────────────────────────────────────────────
banner("K3. a tool that ignores abort entirely: does the wrapper still return?");
{
  const stubborn = {
    name: "stubborn",
    label: "stubborn",
    description: "ignores the abort signal",
    parameters: Type.Object({}),
    execute: () => wait(10_000).then(() => ({ content: [{ type: "text", text: "eventually" }], details: {} })),
  };
  const t0 = Date.now();
  const res = await withToolTimeout(stubborn, 700).execute("k3", {}, undefined, undefined, ctxStub);
  console.log("elapsed ms:", Date.now() - t0, "| details:", JSON.stringify(res.details));
}

// ─── K4 ────────────────────────────────────────────────────────────────────
banner("K4. the same wrapper, registered inside a real createAgentSession");
{
  const session = await openSession({
    customTools: [withToolTimeout(createBashToolDefinition(CWD), 1500)],
  });
  const bash = session.agent.state.tools.find((t) => t.name === "bash");
  const t0 = Date.now();
  const res = await bash.execute("k4", { command: "sleep 40 && echo nope" }, undefined, undefined, ctxStub);
  console.log("registered tool is the wrapper:", res?.details?.timedOut === true, "| elapsed ms:", Date.now() - t0);
  console.log("details:", JSON.stringify(res.details));
  await wait(700);
  console.log("sleep procs:", JSON.stringify(sleepProcs()));
  session.dispose?.();
}

// ─── K5 ────────────────────────────────────────────────────────────────────
banner("K5. is the partial output before the kill recoverable?");
{
  const res = await withToolTimeout(createBashToolDefinition(CWD), 1500).execute(
    "k5",
    { command: "echo first-line; echo second-line; sleep 40" },
    undefined,
    undefined,
    ctxStub,
  );
  console.log("partialChars:", res.details.partialChars);
  console.log("text:", JSON.stringify(res.content[0].text));
  await wait(600);
  console.log("sleep procs:", JSON.stringify(sleepProcs()));
}

// ─── P0/P1 ─────────────────────────────────────────────────────────────────
banner("P0/P1. fidelity: settings the built-ins get, and prompt equality under a spread-through shadow");
{
  // The same instance shape `defaultSessionFactory` hands to createAgentSession
  // (src/agent.ts:871), i.e. exactly what the session reads these from.
  const sm = SettingsManager.inMemory({ compaction: { enabled: false } });
  console.log(
    "settings _buildRuntime reads (agent-session.js:2183-2185): " +
      `shellCommandPrefix=${JSON.stringify(sm.getShellCommandPrefix())} ` +
      `shellPath=${JSON.stringify(sm.getShellPath())} ` +
      `imageAutoResize=${JSON.stringify(sm.getImageAutoResize())}`,
  );
  console.log("  (undefined is the honest answer for an in-memory manager — that is what the session sees too)");
}
{
  const baseline = await openSession({});
  const basePrompt = baseline.agent.state.systemPrompt;
  const baseBash = baseline.getAllTools().find((t) => t.name === "bash");
  const baseActive = baseline.agent.state.tools.map((t) => t.name).join(",");
  baseline.dispose?.();

  const shadowed = await openSession({
    customTools: [
      spreadThrough(createReadToolDefinition(CWD)),
      spreadThrough(createBashToolDefinition(CWD)),
      spreadThrough(createEditToolDefinition(CWD)),
      spreadThrough(createWriteToolDefinition(CWD)),
    ],
  });
  const prompt = shadowed.agent.state.systemPrompt;
  console.log("active tools:", shadowed.agent.state.tools.map((t) => t.name).join(","), "| baseline:", baseActive);
  console.log("bash description identical:", shadowed.getAllTools().find((t) => t.name === "bash").description === baseBash.description);
  console.log("SYSTEM PROMPT IDENTICAL TO BASELINE:", prompt === basePrompt);
  if (prompt !== basePrompt) {
    const a = basePrompt.split("\n");
    const b = prompt.split("\n");
    for (let i = 0; i < Math.max(a.length, b.length); i += 1) {
      if (a[i] !== b[i]) console.log(`  line ${i}: base ${JSON.stringify(a[i])} vs shadow ${JSON.stringify(b[i])}`);
    }
  }
  shadowed.dispose?.();
}

// ─── P2/P3 ─────────────────────────────────────────────────────────────────
banner("P2/P3/P4. the two silences, and composition with tools/excludeTools");
{
  const session = await openSession({
    customTools: [
      {
        name: "bash",
        label: "bash",
        description: "shadow bash with no promptSnippet",
        parameters: createBashToolDefinition(CWD).parameters,
        execute: async () => ({ content: [{ type: "text", text: "x" }], details: {} }),
      },
    ],
  });
  console.log("bash lines with a snippet-less shadow:", JSON.stringify(
    session.agent.state.systemPrompt.split("\n").filter((l) => /bash/i.test(l)),
  ));
  session.dispose?.();
}
{
  const session = await openSession({
    customTools: [
      { ...createBashToolDefinition(CWD), description: "FIRST BASH" },
      { ...createBashToolDefinition(CWD), description: "SECOND BASH" },
    ],
  });
  const e = session.getAllTools().find((t) => t.name === "bash");
  console.log("two customs both named bash → served description:", JSON.stringify(e.description.slice(0, 12)));
  console.log("(no conflict diagnostic anywhere)");
  session.dispose?.();
}

{
  // P4: does a shadow compose with the allowlist/excludeTools pair the work and
  // split sessions already use? Name-keyed exclusion should still win.
  const session = await openSession({
    tools: ["read", "bash", "report_done"],
    excludeTools: ["edit", "write"],
    customTools: [
      spreadThrough(createBashToolDefinition(CWD)),
      spreadThrough(createReadToolDefinition(CWD)),
      markerTool("report_done", "DONE"),
    ],
  });
  console.log(
    "active tools with tools=[read,bash,report_done] excludeTools=[edit,write]:",
    session.agent.state.tools.map((t) => t.name).join(","),
  );
  console.log(
    "(the harness's own prompt override is not a createAgentSession option; it is passed as a " +
      "resourceLoader instead — src/agent.ts:907-916)",
  );
  session.dispose?.();
}

console.log("\nprobe done. Read docs/ADR-010-tool-call-timeout.md for what each line settles.\n");
