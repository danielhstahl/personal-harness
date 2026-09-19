/**
 * Idle mode — the prompt the loop shows when the board has nothing to do.
 *
 * When CHECK_WORK finds no in-progress and no ready work (orchestrator state
 * `idle`), the loop stops being a worker and becomes a keyboard: it waits for a
 * human to say what should happen next. This module owns that waiting surface and
 * nothing else. Per [ADR-001](../docs/ADR-001-transport-and-rendering.md) it is
 * built out of pi's own TUI pieces — `TuiMainScreen`, pi's `CustomEditor`, pi's
 * theme and pi's keybinding registry — so keybindings, autocomplete and colours
 * are pi.dev's, not an imitation of them.
 *
 * Five rules, each tested rather than asserted:
 *
 * 1. **Input only. No model, ever.** This module imports no agent, session or
 *    model API — not even to "be helpful". Idle mode therefore works with no
 *    model configured, cannot spend tokens, and cannot leak a conversation into
 *    the work session, because there is no conversation to leak. The idle text
 *    becomes work by way of the orchestrator's `human_input` event, which the
 *    interpreter (workspace-5yn.9) raises; nothing here raises it.
 * 2. **The submitted text is the human's text.** No trimming of meaning, no
 *    parsing, no splitting into tasks, no interpretation. SPLIT does that. The
 *    only two transforms applied are pi's own editor normalisations (line-ending
 *    normalisation and tab expansion), which the tests pin down explicitly rather
 *    than leave to be discovered later.
 * 3. **Exit is clean and mirrored from pi.** `/exit` (or `/quit`) and Ctrl+C
 *    twice both leave. Ctrl+C behaves the way pi.dev's does — first press clears
 *    the editor and arms, a second press inside the window exits — with the
 *    window and the clock injected so both halves of that are testable without
 *    sleeping. Teardown order follows pi's `shutdown()`: drain in-flight key
 *    releases, stop the TUI, and only then write the goodbye, so nothing can
 *    repaint the final frame while the process is going away.
 * 4. **Nothing survives the exchange.** The TUI, the editor, the signal handlers
 *    and the global keybinding registry are all released on the way out;
 *    `dispose()` is idempotent and no render happens after `stop()`. The next
 *    iteration of the loop builds new ones, per ADR-001.
 * 5. **The status line is facts from elsewhere.** Ready/in-progress counts come
 *    from the beads adapter and the model/thinking level from the resolved
 *    config, both injected. This module reads no board, spawns nothing, and
 *    writes no issue state.
 */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import {
  CustomEditor,
  getAgentDir,
  getSelectListTheme,
  initTheme,
  rawKeyHint,
} from "@earendil-works/pi-coding-agent";
import {
  CombinedAutocompleteProvider,
  Container,
  getKeybindings,
  KeybindingsManager,
  ProcessTerminal,
  Text,
  TUI_KEYBINDINGS,
  TuiMainScreen,
  truncateToWidth,
  setKeybindings,
  type Component,
  type Keybinding,
  type KeybindingDefinition,
  type KeybindingsConfig,
  type SlashCommand,
  type Terminal,
  type TUI,
} from "@earendil-works/pi-tui";

/** How the idle prompt ended. */
export type IdleExitReason = "command" | "signal";

/**
 * What `next()` resolves to: either the human's raw text (on the way to SPLIT)
 * or a request to exit. There is no third possibility, and no ambiguous "ended".
 */
export type IdleOutcome =
  | { kind: "input"; text: string }
  | { kind: "exit"; reason: IdleExitReason };

/** Board + config facts for the status line. Nothing here is invented locally. */
export interface IdleStatus {
  readonly ready: number;
  readonly inProgress: number;
  readonly model?: { readonly provider: string; readonly id: string };
  readonly thinkingLevel?: string;
}

/** The two colours the idle surface needs, supplied by pi's live theme. */
export interface IdleTextTheme {
  dim(text: string): string;
  accent(text: string): string;
}

/** Signals routed into the idle surface, injectable so tests need no real kill. */
export interface IdleSignalAdapter {
  on(signal: string, handler: () => void): () => void;
}

