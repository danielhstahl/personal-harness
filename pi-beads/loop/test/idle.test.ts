/**
 * Tests for idle mode — the prompt the loop shows when the board is empty.
 *
 * The surface is driven through a fake `Terminal` implementing pi-tui's real
 * interface, with an injected clock and signal source, so every behaviour —
 * including the double-press window and the teardown ordering — is observed
 * without a TTY, a model, or a real signal.
 */

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, it } from "node:test";

import {
  stripTerminalSequences,
  visibleWidth,
} from "@earendil-works/pi-tui";
import { initTheme } from "@earendil-works/pi-coding-agent";

import type { IdleOutcome, IdleStatus } from "../src/idle.ts";
import {
  IDLE_APP_KEYBINDINGS,
  IDLE_SLASH_COMMANDS,
  IdleError,
  createIdleKeybindings,
  createIdleMode,
  createIdleTextTheme,
  idleKeyNames,
  loadUserIdleKeybindings,
  parseExitCommand,
  parseSlashCommandName,
  renderIdleHintLine,
  renderIdleStatusLine,
  resolveIdleThemeName,
  type IdleHandle,
} from "../src/idle.ts";
import { FakeSignals, FakeTerminal, fakeClock, settle } from "./idle-fakes.ts";

// ── harness ──────────────────────────────────────────────────────────────────

interface Harness {
  term: FakeTerminal;
  clock: ReturnType<typeof fakeClock>;
  signals: FakeSignals;
  goodbyes: string[];
  handle: IdleHandle;
  /** The in-flight `next()` promise. */
  pending: Promise<IdleOutcome>;
  /** Outcome captured once resolved; undefined until then. */
  resolved: IdleOutcome | undefined;
}

const DEFAULT_STATUS: IdleStatus = {
  ready: 3,
  inProgress: 1,
  model: { provider: "llamacpp", id: "halogen-qwen3.8-flash-next" },
  thinkingLevel: "low",
};

function harness(
  options: {
    status?: IdleStatus;
    columns?: number;
    rows?: number;
    windowMs?: number;
    kitty?: boolean;
  } = {},
): Harness {
  const term = new FakeTerminal(
    options.columns ?? 80,
    options.rows ?? 24,
    options.kitty ?? false,
  );
  const clock = fakeClock();
  const signals = new FakeSignals();
  const goodbyes: string[] = [];
  const handle = createIdleMode({
    terminal: term as never,
    clock: clock.now,
    doublePressWindowMs: options.windowMs ?? 500,
    signals,
    status: async () => options.status ?? DEFAULT_STATUS,
    goodbye: (line) => goodbyes.push(line),
  });

  const box: Harness = {
    term,
    clock,
    signals,
    goodbyes,
    handle,
    pending: undefined as unknown as Promise<IdleOutcome>,
    resolved: undefined,
  };
  box.pending = handle.next().then((outcome) => {
    box.resolved = outcome;
    return outcome;
  });
  // Nothing awaits `box.pending` until the test does; never let an unexpected
  // rejection take the runner down.
  void box.pending.catch(() => undefined);
  return box;
}

// ── raw input passthrough ────────────────────────────────────────────────────

