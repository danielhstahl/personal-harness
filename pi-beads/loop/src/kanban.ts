/**
 * The mini kanban — a read-only, three-column view of the beads board.
 *
 * The monitor beside it answers *what is the server doing*. This answers the
 * other question a person has when they look at this TUI: **what is left, what
 * is being worked, what got finished, and what the loop will pick next.** All
 * three are already in beads — the loop reads two of them on every decision —
 * but nothing on the screen shows them as a picture, so the shape of the queue
 * lives only in the operator's head.
 *
 * The same three rules the monitor runs under apply here, for the same reasons:
 *
 * - **Read-only, by shape.** This module takes no `BdClient` and has no write
 *   path at all. The composition root hands it a `read()` closure that runs
 *   the three list queries and returns plain data, so there is no
 *   `setStatus`, no `closeIssue`, not even a handle that could be persuaded
 *   into one. A display that can move a ticket is not a display, it is a
 *   second driver with different incentives.
 * - **Never load-bearing.** A `bd` that fails is a column reading `?` and a
 *   line in `describe()`, not a run that stops. Reads never overlap, and
 *   consecutive failures back off exponentially: every poll here is a child
 *   process, not a socket read, so a missing binary must cost one notice
 *   rather than ten thousand `ENOENT`s.
 * - **Absent is not zero.** A column that was not read, or failed to read,
 *   renders `?`. Only a column that answered with an empty list renders `0`
 *   and a dash. A board that paints an unread queue as empty is worse than no
 *   board at all — it is a confident statement that there is no work.
 *
 * Rendering is pure and does not know what a terminal is: `kanbanView()` builds
 * the view model, `renderKanban()` lays it out at a given width in one of two
 * modes — a dense one-line `row`, or a bordered `board` grid — and falls back
 * to the row when three columns would crush the titles into unreadability.
 * Colour comes through {@link MonitorTheme}, the same role-based theme the
 * monitor uses, so the board is themed by pi's own theme object like
 * everything else in the chrome.
 */
import { truncateToWidth, visibleWidth, type Component } from "@earendil-works/pi-tui";

import { normaliseDependencies, type Issue } from "./beads.ts";
import { PLAIN_MONITOR_THEME, type MonitorTheme } from "./monitor.ts";

// ── the view model ─────────────────────────────────────────────────────────

/** The three columns on the board, in the order they are drawn. */
export const KANBAN_COLUMNS = ["ready", "progress", "done"] as const;

export type KanbanColumnKey = (typeof KANBAN_COLUMNS)[number];

/**
 * How a column got its contents.
 *
 * `"live"` answered with a list; `"absent"` was not part of the read at all;
 * `"unknown"` was asked and failed. The last two render differently from the
 * first: an empty list is information, no answer is not.
 */
export type KanbanColumnState = "live" | "absent" | "unknown";

export interface KanbanCard {
  readonly id: string;
  readonly title: string;
  readonly column: KanbanColumnKey;
  readonly priority?: number;
  readonly issueType?: string;
  /** Whoever holds it — `owner`, falling back to `assignee`. Display only. */
  readonly holder?: string;
  /** How long since the issue was last touched, on the view's clock. */
  readonly ageMs?: number;
  /** Active blockers, when bd reported enough to count them. */
  readonly blockers?: number;
  /** This is the ticket the run in front of us is working. */
  readonly current: boolean;
}

export interface KanbanColumn {
  readonly key: KanbanColumnKey;
  readonly label: string;
  /**
   * The cards to display — every one for the two live columns, the most recent
   * `doneLimit` for the closed list.
   */
  readonly cards: readonly KanbanCard[];
  /**
   * The authoritative count, which for the closed column can exceed the cards
   * kept. The header shows this so `done 340` never implies twelve.
   */
  readonly total: number;
  readonly state: KanbanColumnState;
  /** Why the column is unknown, when it is. */
  readonly error?: string;
  /**
   * True when the read was capped: we asked for at most `window` closed tickets
   * and got `window` back, so the board may hold far more and we cannot tell.
   * The header renders `done 36+` for that. A bare `done 36` over a board with
   * three hundred closed tickets is a wrong number wearing a confident font,
   * and this flag is the difference between "that is the count" and "that is
   * how far I looked".
   */
  readonly floored?: boolean;
}

export interface KanbanView {
  readonly at: number;
  readonly ageMs: number;
  readonly columns: readonly KanbanColumn[];
  /** False when no column answered — a board with nothing behind it. */
  readonly anyLive: boolean;
  readonly currentId?: string;
}