export interface IdleModeOptions {
  /**
   * Where the status line's facts come from: a fixed snapshot, or a provider
   * re-read on {@link IdleHandle.refresh}. A plain object is accepted so a
   * caller cannot accidentally hand the component something un-callable.
   */
  readonly status?: IdleStatus | (() => IdleStatus | Promise<IdleStatus>);
  /** Terminal to draw on. Defaults to a real `ProcessTerminal`. */
  readonly terminal?: Terminal;
  /** Clock for the double-press window. Defaults to `Date.now`. */
  readonly clock?: () => number;
  /** Double-press window in ms. Defaults to 500 — pi.dev's own value. */
  readonly doublePressWindowMs?: number;
  /** Theme name. Omitted means pi's configured default (see ADR-001). */
  readonly themeName?: string;
  /** Working directory used for path autocomplete. Defaults to the status cwd or `process.cwd()`. */
  readonly cwd?: string;
  /** Keybinding registry. Defaults to pi's bindings plus the idle app actions. */
  readonly keybindings?: KeybindingsManager;
  /** Extra slash commands to expose alongside `/exit` and `/quit`. */
  readonly commands?: readonly SlashCommand[];
  /** Signal source. Defaults to `process`. */
  readonly signals?: IdleSignalAdapter;
  /** Where the post-teardown goodbye goes. Defaults to `process.stdout`. */
  readonly goodbye?: (line: string) => void;
}

export interface IdleHandle {
  /** Resolves on submit or exit. Rejects only if the surface cannot be built. */
  next(): Promise<IdleOutcome>;
  /** Re-read the status snapshot and redraw. A no-op after teardown. */
  refresh(): void;
  /** Tear the surface down. Idempotent: safe to call twice, or after `next()`. */
  dispose(): Promise<void>;
  /** True once the surface is gone (submit, exit or dispose). */
  readonly finished: boolean;
  /**
   * How many render passes have happened. Exposed so the "never repaint after
   * stop" rule is checkable from outside the module.
   */
  readonly renderCount: number;
}

/** Errors from the idle surface, typed so the caller can tell why. */
export class IdleError extends Error {
  readonly kind: "already-finished" | "boot-failed";

  constructor(kind: IdleError["kind"], message: string) {
    super(message);
    this.name = "IdleError";
    this.kind = kind;
  }

  static is(error: unknown): error is IdleError {
    return error instanceof IdleError;
  }
}

const DEFAULT_DOUBLE_PRESS_WINDOW_MS = 500;
const DRAIN_MAX_MS = 250;
const DRAIN_IDLE_MS = 20;

/**
 * The app-level idle keybindings.
 *
 * These mirror pi.dev's own (`app.clear` = ctrl+c, `app.exit` = ctrl+d,
 * `app.interrupt` = escape) rather than reaching into pi's private module
 * layout for its `KEYBINDINGS` table, which the package does not export. The
 * tests pin the behaviour — ctrl+c lands in the `app.clear` handler, ctrl+d
 * lands in `app.exit` only when the editor is empty — so a drift between this
 * mirror and pi's real bindings shows up as a failing test rather than a
 * surprise at the keyboard. A user's `~/.pi/agent/keybindings.json` remaps for
 * these same ids are honoured on a best-effort basis (see
 * {@link loadUserIdleKeybindings}).
 */
export const IDLE_APP_KEYBINDINGS: Readonly<Record<string, KeybindingDefinition>> =
  {
    "app.clear": { defaultKeys: "ctrl+c", description: "Clear the input" },
    "app.exit": {
      defaultKeys: "ctrl+d",
      description: "Exit when the input is empty",
    },
    "app.interrupt": { defaultKeys: "escape", description: "Cancel" },
    "app.clipboard.pasteImage": {
      defaultKeys: [],
      description: "Paste image (unused while idle)",
    },
  };

/** The commands the idle prompt answers to. Everything else is human text. */
export const IDLE_SLASH_COMMANDS: readonly SlashCommand[] = [
  { name: "exit", description: "Leave the loop" },
  { name: "quit", description: "Leave the loop" },
];

const EXIT_COMMAND_NAMES: readonly string[] = ["exit", "quit"];