describe("idle mode returns raw input", () => {
  it("resolves with exactly what was typed before Enter", async () => {
    const h = harness();
    await settle();
    h.term.input("split this into beads issues\r");
    assert.deepEqual(await h.pending, {
      kind: "input",
      text: "split this into beads issues",
    });
  });

  it("pi's editor owns the normalisations, and we add none", async () => {
    // Documenting what "raw" actually means here, rather than leaving it to be
    // discovered: pi's `submitValue()` does `expandPasteMarkers(...).trim()`, so
    // surrounding whitespace is gone before we ever see the text. Interior
    // whitespace is untouched by us.
    const h = harness();
    await settle();
    h.term.input("  keep   inner   spacing  \r");
    assert.deepEqual(await h.pending, {
      kind: "input",
      text: "keep   inner   spacing",
    });
  });

  it("Tab is pi's autocomplete key, so literal tabs never reach the loop", async () => {
    // In pi, Tab belongs to autocomplete. It is consumed there, exactly as it
    // would be in a pi.dev session — the loop does not get to invent a tab key.
    const h = harness();
    await settle();
    h.term.input("a\tb\r");
    const outcome = await h.pending;
    assert.equal(outcome.kind, "input");
    assert.ok(!(outcome as { text: string }).text.includes("\t"));
  });

  it("passes multi-line, markdown-ish input through as one exact piece", async () => {
    const h = harness();
    await settle();
    // ctrl+j / \n is `tui.input.newLine` in pi's default map.
    const typed = [
      "Fix the flaky tests,",
      "then add `--json` to every bd call,",
      "and write the handoff.",
      "",
      "  - indented bullet with trailing spaces   ",
    ].join("\n");
    h.term.input(`${typed}\r`);
    assert.deepEqual(await h.pending, {
      kind: "input",
      // Interior blank lines, indentation and punctuation survive exactly; only
      // pi's own trailing trim applies.
      text: typed.replace(/\s+$/u, ""),
    });
  });

  it("treats prose as input rather than something to interpret", async () => {
    const h = harness();
    await settle();
    h.term.input("2 split that into tasks\r");
    assert.deepEqual(await h.pending, {
      kind: "input",
      text: "2 split that into tasks",
    });
  });
});

// ── empty input ──────────────────────────────────────────────────────────────

describe("idle mode does not turn nothing into work", () => {
  it("an empty submit produces no outcome and says so", async () => {
    const h = harness();
    await settle();
    h.term.input("\r");
    await settle();
    assert.equal(h.resolved, undefined);
    assert.match(stripTerminalSequences(h.term.output), /nothing to act on/u);
    // …and the surface still works afterwards.
    h.term.input("now something real\r");
    assert.deepEqual(await h.pending, {
      kind: "input",
      text: "now something real",
    });
  });

  it("whitespace-only input is not work either", async () => {
    const h = harness();
    await settle();
    h.term.input("   \r");
    await settle();
    assert.equal(h.resolved, undefined);
    assert.equal(h.handle.finished, false);
    // pi's trim makes this the same path as an empty submit — the point is that
    // neither of them can become a work item.
    assert.match(stripTerminalSequences(h.term.output), /nothing to act on/u);
    h.term.input("   \t   \r");
    await settle();
    assert.equal(h.resolved, undefined);
    // Still usable, and a real submission goes through.
    h.term.input("and now a real thought\r");
    assert.deepEqual(await h.pending, {
      kind: "input",
      text: "and now a real thought",
    });
  });

  it("an unknown slash command is reported, not sent anywhere", async () => {
    const h = harness();
    await settle();
    h.term.input("/frobnicate the board\r");
    await settle();
    assert.equal(h.resolved, undefined);
    assert.match(
      stripTerminalSequences(h.term.output),
      /unknown command \/frobnicate/u,
    );
  });
});

// ── slash commands ───────────────────────────────────────────────────────────

describe("idle slash commands", () => {
  it("offers exit and quit as the command set", () => {
    assert.deepEqual(
      IDLE_SLASH_COMMANDS.map((command) => command.name),
      ["exit", "quit"],
    );
  });

  it("/exit leaves", async () => {
    const h = harness();
    await settle();
    h.term.input("/exit\r");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
  });

  it("/quit leaves", async () => {
    const h = harness();
    await settle();
    h.term.input("/quit\r");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
  });

  it("/exit with a trailing argument still leaves", () => {
    assert.equal(parseExitCommand("/exit now"), "exit");
    assert.equal(parseExitCommand("  /QUIT  "), "quit");
    assert.equal(parseExitCommand("/ex"), undefined);
    assert.equal(parseExitCommand("exit"), undefined);
  });

  it("recognises any slash command name for the unknown-command path", () => {
    assert.equal(parseSlashCommandName("/frobnicate x"), "frobnicate");
    assert.equal(parseSlashCommandName("not a command"), undefined);
    assert.equal(parseSlashCommandName("/"), undefined);
  });
});

