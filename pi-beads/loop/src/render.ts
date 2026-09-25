/**
 * Work presenter — the streaming view of an issue being worked, in pi.dev's pixels.
 *
 * Per [ADR-001](../docs/ADR-001-transport-and-rendering.md) the highlighted half
 * of this screen is not ours to draw. Assistant text goes through pi's own
 * `AssistantMessageComponent` — which owns the Markdown renderer and, through it,
 * the syntax highlighter — colours come from pi's live `Theme`, and the frame
 * diffing is pi-tui's `TuiMainScreen`. What this module adds is the thin part:
 *
 *   - one ordered column of *blocks*, so loop notices and agent output interleave
 *     in the order they actually happened;
 *   - one-line tool-call summaries built from `src/format.ts` instead of the raw
 *     JSON the agent protocol carries;
 *   - collapsed tool results with pi's own expand keybinding (`app.tools.expand`);
 *   - the footer: issue, phase, elapsed, tokens, model, thinking level, legend.
 *
 * What it deliberately is not:
 *
 *   - **a state machine.** Phases arrive through {@link WorkPresenter.setContext}
 *     as data and are only ever displayed. This file imports neither the
 *     orchestrator nor the loop, and holds no board or git handle at all, so it
 *     cannot decide anything about the run it is watching.
 *   - **a second owner of the terminal.** Only one surface may be live at a time.
 *     When the idle prompt needs the keyboard it calls {@link WorkPresenter.release}:
 *     this surface flushes, hides its footer, stops painting, and keeps its output
 *     in scrollback. {@link WorkPresenter.acquire} takes it back.
 *   - **a source of colours.** No escape sequence is ever written by hand here.
 *     Every colour comes from {@link PresenterTheme}, which is pi's live theme.
 */

import {
  AssistantMessageComponent,
  getMarkdownTheme,
  initTheme,
  rawKeyHint,
  type Theme,
  type ThemeColor,
} from "@earendil-works/pi-coding-agent";
import {
  Container,
  KeybindingsManager,
  ProcessTerminal,
  stripTerminalSequences,
  Text,
  matchesKey,
  truncateToWidth,
  TuiMainScreen,
  TUI_KEYBINDINGS,
  type Component,
  type Terminal,
  type TUI,
} from "@earendil-works/pi-tui";

import { formatToolArgs, formatToolResult, indentContent } from "./format.ts";
import type { RunnerEvent } from "./agent.ts";

// ── theme ────────────────────────────────────────────────────────────────────

/**
 * The theme roles this presenter uses. A closed list rather than `ThemeColor` so
 * the presenter's colour needs are visible and auditable in one place, while every
 * actual colour is pi's.
 */
export const PRESENTER_ROLES = [
  "accent",
  "dim",
  "muted",
  "text",
  "toolTitle",
  "toolOutput",
  "success",
  "warning",
  "error",
  "borderMuted",
] as const satisfies readonly ThemeColor[];

export type PresenterRole = (typeof PRESENTER_ROLES)[number];

/** Everything the presenter needs from a theme: two styled-string factories. */
export interface PresenterTheme {
  color(role: PresenterRole, text: string): string;
  bold(text: string): string;
}

/**
 * pi keeps the live theme on `globalThis` under a `Symbol.for` key, explicitly so
 * every module instance of pi resolves *one* theme ("This ensures all module
 * instances (tsx, jiti) see the same theme"). Reading it here gets the very
 * object pi's own components style themselves with — not a copy of it.
 */
const THEME_GLOBAL = Symbol.for("@earendil-works/pi-coding-agent:theme");

function readGlobalTheme(): unknown {
  return (globalThis as unknown as Record<symbol, unknown>)[THEME_GLOBAL];
}

/** The default theme: pi's live one, narrowed to {@link PRESENTER_ROLES}. */
export function createPresenterTheme(themeName?: string): PresenterTheme {
  let theme = readGlobalTheme();
  if (theme === undefined) {
    initTheme(themeName, false);
    theme = readGlobalTheme();
  }
  if (theme === undefined) {
    throw new RenderError(
      "theme-unavailable",
      "pi theme is still unset after initTheme(); drawing needs a theme",
    );
  }
  const resolved = theme as Theme;
  return {
    color: (role: PresenterRole, text: string): string => resolved.fg(role, text),
    bold: (text: string): string => resolved.bold(text),
  };
}

// ── errors ───────────────────────────────────────────────────────────────────

export type RenderErrorKind = "theme-unavailable" | "closed";

export class RenderError extends Error {
  readonly kind: RenderErrorKind;

  constructor(kind: RenderErrorKind, message: string) {
    super(message);
    this.name = "RenderError";
    this.kind = kind;
  }
}

// ── footer (pure) ────────────────────────────────────────────────────────────

/** Everything the footer knows. Every field is optional; none renders `undefined`. */
export interface FooterFields {
  readonly issueId?: string;
  readonly phase?: string;
  /**
   * Which run of the surface this is. A new `runId` is a new work unit even when
   * the issue is the same one — the shape a re-queued ticket always takes.
   *
   * Without it the surface keeps one clock per *issue*, and the second pass of a
   * ticket that timed out reports the two passes added together: a 20-minute
   * budget shown as `40:01`, which reads like the budget doubled rather than the
   * ticket failing twice.
   */
  readonly runId?: number;
  readonly elapsedMs?: number;
  readonly tokensIn?: number;
  readonly tokensOut?: number;
  readonly model?: string;
  readonly thinkingLevel?: string;
}

export interface FooterSegment {
  readonly label?: string;
  readonly text: string;
  readonly role: PresenterRole;
}

/** What a field that was never supplied reads as. Never `undefined`, ever. */
export const MISSING = "—";

export const FOOTER_SEPARATOR = " · ";

/**
 * The footer's contents and order, as data:
 * issue · phase · elapsed · tokens in→out · model · thinking · legend.
 *
 * Returning segments rather than a string is what lets the live surface colour the
 * same text the plain path prints: both render this one array.
 */
export function buildFooterSegments(
  fields: FooterFields,
  options: { legend?: string } = {},
): FooterSegment[] {
  const segments: FooterSegment[] = [
    { label: "issue", text: fields.issueId ?? MISSING, role: "accent" },
    { label: "phase", text: fields.phase ?? MISSING, role: "accent" },
    { label: "elapsed", text: formatElapsed(fields.elapsedMs), role: "text" },
    {
      label: "tokens",
      text: formatTokenPair(fields.tokensIn, fields.tokensOut),
      role: "text",
    },
    { label: "model", text: fields.model ?? MISSING, role: "text" },
    {
      label: "thinking",
      text: fields.thinkingLevel ?? MISSING,
      role: thinkingRole(fields.thinkingLevel),
    },
  ];
  if (options.legend !== undefined && options.legend !== "") {
    segments.push({ label: "keys", text: options.legend, role: "muted" });
  }
  return segments;
}

/** pi tints each thinking level differently; an unknown level stays dim. */
function thinkingRole(level: string | undefined): PresenterRole {
  switch (level) {
    case "minimal":
    case "low":
    case "medium":
    case "high":
    case "xhigh":
    case "max":
      return "accent";
    default:
      return "dim";
  }
}