/** Slash-command names that mean "stop", parsed out of a submitted line. */
export function parseExitCommand(text: string): string | undefined {
  const trimmed = text.trim();
  if (!trimmed.startsWith("/")) return undefined;
  const name = trimmed.slice(1).split(/\s+/u)[0]?.toLowerCase();
  if (name === undefined) return undefined;
  return EXIT_COMMAND_NAMES.includes(name) ? name : undefined;
}

/** True when the submitted line is any slash command (exit or otherwise). */
export function parseSlashCommandName(text: string): string | undefined {
  const trimmed = text.trim();
  if (!trimmed.startsWith("/")) return undefined;
  const name = trimmed.slice(1).split(/\s+/u)[0]?.toLowerCase();
  return name === undefined || name.length === 0 ? undefined : name;
}

/**
 * Resolve the theme name for idle mode.
 *
 * ADR-001 left this open — follow the user's theme or pin one. It follows the
 * user: `undefined` means "whatever `initTheme()` would pick", which is pi's
 * own configured theme, so the loop looks like the pi the human already uses.
 * `LOOP_THEME` pins a specific theme when someone wants determinism (screenshots,
 * demos, a light terminal in a bright room). The env is read here, explicitly,
 * as a defaulted argument — not absorbed ambiently at import time.
 */
export function resolveIdleThemeName(
  env: NodeJS.ProcessEnv = process.env,
  explicit?: string,
): string | undefined {
  if (explicit !== undefined && explicit.length > 0) return explicit;
  const pinned = env.LOOP_THEME;
  if (pinned !== undefined && pinned.trim().length > 0) return pinned.trim();
  return undefined;
}

/**
 * Read the user's own remaps for the idle app actions out of pi's keybindings
 * file. Best-effort by design: a missing file, unreadable file, malformed JSON or
 * unrelated keys are all silently uninteresting. Only ids we actually define are
 * taken, so an old or unrelated config cannot bind something we then mis-dispatch.
 */
export function loadUserIdleKeybindings(
  agentDir: string = getAgentDir(),
): KeybindingsConfig {
  const path = join(agentDir, "keybindings.json");
  if (!existsSync(path)) return {};
  let parsed: unknown;
  try {
    parsed = JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return {};
  }
  if (typeof parsed !== "object" || parsed === null) return {};
  const taken: Record<string, string | string[]> = {};
  for (const id of Object.keys(IDLE_APP_KEYBINDINGS)) {
    const value = (parsed as Record<string, unknown>)[id];
    if (typeof value === "string") taken[id] = value;
    else if (Array.isArray(value) && value.every((v) => typeof v === "string")) {
      taken[id] = value as string[];
    }
  }
  // `KeyId` is a template-literal type ("ctrl+…" | "alt+…" | …) that cannot be
  // constructed from parsed JSON without re-implementing pi's key parser, so the
  // boundary cast lives here. pi reads the same file the same way; an invalid
  // binding is reported by the manager, not silently reshaped by us.
  return taken as KeybindingsConfig;
}

/** Build the keybinding registry idle mode runs on: pi's + the app actions. */
export function createIdleKeybindings(
  userBindings: KeybindingsConfig = loadUserIdleKeybindings(),
): KeybindingsManager {
  return new IdleKeybindings(
    { ...TUI_KEYBINDINGS, ...IDLE_APP_KEYBINDINGS },
    userBindings,
  );
}

/**
 * pi-coding-agent's `CustomEditor` wants its own `KeybindingsManager` subclass,
 * which the package does not export a constructor for. Extending pi-tui's class
 * and adding the two members that subclass adds (`reload`, `getEffectiveConfig`)
 * satisfies it structurally, with pi-tui's matching engine underneath.
 */
class IdleKeybindings extends KeybindingsManager {
  reload(): void {
    this.setUserBindings(loadUserIdleKeybindings());
  }

  getEffectiveConfig(): KeybindingsConfig {
    return this.getResolvedBindings();
  }
}

/**
 * The editor's declared keybinding type, pulled off its own constructor so the
 * cast below names what it papers over: pi-coding-agent's `KeybindingsManager`
 * subclass is not exported with a usable constructor, and its private field
 * makes it nominally incompatible with pi-tui's manager even though every member
 * the editor calls (`matches`, `getKeys`) is present at runtime.
 */