// ── ctrl+c ───────────────────────────────────────────────────────────────────

describe("idle ctrl+c mirrors pi", () => {
  it("first press clears the input and arms, without exiting", async () => {
    const h = harness();
    await settle();
    h.term.input("half-typed thought");
    await settle();
    const afterTyping = h.term.writeCount;
    h.term.input("\x03");
    await settle();
    assert.equal(h.resolved, undefined);
    assert.equal(h.handle.finished, false);
    // The frame rendered *after* the press: the arming notice, and the old text
    // gone. The whole write buffer would still contain the earlier frame, so the
    // comparison has to be on the delta.
    const frame = stripTerminalSequences(h.term.outputSince(afterTyping));
    assert.match(frame, /input cleared/u);
    assert.match(frame, /press ctrl\+c again/u);
    assert.ok(!frame.includes("half-typed thought"));
  });

  it("second press inside the window exits", async () => {
    const h = harness();
    await settle();
    h.term.input("\x03");
    await settle();
    h.clock.advance(499);
    h.term.input("\x03");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
  });

  it("outside the window the second press re-arms instead of exiting", async () => {
    const h = harness({ windowMs: 500 });
    await settle();
    h.term.input("\x03");
    await settle();
    h.clock.advance(501);
    h.term.input("\x03");
    await settle();
    assert.equal(h.resolved, undefined);
    assert.equal(h.handle.finished, false);
    // …and a third press right after the second does exit.
    h.clock.advance(1);
    h.term.input("\x03");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
  });

  it("a real SIGINT behaves like ctrl+c", async () => {
    const h = harness();
    await settle();
    h.signals.emit("SIGINT");
    await settle();
    assert.equal(h.resolved, undefined);
    h.clock.advance(10);
    h.signals.emit("SIGINT");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
  });

  it("SIGTERM exits immediately", async () => {
    const h = harness();
    await settle();
    h.signals.emit("SIGTERM");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "signal" });
  });
});

// ── ctrl+d ───────────────────────────────────────────────────────────────────

describe("idle ctrl+d", () => {
  it("exits when the input is empty", async () => {
    const h = harness();
    await settle();
    h.term.input("\x04");
    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
  });

  it("does not exit while there is text — pi deletes forward instead", async () => {
    const h = harness();
    await settle();
    h.term.input("abc");
    await settle();
    // Home so ctrl+d has something in front of it to delete.
    for (let i = 0; i < 3; i += 1) h.term.input("\x1b[D");
    h.term.input("\x04");
    await settle();
    assert.equal(h.resolved, undefined);
    assert.equal(h.handle.finished, false);
    assert.match(stripTerminalSequences(h.term.output), /bc/u);
  });
});

// ── teardown ─────────────────────────────────────────────────────────────────