/**
 * Render the segments to one line. `theme === null` is the plain path: identical
 * text, no colour. That equality is asserted in the suite — it is what makes
 * "same footer, any terminal" a fact rather than a hope.
 */
export function joinFooter(
  segments: readonly FooterSegment[],
  theme: PresenterTheme | null,
  width?: number,
): string {
  const line = segments
    .map((segment) => {
      const label = segment.label ?? "";
      if (theme === null) {
        return label === "" ? segment.text : `${label} ${segment.text}`;
      }
      const styledLabel = label === "" ? "" : theme.color("dim", `${label} `);
      return `${styledLabel}${theme.color(segment.role, segment.text)}`;
    })
    .join(FOOTER_SEPARATOR);
  return width === undefined ? line : truncateToWidth(line, width, "…");
}

/** `mm:ss`, or `h:mm:ss` past an hour. Time that never started reads `—`. */
export function formatElapsed(ms: number | undefined): string {
  if (ms === undefined || !Number.isFinite(ms) || ms < 0) return MISSING;
  const total = Math.floor(ms / 1000);
  const seconds = total % 60;
  const minutes = Math.floor(total / 60) % 60;
  const hours = Math.floor(total / 3600);
  const pad = (n: number): string => String(n).padStart(2, "0");
  if (hours > 0) return `${hours}:${pad(minutes)}:${pad(seconds)}`;
  return `${pad(minutes)}:${pad(seconds)}`;
}

/** `1234→567`; an unknown side reads `—` rather than vanishing. */
export function formatTokenPair(
  tokensIn: number | undefined,
  tokensOut: number | undefined,
): string {
  const one = (n: number | undefined): string =>
    n === undefined || !Number.isFinite(n)
      ? MISSING
      : String(Math.max(0, Math.floor(n)));
  return `${one(tokensIn)}→${one(tokensOut)}`;
}

// ── blocks ───────────────────────────────────────────────────────────────────
//
// A block is one entry in the ordered column. Each is a pi-tui `Component`, so
// the live TUI diffs it directly and the plain path can render the same object and
// strip the result — one renderer, two transports.

export type BlockKind = "notice" | "assistant" | "tool" | "event";
export type NoticeLevel = "info" | "warn" | "error";
export type ToolStatus = "pending" | "ok" | "error";

/** One line the loop said (`say` / `warn`), themed by level. */
class NoticeBlock implements Component {
  readonly kind = "notice" as const;
  readonly level: NoticeLevel;
  readonly text: string;
  private readonly view: Text;

  constructor(level: NoticeLevel, text: string, theme: PresenterTheme) {
    this.level = level;
    this.text = text;
    const role: PresenterRole =
      level === "error" ? "error" : level === "warn" ? "warning" : "text";
    this.view = new Text(theme.color(role, text), 1, 0);
  }

  render(width: number): string[] {
    return this.view.render(width);
  }

  invalidate(): void {
    this.view.invalidate();
  }
}

/** The agent's own prose — pi's Markdown renderer and highlighter, untouched. */
class AssistantBlock implements Component {
  readonly kind = "assistant" as const;
  private readonly component: AssistantMessageComponent;
  /** Plain text seen so far, so a test can read content without pixels. */
  private plain = "";
  private streaming = false;

  constructor() {
    // pi's own arguments: thinking visible, pi's markdown theme, pi's pad of 1.
    this.component = new AssistantMessageComponent(
      undefined,
      false,
      getMarkdownTheme(),
      "Thinking…",
      1,
      [],
    );
  }

  get text(): string {
    return this.plain;
  }

  get isStreaming(): boolean {
    return this.streaming;
  }

  update(message: unknown, streaming: boolean): void {
    this.plain = messageTextOf(message);
    this.streaming = streaming;
    this.component.updateContent(message as never, streaming);
  }

  render(width: number): string[] {
    return this.component.render(width);
  }

  invalidate(): void {
    this.component.invalidate();
  }
}

interface ToolViewOptions {
  readonly collapsedPreviewLines: number;
  readonly maxExpandedChars: number;
  readonly legend: string;
  /**
   * Where the pending-call animation comes from. `null` (or no source at all)
   * means there is no motion to show: the surface is not ours, the output is
   * plain, the presenter has no clock, or the call has settled. The block never
   * reads a clock itself — it is told what frame it is in.
   */
  readonly pulse?: () => ToolPulse | null;
}

/**
 * One pulse of indeterminate motion: which frame, and how long the call has
 * been outstanding. The frame says "still going"; the elapsed time says how
 * long "still" has been, which is the difference between waiting and
 * wondering.
 */
export interface ToolPulse {
  readonly tick: number;
  readonly elapsedMs: number;
}

/**
 * The pending glyph with nothing moving under it. Used when animation is off:
 * plain output, no clock, a surface we do not hold.
 */
const PENDING_GLYPH = "…";

/**
 * One second per turn of the spinner, at the default 120ms cadence. Exported
 * because "what the pending glyph can be" is a fact about the surface that
 * tests and any other reader should be able to ask for rather than guess.
 */
export const SPINNER_FRAMES: readonly string[] = [
  "⠋",
  "⠙",
  "⠹",
  "⠸",
  "⠼",
  "⠴",
  "⠦",
  "⠧",
  "⠇",
  "⠏",
];

function spinnerFrame(tick: number): string {
  return SPINNER_FRAMES[Math.abs(tick) % SPINNER_FRAMES.length] ?? PENDING_GLYPH;
}

/**
 * One tool call: a single summary line, its result collapsed underneath.
 *
 * The summary comes from `formatToolArgs` — `$ command` for bash, `path:1-50` for
 * read, `path (2 edits)` for edit, `path (4.2KB)` for write. The result comes
 * from `formatToolResult`'s bounded text: a short preview until the operator asks
 * for more, with pi's own "… (N more lines, ctrl+o to expand)" phrasing on the cut.
 */
class ToolBlock implements Component {
  readonly kind = "tool" as const;
  readonly callId: string;
  readonly name: string;
  private readonly theme: PresenterTheme;
  private readonly view: ToolViewOptions;
  private args: unknown;
  private status: ToolStatus = "pending";
  private result: unknown;
  private hasResult = false;
  private isExpanded: boolean;

  constructor(
    callId: string,
    name: string,
    args: unknown,
    theme: PresenterTheme,
    view: ToolViewOptions,
    expanded = false,
  ) {
    this.callId = callId;
    this.name = name;
    this.theme = theme;
    this.view = view;
    this.args = args;
    this.isExpanded = expanded;
  }

  get expanded(): boolean {
    return this.isExpanded;
  }

  get toolStatus(): ToolStatus {
    return this.status;
  }

  setExpanded(expanded: boolean): void {
    this.isExpanded = expanded;
  }

  updateArgs(args: unknown): void {
    this.args = args;
  }

  updateResult(result: unknown): void {
    this.result = result;
    this.hasResult = true;
  }

  finish(result: unknown, isError: boolean): void {
    this.result = result;
    this.hasResult = true;
    this.status = isError ? "error" : "ok";
  }

  /** The one-line summary. Single line by construction, whatever the tool said. */
  summary(): string {
    return oneLine(formatToolArgs(this.name, this.args));
  }