type EditorKeybindings = ConstructorParameters<typeof CustomEditor>[2];

/** Narrow cast from our registry to the editor's declared registry type. */
function asEditorKeybindings(manager: KeybindingsManager): EditorKeybindings {
  return manager as unknown as EditorKeybindings;
}

/** pi's editor theme, assembled from the exported pieces of pi's live theme. */
export function createIdleEditorTheme() {
  const select = getSelectListTheme();
  return {
    borderColor: (text: string) => select.description(text),
    selectList: select,
  };
}

/** The default text theme: pi's live theme, through its select-list roles. */
export function createIdleTextTheme(): IdleTextTheme {
  const select = getSelectListTheme();
  return {
    dim: (text) => select.description(text),
    accent: (text) => select.selectedText(text),
  };
}

function countSegment(
  theme: IdleTextTheme,
  label: string,
  value: number,
  last: boolean,
): string {
  const n = theme.accent(String(value));
  return `${theme.dim(label)} ${n}${last ? "" : theme.dim(" ·")}`;
}

/**
 * The status line, as a pure function: same snapshot in, same line out, at any
 * width. Kept apart from the component so its formatting is directly testable.
 */
export function renderIdleStatusLine(
  status: IdleStatus,
  theme: IdleTextTheme,
  width: number,
): string {
  const safeWidth = Math.max(1, Math.trunc(width));
  const parts: string[] = [];

  if (status.ready === 0 && status.inProgress === 0) {
    parts.push(theme.accent("board empty"));
  } else {
    parts.push(countSegment(theme, "ready", status.ready, false));
    parts.push(countSegment(theme, "in progress", status.inProgress, true));
  }

  if (status.model !== undefined) {
    const model = `${status.model.provider}/${status.model.id}`;
    parts.push(`${theme.dim("model")} ${theme.accent(model)}`);
  }
  if (status.thinkingLevel !== undefined) {
    parts.push(`${theme.dim("thinking")} ${theme.accent(status.thinkingLevel)}`);
  }

  const line = parts.join(" ");
  return truncateToWidth(line, safeWidth, theme.dim(" …"));
}

/**
 * A component wrapper so the pure status line participates in pi's layout.
 *
 * Rendering is synchronous: `paint()` reads the snapshot and formats the line
 * ahead of time, because a pi render pass cannot await the board. That also means
 * a slow or broken beads read can never stall the frame — the last good line
 * stays up.
 */
class StatusLineComponent implements Component {
  private line = "";

  constructor(
    private readonly status: () => IdleStatus | Promise<IdleStatus>,
    private readonly theme: IdleTextTheme,
  ) {}

  current(): string {
    return this.line;
  }

  invalidate(): void {}

  render(_width: number): string[] {
    return [this.line];
  }

  async paint(width: number): Promise<void> {
    const status = await this.status();
    this.line = renderIdleStatusLine(status, this.theme, width);
  }
}

/**
 * The keybinding hint line.
 *
 * Key names come from *our* registry via {@link idleKeyNames}, and pi's own
 * `rawKeyHint` does the formatting — deliberately not `keyHint()`, which reads
 * pi-coding-agent's own copy of the keybinding global. The installed package
 * tree carries two copies of `pi-tui` (ours at the top level, pi's nested under
 * `pi-coding-agent`), so `keyHint()` would never see the `app.*` actions we
 * register. `rawKeyHint(key, description)` takes the key string from us and
 * still themes it exactly the way pi does.
 */
export function renderIdleHintLine(
  width: number,
  keybindings: KeybindingsManager,
): string {
  const clear = idleKeyNames("app.clear", keybindings);
  const line = [
    rawKeyHint(clear, "clear"),
    rawKeyHint(`${clear} twice`, "exit"),
    rawKeyHint(idleKeyNames("app.exit", keybindings), "exit when empty"),
    rawKeyHint(idleKeyNames("tui.input.submit", keybindings), "send to split"),
  ].join(" · ");
  return truncateToWidth(line, Math.max(1, Math.trunc(width)), " …");
}