describe("idle teardown", () => {
  it("drains input, then stops the TUI, in that order", async () => {
    const h = harness();
    await settle();
    h.term.input("/exit\r");
    await h.pending;
    const drain = h.term.indexOf("drainInput");
    const stop = h.term.indexOf("stop");
    assert.ok(drain >= 0, "drainInput was called");
    assert.ok(stop > drain, "stop comes after the drain");
    assert.equal(h.term.count("stop"), 1);
  });

  it("writes nothing to the terminal after the TUI stopped", async () => {
    const h = harness();
    await settle();
    h.term.input("/exit\r");
    await h.pending;
    assert.ok(h.term.indexOf("stop") >= 0);
    // Every terminal write happened before stop(); the goodbye goes to its own sink.
    assert.equal(h.term.writes.length, h.term.writesAtStop);
  });

  it("leaves the cursor visible and writes the goodbye once", async () => {
    const h = harness();
    await settle();
    h.term.input("/exit\r");
    await h.pending;
    assert.ok(
      h.term.count("showCursor") >= h.term.count("hideCursor"),
      "cursor restoration must balance against hiding",
    );
    assert.equal(h.goodbyes.length, 1);
    assert.match(h.goodbyes[0] ?? "", /terminal restored/u);
  });

  it("unregisters its signal handlers", async () => {
    const h = harness();
    await settle();
    assert.equal(h.signals.listening("SIGINT"), 1);
    assert.equal(h.signals.listening("SIGTERM"), 1);
    h.term.input("/exit\r");
    await h.pending;
    assert.equal(h.signals.listening("SIGINT"), 0);
    assert.equal(h.signals.listening("SIGTERM"), 0);
  });

  it("never repaints after stop", async () => {
    const h = harness();
    await settle();
    assert.ok(h.handle.renderCount > 0, "the surface painted at least once");
    h.term.input("/exit\r");
    await h.pending;
    const atExit = h.handle.renderCount;
    // Late refreshes and repeated disposes must not trigger another render pass.
    h.handle.refresh();
    await settle();
    await h.handle.dispose();
    await h.handle.dispose();
    assert.equal(h.handle.renderCount, atExit);
  });

  it("dispose() is idempotent", async () => {
    const h = harness();
    await settle();
    await h.handle.dispose();
    await h.handle.dispose();
    assert.equal(h.handle.finished, true);
    assert.equal(h.term.count("stop"), 1);
    assert.equal(h.goodbyes.length, 1);
  });

  it("waiting on a dead surface is a typed error, not a hang", async () => {
    const h = harness();
    await settle();
    await h.handle.dispose();
    await assert.rejects(h.handle.next(), (error: unknown) => {
      assert.ok(IdleError.is(error), "expected an IdleError");
      assert.equal((error as IdleError).kind, "already-finished");
      return true;
    });
  });

  it("is finished only after an outcome or a dispose", async () => {
    const h = harness();
    await settle();
    assert.equal(h.handle.finished, false);
    h.term.input("go\r");
    await h.pending;
    assert.equal(h.handle.finished, true);
  });
});

// ── status line ──────────────────────────────────────────────────────────────