  /** The result text currently shown, escape-free and bounded. */
  visibleResult(): string {
    if (!this.hasResult) return "";
    const lines = this.fullResultLines();
    if (this.isExpanded) return lines.join("\n");
    return lines.slice(0, Math.max(1, this.view.collapsedPreviewLines)).join("\n");
  }

  hiddenLineCount(): number {
    if (!this.hasResult) return 0;
    const total = this.fullResultLines().length;
    const shown = this.isExpanded
      ? total
      : Math.min(total, Math.max(1, this.view.collapsedPreviewLines));
    return Math.max(0, total - shown);
  }

  private fullResultLines(): string[] {
    return formatToolResult(this.name, this.result, this.view.maxExpandedChars).split(
      "\n",
    );
  }

  private glyph(): string {
    if (this.status === "error") return "✗";
    if (this.status === "ok") return "✓";
    const pulse = this.pulseNow();
    return pulse === null ? PENDING_GLYPH : spinnerFrame(pulse.tick);
  }

  private pulseNow(): ToolPulse | null {
    if (this.status !== "pending") return null;
    return this.view.pulse?.() ?? null;
  }

  /**
   * How long this call has been outstanding, undecorated. Suppressed under a
   * second: an instant call showing `00:00` is noise, and a call that has been
   * going two minutes is the thing worth saying.
   */
  pendingTimePlain(): string {
    const pulse = this.pulseNow();
    if (pulse === null || pulse.elapsedMs < 1_000) return "";
    return ` · ${formatElapsed(pulse.elapsedMs)}`;
  }

  private pendingTime(): string {
    const text = this.pendingTimePlain();
    return text === "" ? "" : this.theme.color("muted", text);
  }

  /** Header line only — exposed so the one-line rule is directly testable. */
  headerPlain(): string {
    return `${this.glyph()} ${this.name} ${this.summary()}${this.pendingTimePlain()}`;
  }

  /** Nothing is cached between frames: every render is a fresh string build. */
  invalidate(): void {
    /* no cache */
  }

  render(width: number): string[] {
    const glyphRole: PresenterRole =
      this.status === "error" ? "error" : this.status === "ok" ? "success" : "muted";
    const header = [
      " ",
      this.theme.color(glyphRole, this.glyph()),
      " ",
      this.theme.bold(this.theme.color("toolTitle", this.name)),
      " ",
      this.theme.color("muted", this.summary()),
      this.pendingTime(),
    ].join("");

    const lines = [truncateToWidth(header, width, "…")];
    const text = this.visibleResult();
    if (text === "") return lines;

    // A uniform two-space gutter under the header, pi's `toolOutput` role on
    // every line: the result reads as one block, not as a ragged tail.
    for (const line of text.split("\n")) {
      lines.push(truncateToWidth(`   ${this.theme.color("toolOutput", line)}`, width, "…"));
    }

    const hidden = this.hiddenLineCount();
    if (hidden > 0) {
      // The count is explicit and leads the line: `+N more lines`. A reader who
      // never touches the expand key still knows how much they did not see.
      const cut = `+${hidden} more ${hidden === 1 ? "line" : "lines"}`;
      const hint = `${this.theme.color("muted", cut)} ${this.theme.color(
        "muted",
        `(${this.view.legend})`,
      )}`;
      lines.push(truncateToWidth(`   ${hint}`, width, "…"));
    }
    return lines;
  }
}

/** One honest line for anything the presenter has no richer rendering for. */
class EventBlock implements Component {
  readonly kind = "event" as const;
  readonly text: string;
  private readonly view: Text;

  constructor(text: string, theme: PresenterTheme) {
    this.text = text;
    this.view = new Text(theme.color("dim", text), 1, 0);
  }

  render(width: number): string[] {
    return this.view.render(width);
  }

  invalidate(): void {
    this.view.invalidate();
  }
}

export type RenderBlock = NoticeBlock | AssistantBlock | ToolBlock | EventBlock;

/**
 * The footer: one line at normal widths, wrapped rather than truncated when the
 * terminal is narrow — a truncated footer is a footer that lost a field, and the
 * whole point is that every field is readable at a glance. Hidden at teardown so it
 * is the last thing to go and does not sit in scrollback pretending to be current.
 */
class FooterBlock implements Component {
  private readonly theme: PresenterTheme;
  private readonly legend: string;
  private line = "";
  private visible = true;
  private readonly view = new Text("", 0, 0);

  constructor(theme: PresenterTheme, legend: string) {
    this.theme = theme;
    this.legend = legend;
  }

  show(): void {
    this.visible = true;
  }

  hide(): void {
    this.visible = false;
  }

  setFields(fields: FooterFields, elapsedMs: number | undefined): void {
    const segments = buildFooterSegments(
      { ...fields, elapsedMs },
      { legend: this.legend },
    );
    // One space of gutter, so the footer lines up with the blocks above it.
    this.line = ` ${joinFooter(segments, this.theme)}`;
    this.view.setText(this.line);
  }

  /** The same line with no colour — exactly what the plain path prints. */
  plainLine(): string {
    return this.line === "" ? "" : stripTerminalSequences(this.line);
  }

  render(width: number): string[] {
    if (!this.visible || this.line === "") return [];
    return this.view.render(width);
  }

  invalidate(): void {
    this.view.invalidate();
  }
}

// ── presenter ────────────────────────────────────────────────────────────────

export interface WorkPresenterOptions {
  /** Real terminal for the live surface. Defaults to `ProcessTerminal`. */
  readonly terminal?: Terminal;
  /** Where the escape-free path writes. Defaults to `process.stdout`. */
  readonly write?: (chunk: string) => void;
  /**
   * Force the rendering path. Default: live (colour, diffed) when a TTY is
   * attached, plain text otherwise. `tty: false` alongside an injected terminal is
   * how the suite checks the escape-free degradation without a real pipe.
   */
  readonly tty?: boolean;
  readonly now?: () => number;
  /** Timer injection for the coalescing window; returns a cancel function. */
  readonly schedule?: (run: () => void, ms: number) => () => void;
  /**
   * Frame coalescing window in ms — this is the refresh-rate knob. Default 33:
   * a *burst* of deltas costs one frame, so a stream paints at ~30fps however
   * fast the tokens arrive. 16 gives ~60fps; below that the terminal is being
   * written more often than it paints, and nothing looks smoother.
   */
  readonly coalesceMs?: number;
  /**
   * Repaint cadence while the live surface is held, in ms. Default 500. Without
   * one, a frame only appears when an event arrives — so the footer's elapsed
   * field stops moving during a long tool call or a slow first token. A tick is
   * not a paint: it goes through the coalescing path and repaints only when
   * time-derived content has actually changed. 0 disables the heartbeat.
   */
  readonly heartbeatMs?: number;
  /**
   * Frame cadence while a call is outstanding, in ms. Default 120.
   *
   * The heartbeat keeps a clock, which is a one-second thing. A pending tool
   * line needs a faster beat than that: at one frame a second a spinner is a
   * flicker, and the screen is indistinguishable from one that hung. The two
   * cadences share one timer — while something is outstanding the heartbeat runs
   * at this rate instead of its own, and drops back the moment it is not.
   * 0 turns the animation off and leaves the plain `…` pending glyph.
   */
  readonly spinnerMs?: number;
  readonly theme?: PresenterTheme;
  readonly themeName?: string;
  readonly keybindings?: KeybindingsManager;
  /** Key(s) that expand tool output. Default: pi's `app.tools.expand` binding. */
  readonly expandKey?: string | readonly string[];
  /** Result lines shown before the expand hint. Default 2. */
  readonly collapsedPreviewLines?: number;
  /** Hard cap on rendered result characters, expanded included. Default 4000. */
  readonly maxExpandedChars?: number;
  /** Width the plain path uses when there is no terminal to measure. */
  readonly plainWidth?: number;
  /** Put the key legend in the footer. Default true (dropped if no key is known). */
  readonly legend?: boolean;
}