/**
 * What one read produced.
 *
 * A list that is `undefined` was not read; a list that is `[]` was read and is
 * empty. Collapsing those two is how a dashboard ends up telling you the queue
 * is empty while `bd` is failing, so the distinction is carried all the way to
 * the pixels.
 */
export interface KanbanRead {
  readonly ready?: readonly Issue[];
  readonly inProgress?: readonly Issue[];
  readonly closed?: readonly Issue[];
  /** Columns whose read failed. They render `?`, never `0`. */
  readonly failed?: readonly KanbanColumnKey[];
  /** The failure text, when there is one. Shown by `describe()`, not the board. */
  readonly error?: string;
  /**
   * The limit `closed` was read with, so a full window can be reported as a
   * floor rather than a count. Omit it and the closed count is taken at face
   * value — which is only honest when the read was unbounded.
   */
  readonly closedWindow?: number;
}

export interface KanbanViewOptions {
  readonly now: number;
  readonly read: KanbanRead;
  /** The ticket this run is working, highlighted wherever it appears. */
  readonly currentId?: string;
  /** How many closed tickets to keep in view. Default 12. */
  readonly doneLimit?: number;
}

const COLUMN_LABELS: Readonly<Record<KanbanColumnKey, string>> = {
  ready: "ready",
  progress: "in progress",
  done: "done",
};

/** An ISO timestamp bd might have put on a field. */
function timestamp(value: unknown): number | undefined {
  if (typeof value !== "string" || value === "") return undefined;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : undefined;
}

/**
 * The most relevant recent timestamp on an issue.
 *
 * `closed_at` is read off the raw payload rather than through the declared
 * fields: bd reports it on closed work and {@link Issue} does not carry it,
 * and a `done` column that sorted by `updated_at` would be right often enough
 * to be trusted and wrong often enough to matter.
 */
function recencyOf(issue: Issue): number | undefined {
  const closed = timestamp((issue as { closed_at?: unknown }).closed_at);
  return (
    closed ??
    timestamp(issue.updated_at) ??
    timestamp(issue.started_at) ??
    timestamp(issue.created_at)
  );
}