describe("idle status line", () => {
  const theme = createIdleTextTheme();

  it("shows ready and in-progress counts", () => {
    const line = stripTerminalSequences(
      renderIdleStatusLine({ ready: 3, inProgress: 1 }, theme, 200),
    );
    assert.match(line, /ready 3/u);
    assert.match(line, /in progress 1/u);
  });

  it("says what is held back instead of leaving the count looking wrong", () => {
    const held = stripTerminalSequences(
      renderIdleStatusLine({ ready: 2, inProgress: 0, heldOut: 1 }, theme, 200),
    );
    assert.match(held, /ready 2/u);
    assert.match(held, /\(\+1 epic not work\)/u);

    const many = stripTerminalSequences(
      renderIdleStatusLine({ ready: 1, inProgress: 1, heldOut: 3 }, theme, 200),
    );
    assert.match(many, /\(\+3 epics not work\)/u);
  });

  it("a board of nothing but containers reads as no work, not as an empty board", () => {
    const line = stripTerminalSequences(
      renderIdleStatusLine({ ready: 0, inProgress: 0, heldOut: 2 }, theme, 200),
    );
    assert.match(line, /no work to pick · 2 epics open, none pickable/u);
    assert.doesNotMatch(line, /board empty/u);
  });

  it("never counts the held-out issues as ready", () => {
    const line = stripTerminalSequences(
      renderIdleStatusLine({ ready: 0, inProgress: 0, heldOut: 1 }, theme, 200),
    );
    assert.doesNotMatch(line, /ready \d/u, `held-out work leaked into a count: ${line}`);
  });

  it("shows model and thinking level when known", () => {
    const line = stripTerminalSequences(
      renderIdleStatusLine(
        {
          ready: 0,
          inProgress: 0,
          model: { provider: "llamacpp", id: "halogen-qwen3.8-flash-next" },
          thinkingLevel: "medium",
        },
        theme,
        200,
      ),
    );
    assert.match(line, /board empty/u);
    assert.match(line, /model llamacpp\/halogen-qwen3\.8-flash-next/u);
    assert.match(line, /thinking medium/u);
  });

  it("omits unknown fields instead of rendering undefined", () => {
    const line = stripTerminalSequences(
      renderIdleStatusLine({ ready: 2, inProgress: 0 }, theme, 200),
    );
    for (const bad of ["undefined", "null", "NaN"]) {
      assert.ok(!line.includes(bad), `status line must not contain ${bad}`);
    }
  });

  it("never renders a line wider than the terminal", () => {
    const long: IdleStatus = {
      ready: 12345,
      inProgress: 67890,
      model: {
        provider: "some-really-long-provider-name",
        id: "a-model-id-that-keeps-going-and-going",
      },
      thinkingLevel: "maximum-effort-forever",
    };
    for (const width of [20, 40, 60, 80, 120]) {
      const line = renderIdleStatusLine(long, theme, width);
      assert.ok(
        visibleWidth(line) <= width,
        `line too wide at ${width}: ${visibleWidth(line)}`,
      );
    }
  });

  it("the idle surface paints the status it was given", async () => {
    const h = harness({ status: { ready: 7, inProgress: 2 } });
    await settle();
    const output = stripTerminalSequences(h.term.output);
    assert.match(output, /ready 7/u);
    assert.match(output, /in progress 2/u);
    await h.handle.dispose();
  });

  it("re-paints when refreshed", async () => {
    let status: IdleStatus = { ready: 1, inProgress: 0 };
    const term = new FakeTerminal(120, 24);
    const handle = createIdleMode({
      terminal: term as never,
      clock: fakeClock().now,
      signals: new FakeSignals(),
      status: async () => status,
      goodbye: () => undefined,
    });
    const pending = handle.next();
    void pending.catch(() => undefined);
    await settle();
    assert.match(stripTerminalSequences(term.output), /ready 1/u);
    status = { ready: 42, inProgress: 9 };
    handle.refresh();
    await settle();
    const output = stripTerminalSequences(term.output);
    assert.match(output, /ready 42/u);
    assert.match(output, /in progress 9/u);
    await handle.dispose();
  });

  it("hint line names the real keys from our keybinding registry", () => {
    initTheme();
    const kb = createIdleKeybindings({});
    const hint = stripTerminalSequences(renderIdleHintLine(200, kb));
    assert.match(hint, /ctrl\+c clear/u);
    assert.match(hint, /ctrl\+c twice exit/u);
    assert.match(hint, /ctrl\+d exit when empty/u);
    assert.match(hint, /enter send to split/u);
    // And a remap shows up in the hint, because the hint reads the registry.
    const remapped = stripTerminalSequences(
      renderIdleHintLine(200, createIdleKeybindings({ "app.clear": "ctrl+q" })),
    );
    assert.match(remapped, /ctrl\+q clear/u);
    assert.equal(idleKeyNames("app.exit", kb), "ctrl+d");
  });
});

// ── resize ───────────────────────────────────────────────────────────────────

describe("idle resize", () => {
  it("redraws at the new width without leaving wide lines", async () => {
    const h = harness({ columns: 120 });
    await settle();
    h.term.resize(40, 24);
    await settle();
    // The status line is re-rendered for the new width, so a line that no longer
    // fits comes back truncated rather than wrapped-wide.
    const line = renderIdleStatusLine(DEFAULT_STATUS, createIdleTextTheme(), 40);
    assert.ok(visibleWidth(line) <= 40);
    const after = stripTerminalSequences(h.term.output);
    assert.ok(after.length > 0, "the resize produced output");
    await h.handle.dispose();
  });

  it("keeps typed text across a resize", async () => {
    const h = harness({ columns: 80 });
    await settle();
    h.term.input("persist me");
    await settle();
    h.term.resize(60, 20);
    await settle();
    assert.match(stripTerminalSequences(h.term.output), /persist me/u);
    h.term.input("\r");
    assert.deepEqual(await h.pending, { kind: "input", text: "persist me" });
  });
});

// ── boundaries ───────────────────────────────────────────────────────────────