export interface PresenterStats {
  readonly paints: number;
  readonly plainWrites: number;
  readonly blocks: number;
  readonly coalescedTicks: number;
  /** The cadence this presenter was built with, so the knob is checkable. */
  readonly coalesceMs: number;
  readonly heartbeatMs: number;
  readonly spinnerMs: number;
  /** A pending tool call is outstanding, so the surface is animating. */
  readonly animating: boolean;
  /** A frame has been asked for and has not been drawn yet. */
  readonly paintPending: boolean;
  readonly live: boolean;
  readonly expanded: boolean;
}

/** What `app.ts` wires up. Nothing more, so nothing more can be reached. */
export interface WorkPresenter {
  readonly isLive: boolean;
  readonly path: "live" | "plain";
  feed(event: RunnerEvent): void;
  notice(level: NoticeLevel, text: string): void;
  say(text: string): void;
  warn(text: string): void;
  setContext(patch: Partial<FooterFields>): void;
  setExpanded(expanded: boolean): void;
  toggleExpanded(): void;
  acquire(): void;
  release(): void;
  flushSync(): void;
  captureFrame(): string[];
  capturePlain(): string[];
  stats(): PresenterStats;
  dispose(): void;
}

const DEFAULT_COALESCE_MS = 33;
const DEFAULT_HEARTBEAT_MS = 500;
const DEFAULT_SPINNER_MS = 120;
const DEFAULT_COLLAPSED_LINES = 2;
const DEFAULT_MAX_EXPANDED_CHARS = 4_000;
const DEFAULT_PLAIN_WIDTH = 80;

/** pi's own id, so the legend and any user remap mean what they mean in pi. */
const EXPAND_KEYBINDING = "app.tools.expand" as never;

function defaultSchedule(run: () => void, ms: number): () => void {
  const timer = setTimeout(run, ms);
  timer.unref?.();
  return (): void => {
    clearTimeout(timer);
  };
}

function oneLine(text: string): string {
  return (text.split("\n")[0] ?? "").replace(/\s+$/, "");
}

function messageTextOf(message: unknown): string {
  const content = asRecord(message)?.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter(
      (part) =>
        typeof part === "object" &&
        part !== null &&
        (part as { type?: unknown }).type === "text" &&
        typeof (part as { text?: unknown }).text === "string",
    )
    .map((part) => (part as { text: string }).text)
    .join("");
}

/** True when a message has prose or thinking worth a block of its own. */
function isRenderableAssistant(message: unknown): boolean {
  const record = asRecord(message);
  if (record?.role !== "assistant") return false;
  const content = record.content;
  if (typeof content === "string") return content.trim() !== "";
  if (!Array.isArray(content)) return false;
  return content.some((part) => {
    const item = asRecord(part);
    if (item === null) return false;
    if (item.type === "text" || item.type === "thinking") {
      const value = item.text ?? item.thinking;
      return typeof value === "string" && value.trim() !== "";
    }
    return false;
  });
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (typeof value !== "object" || value === null) return null;
  return value as Record<string, unknown>;
}

function rawType(raw: unknown): string | null {
  const type = asRecord(raw)?.type;
  return typeof type === "string" ? type : null;
}

/** pi's `stopReason`, read defensively — an absent one is not an error. */
function stopReasonOf(message: unknown): string | null {
  const reason = asRecord(message)?.stopReason;
  return typeof reason === "string" ? reason : null;
}

/** The least the presenter needs to describe a finished work unit honestly. */
export interface OutcomeFacts {
  readonly kind: string;
  readonly issueId?: string;
  /** Failure text, where the kind carries one. */
  readonly message?: string;
  /** Timeout budget, in ms. */
  readonly budgetMs?: number;
  /** How long the run had actually been going, in ms. Not the budget. */
  readonly elapsedMs?: number;
  /** Did the session settle after `abort()`? */
  readonly settledAfterAbort?: boolean;
  /** Assistant turns taken, for the kinds that stop early. */
  readonly turns?: number;
  /** Where the context stood when the run was stopped, pre-formatted. */
  readonly contextText?: string;
  /** Extra context on what the run left behind (files, commits, verdicts). */
  readonly leftBehind?: string;
}

/**
 * What a finished work unit says about itself: a level and one line.
 *
 * Pure, and exported so the composition root can name the outcome without the
 * presenter ever deciding anything: this maps a fact that already happened to
 * words and a colour. Each kind gets its own word — "timed out", "aborted",
 * "failed", "finished" — because "it ended" tells an operator nothing.
 */
export function describeOutcome(facts: OutcomeFacts): {
  level: NoticeLevel;
  text: string;
} {
  const issue = facts.issueId ?? MISSING;
  const tail =
    facts.leftBehind === undefined || facts.leftBehind.trim() === ""
      ? ""
      : ` — left behind: ${oneLine(facts.leftBehind)}`;
  switch (facts.kind) {
    case "done":
      return { level: "info", text: `finished ${issue}${tail}` };
    case "incomplete":
      return {
        level: "warn",
        text: `incomplete ${issue}: the model stopped short of done${tail}`,
      };
    case "unstructured-verdict":
      return {
        level: "warn",
        text: `unstructured verdict on ${issue}: no verdict block was reported${tail}`,
      };
    case "malformed-verdict":
      return {
        level: "error",
        text: `malformed verdict on ${issue}${tail}`,
      };
    case "context-exhausted":
      return {
        level: "error",
        text:
          `context exhausted ${issue}` +
          (facts.turns === undefined ? "" : ` after ${facts.turns} turn(s)`) +
          (facts.contextText === undefined ? "" : `: ${oneLine(facts.contextText)}`) +
          tail,
      };
    case "timeout": {
      // `elapsed` and `budget` are two different numbers and the old line passed
      // the budget as if it were the first. On a re-queued ticket that is the
      // difference between "this pass ran out" and "something is 40 minutes
      // late", and only one of those is true.
      const clock =
        facts.elapsedMs === undefined
          ? `after ${formatElapsed(facts.budgetMs)}`
          : `after ${formatElapsed(facts.elapsedMs)} of a ${formatElapsed(facts.budgetMs)} budget`;
      return {
        level: "error",
        text:
          `timed out ${issue} ${clock}` +
          ` (${facts.settledAfterAbort === false ? "still running when the grace period ended" : "session settled after abort"})${tail}`,
      };
    }
    case "error":
      return {
        level: "error",
        text: `failed ${issue}${
          facts.message === undefined ? "" : `: ${oneLine(facts.message)}`
        }${tail}`,
      };
    default:
      return {
        level: "warn",
        text: `ended ${issue} with an unknown outcome (${oneLine(facts.kind)})${tail}`,
      };
  }
}