function finite(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/**
 * How many things are actively blocking this issue.
 *
 * Counted through {@link normaliseDependencies}: bd reports dependencies in
 * two different shapes (`bd list` gives an edge, `bd show` gives the other
 * issue), and reading only one of them yields a fail-open zero — the board
 * would say "no blockers" about a ticket that is blocked.
 */
function blockerCount(issue: Issue): number | undefined {
  if (issue.dependencies === undefined) return finite(issue.dependency_count);
  return normaliseDependencies(issue).length;
}

function toCard(
  issue: Issue,
  column: KanbanColumnKey,
  now: number,
  currentId: string | undefined,
): KanbanCard {
  const at = recencyOf(issue);
  const holder = issue.owner ?? issue.assignee;
  const priority = finite(issue.priority);
  const blockers = blockerCount(issue);
  return {
    id: issue.id,
    title: issue.title ?? "",
    column,
    ...(priority === undefined ? {} : { priority }),
    ...(typeof issue.issue_type === "string" && issue.issue_type !== ""
      ? { issueType: issue.issue_type }
      : {}),
    ...(holder === undefined || holder === "" ? {} : { holder }),
    ...(at === undefined ? {} : { ageMs: Math.max(0, now - at) }),
    ...(blockers === undefined ? {} : { blockers }),
    current: currentId !== undefined && issue.id === currentId,
  };
}

/** Ready first, the way the picker sees it: by priority, then oldest. */
function compareReady(a: KanbanCard, b: KanbanCard): number {
  const pa = a.priority ?? 2;
  const pb = b.priority ?? 2;
  if (pa !== pb) return pa - pb;
  const aa = a.ageMs ?? Number.MAX_SAFE_INTEGER;
  const ab = b.ageMs ?? Number.MAX_SAFE_INTEGER;
  if (aa !== ab) return ab - aa; // the one that has waited longest leads
  return a.id.localeCompare(b.id);
}

/** Our own ticket leads the in-progress column; the rest keep a stable order. */
function compareProgress(a: KanbanCard, b: KanbanCard): number {
  if (a.current !== b.current) return a.current ? -1 : 1;
  return a.id.localeCompare(b.id);
}

/** Most recently closed first: the bottom of the board is the most recent past. */
function compareDone(a: KanbanCard, b: KanbanCard): number {
  const aa = a.ageMs ?? Number.MAX_SAFE_INTEGER;
  const ab = b.ageMs ?? Number.MAX_SAFE_INTEGER;
  if (aa !== ab) return aa - ab;
  return a.id.localeCompare(b.id);
}

/**
 * Build the board from a read.
 *
 * Pure: the caller's clock comes in as `now`, there is no I/O, and there is no
 * bd handle to misuse. Deduplication is by id with the live columns winning,
 * because a ticket showing up in two columns is a race in the read and the
 * more-current state is the one worth painting.
 */
export function kanbanView(options: KanbanViewOptions): KanbanView {
  const now = Math.max(0, Math.trunc(options.now));
  const { read } = options;
  const currentId = options.currentId;
  const doneLimit = Math.max(0, Math.trunc(options.doneLimit ?? 12));
  const failed = new Set(read.failed ?? []);

  const seen = new Set<string>();
  const collect = (
    issues: readonly Issue[] | undefined,
    key: KanbanColumnKey,
    order: (a: KanbanCard, b: KanbanCard) => number,
  ): { cards: KanbanCard[]; total: number } => {
    if (issues === undefined) return { cards: [], total: 0 };
    const cards: KanbanCard[] = [];
    for (const issue of issues) {
      if (typeof issue?.id !== "string" || issue.id === "") continue;
      if (seen.has(issue.id)) continue;
      seen.add(issue.id);
      cards.push(toCard(issue, key, now, currentId));
    }
    cards.sort(order);
    return { cards, total: cards.length };
  };

  const progress = collect(read.inProgress, "progress", compareProgress);
  const ready = collect(read.ready, "ready", compareReady);
  // The closed list is the only one that can be long, so it is the only one
  // that is capped — and it is capped *after* sorting, so what is kept is the
  // recent past rather than whatever bd happened to return first.
  const closed = collect(read.closed, "done", compareDone);

  const stateOf = (key: KanbanColumnKey, list: readonly Issue[] | undefined): KanbanColumnState => {
    if (failed.has(key)) return "unknown";
    return list === undefined ? "absent" : "live";
  };
  const build = (
    key: KanbanColumnKey,
    gathered: { cards: KanbanCard[]; total: number },
    list: readonly Issue[] | undefined,
    cap?: number,
    floored = false,
  ): KanbanColumn => {
    const error = failed.has(key) ? (read.error ?? `${key} read failed`) : undefined;
    return {
      key,
      label: COLUMN_LABELS[key],
      cards: cap === undefined ? gathered.cards : gathered.cards.slice(0, cap),
      total: gathered.total,
      state: stateOf(key, list),
      ...(error === undefined ? {} : { error }),
      ...(floored ? { floored: true } : {}),
    };
  };

  // A closed read that came back exactly as long as the window asked for is a
  // truncated read: there may be thousands more behind it. `>=` rather than
  // `==` because a reader may hand back more than it asked for.
  const closedFloored =
    read.closed !== undefined &&
    read.closedWindow !== undefined &&
    read.closedWindow > 0 &&
    read.closed.length >= read.closedWindow;

  const columns: KanbanColumn[] = [
    build("ready", ready, read.ready),
    build("progress", progress, read.inProgress),
    build("done", closed, read.closed, doneLimit, closedFloored),
  ];

  return {
    at: now,
    ageMs: 0,
    columns,
    anyLive: columns.some((column) => column.state === "live"),
    ...(currentId === undefined ? {} : { currentId }),
  };
}

/**
 * Re-point the "this run's ticket" highlight at `currentId`.
 *
 * The `current` flags baked into a view come from whatever the read believed was
 * ours at read time, which is one interval stale by construction: the presenter
 * learns the issue id the moment work starts, and the board on screen was read
 * before that. So the highlight is applied over the snapshot at draw time.
 * Doing it any earlier — and only there — is how `setCurrent()` becomes a setter
 * whose effect nobody can see.
 *
 * Re-ordering is part of the job: the in-progress column leads with our ticket,
 * so a card becoming ours moves it where a card that is ours belongs.
 */
export function withCurrentId(
  view: KanbanView,
  currentId: string | undefined,
): KanbanView {
  if (view.currentId === currentId) return view;
  const columns = view.columns.map((column) => {
    const cards = column.cards.map((card) =>
      card.current === (card.id === currentId) ? card : { ...card, current: card.id === currentId },
    );
    return {
      ...column,
      cards: column.key === "progress" ? [...cards].sort(compareProgress) : cards,
    };
  });
  return {
    ...view,
    columns,
    ...(currentId === undefined ? {} : { currentId }),
  };
}

/** A view in which nothing was ever read and every column refused to answer. */
export function unreadKanbanView(now: number, error?: string): KanbanView {
  const columns = KANBAN_COLUMNS.map((key) => ({
    key,
    label: COLUMN_LABELS[key],
    cards: [],
    total: 0,
    state: "unknown" as KanbanColumnState,
    ...(error === undefined ? {} : { error }),
  }));
  return { at: now, ageMs: 0, columns, anyLive: false };
}

// ── rendering ──────────────────────────────────────────────────────────────

/** `off` hides it; `row` is one dense line; `board` is the bordered grid. */
export type KanbanMode = "off" | "row" | "board";

export interface KanbanRenderOptions {
  /**
   * The full line width available, the indent gutter included. Every renderer
   * returns lines no wider than this; passing the width with the gutter already
   * subtracted makes the board two columns narrower than it needs to be.
   */
  readonly width: number;
  /** Total rows the board may occupy, borders included. Default 4. */
  readonly maxLines?: number;
  readonly indent?: string;
  /** Default `"board"`, falling back to `"row"` when the columns will not fit. */
  readonly mode?: KanbanMode;
  /** Colour. `null` means none, which is what the plain path wants. */
  readonly theme?: MonitorTheme | null;
  /** Highlight this ticket, overriding whatever the read thought was ours. */
  readonly currentId?: string;
}

/** Below this, a column holds an id and nothing else, which is not a board. */
const BOARD_MIN_COLUMN = 16;

const BORDER = {
  topLeft: "╭",
  topRight: "╮",
  bottomLeft: "╰",
  bottomRight: "╯",
  horizontal: "─",
  vertical: "│",
  teeDown: "┬",
  teeUp: "┴",
} as const;

/** `45s`, `7m`, `3h`, `2d` — the compact forms a glance can parse. */
export function compactAge(ms: number | undefined): string | undefined {
  if (ms === undefined || !Number.isFinite(ms)) return undefined;
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

function paint(
  theme: MonitorTheme | null,
  role: Parameters<MonitorTheme["color"]>[0],
  text: string,
): string {
  return theme === null ? text : theme.color(role, text);
}

/** Pad to a *visible* width: escape sequences cost no columns. */
function padTo(text: string, width: number): string {
  const room = width - visibleWidth(text);
  return room > 0 ? `${text}${" ".repeat(room)}` : text;
}

/**
 * Column widths for a bordered grid, or `undefined` when three columns will
 * not fit. The slack left over by the division goes to the last column so the
 * frame lands exactly on the terminal edge rather than wrapping one char short.
 */
export function boardColumnWidths(
  width: number,
  indentWidth: number,
  columns: number,
  minColumn = BOARD_MIN_COLUMN,
): number[] | undefined {
  if (columns <= 0) return undefined;
  const frame = columns + 1; // one vertical rule per column boundary, plus the edges
  const slack = Math.trunc(width) - indentWidth - frame;
  if (slack < minColumn * columns) return undefined;
  const base = Math.floor(slack / columns);
  const remainder = slack - base * columns;
  // The undividables go to the right-hand columns: the leftmost column carries
  // the next pick, and one character there is worth more than one on `done`.
  return Array.from({ length: columns }, (_unused, index) =>
    index >= columns - remainder ? base + 1 : base,
  );
}

/** The count, or a marker when the column was not read. Never a silent zero. */
function columnCount(column: KanbanColumn, theme: MonitorTheme | null): string {
  if (column.state === "unknown") return paint(theme, "warning", "?");
  if (column.state === "absent") return paint(theme, "dim", "—");
  // A count from a truncated read is a floor, and says so. `+` is the whole
  // difference between "twelve closed tickets" and "at least twelve".
  return column.floored === true ? `${column.total}+` : String(column.total);
}

/** The glyph that says what a card is, before its id. */
function markerFor(card: KanbanCard): { glyph: string; role: "accent" | "success" | "dim" } {
  if (card.current) return { glyph: "▸", role: "accent" };
  if (card.column === "progress") return { glyph: "●", role: "accent" };
  if (card.column === "done") return { glyph: "✓", role: "success" };
  return { glyph: "·", role: "dim" };
}

/**
 * One card inside a column of `columnWidth`.
 *
 * The id is never elided — an id that has been truncated cannot be typed into
 * `bd show`, which makes it worse than useless — so the title is what gives
 * way, and only what is left of the width after the id and the footnotes.
 *
 * `overflow` is the count of this column's cards that never got a row. It rides
 * on the last card line rather than getting a line of its own because a row set
 * aside for it is a row taken away from a card, and on a three-row board that is
 * a third of the column.
 */
export function renderCardLine(
  card: KanbanCard,
  theme: MonitorTheme | null,
  columnWidth: number,
  overflow?: number,
): string {
  const marker = markerFor(card);
  const idRole = card.current ? "accent" : card.column === "done" ? "dim" : "text";
  const titleRole = card.column === "done" ? "dim" : "text";
  const prefix = `${paint(theme, marker.role, marker.glyph)} `;
  const idText = paint(theme, idRole, card.id);

  const footnotes: string[] = [];
  if (card.column !== "ready") {
    const age = compactAge(card.ageMs);
    if (age !== undefined) footnotes.push(paint(theme, "dim", `[${age}]`));
  }
  if (card.column === "ready" && (card.blockers ?? 0) > 0) {
    footnotes.push(paint(theme, "warning", `⚠${card.blockers}`));
  }
  if (overflow !== undefined && overflow > 0) {
    footnotes.push(paint(theme, "dim", `+${overflow}`));
  }
  const suffix = footnotes.length > 0 ? ` ${footnotes.join(" ")}` : "";

  const overhead = visibleWidth(prefix) + visibleWidth(card.id) + visibleWidth(suffix) + 1;
  const titleRoom = Math.max(0, Math.trunc(columnWidth) - overhead);
  const title = truncateToWidth(card.title, titleRoom, "…");
  const line =
    title === ""
      ? `${prefix}${idText}${suffix}`
      : `${prefix}${idText} ${paint(theme, titleRole, title)}${suffix}`;
  return truncateToWidth(line, Math.max(1, Math.trunc(columnWidth)), "…");
}

/** Top border with each column's label punched into the rule. */
function topBorder(
  widths: readonly number[],
  labels: readonly string[],
  theme: MonitorTheme | null,
): string {
  const cells = widths.map((width, index) => {
    const label = ` ${labels[index] ?? ""} `;
    const fill = Math.max(1, width - visibleWidth(label));
    return `${paint(theme, "accent", label)}${paint(theme, "borderMuted", BORDER.horizontal.repeat(fill))}`;
  });
  return `${BORDER.topLeft}${cells.join(BORDER.teeDown)}${BORDER.topRight}`;
}

function bottomBorder(widths: readonly number[], theme: MonitorTheme | null): string {
  const cells = widths.map((width) =>
    paint(theme, "borderMuted", BORDER.horizontal.repeat(Math.max(1, width))),
  );
  return `${BORDER.bottomLeft}${cells.join(BORDER.teeUp)}${BORDER.bottomRight}`;
}

function rule(theme: MonitorTheme | null): string {
  return paint(theme, "borderMuted", BORDER.vertical);
}

/** What goes in an empty cell: a more-marker, an unread marker, or a dash. */
function emptyCell(
  column: KanbanColumn,
  hidden: number,
  theme: MonitorTheme | null,
): string {
  if (hidden > 0) return paint(theme, "dim", `+${hidden} more`);
  if (column.state === "unknown") return paint(theme, "warning", "unread");
  return paint(theme, "dim", "—");
}

/**
 * The grid: `cardRows` cards per column between two borders.
 *
 * Overflow is shown where it happens, per column: a column with more cards than
 * rows carries `+N` on its last visible card, and a column that ran out of
 * cards early marks its first empty cell (`unread`, `+N more`, or a dash) and
 * leaves the rest of the column empty rather than stamping the same marker down
 * every remaining row.
 */
export function renderKanbanBoard(
  view: KanbanView,
  theme: MonitorTheme | null,
  options: { readonly width: number; readonly cardRows: number; readonly indent?: string },
): string[] {
  const indent = options.indent ?? "";
  const widths = boardColumnWidths(options.width, visibleWidth(indent), view.columns.length);
  if (widths === undefined) return [];

  const labels = view.columns.map((column) => `${column.label} ${columnCount(column, theme)}`);
  const lines: string[] = [`${indent}${topBorder(widths, labels, theme)}`];

  const rows = Math.max(1, Math.trunc(options.cardRows));
  const vertical = rule(theme);
  // Counted against the column's *total*, not its carried cards: the done
  // column holds `doneLimit` cards over a board that may have hundreds, and
  // "hidden" has to mean "there is more you are not seeing", which it does not
  // if it is measured only over what already made it into the view.
  const hiddenOf = (column: KanbanColumn): number =>
    Math.max(0, column.total - Math.min(column.cards.length, rows));
  for (let row = 0; row < rows; row += 1) {
    const cells = view.columns.map((column, index) => {
      const width = widths[index] ?? BOARD_MIN_COLUMN;
      const card = column.cards[row];
      const hidden = hiddenOf(column);
      const lastRow = row === rows - 1;
      // The first row past the last card carries the column's marker; the rows
      // under it stay empty. Stamping `—` on every spare row made a column with
      // one card look like a column whose contents were all dashes.
      const firstEmpty = Math.min(column.cards.length, rows);
      const text =
        card === undefined
          ? row === firstEmpty
            ? emptyCell(column, hidden, theme)
            : ""
          : renderCardLine(card, theme, width, lastRow ? hidden : undefined);
      return padTo(text, width);
    });
    lines.push(`${indent}${vertical}${cells.join(vertical)}${vertical}`);
  }

  lines.push(`${indent}${bottomBorder(widths, theme)}`);
  return lines;
}

/**
 * The one-line form: the three counts, plus the next ticket the picker will
 * take when the current one finishes.
 *
 * The `next` segment is added only if it fits whole. Truncating a card title on
 * this line would leave `next ws-4k the thing th…`, which is neither readable
 * nor wide enough to be worth the columns.
 */
export function renderKanbanRow(
  view: KanbanView,
  theme: MonitorTheme | null,
  options: { readonly width: number; readonly indent?: string },
): string[] {
  const indent = options.indent ?? "";
  const safeWidth = Math.max(1, Math.trunc(options.width));
  const sep = paint(theme, "dim", " · ");
  const counts =
    `${indent}${paint(theme, "dim", "▦ ")}${view.columns
      .map((column) => `${paint(theme, "dim", column.label)} ${columnCount(column, theme)}`)
      .join(sep)}`;

  const stale =
    view.ageMs > 15_000
      ? `${sep}${paint(
          theme,
          view.ageMs > 60_000 ? "warning" : "dim",
          `${compactAge(view.ageMs) ?? ""} old`,
        )}`
      : "";
  const next = view.columns.find((column) => column.key === "ready")?.cards[0];
  if (next !== undefined) {
    const withNext = `${counts}${sep}${paint(theme, "dim", "next")} ${paint(
      theme,
      "accent",
      `${next.id} ${next.title}`,
    )}${stale}`;
    // Rather than truncate the interesting part, drop it: the counts are the
    // load-bearing half of this line.
    if (visibleWidth(withNext) <= safeWidth) return [withNext];
  }
  return [truncateToWidth(`${counts}${stale}`, safeWidth, "…")];
}

/**
 * The board, in whichever form fits.
 *
 * A mode is a preference, not a promise: `board` at 40 columns is three 12-char
 * columns, which is worse than one honest row, so the layout degrades rather
 * than crushing titles into unreadability. `off` renders nothing at all.
 */
export function renderKanban(
  view: KanbanView,
  theme: MonitorTheme | null,
  options: KanbanRenderOptions,
): string[] {
  const mode = options.mode ?? "board";
  if (mode === "off") return [];
  const indent = options.indent ?? "";
  // A draw-time highlight overrides; absence of one leaves the view's own.
  // Applying `withCurrentId` unconditionally would *clear* a highlight the view
  // was built with, and the read-time marker would be invisible everywhere.
  const board = options.currentId === undefined ? view : withCurrentId(view, options.currentId);
  if (mode === "row") {
    return renderKanbanRow(board, theme, { width: options.width, indent });
  }

  const maxLines = Math.max(1, Math.trunc(options.maxLines ?? 4));
  const cardRows = maxLines - 2;
  if (cardRows >= 1) {
    const grid = renderKanbanBoard(board, theme, { width: options.width, cardRows, indent });
    if (grid.length > 0) return grid.slice(0, maxLines);
  }
  // Too narrow for a grid: the row says the same thing in one line.
  return renderKanbanRow(board, theme, { width: options.width, indent });
}

// ── the source that keeps it current ───────────────────────────────────────

export interface KanbanLinesOptions {
  readonly maxLines?: number;
  readonly indent?: string;
  readonly mode?: KanbanMode;
  /** Highlight this ticket — usually the one the surface holding the board is working. */
  readonly currentId?: string;
}

export interface KanbanSource {
  /** The board, rendered. Empty when off, or before the first read lands. */
  lines(width: number, options?: KanbanLinesOptions): string[];
  /** Register a repaint hook. The returned function unregisters it. */
  subscribe(listener: () => void): () => void;
  readonly view: KanbanView;
  /** Read now. For the moment after this run itself changed the board. */
  refresh(): Promise<void>;
  start(): void;
  stop(): void;
  readonly running: boolean;
  describe(): string[];
}

export interface KanbanSourceOptions {
  /** The only beads access this module has, and it is a read. */
  readonly read: () => Promise<KanbanRead>;
  readonly intervalMs?: number;
  readonly now?: () => number;
  readonly schedule?: (run: () => void, ms: number) => () => void;
  /** `null` renders without colour — what the plain path wants. */
  readonly theme?: MonitorTheme | null;
  readonly maxLines?: number;
  readonly mode?: KanbanMode;
  readonly doneLimit?: number;
  /** Which ticket this run holds. An accessor, because it changes mid-run. */
  readonly currentId?: () => string | undefined;
  readonly verbose?: boolean;
  /** Ceiling on retry backoff after consecutive failures. Default 60s. */
  readonly maxBackoffMs?: number;
  /** Notice sink for what the board itself cannot show. */
  readonly onEvent?: (line: string) => void;
}

const DEFAULT_KANBAN_INTERVAL_MS = 5_000;
const DEFAULT_MAX_BACKOFF_MS = 60_000;

function defaultSchedule(run: () => void, ms: number): () => void {
  const timer = setTimeout(run, ms);
  timer.unref?.();
  return (): void => {
    clearTimeout(timer);
  };
}

const EMPTY_VIEW: KanbanView = {
  at: 0,
  ageMs: 0,
  columns: KANBAN_COLUMNS.map((key) => ({
    key,
    label: COLUMN_LABELS[key],
    cards: [],
    total: 0,
    state: "absent" as KanbanColumnState,
  })),
  anyLive: false,
};

/**
 * The poller.
 *
 * Same discipline as the monitor's — one cycle at a time, joined rather than
 * queued, listeners notified once per read, timers `unref`'d so the process
 * can exit — plus **backoff**, because a poll here is a child process. Failing
 * reads double the wait up to a ceiling instead of spawning a `bd` every second
 * forever, and one good read resets it.
 */
export function createKanbanSource(options: KanbanSourceOptions): KanbanSource {
  const now = options.now ?? Date.now;
  const schedule = options.schedule ?? defaultSchedule;
  const theme = options.theme === undefined ? PLAIN_MONITOR_THEME : options.theme;
  const intervalMs = Math.max(500, options.intervalMs ?? DEFAULT_KANBAN_INTERVAL_MS);
  const maxBackoffMs = Math.max(intervalMs, options.maxBackoffMs ?? DEFAULT_MAX_BACKOFF_MS);
  const mode = options.mode ?? "board";
  const maxLines = Math.max(1, options.maxLines ?? 4);
  const doneLimit = Math.max(0, options.doneLimit ?? 12);
  const verbose = options.verbose === true;
  const emit = options.onEvent ?? ((_line: string): void => undefined);

  const listeners = new Set<() => void>();
  let view: KanbanView = EMPTY_VIEW;
  let failures = 0;
  let lastError: string | undefined;
  let lastReadAt = 0;
  let inFlight: Promise<void> | undefined;
  let cancelTimer: (() => void) | null = null;
  let running = false;

  function delay(): number {
    if (failures === 0) return intervalMs;
    return Math.min(maxBackoffMs, intervalMs * 2 ** Math.min(failures, 8));
  }

  function arm(after: number): void {
    if (cancelTimer !== null) cancelTimer();
    cancelTimer = schedule(() => {
      cancelTimer = null;
      // Re-arm from the *cycle's* completion, not from the timer's. The read is
      // async: arming before it returns would measure the interval against the
      // wait for `bd` rather than after it, and — if the cycle threw — arm
      // nothing at all, leaving a running source that never polls again.
      void cycle().finally(armNext);
    }, after);
  }

  /** What every completed cycle does when we are still running: queue the next. */
  function armNext(): void {
    if (running) arm(delay());
  }

  function notify(): void {
    for (const listener of [...listeners]) {
      try {
        listener();
      } catch {
        // A surface that throws on repaint does not get to stop the others,
        // nor the next read.
      }
    }
  }

  async function runCycle(): Promise<void> {
    const at = now();
    const current = options.currentId?.();
    try {
      const read = await options.read();
      view = kanbanView({
        now: at,
        read,
        doneLimit,
        ...(current === undefined ? {} : { currentId: current }),
      });
      failures = 0;
      lastError = undefined;
    } catch (error: unknown) {
      failures += 1;
      lastError = error instanceof Error ? error.message : String(error);
      // The previous board stays up, ageing. Blanking the screen because a
      // subprocess hiccuped would throw away the one thing the operator
      // still knew was true.
      emit(`kanban: read failed (${failures}x): ${lastError}`);
    }
    lastReadAt = at;
    notify();
  }

  /** One cycle, joined rather than doubled. */
  function cycle(): Promise<void> {
    if (inFlight !== undefined) return inFlight;
    const runningCycle = runCycle().finally(() => {
      if (inFlight === runningCycle) inFlight = undefined;
    });
    inFlight = runningCycle;
    return runningCycle;
  }

  function currentView(): KanbanView {
    // Age is derived when someone asks, so an unpainted board still ages and
    // nothing here needs a timer to keep "3s ago" honest.
    if (view.at === 0) return view;
    return { ...view, ageMs: Math.max(0, now() - view.at) };
  }

  return {
    start(): void {
      if (running) return;
      running = true;
      void cycle().finally(armNext);
    },
    stop(): void {
      running = false;
      if (cancelTimer !== null) {
        cancelTimer();
        cancelTimer = null;
      }
    },
    async refresh(): Promise<void> {
      await cycle();
      armNext();
    },
    get running(): boolean {
      return running;
    },
    get view(): KanbanView {
      return currentView();
    },
    lines(width: number, lineOptions: KanbanLinesOptions = {}): string[] {
      const current = lineOptions.currentId ?? options.currentId?.();
      const snapshot = currentView();
      const indent = lineOptions.indent ?? "";
      // Nothing has ever been read: draw nothing rather than a frame full of
      // dashes, which would look like an empty board.
      if (snapshot.at === 0) {
        // ...but a board that has *failed* must say so. Silence here is
        // indistinguishable from `off`, and an operator who cannot tell the two
        // will spend the run assuming the queue was empty.
        if (failures === 0) return [];
        return renderKanban(unreadKanbanView(now(), lastError), theme, {
          width,
          maxLines: 1,
          mode: "row",
          indent,
          ...(current === undefined ? {} : { currentId: current }),
        });
      }
      return renderKanban(snapshot, theme, {
        width,
        maxLines: Math.max(1, lineOptions.maxLines ?? maxLines),
        mode: lineOptions.mode ?? mode,
        ...(indent === "" ? {} : { indent }),
        ...(current === undefined ? {} : { currentId: current }),
      });
    },
    subscribe(listener: () => void): () => void {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    describe(): string[] {
      const age = lastReadAt === 0 ? "never read" : `${compactAge(Math.max(0, now() - lastReadAt)) ?? "0s"} ago`;
      const head = `  kanban  ${age}`;
      const lines = [
        head + (failures > 0 ? `  failing: ${lastError ?? "unknown"} (${failures}x)` : "  ok"),
      ];
      if (!verbose) return lines;
      for (const column of view.columns) {
        lines.push(
          `    ${column.key.padEnd(9)} ${column.state.padEnd(7)} ${column.total} total,` +
            ` ${column.cards.length} shown` +
            (column.error === undefined ? "" : `  ${column.error}`),
        );
      }
      lines.push(`    interval ${intervalMs}ms, backoff ceiling ${maxBackoffMs}ms`);
      return lines;
    },
  };
}

/**
 * The board for a run that must not show one — the same trick as
 * `createNullMonitor()`, so no surface ever has to reason about whether a
 * board exists.
 */
export function createNullKanban(reason?: string): KanbanSource {
  return {
    start(): void {},
    stop(): void {},
    async refresh(): Promise<void> {},
    get running(): boolean {
      return false;
    },
    get view(): KanbanView {
      return EMPTY_VIEW;
    },
    lines(): string[] {
      return [];
    },
    subscribe(): () => void {
      return () => undefined;
    },
    describe(): string[] {
      return [`  kanban: off${reason === undefined ? "" : ` — ${reason}`}`];
    },
  };
}

/**
 * The board as a pi-tui component. Like the monitor's, it caches nothing: the
 * lines are produced at render time from a snapshot already in memory, so a
 * slow `bd` can never hold up a frame.
 */
export class KanbanComponent implements Component {
  readonly kind = "kanban" as const;
  private readonly source: KanbanSource;
  private readonly maxLines: number;
  private readonly indent: string;
  private mode: KanbanMode | undefined;
  private currentId: string | undefined;

  constructor(source: KanbanSource, maxLines = 4, indent = " ") {
    this.source = source;
    this.maxLines = maxLines;
    this.indent = indent;
  }

  /** Which ticket the surface holding this board is working. */
  setCurrent(id: string | undefined): void {
    this.currentId = id;
  }

  /** Override the source's mode for this surface. */
  setMode(mode: KanbanMode | undefined): void {
    this.mode = mode;
  }

  render(width: number): string[] {
    const available = Math.max(8, width - visibleWidth(this.indent));
    return this.source.lines(available, {
      maxLines: this.maxLines,
      indent: this.indent,
      ...(this.mode === undefined ? {} : { mode: this.mode }),
      ...(this.currentId === undefined ? {} : { currentId: this.currentId }),
    });
  }

  invalidate(): void {
    /* nothing cached between frames */
  }
}