describe("idle mode leaves the loop's boundaries alone", () => {
  const source = stripComments(readFileSync("src/idle.ts", "utf8"));

  it("imports no agent, session or model API — idle cannot call a model", () => {
    for (const banned of [
      "createAgentSession",
      "ModelRuntime",
      "AgentSession",
      "runWork",
      "report_done",
    ]) {
      assert.ok(!source.includes(banned), `idle.ts must not mention ${banned}`);
    }
  });

  it("never spawns a process", () => {
    assert.ok(!/\bchild_process\b/u.test(source));
    assert.ok(!/\bspawn[A-Za-z]*\s*\(/u.test(source));
  });

  it("never writes to the board", () => {
    for (const banned of [
      "setStatus",
      "closeIssue",
      "createIssue",
      "remember",
      "--assignee",
      "--claim",
    ]) {
      assert.ok(!source.includes(banned), `idle.ts must not call ${banned}`);
    }
  });

  it("is not the rpc transport path", () => {
    assert.ok(!source.includes("--mode rpc"));
    assert.ok(!source.includes("RpcClient"));
  });

  it("styles nothing itself — every escape comes from a pi theme function", () => {
    // No hand-rolled colour, cursor or clearing sequence. If this fails, someone
    // painted instead of asking the theme.
    assert.ok(!source.includes("\\x1b"), "escaped-ESC literal in idle.ts");
    assert.ok(!source.includes("\\u001b"), "unicode-escaped ESC in idle.ts");
    assert.ok(
      !source.includes(String.fromCharCode(27)),
      "raw ESC character in idle.ts",
    );
    assert.ok(!/\\d+;?\d*m/u.test(source), "SGR-looking literal in idle.ts");
  });

  it("renders with pi's own components, editor and theme", () => {
    for (const required of [
      'from "@earendil-works/pi-tui"',
      'from "@earendil-works/pi-coding-agent"',
      "CustomEditor",
      "TuiMainScreen",
      "ProcessTerminal",
      "initTheme",
      "getSelectListTheme",
      "CombinedAutocompleteProvider",
    ]) {
      assert.ok(source.includes(required), `idle.ts should use ${required}`);
    }
    // …and not a substitute line editor.
    for (const banned of ["readline", "inquirer", "prompts(", "tty.read"] as const) {
      assert.ok(!source.includes(banned), `idle.ts must not use ${banned}`);
    }
  });
});

// ── theme + keybindings ──────────────────────────────────────────────────────

describe("idle theme resolution (ADR-001)", () => {
  it("follows pi's configured theme by default", () => {
    assert.equal(resolveIdleThemeName({}), undefined);
    assert.equal(resolveIdleThemeName({ LOOP_THEME: "   " }), undefined);
  });

  it("LOOP_THEME pins a theme", () => {
    assert.equal(resolveIdleThemeName({ LOOP_THEME: "light" }), "light");
  });

  it("an explicit theme beats the environment", () => {
    assert.equal(resolveIdleThemeName({ LOOP_THEME: "light" }, "dark"), "dark");
  });
});

describe("idle keybindings", () => {
  it("mirrors pi's app-level idle bindings", () => {
    assert.equal(
      IDLE_APP_KEYBINDINGS["app.clear"]?.defaultKeys as string,
      "ctrl+c",
    );
    assert.equal(IDLE_APP_KEYBINDINGS["app.exit"]?.defaultKeys as string, "ctrl+d");
  });

  it("registers both the tui map and the app actions", () => {
    const kb = createIdleKeybindings({});
    assert.equal(kb.matches("\x03", "app.clear"), true);
    assert.equal(kb.matches("\x04", "app.exit"), true);
    assert.ok(kb.getKeys("app.clear").includes("ctrl+c"));
  });

  it("honours a user remap for an idle action", () => {
    const kb = createIdleKeybindings({ "app.clear": "ctrl+q" });
    assert.equal(kb.matches("\x11", "app.clear"), true);
    assert.equal(kb.matches("\x03", "app.clear"), false);
  });

  it("reads only idle-owned ids out of a user keybindings.json", () => {
    const dir = mkdtempSync(join(tmpdir(), "loop-idle-keys-"));
    writeFileSync(
      join(dir, "keybindings.json"),
      JSON.stringify({
        "app.clear": "ctrl+q",
        "app.exit": ["ctrl+w"],
        "tui.editor.cursorUp": "ctrl+k",
        nonsense: 42,
      }),
      "utf8",
    );
    assert.deepEqual(loadUserIdleKeybindings(dir), {
      "app.clear": "ctrl+q",
      "app.exit": ["ctrl+w"],
    });
  });

  it("tolerates a missing or broken keybindings.json", () => {
    const dir = mkdtempSync(join(tmpdir(), "loop-idle-junk-"));
    assert.deepEqual(loadUserIdleKeybindings(dir), {});
    writeFileSync(join(dir, "keybindings.json"), "{not json", "utf8");
    assert.deepEqual(loadUserIdleKeybindings(dir), {});
    writeFileSync(join(dir, "keybindings.json"), "[1,2]", "utf8");
    assert.deepEqual(loadUserIdleKeybindings(dir), {});
  });
});

// ── end to end on a driven terminal ─────────────────────────────────────────
//
// This is the evidence a captured pty transcript used to provide, derived at
// run time instead of committed as an artifact. The real `createIdleMode` is
// booted on a fake `Terminal` that implements pi-tui's real contract and feeds
// keystrokes through pi's own `StdinBuffer`, so "a frame was drawn", "Enter
// submits" and "the terminal came back intact" are observed rather than
// remembered. A committed capture can rot, be edited, or describe a different
// version of the code; this cannot.

describe("idle mode end to end on a driven terminal", () => {
  it("draws a real frame, takes a submit as input, and hands the terminal back", async () => {
    // The surface must live entirely on the injected terminal. If it ever grabs
    // the real process stdin, `isRaw` moves and this test says so.
    const rawBefore = process.stdin.isRaw;
    const h = harness({ columns: 100 });

    await settle();
    const frame = stripTerminalSequences(h.term.output);
    assert.match(frame, /ready 3 · in progress 1/u, "no status line was drawn");
    assert.match(frame, /model llamacpp/u, "model missing from the frame");
    assert.match(frame, /enter send to split/u, "hint line missing from the frame");
    assert.equal(
      process.stdin.isRaw,
      rawBefore,
      "idle took over the real process stdin instead of the injected terminal",
    );

    h.term.input("make a todo list app\r");
    assert.deepEqual(await h.pending, {
      kind: "input",
      text: "make a todo list app",
    });

    // Teardown: cursor shown last, TUI stopped, and nothing written to the
    // terminal after `stop()` — the goodbye goes to its own sink.
    await h.handle.dispose();
    assert.notEqual(h.term.indexOf("stop"), -1, "the TUI was never stopped");
    assert.ok(
      h.term.calls.lastIndexOf("showCursor") >
        h.term.calls.lastIndexOf("hideCursor"),
      `cursor left hidden at teardown: ${h.term.calls.join(", ")}`,
    );
    assert.equal(
      h.term.writes.length,
      h.term.writesAtStop,
      "something was written to the terminal after stop()",
    );
    assert.equal(process.stdin.isRaw, rawBefore);
    assert.equal(h.handle.finished, true);
  });

  it("ctrl+c twice exits with a command reason and a restored terminal", async () => {
    const rawBefore = process.stdin.isRaw;
    const h = harness();

    await settle();
    h.term.input("\x03");
    await settle();
    h.term.input("\x03");

    assert.deepEqual(await h.pending, { kind: "exit", reason: "command" });
    assert.equal(h.handle.finished, true);
    assert.equal(
      process.stdin.isRaw,
      rawBefore,
      "exiting idle left the real stdin in raw mode",
    );
    assert.equal(h.term.writes.length, h.term.writesAtStop);
  });
});

/** Strip line comments and block comments so guard tests ignore prose. */
function stripComments(code: string): string {
  return code
    .replace(/\/\*[\s\S]*?\*\//gu, "")
    .split("\n")
    .map((line) => {
      const idx = line.indexOf("//");
      return idx === -1 ? line : line.slice(0, idx);
    })
    .join("\n");
}