class Presenter implements WorkPresenter {
  readonly path: "live" | "plain";
  private readonly theme: PresenterTheme;
  private readonly keybindings: KeybindingsManager;
  /** The keys shown in the footer legend. */
  private readonly expandKeys: readonly string[];
  /** The live matcher: the binding through the manager, or an explicit override. */
  private readonly matchExpand: (data: string) => boolean;
  private readonly legend: string;
  private readonly now: () => number;
  private readonly schedule: (run: () => void, ms: number) => () => void;
  private readonly coalesceMs: number;
  private readonly write: (chunk: string) => void;
  private readonly plainWidth: number;
  private readonly viewOptions: ToolViewOptions;
  private readonly terminalFactory: () => Terminal;

  private blocks: RenderBlock[] = [];
  private readonly tools = new Map<string, ToolBlock>();
  private activeAssistant: AssistantBlock | null = null;
  private fields: FooterFields = {};
  private footer: FooterBlock;
  private readonly body = new Container();
  private readonly surface = new Container();

  private terminal: Terminal | null = null;
  private tui: TUI | null = null;
  private live = false;
  private closed = false;

  /** Blocks the live surface started from; earlier ones are scrollback already. */
  private anchoredAt = 0;
  /** Blocks already written out through the plain path. */
  private printedThrough = 0;

  private dirty = false;
  private cancelTimer: (() => void) | null = null;
  /** The re-arming heartbeat timer, live only while the surface is held. */
  private heartbeatCancel: (() => void) | null = null;
  private heartbeatMs: number;
  private readonly spinnerMs: number;
  /** Bumped on each animation beat; picks the spinner frame. */
  private spinnerTick = 0;
  /** Calls opened and not yet reported back. Drives the fast beat. */
  private pendingCalls = 0;
  /** When each outstanding call opened, for the per-call elapsed time. */
  private readonly toolOpenedAt = new Map<string, number>();
  /** Elapsed text as of the last footer sync, so a tick can tell if time moved. */
  private lastElapsed = "";
  private paints = 0;
  private ticks = 0;
  private plainWrites = 0;
  private expandedAll = false;
  private workStartedAt: number | null = null;
  private tokensIn: number | undefined = undefined;
  private tokensOut: number | undefined = undefined;
  private lastPlainFooter = "";

  constructor(options: WorkPresenterOptions = {}) {
    this.theme = options.theme ?? createPresenterTheme(options.themeName);
    this.now = options.now ?? ((): number => Date.now());
    this.schedule = options.schedule ?? defaultSchedule;
    this.coalesceMs = options.coalesceMs ?? DEFAULT_COALESCE_MS;
    this.heartbeatMs = Math.max(0, options.heartbeatMs ?? DEFAULT_HEARTBEAT_MS);
    this.spinnerMs = Math.max(0, options.spinnerMs ?? DEFAULT_SPINNER_MS);
    this.plainWidth = options.plainWidth ?? DEFAULT_PLAIN_WIDTH;
    this.terminalFactory =
      options.terminal !== undefined
        ? (): Terminal => options.terminal as Terminal
        : (): Terminal => new ProcessTerminal();
    // The escape-free path shares the injected terminal when there is one, so a
    // released presenter and a live one are never writing to two different
    // streams. Without an injected terminal it is stdout, like any log.
    this.write =
      options.write ??
      ((chunk: string): void => {
        if (options.terminal !== undefined) options.terminal.write(chunk);
        else process.stdout.write(chunk);
      });

    const tty = options.tty ?? Boolean(process.stdout.isTTY);
    this.path = tty ? "live" : "plain";

    // Our own manager, never the process-global one: presenting must not rewrite
    // anyone else's input configuration.
    this.keybindings =
      options.keybindings ??
      (new KeybindingsManager({
        ...TUI_KEYBINDINGS,
        [EXPAND_KEYBINDING]: {
          defaultKeys: ["ctrl+o"],
          description: "Toggle tool output",
        },
      } as never));

    if (options.expandKey !== undefined) {
      const explicit = (
        typeof options.expandKey === "string"
          ? [options.expandKey]
          : Array.from(options.expandKey)
      ).filter((key) => typeof key === "string" && key.length > 0);
      this.expandKeys = explicit;
      this.matchExpand = (data: string): boolean =>
        explicit.some((key) => matchesKey(data, key as never));
    } else {
      this.expandKeys = keysFor(this.keybindings, EXPAND_KEYBINDING);
      this.matchExpand = (data: string): boolean => {
        try {
          return this.keybindings.matches(data, EXPAND_KEYBINDING);
        } catch {
          return false;
        }
      };
    }
    this.legend =
      options.legend === false || this.expandKeys.length === 0
        ? ""
        : rawKeyHint(this.expandKeys.join("/"), "toggle tool output");

    this.viewOptions = {
      collapsedPreviewLines:
        options.collapsedPreviewLines ?? DEFAULT_COLLAPSED_LINES,
      maxExpandedChars: options.maxExpandedChars ?? DEFAULT_MAX_EXPANDED_CHARS,
      legend: this.legend,
    };

    this.footer = new FooterBlock(this.theme, this.legend);
    this.surface.addChild(this.body);
    this.surface.addChild(this.footer);
  }

  get isLive(): boolean {
    return this.live;
  }

  // ── surface ownership ──────────────────────────────────────────────────────

  /** Take the terminal back from whatever held it (the idle prompt). */
  acquire(): void {
    if (this.closed || this.live || this.path !== "live") return;
    // Everything up to here is already on screen or written out, so the live
    // surface starts empty and never paints twice over its own scrollback.
    this.anchoredAt = this.blocks.length;
    this.terminal = this.terminalFactory();
    const tui = new TuiMainScreen(this.terminal, false);
    tui.addChild(this.surface);
    tui.addInputListener((data: string) => this.onInput(data));
    tui.start();
    this.tui = tui;
    this.live = true;
    this.footer.show();
    this.syncFooter();
    this.paint();
    // The surface is ours and the clock is still running: keep it honest between
    // events. Stopped again by release(), so idle never inherits our timer.
    this.armHeartbeat();
  }

  /** Hand the terminal over: flush, drop the footer, stop painting. */
  release(): void {
    if (this.closed) return;
    this.stopHeartbeat();
    if (this.path === "plain") {
      this.emitPlainTail(true);
      this.emitPlainFooter(true);
      return;
    }
    if (!this.live) {
      this.printedThrough = Math.max(this.printedThrough, this.blocks.length);
      return;
    }
    this.flushSync();
    // The footer is a live artefact. It goes before the surface does, so what is
    // left in scrollback is content, not a status line claiming to be current.
    this.footer.hide();
    this.paint();
    this.live = false;
    this.cancelPending();
    if (this.tui !== null) {
      this.tui.stop();
      this.tui = null;
    }
    this.terminal = null;
    this.printedThrough = this.blocks.length;
  }

  // ── input ──────────────────────────────────────────────────────────────────