/** Human-readable key(s) for a binding id, straight from the given registry. */
export function idleKeyNames(
  id: Keybinding,
  keybindings: KeybindingsManager,
): string {
  return keybindings.getKeys(id).join("/");
}

/**
 * The hint line as a component. It is a pure function of the width it is asked
 * for, so a resize re-renders it correctly with no cache to invalidate.
 */
class HintLineComponent implements Component {
  constructor(private readonly keybindings: KeybindingsManager) {}

  invalidate(): void {}

  render(width: number): string[] {
    return [renderIdleHintLine(width, this.keybindings)];
  }
}

const DEFAULT_STATUS: IdleStatus = { ready: 0, inProgress: 0 };

/**
 * Build the idle surface.
 *
 * Nothing is drawn until {@link IdleHandle.next} is called, so constructing a
 * mode is side-effect free and safe in a test.
 */
export function createIdleMode(options: IdleModeOptions = {}): IdleHandle {
  const clock = options.clock ?? (() => Date.now());
  const windowMs = options.doublePressWindowMs ?? DEFAULT_DOUBLE_PRESS_WINDOW_MS;
  const statusOption = options.status;
  const statusSource: () => Promise<IdleStatus> =
    statusOption === undefined
      ? async () => DEFAULT_STATUS
      : typeof statusOption === "function"
        ? async () => await statusOption()
        : async () => statusOption;
  const signals = options.signals ?? {
    on(signal, handler) {
      const target = process as unknown as {
        on(name: string, fn: () => void): unknown;
        removeListener(name: string, fn: () => void): unknown;
      };
      target.on(signal, handler);
      return () => {
        target.removeListener(signal, handler);
      };
    },
  };
  const goodbye =
    options.goodbye ?? ((line: string) => void process.stdout.write(`${line}\n`));

  let booted = false;
  let finished = false;
  let renderCount = 0;
  let lastArmedAt: number | undefined;
  let settled: ((outcome: IdleOutcome) => void) | undefined;

  let terminal: Terminal | undefined;
  let tui: TUI | undefined;
  let editor: CustomEditor | undefined;
  let statusLine: StatusLineComponent | undefined;
  let noticeComponent: Text | undefined;
  let previousKeybindings: KeybindingsManager | undefined;
  let activeKeybindings: KeybindingsManager | undefined;
  let signalUnsubs: readonly (() => void)[] = [];

  const theme = createIdleTextTheme();

  function showNotice(text: string): void {
    noticeComponent?.setText(theme.accent(`  ${text}`));
  }

  function paint(): void {
    if (finished) return;
    renderCount += 1;
    tui?.requestRender();
  }

  /** Put the freshly-read status into the line, at the current width. */
  async function repaintStatus(): Promise<void> {
    if (finished || statusLine === undefined || terminal === undefined) return;
    await statusLine.paint(terminal.columns);
    paint();
  }

  function armOrExit(): void {
    const now = clock();
    if (lastArmedAt !== undefined && now - lastArmedAt < windowMs) {
      void finishAndResolve({ kind: "exit", reason: "command" });
      return;
    }
    lastArmedAt = now;
    editor?.setText("");
    showNotice(
      `input cleared — press ${
        activeKeybindings === undefined
          ? "ctrl+c"
          : idleKeyNames("app.clear", activeKeybindings)
      } again within ${windowMs}ms to exit`,
    );
  }

  function handleSubmit(text: string): void {
    if (finished) return;
    const trimmed = text.trim();

    if (trimmed.length === 0) {
      // An empty submit is not work. The orchestrator rejects empty input as its
      // own event kind, so raising it here would be inventing a transition; we
      // just keep waiting.
      showNotice("nothing to act on — type what you want, or /exit to leave");
      paint();
      return;
    }

    const commandName = parseSlashCommandName(text);
    if (commandName !== undefined) {
      if (parseExitCommand(text) !== undefined) {
        void finishAndResolve({ kind: "exit", reason: "command" });
        return;
      }
      showNotice(`unknown command /${commandName} — still idle`);
      paint();
      return;
    }

    void finishAndResolve({ kind: "input", text });
  }

  async function finishAndResolve(outcome: IdleOutcome): Promise<void> {
    if (finished) return;
    const resolve = settled;
    settled = undefined;
    await teardown();
    resolve?.(outcome);
  }

  async function teardown(): Promise<void> {
    if (finished) return;
    finished = true;

    try {
      // pi's shutdown order: drain first so in-flight Kitty key-release events
      // cannot escape to the parent shell, then stop the renderer, then write.
      if (terminal !== undefined) {
        await terminal.drainInput(DRAIN_MAX_MS, DRAIN_IDLE_MS);
      }
    } catch {
      // A terminal that cannot drain is not a reason to leave the tty in raw mode.
    }

    // No further submits, no further renders.
    if (editor !== undefined) {
      editor.disableSubmit = true;
      editor.onSubmit = undefined;
      editor.onChange = undefined;
    }
    for (const unsubscribe of signalUnsubs) {
      try {
        unsubscribe();
      } catch {
        // A signal adapter that refuses to unregister is not ours to fix.
      }
    }
    signalUnsubs = [];

    try {
      tui?.stop();
    } catch {
      // Stop failing must still leave the terminal restored below.
    }

    if (previousKeybindings !== undefined) {
      setKeybindings(previousKeybindings);
      previousKeybindings = undefined;
    }

    // Only now, with the renderer stopped, is it safe to write the goodbye.
    goodbye(theme.dim("idle — terminal restored"));
  }

  function boot(): void {
    if (booted) return;

    initTheme(resolveIdleThemeName(process.env, options.themeName), false);

    const keybindings = options.keybindings ?? createIdleKeybindings();
    activeKeybindings = keybindings;
    previousKeybindings = getKeybindings();
    setKeybindings(keybindings);

    terminal = options.terminal ?? new ProcessTerminal();
    tui = new TuiMainScreen(terminal, true);

    statusLine = new StatusLineComponent(statusSource, theme);
    noticeComponent = new Text("", 0, 0);

    const root = new Container();
    root.addChild(statusLine);
    root.addChild(noticeComponent);
    editor = new CustomEditor(
      tui,
      createIdleEditorTheme(),
      asEditorKeybindings(keybindings),
      { paddingX: 1 },
    );
    editor.onSubmit = handleSubmit;
    editor.onAction("app.clear", armOrExit);
    // A notice is about the last thing that happened, not a permanent banner:
    // typing dismisses it.
    editor.onChange = () => {
      if (editor !== undefined && editor.getText().length > 0) {
        noticeComponent?.setText("");
      }
    };
    // CustomEditor only calls this when the editor is empty, matching pi.dev.
    editor.onCtrlD = () => {
      void finishAndResolve({ kind: "exit", reason: "command" });
    };
    editor.setAutocompleteProvider(
      new CombinedAutocompleteProvider(
        [...IDLE_SLASH_COMMANDS, ...(options.commands ?? [])],
        options.cwd ?? process.cwd(),
        null,
      ),
    );
    root.addChild(editor);
    root.addChild(new HintLineComponent(keybindings));
    tui.addChild(root);
    tui.setFocus(editor);
    booted = true;
  }

  return {
    next(): Promise<IdleOutcome> {
      if (finished) {
        return Promise.reject(
          new IdleError(
            "already-finished",
            "idle mode is already torn down; build a new one instead of waiting on a dead surface",
          ),
        );
      }
      const pending = new Promise<IdleOutcome>((resolve) => {
        settled = resolve;
      });
      if (!booted) {
        try {
          boot();
        } catch (error) {
          return Promise.reject(
            new IdleError(
              "boot-failed",
              `could not start the idle surface: ${error instanceof Error ? error.message : String(error)}`,
            ),
          );
        }
        tui?.start();
        signalUnsubs = [
          signals.on("SIGINT", () => armOrExit()),
          signals.on("SIGTERM", () => {
            void finishAndResolve({ kind: "exit", reason: "signal" });
          }),
        ];
      }
      void repaintStatus();
      return pending;
    },

    refresh(): void {
      void repaintStatus();
    },

    async dispose(): Promise<void> {
      await teardown();
    },

    get finished(): boolean {
      return finished;
    },

    get renderCount(): number {
      return renderCount;
    },
  };
}