  private onInput(data: string): { consume?: boolean } | undefined {
    if (this.expandKeys.length === 0 || !this.matchExpand(data)) return undefined;
    this.toggleExpanded();
    return { consume: true };
  }

  setExpanded(expanded: boolean): void {
    this.expandedAll = expanded;
    for (const block of this.tools.values()) block.setExpanded(expanded);
    this.body.invalidate();
    this.markDirty();
  }

  toggleExpanded(): void {
    this.setExpanded(!this.expandedAll);
  }

  // ── content ────────────────────────────────────────────────────────────────

  say(text: string): void {
    this.notice("info", text);
  }

  warn(text: string): void {
    this.notice("warn", text);
  }

  notice(level: NoticeLevel, text: string): void {
    if (this.closed) return;
    const safe = text ?? "";
    if (safe.trim() === "") return;
    this.append(new NoticeBlock(level, safe, this.theme));
  }

  /**
   * Footer facts arrive as data. The presenter never derives a phase, never
   * infers an issue, and never touches a board: it is told, and it displays.
   */
  setContext(patch: Partial<FooterFields>): void {
    if (this.closed) return;
    const previousIssue = this.fields.issueId;
    const previousRun = this.fields.runId;
    this.fields = { ...this.fields, ...patch };
    // A new work unit: counts, clock and expand state reset, so the footer
    // describes the thing actually running. A new issue is one; so is a new run
    // of the issue that is already showing, which is what a re-queued ticket is.
    const newUnit =
      (patch.runId !== undefined && patch.runId !== previousRun) ||
      (patch.issueId !== undefined && patch.issueId !== previousIssue);
    if (newUnit) {
      this.tokensIn = undefined;
      this.tokensOut = undefined;
      this.workStartedAt = this.now();
      this.activeAssistant = null;
      this.setExpanded(false);
    }
    if (this.workStartedAt === null && this.fields.issueId !== undefined) {
      this.workStartedAt = this.now();
    }
    this.syncFooter();
    if (this.path === "plain") this.emitPlainFooter(false);
    else this.markDirty();
  }

  /**
   * The one entry point for runner events. It does not throw: a shape it does not
   * recognise becomes one honest line, which is more than the protocol promised.
   */
  feed(event: RunnerEvent): void {
    if (this.closed || typeof event !== "object" || event === null) return;
    try {
      this.applyEvent(event);
    } catch {
      // Presentation must never take the run down with it.
      this.append(
        new EventBlock("event: presenter could not render this (suppressed)", this.theme),
      );
    }
  }

  private applyEvent(event: RunnerEvent): void {
    switch (event.type) {
      case "agent_event":
        this.applyAgentEvent(event);
        return;
      case "timeout": {
        const issue = this.fields.issueId;
        const detail = event.detail === undefined ? "" : ` — ${oneLine(event.detail)}`;
        // The runner's own elapsed when it called the budget spent. Its own clock
        // is the fallback only: this surface outlives a run, and its clock is the
        // wrong answer for a pass that started after the one being reported.
        const elapsed = event.elapsedMs ?? this.elapsedMs();
        const budget =
          event.budgetMs === undefined ? "" : ` of a ${formatElapsed(event.budgetMs)} budget`;
        this.notice(
          "error",
          `timed out after ${formatElapsed(elapsed)}${budget}${
            issue === undefined ? "" : ` (issue ${issue})`
          }${detail}`,
        );
        return;
      }
      case "wrap_up": {
        const detail = event.detail === undefined ? "" : ` — ${oneLine(event.detail)}`;
        this.notice(
          "warn",
          `wrapping up${this.fields.issueId === undefined ? "" : ` ${this.fields.issueId}`}${detail}`,
        );
        return;
      }
      case "context_note": {
        const detail = event.detail ?? "";
        if (detail.trim() === "") return;
        // A note is prose: multi-line payloads hang under their own first line so
        // the block reads as one quoted item rather than a wall.
        this.append(new EventBlock(indentContent(detail, 2), this.theme));
        return;
      }
      case "session_disposed": {
        // A clean teardown is noise; a teardown that reported something is not.
        const detail = event.detail;
        if (detail !== undefined && detail.trim() !== "") {
          this.notice("warn", `session closed: ${oneLine(detail)}`);
        }
        return;
      }
      case "session_created":
        return;
      case "tool_call": {
        const raw = asRecord(event.raw);
        const name =
          typeof raw?.toolName === "string"
            ? raw.toolName
            : oneLine(String(event.detail ?? "tool"));
        const callId =
          typeof raw?.toolCallId === "string"
            ? raw.toolCallId
            : `call-${this.blocks.length}`;
        this.openTool(callId, name, raw?.args);
        return;
      }
      default:
        this.append(new EventBlock(`event ${String(event.type)}: no rendering`, this.theme));
        return;
    }
  }

  private applyAgentEvent(event: RunnerEvent): void {
    const raw = event.raw;
    const type = rawType(raw);
    if (type === null) {
      this.append(new EventBlock("event: payload is not a pi event", this.theme));
      return;
    }
    const record = asRecord(raw);

    switch (type) {
      case "message_start":
      case "message_update":
      case "message_end": {
        const message = record?.message;
        // Usage is counted whatever the message contained: a turn that produced
        // only a tool call still spent tokens, and a footer that ignores them
        // reads as a run that did nothing.
        if (type === "message_end") this.addUsage(message);
        if (!isRenderableAssistant(message)) {
          if (type === "message_end") {
            this.activeAssistant = null;
            this.reportStop(stopReasonOf(message));
          }
          return;
        }
        const block = this.ensureAssistant();
        block.update(message, type !== "message_end");
        // The block changed; the terminal has not been told. Asked here, every
        // delta asks — `markDirty` is coalesced, so rule 3 still holds (a burst
        // costs one frame, not one write per token). Without this ask a
        // streamed reply paints exactly once, when its block is created, and
        // then only when the heartbeat notices the footer's `elapsed` string
        // moved: one frame per second, on a reply that is still streaming.
        this.markDirty();
        if (type === "message_end") {
          this.activeAssistant = null;
          // A reply that stopped because it was cut off, aborted or errored is
          // not prose that finished. Say so, in the theme's own warning colour.
          this.reportStop(stopReasonOf(message));
        }
        return;
      }
      case "tool_execution_start": {
        const callId =
          typeof record?.toolCallId === "string"
            ? record.toolCallId
            : `call-${this.blocks.length}`;
        const name = typeof record?.toolName === "string" ? record.toolName : "tool";
        this.openTool(callId, name, record?.args);
        return;
      }
      case "tool_execution_update": {
        const block =
          typeof record?.toolCallId === "string"
            ? this.tools.get(record.toolCallId) ?? null
            : null;
        if (block === null) return;
        if (record?.args !== undefined) block.updateArgs(record.args);
        if (record?.partialResult !== undefined) block.updateResult(record.partialResult);
        this.markDirty();
        return;
      }
      case "tool_execution_end": {
        const block =
          typeof record?.toolCallId === "string"
            ? this.tools.get(record.toolCallId) ?? null
            : null;
        if (block === null) return;
        const wasPending = block.toolStatus === "pending";
        block.finish(record?.result, record?.isError === true);
        if (wasPending) {
          // The call reported back: stop animating it, and stop paying for the
          // fast beat. The bookkeeping is deleted rather than zeroed so a long
          // run does not accumulate timestamps for calls that finished hours ago.
          this.pendingCalls = Math.max(0, this.pendingCalls - 1);
          this.toolOpenedAt.delete(block.callId);
          this.refreshCadence();
        }
        this.markDirty();
        return;
      }
      case "thinking_level_changed": {
        if (typeof record?.level === "string") {
          this.setContext({ thinkingLevel: record.level });
        }
        return;
      }
      case "compaction_start":
        this.notice("info", `compacting context (${String(record?.reason ?? "manual")})`);
        return;
      case "compaction_end": {
        const aborted = record?.aborted === true;
        const failure =
          typeof record?.errorMessage === "string" ? record.errorMessage : undefined;
        if (aborted || failure !== undefined) {
          this.notice(
            "warn",
            `compaction ${aborted ? "aborted" : "failed"}${
              failure === undefined ? "" : `: ${oneLine(failure)}`
            }`,
          );
        } else {
          this.notice("info", "context compacted");
        }
        return;
      }
      case "auto_retry_start":
        this.notice(
          "warn",
          `retry ${String(record?.attempt ?? "?")}/${String(record?.maxAttempts ?? "?")} in ${Math.round(
            Number(record?.delayMs ?? 0) / 1000,
          )}s: ${oneLine(String(record?.errorMessage ?? ""))}`,
        );
        return;
      case "auto_retry_end": {
        const ok = record?.success === true;
        const finalError =
          typeof record?.finalError === "string" ? `: ${oneLine(record.finalError)}` : "";
        this.notice(
          ok ? "info" : "error",
          `retry ${ok ? "succeeded" : "gave up"} after ${String(record?.attempt ?? "?")} attempt(s)${
            ok ? "" : finalError
          }`,
        );
        return;
      }
      // Stream-level bookkeeping the blocks and footer already cover. Drawing it
      // would be noise where the operator is reading prose.
      case "agent_start":
      case "agent_end":
      case "agent_settled":
      case "turn_start":
      case "turn_end":
      case "queue_update":
      case "entry_appended":
      case "session_info_changed":
      case "bash_execution_update":
      case "summarization_retry_scheduled":
      case "summarization_retry_attempt_start":
      case "summarization_retry_finished":
        return;
      default:
        // One honest line, no JSON, no throw.
        this.append(new EventBlock(`event ${type}: no rendering`, this.theme));
        return;
    }
  }

  private ensureAssistant(): AssistantBlock {
    if (this.activeAssistant !== null) return this.activeAssistant;
    const block = new AssistantBlock();
    this.activeAssistant = block;
    this.append(block);
    return block;
  }

  private openTool(callId: string, name: string, args: unknown): ToolBlock {
    const existing = this.tools.get(callId);
    if (existing !== undefined) {
      existing.updateArgs(args);
      this.markDirty();
      return existing;
    }
    const view: ToolViewOptions = {
      ...this.viewOptions,
      pulse: (): ToolPulse | null => this.pulseFor(callId),
    };
    const block = new ToolBlock(callId, name, args, this.theme, view, this.expandedAll);
    this.tools.set(callId, block);
    this.toolOpenedAt.set(callId, this.now());
    this.pendingCalls += 1;
    // The prose resumes after the tool, in a new block — never inside this one.
    this.activeAssistant = null;
    this.append(block);
    return block;
  }

  /**
   * A turn that stopped for a reason other than finishing says so out loud.
   * `stop` and `toolUse` are normal and silent; `length`, `aborted` and `error`
   * are each a different problem and get a different word and colour.
   */
  private reportStop(reason: string | null): void {
    switch (reason) {
      case "aborted":
        this.notice("warn", "reply aborted");
        return;
      case "error":
        this.notice("error", "reply ended in an error");
        return;
      case "length":
        this.notice("warn", "reply cut off at the token limit");
        return;
      default:
        return;
    }
  }

  private addUsage(message: unknown): void {
    const usage = asRecord(asRecord(message)?.usage);
    if (usage === null) return;
    if (typeof usage.input === "number") {
      this.tokensIn = (this.tokensIn ?? 0) + usage.input;
    }
    if (typeof usage.output === "number") {
      this.tokensOut = (this.tokensOut ?? 0) + usage.output;
    }
  }

  // ── rendering ──────────────────────────────────────────────────────────────

  private append(block: RenderBlock): void {
    this.blocks.push(block);
    if (this.live) {
      this.body.addChild(block);
      this.markDirty();
      return;
    }
    this.emitPlainTail();
  }

  /**
   * One tick per burst. Whatever arrives inside the coalescing window costs a
   * single frame, which is what keeps a token-storm from becoming a write-storm.
   */
  private markDirty(): void {
    if (this.closed) return;
    if (this.path !== "live" || !this.live) {
      if (this.path === "plain") this.syncFooter();
      return;
    }
    if (this.dirty) return;
    this.dirty = true;
    this.ticks += 1;
    this.cancelTimer = this.schedule(() => {
      this.cancelTimer = null;
      this.dirty = false;
      this.paint();
    }, this.coalesceMs);
  }

  /** Paint now rather than at the end of the tick. Plain mode has nothing pending. */
  flushSync(): void {
    if (this.closed) return;
    this.cancelPending();
    this.dirty = false;
    if (this.live) this.paint();
  }

  private cancelPending(): void {
    if (this.cancelTimer !== null) {
      this.cancelTimer();
      this.cancelTimer = null;
    }
  }

  /**
   * The pulse a pending tool block is shown. Null whenever there is no motion to
   * show: not live, no clock running, the call already settled. Computed at render
   * time rather than pushed, so a frame always reflects the instant it was drawn.
   */
  private pulseFor(callId: string): ToolPulse | null {
    if (!this.animatingNow()) return null;
    const openedAt = this.toolOpenedAt.get(callId);
    if (openedAt === undefined) return null;
    return { tick: this.spinnerTick, elapsedMs: Math.max(0, this.now() - openedAt) };
  }

  /**
   * True while a call the surface is showing has not reported back.
   *
   * This is the state the animation exists for. A line that reads `… bash $ cargo
   * test` and does not move looks exactly like a screen that hung, which is the
   * worst thing a status line can do: it trains the operator to ignore it.
   */
  private animatingNow(): boolean {
    return (
      this.pendingCalls > 0 &&
      this.live &&
      this.path === "live" &&
      this.heartbeatMs > 0 &&
      this.spinnerMs > 0
    );
  }

  /** One timer, two cadences: fast while outstanding, slow otherwise. */
  private beatMs(): number {
    if (this.animatingNow()) return Math.min(this.spinnerMs, this.heartbeatMs);
    return this.heartbeatMs;
  }

  /**
   * Re-arm the beat at the current cadence. Called when a call opens so the
   * spinner starts with it, instead of up to a heartbeat later.
   */
  private refreshCadence(): void {
    if (this.closed || this.heartbeatCancel === null) return;
    this.stopHeartbeat();
    this.armHeartbeat();
  }

  private paint(): void {
    if (this.tui === null || !this.live) return;
    this.syncFooter();
    this.tui.renderNow();
    this.paints += 1;
  }

  private syncFooter(): void {
    const elapsed = this.elapsedMs();
    this.lastElapsed = formatElapsed(elapsed);
    this.footer.setFields(
      { ...this.fields, tokensIn: this.tokensIn, tokensOut: this.tokensOut },
      elapsed,
    );
  }

  /**
   * Arm the next heartbeat. Re-arms itself rather than using setInterval, so the
   * injected scheduler is the only timer source and a test can drive the clock
   * by hand. One timer at a time, always cancelled on release and dispose.
   */
  private armHeartbeat(): void {
    if (this.closed || this.heartbeatCancel !== null || this.heartbeatMs <= 0) return;
    this.heartbeatCancel = this.schedule(() => {
      this.heartbeatCancel = null;
      this.onHeartbeat();
    }, this.beatMs());
  }

  private stopHeartbeat(): void {
    if (this.heartbeatCancel !== null) {
      this.heartbeatCancel();
      this.heartbeatCancel = null;
    }
  }

  /**
   * One beat of the clock. A tick is not a paint: if nothing time-derived on the
   * footer moved since the last sync, no frame is requested. This keeps the cost
   * of an idle-but-attached surface at zero paints, and the ceiling at one per
   * second — the resolution `elapsed` is rendered at.
   */
  private onHeartbeat(): void {
    if (this.closed || !this.live) return;
    if (this.animatingNow()) {
      // The frame changed by definition, so the frame is asked for. No footer
      // comparison: the spinner is the change.
      this.spinnerTick += 1;
      this.markDirty();
    } else if (formatElapsed(this.elapsedMs()) !== this.lastElapsed) {
      this.markDirty();
    }
    this.armHeartbeat();
  }

  private elapsedMs(): number | undefined {
    if (this.workStartedAt === null) return undefined;
    return Math.max(0, this.now() - this.workStartedAt);
  }

  /** Content the live surface owns: everything appended since the last acquire. */
  private visibleBlocks(): RenderBlock[] {
    return this.blocks.slice(this.anchoredAt);
  }

  /** What the live surface shows right now, colours included. */
  captureFrame(): string[] {
    const width = this.width();
    const lines = this.visibleBlocks().flatMap((block) => block.render(width));
    const footer = this.footer.render(width);
    return footer.length > 0 ? [...lines, ...footer] : lines;
  }

  /** The same surface with every escape sequence stripped — rule 9's view. */
  capturePlain(): string[] {
    return this.captureFrame().map((line) =>
      stripTerminalSequences(line).replace(/\s+$/, ""),
    );
  }

  private width(): number {
    const terminal = this.terminal ?? this.terminalFactory();
    const columns = terminal.columns;
    return typeof columns === "number" && columns > 0 ? columns : this.plainWidth;
  }

  /**
   * The escape-free path: render the blocks, strip them, write. The same
   * components as the live path, so nothing is ever described twice.
   *
   * A tool call that has not settled is held at the tail: an append-only stream
   * cannot repaint it, so writing it early would freeze a "…" over a call that was
   * about to report ✓ or ✗. `force` (teardown) writes whatever is left.
   */
  private emitPlainTail(force = false): void {
    let end = this.blocks.length;
    if (!force) {
      while (end > this.printedThrough) {
        const last = this.blocks[end - 1];
        if (last !== undefined && last.kind === "tool" && last.toolStatus === "pending") {
          end -= 1;
        } else {
          break;
        }
      }
    }
    const pending = this.blocks.slice(this.printedThrough, end);
    this.printedThrough = end;
    const lines = pending
      .flatMap((block) =>
        block.render(this.plainWidth).map((line) =>
          stripTerminalSequences(line).replace(/\s+$/, ""),
        ),
      )
      .filter((line) => line !== "");
    if (lines.length === 0) return;
    this.write(`${lines.join("\n")}\n`);
    this.plainWrites += 1;
  }

  /**
   * The plain path has no live footer, so it prints one at each context change and
   * a final one at teardown. Keyed on the *identity* of the context — not on
   * elapsed or tokens, which move with every event and would bury the log.
   */
  private emitPlainFooter(final: boolean): void {
    const key = [
      this.fields.issueId ?? MISSING,
      this.fields.phase ?? MISSING,
      this.fields.model ?? MISSING,
      this.fields.thinkingLevel ?? MISSING,
      final ? "final" : "live",
    ].join("|");
    if (key === this.lastPlainFooter) return;
    this.lastPlainFooter = key;
    const line = this.footer.plainLine().replace(/\s+$/, "");
    if (line === "") return;
    this.write(`${line}\n`);
    this.plainWrites += 1;
  }

  stats(): PresenterStats {
    return {
      paints: this.paints,
      plainWrites: this.plainWrites,
      blocks: this.blocks.length,
      coalescedTicks: this.ticks,
      coalesceMs: this.coalesceMs,
      heartbeatMs: this.heartbeatMs,
      spinnerMs: this.spinnerMs,
      animating: this.animatingNow(),
      paintPending: this.dirty,
      live: this.live,
      expanded: this.expandedAll,
    };
  }

  dispose(): void {
    if (this.closed) return;
    this.stopHeartbeat();
    this.release();
    this.closed = true;
    this.cancelPending();
    this.stopHeartbeat();
    this.body.clear();
    this.surface.clear();
    this.tools.clear();
    this.activeAssistant = null;
    this.blocks = [];
    this.anchoredAt = 0;
    this.printedThrough = 0;
  }
}

/** The keys a binding resolves to, or empty when the manager cannot answer. */
function keysFor(manager: KeybindingsManager, id: string): readonly string[] {
  try {
    return [...manager.getKeys(id as never)];
  } catch {
    return [];
  }
}

/**
 * A presenter that renders nothing and owns no surface.
 *
 * For callers that must not touch the terminal — a log-only spike whose
 * transcript would be painted over, a test that only wants to count calls. It
 * satisfies {@link WorkPresenter} so the wiring at the composition root is the
 * same either way; there is no `if (presenter)` branch to get wrong.
 */
export function createNullPresenter(): WorkPresenter {
  return {
    isLive: false,
    path: "plain",
    feed(): void {},
    notice(): void {},
    say(): void {},
    warn(): void {},
    setContext(): void {},
    setExpanded(): void {},
    toggleExpanded(): void {},
    acquire(): void {},
    release(): void {},
    flushSync(): void {},
    captureFrame: (): string[] => [],
    capturePlain: (): string[] => [],
    stats: (): PresenterStats => ({
      paints: 0,
      plainWrites: 0,
      blocks: 0,
      coalescedTicks: 0,
      coalesceMs: 0,
      heartbeatMs: 0,
      spinnerMs: 0,
      animating: false,
      paintPending: false,
      live: false,
      expanded: false,
    }),
    dispose(): void {},
  };
}

/**
 * Build the presenter the app wires: `feed` gets the runner's `onEvent`, `say` /
 * `warn` get the loop's `ui`, and `acquire` / `release` bracket the idle surface.
 */
export function createWorkPresenter(options: WorkPresenterOptions = {}): WorkPresenter {
  return new Presenter(options);
}
