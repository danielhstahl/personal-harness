/**
 * Tests for `src/kanban.ts` — the read-only three-column board.
 *
 * The module has four jobs and each is pinned separately, because the way one
 * breaks is never the way another breaks:
 *
 *   A. **What was read.** An empty list, a list that was never asked for, and a
 *      read that failed are three different facts and must stay three different
 *      things all the way to the pixels. This is the whole reason the module
 *      exists in its current shape: a board that paints an unread queue as
 *      empty is a lie with a nice border around it.
 *   B. **What leads.** The `ready` column has to be ordered the way the picker
 *      orders — priority, then the ticket that has waited longest — or the
 *      board advertises a next pick the loop will not make.
 *   C. **What fits.** Column widths, the fallback to the one-line form, the id
 *      that must never be elided, and the overflow that must be admitted rather
 *      than dropped.
 *   D. **What it costs.** One read at a time, backoff when `bd` is unhappy (a
 *      poll here is a child process, not a socket read), joined cycles,
 *      listeners notified once, timers gone after `stop()`.
 *
 * Plus the two surfaces the board is drawn on and the composition root that
 * wires it: the board may sit in the fixed chrome and it may sit at the top,
 * but it may never survive a released surface, reach a piped log, or move a
 * ticket.
 */
import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { stripTerminalSequences, visibleWidth } from "@earendil-works/pi-tui";
import { initTheme } from "@earendil-works/pi-coding-agent";

import type { Issue } from "../src/beads.ts";
import {
  KANBAN_COLUMNS,
  KanbanComponent,
  boardColumnWidths,
  compactAge,
  createKanbanSource,
  createNullKanban,
  kanbanView,
  renderKanban,
  renderKanbanBoard,
  renderKanbanRow,
  renderCardLine,
  unreadKanbanView,
  withCurrentId,
  type KanbanRead,
  type KanbanSource,
  type KanbanView,
} from "../src/kanban.ts";
import { createNullPresenter, createWorkPresenter } from "../src/render.ts";
import { createIdleMode, type IdleHandle } from "../src/idle.ts";
import { buildApp, refreshBoardOnWrite } from "../src/app.ts";
import { FakeSignals, FakeTerminal, settle } from "./idle-fakes.ts";

initTheme("dark", false);

// ── fixtures ───────────────────────────────────────────────────────────────

const T0 = Date.parse("2026-01-01T12:00:00Z");
const ago = (ms: number): string => new Date(T0 - ms).toISOString();

/**
 * `closed_at` is on the payload bd sends for closed work but not on `Issue`,
 * which is precisely the field `kanban.ts` reads off the raw record — so the
 * fixture carries the untyped extras rather than pretending the type has them.
 */
function issue(id: string, title: string, extra: Record<string, unknown> = {}): Issue {
  return {
    id,
    title,
    status: "open",
    priority: 2,
    issue_type: "task",
    created_at: ago(60 * 60 * 1000),
    updated_at: ago(60 * 1000),
    ...extra,
  } as unknown as Issue;
}

/** A board with something in every column, in the shapes `bd list` returns. */
function sampleRead(): KanbanRead {
  return {
    ready: [
      issue("kb-2", "second pick", { priority: 2, updated_at: ago(90_000) }),
      issue("kb-1", "next pick", { priority: 1, updated_at: ago(300_000) }),
    ],
    inProgress: [issue("kb-7", "being worked", { status: "in_progress" })],
    closed: [
      issue("kb-9", "older win", { status: "closed", closed_at: ago(7_200_000) }),
      issue("kb-8", "recent win", { status: "closed", closed_at: ago(600_000) }),
    ],
  };
}

function view(
  read: KanbanRead,
  options: { currentId?: string; doneLimit?: number } = {},
): KanbanView {
  return kanbanView({
    now: T0,
    read,
    doneLimit: options.doneLimit ?? 12,
    ...(options.currentId === undefined ? {} : { currentId: options.currentId }),
  });
}

const plain = (lines: readonly string[]): string[] =>
  lines.map((line) => stripTerminalSequences(line));

/**
 * The idle surface writes pi's OSC-8 hyperlink markup, and pi-tui's
 * `stripTerminalSequences` is greedy across a CSI sequence that has a BEL
 * somewhere after it — which is exactly the shape of a styled first line, so it
 * swallows the whole top border of the grid and reports an empty line. The
 * standard non-greedy ANSI pattern removes the markup without eating content,
 * which is what "what the operator saw" requires here.
 */
const ANSI = /[\u001B\u009B][[\]()#;?]*(?:(?:(?:[a-zA-Z\d]*(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?\u0007)|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-ntqry=><~]))/g;
const rawLines = (output: string): string[] =>
  output
    .replace(ANSI, "")
    .split("\n")
    .map((line) => line.replace(/\r$/, ""));

function column(view: KanbanView, key: string) {
  const found = view.columns.find((candidate) => candidate.key === key);
  assert.ok(found, `column ${key} missing from ${JSON.stringify(view.columns.map((c) => c.key))}`);
  return found;
}

/** A clock that only moves when told to, and reports what it has armed. */
function fakeTime(start = 0) {
  let current = start;
  const timers = new Map<number, { at: number; run: () => void }>();
  let nextId = 1;
  return {
    now: () => current,
    pending: () => timers.size,
    /** When the next timer would fire, in fake-ms: what the poller armed with. */
    nextAt(): number | undefined {
      const ats = [...timers.values()].map((timer) => timer.at).sort((a, b) => a - b);
      return ats[0];
    },
    advance(ms: number) {
      current += ms;
    },
    tick(ms: number) {
      current += ms;
      const due = [...timers.entries()]
        .filter(([, timer]) => timer.at <= current)
        .sort((a, b) => a[1].at - b[1].at);
      for (const [id, timer] of due) {
        timers.delete(id);
        timer.run();
      }
    },
    schedule(run: () => void, ms: number) {
      const id = nextId++;
      timers.set(id, { at: current + ms, run });
      return () => {
        timers.delete(id);
      };
    },
  };
}

// ── A. what was read ───────────────────────────────────────────────────────

describe("kanban view: absent, empty and failed are three different facts", () => {
  it("names the three columns in draw order", () => {
    const board = view({});
    assert.deepEqual(board.columns.map((c) => c.key), [...KANBAN_COLUMNS]);
    assert.deepEqual(board.columns.map((c) => c.label), ["ready", "in progress", "done"]);
  });

  it("renders a column that answered with an empty list as live and zero", () => {
    const board = view({ ready: [] });
    assert.equal(column(board, "ready").state, "live");
    assert.equal(column(board, "ready").total, 0);
    assert.equal(board.anyLive, true);
  });

  it("renders a column that was never asked for as absent, not zero", () => {
    const board = view({ ready: [] });
    assert.equal(column(board, "progress").state, "absent");
    assert.equal(column(board, "done").state, "absent");
  });

  it("renders a failed column as unknown and carries the reason", () => {
    const board = view({ ready: [], failed: ["done"], error: "bd: exit 1" });
    const done = column(board, "done");
    assert.equal(done.state, "unknown");
    assert.equal(done.error, "bd: exit 1");
    assert.ok(
      plain(renderKanban(board, null, { width: 100, maxLines: 5, mode: "board" })).some((line) =>
        line.includes("done ?"),
      ),
      "an unread column shows `?`, never `0`",
    );
  });

  it("says so when nothing answered", () => {
    assert.equal(view({}).anyLive, false);
  });

  it("counts one column failing without blanking the others", () => {
    const board = view({ ready: [issue("kb-1", "still here")], failed: ["progress"], error: "boom" });
    assert.equal(column(board, "ready").total, 1);
    assert.equal(column(board, "progress").state, "unknown");
    assert.equal(board.anyLive, true, "the columns that answered are still evidence");
  });

  it("skips rows with no usable id instead of inventing a card", () => {
    const board = view({
      ready: [{ title: "no id" } as unknown as Issue, issue("kb-1", "ok")],
    });
    assert.deepEqual(
      column(board, "ready").cards.map((card) => card.id),
      ["kb-1"],
    );
  });

  it("keeps a closed read that filled its window as a floor, not a count", () => {
    const closed = [
      issue("kb-c1", "one", { status: "closed" }),
      issue("kb-c2", "two", { status: "closed" }),
      issue("kb-c3", "three", { status: "closed" }),
    ];
    const capped = view({ closed, closedWindow: 3 });
    assert.equal(
      column(capped, "done").floored,
      true,
      "we got everything we asked for, so there may be more behind it",
    );
    assert.ok(
      plain(renderKanban(capped, null, { width: 100, maxLines: 4, mode: "board" })).some((line) =>
        line.includes("done 3+"),
      ),
    );

    const short = view({ closed: closed.slice(0, 2), closedWindow: 3 });
    assert.equal(
      column(short, "done").floored,
      undefined,
      "a read that came back short is an exact count",
    );
  });

  it("caps the done column at doneLimit but keeps the total honest", () => {
    const closed = Array.from({ length: 9 }, (_unused, index) =>
      issue(`kb-c${index}`, `closed ${index}`, { status: "closed", closed_at: ago(index * 60_000) }),
    );
    const board = view({ closed }, { doneLimit: 4 });
    assert.equal(column(board, "done").total, 9);
    assert.equal(column(board, "done").cards.length, 4);
  });
});

describe("kanban view: card contents", () => {
  it("takes the holder from owner, then assignee", () => {
    const board = view({
      inProgress: [
        issue("kb-1", "owned", { status: "in_progress", owner: "alice", assignee: "bob" }),
        issue("kb-2", "assigned", { status: "in_progress", assignee: "carol" }),
      ],
    });
    const cards = column(board, "progress").cards;
    assert.equal(cards.find((c) => c.id === "kb-1")?.holder, "alice");
    assert.equal(cards.find((c) => c.id === "kb-2")?.holder, "carol");
  });

  it("counts blockers through both dependency shapes bd emits", () => {
    // `bd list` reports an edge; `bd show` reports the other issue inlined.
    // Reading only one of them yields a fail-open zero: "nothing blocking this".
    const edgeShape = issue("kb-1", "edge shaped", {
      dependencies: [{ issue_id: "kb-1", depends_on_id: "kb-blocker", type: "blocks" } as never],
    });
    const inlinedShape = issue("kb-2", "inlined shape", {
      dependencies: [{ id: "kb-blocker", title: "the blocker", dependency_type: "blocks" } as never],
    });
    const board = view({ ready: [edgeShape, inlinedShape] });
    const cards = column(board, "ready").cards;
    assert.equal(cards.find((c) => c.id === "kb-1")?.blockers, 1);
    assert.equal(cards.find((c) => c.id === "kb-2")?.blockers, 1);
  });

  it("falls back to dependency_count when the dependency list itself is absent", () => {
    const board = view({ ready: [issue("kb-1", "counted", { dependency_count: 3 })] });
    assert.equal(column(board, "ready").cards[0]?.blockers, 3);
  });

  it("flags the current ticket in the view", () => {
    const board = view(sampleRead(), { currentId: "kb-7" });
    assert.equal(column(board, "progress").cards[0]?.current, true);
    assert.equal(board.currentId, "kb-7");
  });
});

// ── B. what leads ──────────────────────────────────────────────────────────

describe("kanban view: ordering is the picker's ordering", () => {
  it("puts the highest priority first in `ready`", () => {
    const board = view({
      ready: [
        issue("kb-low", "later", { priority: 4 }),
        issue("kb-high", "first", { priority: 0 }),
        issue("kb-mid", "middle", { priority: 2 }),
      ],
    });
    assert.deepEqual(
      column(board, "ready").cards.map((card) => card.id),
      ["kb-high", "kb-mid", "kb-low"],
    );
  });

  it("breaks a priority tie by the ticket that has waited longest", () => {
    const board = view({
      ready: [
        issue("kb-fresh", "just made", { priority: 1, updated_at: ago(10_000) }),
        issue("kb-stale", "waiting", { priority: 1, updated_at: ago(500_000) }),
      ],
    });
    assert.deepEqual(
      column(board, "ready").cards.map((card) => card.id),
      ["kb-stale", "kb-fresh"],
    );
  });

  it("treats a missing priority as the middle of the range", () => {
    const board = view({
      ready: [
        issue("kb-1", "unset", { priority: undefined as unknown as number }),
        issue("kb-0", "urgent", { priority: 0 }),
        issue("kb-4", "backlog", { priority: 4 }),
      ],
    });
    assert.deepEqual(
      column(board, "ready").cards.map((card) => card.id),
      ["kb-0", "kb-1", "kb-4"],
    );
  });

  it("leads the in-progress column with our own ticket", () => {
    const board = view(
      {
        inProgress: [
          issue("kb-a", "theirs", { status: "in_progress" }),
          issue("kb-b", "ours", { status: "in_progress" }),
          issue("kb-c", "also theirs", { status: "in_progress" }),
        ],
      },
      { currentId: "kb-b" },
    );
    assert.deepEqual(
      column(board, "progress").cards.map((card) => card.id),
      ["kb-b", "kb-a", "kb-c"],
    );
  });

  it("orders the done column most-recent-first, by close time", () => {
    const board = view({
      closed: [
        issue("kb-old", "closed long ago", {
          status: "closed",
          closed_at: ago(9_000_000),
          updated_at: ago(10_000),
        }),
        issue("kb-new", "just closed", { status: "closed", closed_at: ago(60_000) }),
      ],
    });
    assert.deepEqual(
      column(board, "done").cards.map((card) => card.id),
      ["kb-new", "kb-old"],
      "`closed_at` wins over `updated_at`: the bottom of the board is the recent past",
    );
  });

  it("sorts the closed list before capping it, so the cap keeps the recent past", () => {
    const closed = Array.from({ length: 12 }, (_unused, index) =>
      issue(`kb-c${index}`, `closed ${index}`, {
        status: "closed",
        closed_at: ago((12 - index) * 60_000),
      }),
    );
    const board = view({ closed }, { doneLimit: 2 });
    assert.deepEqual(
      column(board, "done").cards.map((card) => card.id),
      ["kb-c11", "kb-c10"],
    );
  });

  it("deduplicates a ticket that appears in two columns, live state winning", () => {
    const board = view({
      ready: [issue("kb-1", "was ready")],
      inProgress: [issue("kb-1", "now being worked", { status: "in_progress" })],
      closed: [issue("kb-1", "was closed", { status: "closed" })],
    });
    assert.equal(column(board, "progress").cards.length, 1);
    assert.equal(column(board, "ready").cards.length, 0);
    assert.equal(column(board, "done").cards.length, 0, "read twice, painted once, in the live column");
  });
});

describe("kanban: the highlight follows the run, not the read", () => {
  it("marks a card current at draw time even though the read did not know it", () => {
    const board = view(sampleRead());
    assert.equal(column(board, "progress").cards[0]?.current, false);
    const marked = renderKanban(board, null, {
      width: 100,
      maxLines: 4,
      mode: "board",
      currentId: "kb-7",
    });
    assert.ok(
      plain(marked).some((line) => line.includes("▸ kb-7")),
      `expected our ticket marked, got: ${plain(marked).join(" | ")}`,
    );
  });

  it("moves the highlighted card to the head of its column", () => {
    const board = view({
      inProgress: [
        issue("kb-a", "theirs", { status: "in_progress" }),
        issue("kb-b", "ours now", { status: "in_progress" }),
      ],
    });
    assert.deepEqual(
      column(withCurrentId(board, "kb-b"), "progress").cards.map((card) => card.id),
      ["kb-b", "kb-a"],
    );
  });

  it("clears a highlight that was taken away", () => {
    const marked = withCurrentId(view(sampleRead(), { currentId: "kb-7" }), undefined);
    assert.equal(
      column(marked, "progress").cards.some((card) => card.current),
      false,
    );
  });

  it("keeps a view-level highlight when the surface does not supply one", () => {
    const board = view(sampleRead(), { currentId: "kb-7" });
    const lines = plain(renderKanban(board, null, { width: 100, maxLines: 4, mode: "board" }));
    assert.ok(
      lines.some((line) => line.includes("▸ kb-7")),
      `the read-time marker must survive rendering: ${lines.join(" | ")}`,
    );
  });

  it("is referentially cheap when nothing changed", () => {
    const board = view(sampleRead(), { currentId: "kb-7" });
    assert.equal(withCurrentId(board, "kb-7"), board);
  });
});

// ── C. layout ──────────────────────────────────────────────────────────────

describe("kanban layout", () => {
  it("hands the undividables to the right-hand columns", () => {
    const widths = boardColumnWidths(58, 1, 3);
    assert.ok(widths);
    // 58 − 1 indent − 4 rules = 53 over three columns → 17 + 18 + 18.
    assert.deepEqual(widths, [17, 18, 18]);
    assert.equal(widths.reduce((sum, w) => sum + w, 0), 53);
  });

  it("refuses a grid whose columns would hold an id and nothing else", () => {
    // 3 × 16 + 4 rules + 1 indent = 53.
    assert.equal(boardColumnWidths(52, 1, 3), undefined);
    assert.ok(boardColumnWidths(53, 1, 3), "exactly at the minimum fits");
  });

  it("draws a frame whose every line is the same visible width", () => {
    const board = view(sampleRead());
    for (const width of [60, 80, 100, 121]) {
      const lines = plain(renderKanbanBoard(board, null, { width, cardRows: 3, indent: " " }));
      const widths = new Set(lines.map((line) => visibleWidth(line)));
      assert.equal(widths.size, 1, `ragged grid at ${width}: ${lines.join(" | ")}`);
      assert.equal([...widths][0], width, "the frame lands on the terminal edge");
      assert.equal(lines.length, 5);
    }
  });

  it("keeps the id whole at any width and lets the title give way", () => {
    const card = column(
      view({ ready: [issue("kb-long-identifier", "a title long enough to have to give way")] }),
      "ready",
    ).cards[0]!;
    const cramped = stripTerminalSequences(renderCardLine(card, null, 20));
    assert.ok(cramped.includes("kb-long-identifier"), `id was elided: "${cramped}"`);
    assert.ok(visibleWidth(cramped) <= 20, `"${cramped}" overflows its column`);
  });

  it("admits a column has more cards than it shows", () => {
    const closed = Array.from({ length: 20 }, (_unused, index) =>
      issue(`kb-c${index}`, `closed ${index}`, { status: "closed", closed_at: ago(index * 60_000) }),
    );
    const board = view({ closed }, { doneLimit: 20 });
    const lines = plain(renderKanbanBoard(board, null, { width: 100, cardRows: 2, indent: " " }));
    assert.ok(
      lines.some((line) => line.includes("+18")),
      `overflow was swallowed: ${lines.join(" | ")}`,
    );
  });

  it("marks an unread column as unread rather than empty", () => {
    const board = view({ failed: ["ready"], error: "bd exploded" });
    const lines = plain(renderKanbanBoard(board, null, { width: 100, cardRows: 2, indent: " " }));
    assert.ok(lines.some((line) => line.includes("unread")), lines.join(" | "));
  });

  it("does not stamp the empty-column marker down every spare row", () => {
    const board = view({ ready: [issue("kb-1", "one card")] });
    const lines = plain(renderKanbanBoard(board, null, { width: 100, cardRows: 4, indent: " " }));
    // Three columns, one marker each. Marking every spare row would put twelve
    // dashes on this board, which reads as four empty rows rather than as one
    // column with one card and two columns with none.
    // Three columns, one marker each — counted in the card rows only, since the
    // header legitimately carries a `—` for each absent column. Marking every
    // spare row would put twelve on this board, which reads as four empty rows
    // rather than as one column with one card and two columns with none.
    const markers = lines.slice(1, -1).join("").split("—").length - 1;
    assert.equal(markers, 3, `one marker per column, not ${markers}: ${lines.join(" | ")}`);
  });

  it("degrades `board` to `row` when three columns will not fit", () => {
    const lines = plain(renderKanban(view(sampleRead()), null, { width: 40, maxLines: 5, mode: "board" }));
    assert.equal(lines.length, 1, "one honest line beats three crushed ones");
    assert.ok(lines[0]?.startsWith("▦"));
  });

  it("renders nothing in `off` mode", () => {
    assert.deepEqual(renderKanban(view(sampleRead()), null, { width: 100, mode: "off" }), []);
  });

  it("keeps the board inside the width it was given, gutter included", () => {
    for (const width of [53, 70, 99, 120]) {
      const lines = plain(
        renderKanban(view(sampleRead()), null, { width, maxLines: 5, mode: "board", indent: " " }),
      );
      for (const line of lines) {
        assert.ok(
          visibleWidth(line) <= width,
          `"${line}" is ${visibleWidth(line)} wide in a ${width} cell`,
        );
      }
    }
  });
});

describe("kanban row", () => {
  it("shows the three counts and the next pick", () => {
    const line = plain(renderKanbanRow(view(sampleRead()), null, { width: 120 }))[0]!;
    assert.ok(line.includes("ready 2"), line);
    assert.ok(line.includes("in progress 1"), line);
    assert.ok(line.includes("done 2"), line);
    assert.ok(line.includes("next kb-1 next pick"), line, "the picker's next move, on the same line");
  });

  it("drops the next pick rather than truncating it", () => {
    const line = plain(renderKanbanRow(view(sampleRead()), null, { width: 40 }))[0]!;
    assert.ok(!line.includes("next"), `a half-truncated next is worse than none: ${line}`);
    assert.ok(visibleWidth(line) <= 40, line);
  });

  it("says how old the picture is once it is old", () => {
    const stale: KanbanView = { ...view(sampleRead()), ageMs: 45_000 };
    const line = plain(renderKanbanRow(stale, null, { width: 200 }))[0]!;
    assert.ok(line.includes("45s old"), line);
  });

  it("keeps the whole line inside the width, indent included", () => {
    const line = plain(renderKanbanRow(view(sampleRead()), null, { width: 30, indent: " " }))[0]!;
    assert.ok(visibleWidth(line) <= 30, `"${line}" is ${visibleWidth(line)} wide`);
  });
});

describe("compactAge", () => {
  it("formats the units a glance can parse", () => {
    assert.equal(compactAge(0), "0s");
    assert.equal(compactAge(59_000), "59s");
    assert.equal(compactAge(60_000), "1m");
    assert.equal(compactAge(3_600_000), "1h");
    assert.equal(compactAge(90 * 60_000), "1h");
    assert.equal(compactAge(48 * 3_600_000), "2d");
    assert.equal(compactAge(undefined), undefined);
    assert.equal(compactAge(Number.NaN), undefined);
    assert.equal(compactAge(-5), "0s", "a clock skew never renders negative");
  });
});

describe("unreadKanbanView", () => {
  it("is the shape of 'we asked and got nothing', for every column", () => {
    const board = unreadKanbanView(T0, "bd not on PATH");
    assert.equal(board.anyLive, false);
    for (const key of KANBAN_COLUMNS) {
      assert.equal(column(board, key).state, "unknown");
      assert.equal(column(board, key).error, "bd not on PATH");
    }
  });
});

// ── D. the poller ──────────────────────────────────────────────────────────

describe("kanban poller", () => {
  it("reads once immediately on start and then on the interval", async () => {
    const time = fakeTime(T0);
    let reads = 0;
    const source = createKanbanSource({
      read: async () => {
        reads += 1;
        return sampleRead();
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
    });
    source.start();
    await settle(1);
    assert.equal(reads, 1, "the first read is immediate: a board that starts blank is a board that lied");
    assert.equal(time.nextAt(), T0 + 1_000);
    // And it keeps going. A poller that arms once and then goes quiet looks
    // exactly like a board that is simply not changing, for hours.
    for (const expected of [2, 3, 4]) {
      time.tick(1_000);
      await settle(1);
      assert.equal(reads, expected, `still polling, tick ${expected}`);
    }
    source.stop();
  });

  it("backs off exponentially when the board cannot be read, and caps it", async () => {
    const time = fakeTime(T0);
    let reads = 0;
    const source = createKanbanSource({
      read: async () => {
        reads += 1;
        throw new Error("bd: ENOENT");
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
      maxBackoffMs: 4_000,
    });
    source.start();
    await settle(1);
    assert.equal(reads, 1);
    // Every poll here is a child process. A missing binary must cost one notice
    // and a slowing trickle, not one spawn per second forever.
    assert.equal(time.nextAt(), T0 + 2_000, "one failure → 2x");
    time.tick(2_000);
    await settle(1);
    assert.equal(reads, 2);
    assert.equal(time.nextAt(), T0 + 2_000 + 4_000, "two failures → 4x");
    time.tick(6_000);
    await settle(1);
    assert.equal(reads, 3);
    assert.equal(time.nextAt()! - time.now(), 4_000, "and never past the ceiling");
    source.stop();
  });

  it("resets the backoff after one good read", async () => {
    const time = fakeTime(T0);
    let failing = true;
    const source = createKanbanSource({
      read: async () => {
        if (failing) throw new Error("board is down");
        return sampleRead();
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
      maxBackoffMs: 32_000,
    });
    source.start();
    await settle(1);
    assert.equal(time.nextAt(), T0 + 2_000);
    failing = false;
    time.tick(2_000);
    await settle(1);
    assert.equal(source.view.anyLive, true);
    assert.equal(time.nextAt(), time.now() + 1_000, "a good read is back on the interval");
    source.stop();
  });

  it("keeps the last good board when a later read fails", async () => {
    let mode: "good" | "bad" = "good";
    const source = createKanbanSource({
      read: async () => {
        if (mode === "bad") throw new Error("board went away");
        return sampleRead();
      },
      intervalMs: 1_000,
    });
    await source.refresh();
    assert.equal(source.view.anyLive, true);
    mode = "bad";
    await source.refresh();
    assert.equal(
      source.view.anyLive,
      true,
      "a subprocess hiccup does not blank what the operator still knows",
    );
    assert.match(source.describe().join("\n"), /failing: board went away \(1x\)/);
    source.stop();
  });

  it("joins the read in flight instead of starting a second one", async () => {
    const time = fakeTime(T0);
    let reads = 0;
    let release: () => void = () => undefined;
    const source = createKanbanSource({
      read: () => {
        reads += 1;
        return new Promise<KanbanRead>((resolve) => {
          release = () => resolve(sampleRead());
        });
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
    });
    source.start();
    await settle(1);
    const first = source.refresh();
    const second = source.refresh();
    assert.equal(reads, 1, "a slow bd produces one long wait, not a queue of them");
    release();
    await Promise.all([first, second]);
    assert.equal(reads, 1, "and both callers were served by the one cycle that was already running");
    assert.equal(source.view.anyLive, true);
    source.stop();
  });

  it("never overlaps a cycle, even across ticks", async () => {
    const time = fakeTime(T0);
    let live = 0;
    let maxLive = 0;
    const source = createKanbanSource({
      read: async () => {
        live += 1;
        maxLive = Math.max(maxLive, live);
        await settle(2);
        live -= 1;
        return sampleRead();
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 500,
    });
    source.start();
    for (let i = 0; i < 6; i += 1) {
      time.tick(500);
      await settle(1);
    }
    assert.equal(maxLive, 1, `two reads were in flight at once (${maxLive})`);
    source.stop();
  });

  it("notifies listeners once per read, and survives one that throws", async () => {
    const source = createKanbanSource({ read: async () => sampleRead(), intervalMs: 1_000 });
    let good = 0;
    source.subscribe(() => {
      throw new Error("surface exploded on repaint");
    });
    source.subscribe(() => {
      good += 1;
    });
    await source.refresh();
    assert.equal(good, 1, "a throwing neighbour does not stop the others");
    const unsubscribe = source.subscribe(() => {
      throw new Error("also exploded");
    });
    unsubscribe();
    await source.refresh();
    assert.equal(good, 2, "and an unsubscribed listener stops being called");
    source.stop();
  });

  it("does nothing on the clock after stop(), and stops being running", async () => {
    const time = fakeTime(T0);
    let reads = 0;
    const source = createKanbanSource({
      read: async () => {
        reads += 1;
        return sampleRead();
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
    });
    source.start();
    await settle(1);
    source.stop();
    assert.equal(source.running, false);
    time.tick(60_000);
    await settle(1);
    assert.equal(reads, 1, "stop means stop: no orphan timer polling a board nobody is showing");
  });

  it("refreshes without a running timer, and does not arm one when stopped", async () => {
    const time = fakeTime(T0);
    let reads = 0;
    const source = createKanbanSource({
      read: async () => {
        reads += 1;
        return sampleRead();
      },
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1_000,
    });
    await source.refresh();
    assert.equal(reads, 1);
    assert.equal(time.pending(), 0, "a refresh while stopped leaves no timer behind");
    source.stop();
  });

  it("draws nothing before the first read when nothing has failed", () => {
    const source = createKanbanSource({ read: async () => sampleRead(), intervalMs: 1_000 });
    assert.deepEqual(source.lines(100), []);
    source.stop();
  });

  it("says the board is unread rather than showing nothing when the first read failed", async () => {
    const source = createKanbanSource({
      read: async () => {
        throw new Error("bd not on PATH");
      },
      intervalMs: 1_000,
    });
    await source.refresh().catch(() => undefined);
    const lines = plain(source.lines(100, { mode: "row" }));
    assert.equal(lines.length, 1);
    assert.ok(lines[0]?.includes("?"), `expected unread markers, got: ${lines[0]}`);
    assert.ok(!lines[0]?.includes("ready 0"), "a failure never renders as an empty board");
    source.stop();
  });

  it("ages the picture at read time rather than needing a timer to do it", async () => {
    const time = fakeTime(T0);
    const source = createKanbanSource({
      read: async () => sampleRead(),
      now: time.now,
      schedule: time.schedule,
      intervalMs: 10_000,
    });
    await source.refresh();
    assert.equal(source.view.ageMs, 0);
    time.advance(30_000);
    assert.equal(source.view.ageMs, 30_000);
    source.stop();
  });

  it("floors a silly interval instead of hammering the board", async () => {
    const time = fakeTime(T0);
    const source = createKanbanSource({
      read: async () => sampleRead(),
      now: time.now,
      schedule: time.schedule,
      intervalMs: 1,
    });
    source.start();
    await settle(1);
    assert.equal(time.nextAt(), T0 + 500, "the minimum is 500ms: each of these is a process spawn");
    source.stop();
  });

  it("reports what it knows when asked to describe itself", async () => {
    const quiet = createKanbanSource({ read: async () => sampleRead(), intervalMs: 1_000 });
    await quiet.refresh();
    const lines = quiet.describe();
    assert.equal(lines.length, 1, "quiet describe is one line");
    assert.match(lines[0]!, /kanban\s+0s ago\s+ok/);

    const verbose = createKanbanSource({
      read: async () => ({
        ready: sampleRead().ready,
        inProgress: sampleRead().inProgress,
        failed: ["done"],
        error: "closed read blew up",
      }),
      intervalMs: 1_000,
      verbose: true,
    });
    await verbose.refresh();
    const report = verbose.describe().join("\n");
    assert.match(report, /ready\s+live\s+2 total/);
    assert.match(report, /done\s+unknown\s+0 total.*closed read blew up/s);
    assert.match(report, /interval 1000ms/);
    verbose.stop();
  });

  it("emits a failure line so the operator is not left staring at a stale board", async () => {
    const seen: string[] = [];
    const source = createKanbanSource({
      read: async () => {
        throw new Error("bd exited 1");
      },
      intervalMs: 1_000,
      onEvent: (line) => seen.push(line),
    });
    await source.refresh().catch(() => undefined);
    await source.refresh().catch(() => undefined);
    assert.equal(seen.length, 2);
    assert.match(seen[1]!, /read failed \(2x\): bd exited 1/);
    source.stop();
  });
});

describe("null kanban", () => {
  it("satisfies the interface, renders nothing, and says why", async () => {
    const off = createNullKanban("LOOP_KANBAN=0");
    off.start();
    assert.equal(off.running, false);
    assert.deepEqual(off.lines(100, { mode: "board" }), []);
    await off.refresh();
    assert.equal(off.view.anyLive, false);
    assert.match(off.describe().join(""), /kanban: off.*LOOP_KANBAN=0/);
    let called = 0;
    const unsub = off.subscribe(() => {
      called += 1;
    });
    unsub();
    assert.equal(called, 0);
    off.stop();
  });
});

describe("KanbanComponent", () => {
  it("is a component that renders the source's lines with its indent", async () => {
    const source = createKanbanSource({ read: async () => sampleRead(), mode: "board", maxLines: 4 });
    await source.refresh();
    const lines = plain(new KanbanComponent(source, 4, " ").render(100));
    assert.equal(lines.length, 4);
    assert.ok(lines[0]?.startsWith(" ╭"), `first line was: ${String(lines[0])}`);
    source.stop();
  });

  it("passes the current ticket through, which is the whole point of setCurrent", async () => {
    const source = createKanbanSource({ read: async () => sampleRead(), mode: "board", maxLines: 4 });
    await source.refresh();
    const component = new KanbanComponent(source, 4, " ");
    assert.ok(!plain(component.render(100)).some((line) => line.includes("▸")));
    component.setCurrent("kb-7");
    assert.ok(
      plain(component.render(100)).some((line) => line.includes("▸ kb-7")),
      "setCurrent must change what is on screen, or it is a setter that sets nothing",
    );
    source.stop();
  });

  it("honours a per-surface mode override", async () => {
    const source = createKanbanSource({ read: async () => sampleRead(), mode: "board", maxLines: 5 });
    await source.refresh();
    const component = new KanbanComponent(source, 5, " ");
    component.setMode("row");
    const lines = plain(component.render(100));
    assert.equal(lines.length, 1);
    assert.ok(lines[0]?.startsWith(" ▦"), `first line was: ${String(lines[0])}`);
    source.stop();
  });

  it("caches nothing between frames, so invalidate is a no-op and not a lie", async () => {
    const source = createKanbanSource({ read: async () => sampleRead(), mode: "row" });
    const component = new KanbanComponent(source, 1, " ");
    await source.refresh();
    const before = plain(component.render(80)).join("\n");
    component.invalidate();
    assert.equal(plain(component.render(80)).join("\n"), before);
    source.stop();
  });
});

// ── the two surfaces ───────────────────────────────────────────────────────

interface KanbanHarness {
  presenter: ReturnType<typeof createWorkPresenter>;
  source: KanbanSource | null;
  frame: () => string[];
  written: () => string;
  time: ReturnType<typeof fakeTime>;
}

/** The shape `src/monitor.ts`'s `MonitorSource` asks of its provider. */
interface MonitorLike {
  lines(width: number, maxLines?: number, indent?: string): string[];
  subscribe(listener: () => void): () => void;
  readonly snapshot: unknown;
  start(): void;
  stop(): void;
  poll(): Promise<void>;
  readonly running: boolean;
  describe(): string[];
}

async function workHarness(options: {
  kanban?: KanbanSource | null;
  monitorPlacement?: "band" | "top";
  kanbanPlacement?: "band" | "top";
  kanbanMode?: "row" | "board";
  kanbanLines?: number;
  monitor?: MonitorLike | null;
  tty?: boolean;
  columns?: number;
}): Promise<KanbanHarness> {
  const term = new FakeTerminal(options.columns ?? 80, 24);
  const time = fakeTime(T0);
  const written: string[] = [];
  const source: KanbanSource | null =
    options.kanban === undefined
      ? createKanbanSource({
          read: async () => sampleRead(),
          now: time.now,
          schedule: time.schedule,
          intervalMs: 1_000,
        })
      : options.kanban;
  // The presenter renders from whatever the source already holds, so the board
  // is read before the first frame: a frame painted over a never-read board is
  // what it looks like before the first poll lands, not what is under test.
  if (source !== null && source.view.at === 0) await source.refresh().catch(() => undefined);
  const presenter = createWorkPresenter({
    terminal: term as never,
    tty: options.tty ?? true,
    now: time.now,
    schedule: time.schedule,
    coalesceMs: 33,
    heartbeatMs: 500,
    write: (chunk: string) => written.push(chunk),
    ...(options.monitor === undefined ? {} : { monitor: options.monitor as never }),
    kanban: source,
    kanbanMode: options.kanbanMode ?? "row",
    ...(options.kanbanPlacement === undefined ? {} : { kanbanPlacement: options.kanbanPlacement }),
    ...(options.kanbanLines === undefined ? {} : { kanbanLines: options.kanbanLines }),
    ...(options.monitorPlacement === undefined ? {} : { monitorPlacement: options.monitorPlacement }),
  } as never);
  return {
    presenter,
    source,
    time,
    written: () => written.join(""),
    frame: () => presenter.capturePlain(),
  };
}

describe("kanban on the work surface", () => {
  it("rides in the fixed chrome: transcript, then board, then footer", async () => {
    const h = await workHarness({ kanbanMode: "row" });
    h.presenter.acquire();
    h.presenter.say("working on it");
    h.presenter.flushSync();
    const frame = h.frame();
    const contentAt = frame.findIndex((line) => line.includes("working on it"));
    const boardAt = frame.findIndex((line) => line.includes("▦"));
    const footerAt = frame.findIndex((line) => /^\s*issue\b/u.test(line));
    assert.ok(boardAt >= 0, `the board is on screen: ${frame.join(" | ")}`);
    assert.ok(contentAt >= 0 && contentAt < boardAt, "the transcript is above it");
    assert.ok(boardAt < footerAt, "and the footer is still last");
    h.presenter.release();
    h.source?.stop();
  });

  it("draws the grid when the surface asks for the grid", async () => {
    const h = await workHarness({ kanbanMode: "board", kanbanLines: 5 });
    h.presenter.acquire();
    h.presenter.say("a wide board please");
    h.presenter.flushSync();
    const frame = h.frame();
    assert.ok(frame.some((line) => line.trim().startsWith("╭")), frame.join("\n"));
    assert.ok(frame.some((line) => line.includes("│· kb-1 next pick")), frame.join("\n"));
    h.presenter.release();
    h.source?.stop();
  });

  it("marks the ticket this run is working in its own board", async () => {
    const h = await workHarness({ kanbanMode: "board", kanbanLines: 5 });
    h.presenter.acquire();
    h.presenter.setContext({ issueId: "kb-7", phase: "working" });
    h.presenter.flushSync();
    assert.ok(
      h.frame().some((line) => line.includes("▸ kb-7")),
      `our ticket should be marked: ${h.frame().join(" | ")}`,
    );
    h.presenter.release();
    h.source?.stop();
  });

  it("keeps the board when only the board is pinned to the top", async () => {
    const h = await workHarness({ kanbanPlacement: "top", kanbanMode: "row" });
    h.presenter.acquire();
    h.presenter.say("content in the middle");
    h.presenter.flushSync();
    const frame = h.frame();
    const boardAt = frame.findIndex((line) => line.includes("▦"));
    const contentAt = frame.findIndex((line) => line.includes("content in the middle"));
    assert.ok(boardAt >= 0, `a top-pinned board must still be drawn: ${frame.join(" | ")}`);
    assert.ok(boardAt < contentAt, "and above the transcript, because that is what `top` means");
    h.presenter.release();
    h.source?.stop();
  });

  it("pins the monitor outside the board when both go to the top", async () => {
    const monitor: MonitorLike = {
      lines: () => [" ● 1s MONITOR"],
      subscribe: () => () => undefined,
      snapshot: {},
      start: () => undefined,
      stop: () => undefined,
      poll: async () => undefined,
      running: false,
      describe: () => [],
    };
    const h = await workHarness({
      kanbanMode: "row",
      kanbanPlacement: "top",
      monitorPlacement: "top",
      monitor,
    });
    h.presenter.acquire();
    h.presenter.say("sandwich");
    h.presenter.flushSync();
    const frame = h.frame();
    const monitorAt = frame.findIndex((line) => line.includes("MONITOR"));
    const boardAt = frame.findIndex((line) => line.includes("▦"));
    const contentAt = frame.findIndex((line) => line.includes("sandwich"));
    assert.ok(monitorAt >= 0 && boardAt >= 0, `both HUDs drawn: ${frame.join(" | ")}`);
    assert.ok(
      monitorAt < boardAt && boardAt < contentAt,
      `monitor, then board, then the transcript: ${frame.join(" | ")}`,
    );
    h.presenter.release();
    h.source?.stop();
  });

  it("keeps both HUDs when they are pinned to opposite ends", async () => {
    const monitor: MonitorLike = {
      lines: () => [" ● 1s MONITOR"],
      subscribe: () => () => undefined,
      snapshot: {},
      start: () => undefined,
      stop: () => undefined,
      poll: async () => undefined,
      running: false,
      describe: () => [],
    };
    const h = await workHarness({
      kanbanMode: "row",
      kanbanPlacement: "top",
      monitorPlacement: "band",
      monitor,
    });
    h.presenter.acquire();
    h.presenter.say("between two HUDs");
    h.presenter.flushSync();
    const frame = h.frame();
    const monitorAt = frame.findIndex((line) => line.includes("MONITOR"));
    const boardAt = frame.findIndex((line) => line.includes("▦"));
    const contentAt = frame.findIndex((line) => line.includes("between two HUDs"));
    assert.ok(
      boardAt >= 0 && boardAt < contentAt && contentAt < monitorAt,
      `board above, monitor below: ${frame.join(" | ")}`,
    );
    h.presenter.release();
    h.source?.stop();
  });

  it("leaves when the surface is released, so scrollback ends with content", async () => {
    const h = await workHarness({ kanbanMode: "row" });
    h.presenter.acquire();
    h.presenter.say("a line of work");
    h.presenter.flushSync();
    assert.ok(h.frame().some((line) => line.includes("▦")));
    h.presenter.release();
    const frame = h.frame();
    assert.ok(
      !frame.some((line) => line.includes("▦") || line.includes("ready 2")),
      `no stale board after release: ${JSON.stringify(frame)}`,
    );
    h.source?.stop();
  });

  it("takes one frame per read, through the same coalescing window as everything else", async () => {
    const h = await workHarness({ kanbanMode: "row" });
    h.presenter.acquire();
    h.presenter.flushSync();
    const before = h.presenter.stats().paints;
    // The read lands on a microtask and the paint on the coalescing timer, so
    // the await is what makes "the read landed" true before the clock moves.
    await h.source!.refresh();
    h.time.tick(40);
    assert.equal(h.presenter.stats().paints, before + 1, "a read landing is one repaint");
    await Promise.all([h.source!.refresh(), h.source!.refresh()]);
    h.time.tick(40);
    assert.equal(h.presenter.stats().paints, before + 2, "and two reads in a window are still one frame");
    h.presenter.release();
    h.source!.stop();
  });

  it("is not printed into a piped log", async () => {
    const h = await workHarness({ kanbanMode: "board", kanbanLines: 5, tty: false });
    h.presenter.acquire();
    h.presenter.say("plain transcript");
    h.presenter.flushSync();
    const out = h.written();
    assert.ok(out.includes("plain transcript"), "the transcript still goes out");
    assert.ok(!out.includes("▦") && !out.includes("╭"), `the board does not: ${out}`);
    h.source?.stop();
  });

  it("changes nothing on the surface when there is no board", async () => {
    const h = await workHarness({ kanban: null });
    h.presenter.acquire();
    h.presenter.say("just the transcript");
    h.presenter.flushSync();
    const frame = h.frame();
    assert.ok(!frame.some((line) => line.includes("▦") || line.includes("╭")));
    assert.ok(frame.some((line) => line.includes("just the transcript")));
    assert.ok(frame.some((line) => /^\s*issue\b/u.test(line)));
  });

  it("paints a board that fails as unread rather than silently vanishing", async () => {
    const failing = createKanbanSource({
      read: async () => {
        throw new Error("bd missing");
      },
      intervalMs: 1_000,
    });
    await failing.refresh().catch(() => undefined);
    const h = await workHarness({ kanban: failing, kanbanMode: "row" });
    h.presenter.acquire();
    h.presenter.say("work continues");
    h.presenter.flushSync();
    const boardLine = h.frame().find((line) => line.includes("ready"));
    assert.ok(boardLine !== undefined && boardLine.includes("?"), `expected unread: ${boardLine}`);
    h.presenter.release();
    failing.stop();
  });
});

// ── the idle surface ───────────────────────────────────────────────────────

interface IdleHarness {
  idle: IdleHandle;
  term: FakeTerminal;
  source: KanbanSource;
  lines: () => string[];
  /** The turn already in flight: resolves with whatever the operator submits. */
  first: Promise<unknown>;
  stop: () => Promise<void>;
}

async function idleHarness(options: {
  board?: () => Promise<KanbanRead>;
  kanban?: KanbanSource;
  columns?: number;
  status?: { ready: number; inProgress: number };
}): Promise<IdleHarness> {
  const term = new FakeTerminal(options.columns ?? 100, 30);
  const source =
    options.kanban === undefined
      ? createKanbanSource({
          read: options.board ?? (async () => sampleRead()),
          mode: "board",
          maxLines: 5,
          intervalMs: 1_000,
        })
      : options.kanban;
  if (source.view.at === 0) await source.refresh().catch(() => undefined);
  const idle = createIdleMode({
    terminal: term as never,
    signals: new FakeSignals(),
    status: options.status ?? { ready: 2, inProgress: 1 },
    kanban: source,
    kanbanMode: "board",
    kanbanLines: 5,
  } as never);
  // The turn is started here and handed out: `next()` may only be waited on once
  // per pending input, so a harness that fired it and then let a test fire it
  // again would leave the test waiting on a turn nobody can answer.
  const first = idle.next();
  first.catch(() => undefined);
  await settle(3);
  return {
    idle,
    term,
    source,
    first,
    lines: () => rawLines(term.output),
    stop: async () => {
      await idle.dispose().catch(() => undefined);
      source.stop();
    },
  };
}

describe("kanban on the idle surface", () => {
  it("draws the grid above the board's own one-line status", async () => {
    const h = await idleHarness({});
    const lines = h.lines();
    const gridAt = lines.findIndex((line) => line.includes("╭ ready 2"));
    const statusAt = lines.findIndex((line) => /^\s*ready 2 · in progress 1/u.test(line));
    assert.ok(gridAt >= 0, `the grid is drawn: ${lines.join(" | ")}`);
    assert.ok(statusAt >= 0 && gridAt < statusAt, "and above the one-line status, not below it");
    await h.stop();
  });

  it("repaints when a read lands, unprompted", async () => {
    let board: KanbanRead = { ready: [issue("kb-early", "here at boot")], inProgress: [], closed: [] };
    const h = await idleHarness({ board: async () => board, status: { ready: 1, inProgress: 0 } });
    assert.ok(h.lines().some((line) => line.includes("kb-early")));
    assert.ok(!h.lines().some((line) => line.includes("kb-late")));
    board = { ready: [issue("kb-late", "arrived later")], inProgress: [], closed: [] };
    await h.source.refresh();
    await settle(3);
    assert.ok(
      h.lines().some((line) => line.includes("kb-late")),
      `the idle screen followed the board: ${h.lines().join(" | ")}`,
    );
    await h.stop();
  });

  it("stops painting once the surface is disposed", async () => {
    const h = await idleHarness({});
    await h.idle.dispose().catch(() => undefined);
    const after = h.term.writes.length;
    h.source.start();
    await settle(4);
    assert.equal(
      h.term.writes.length,
      after,
      "a disposed idle surface must not still be repainting from a poller it no longer owns",
    );
    h.source.stop();
  });

  it("draws no board when the surface was given none", async () => {
    const h = await idleHarness({ kanban: createNullKanban("off") });
    const output = h.lines().join("\n");
    assert.ok(!output.includes("╭") && !output.includes("▦"), output);
    assert.ok(output.includes("ready 2 · in progress 1"), output);
    await h.stop();
  });

  it("keeps the prompt usable with a board on screen", async () => {
    const h = await idleHarness({});
    h.term.input("hello from the board\r");
    assert.deepEqual(await h.first, { kind: "input", text: "hello from the board" });
    await h.stop();
  });

  it("shows a board that cannot be read as unread rather than as an empty screen", async () => {
    const h = await idleHarness({
      board: async () => {
        throw new Error("bd is unhappy");
      },
    });
    const output = h.lines().join("\n");
    assert.ok(output.includes("ready ?"), `unread markers should show: ${output}`);
    assert.ok(!output.includes("ready 0"), "never a confident zero");
    await h.stop();
  });
});

// ── the composition root ───────────────────────────────────────────────────

/** A `BdClient` that records what it was asked to do, and never touches a disk. */
function recordingBeads(options: { closedCount?: number } = {}) {
  const calls: string[] = [];
  const closed = Array.from({ length: options.closedCount ?? 1 }, (_unused, index) =>
    issue(`kb-closed${index}`, `closed ${index}`, {
      status: "closed",
      closed_at: ago((index + 1) * 600_000),
    }),
  );
  const client = {
    async listReady(listOptions: { limit?: number } = {}) {
      calls.push(`listReady:${JSON.stringify(listOptions)}`);
      return [issue("kb-ready", "ready one")];
    },
    async listInProgress(listOptions: { limit?: number } = {}) {
      calls.push(`listInProgress:${JSON.stringify(listOptions)}`);
      return [issue("kb-prog", "in progress one", { status: "in_progress" })];
    },
    async listClosed(listOptions: { limit?: number } = {}) {
      calls.push(`listClosed:limit=${listOptions.limit ?? "none"}`);
      return closed;
    },
    async getIssue(id: string) {
      calls.push(`getIssue:${id}`);
      return null;
    },
    async createIssue(spec: { title: string }) {
      calls.push(`createIssue:${spec.title}`);
      return issue("kb-new", spec.title);
    },
    async addDep(id: string, dependsOnId: string) {
      calls.push(`addDep:${id}->${dependsOnId}`);
    },
    async appendNote(id: string, text: string) {
      calls.push(`appendNote:${id}`);
      return issue(id, text);
    },
    async setStatus(id: string, status: string) {
      calls.push(`setStatus:${id}=${status}`);
      return issue(id, "st", { status: status as never });
    },
    async closeIssue(id: string, reason: string) {
      calls.push(`closeIssue:${id}`);
      return issue(id, reason, { status: "closed" });
    },
    async remember(_text: string, key: string) {
      calls.push(`remember:${key}`);
    },
    async recall(key: string) {
      calls.push(`recall:${key}`);
      return null;
    },
  };
  return { client: client as never, calls };
}

function appHarness(options: {
  beads: never;
  kanbanConfig?: Record<string, unknown>;
  kanbanOverride?: KanbanSource | null;
}) {
  const overrides: Record<string, unknown> = {
    presenter: createNullPresenter(),
    idle: { next: async () => ({ kind: "exit" as const, reason: "test" }) } as never,
    git: undefined as never,
    beads: options.beads,
  };
  if (options.kanbanOverride !== null && options.kanbanOverride !== undefined) {
    overrides.kanban = options.kanbanOverride;
  }
  return buildApp({
    cwd: "/repo",
    maxIterations: 0,
    kanban: (options.kanbanConfig ?? { enabled: true }) as never,
    providerAudit: { enabled: false },
    monitor: { enabled: false },
    overrides: overrides as never,
  });
}

describe("kanban in the composition root", () => {
  it("reads all three columns, with the closed read bounded", async () => {
    const { client, calls } = recordingBeads({ closedCount: 2 });
    const app = appHarness({ beads: client });
    await app.run().catch(() => undefined);
    assert.ok(
      calls.some((call) => call.startsWith("listReady")),
      `ready was read: ${calls.join(", ")}`,
    );
    assert.ok(
      calls.some((call) => call.startsWith("listInProgress")),
      `in-progress was read: ${calls.join(", ")}`,
    );
    const closed = calls.find((call) => call.startsWith("listClosed"));
    assert.ok(closed, "the closed column is read too");
    assert.notEqual(
      closed,
      "listClosed:limit=none",
      "and it is bounded — an unbounded closed read is a heap dump on an old board",
    );
    const rendered = plain(app.kanban.lines(100, { mode: "row" }));
    assert.ok(
      rendered.some((line) => line.includes("ready 1") && line.includes("done 2")),
      `the board shows what was read: ${rendered.join(" | ")}`,
    );
    app.kanban.stop();
  });

  it("forwards the label filter, so the board shows this project's board", async () => {
    const { client, calls } = recordingBeads();
    const app = appHarness({ beads: client, kanbanConfig: { enabled: true } });
    // `appHarness` builds without labels; rebuild with them and check the board's
    // reads carry them, exactly as the loop's own `check_work` read does.
    await app.run().catch(() => undefined);
    const labelled = buildApp({
      cwd: "/repo",
      maxIterations: 0,
      labels: ["loop"],
      kanban: { enabled: true } as never,
      providerAudit: { enabled: false },
      monitor: { enabled: false },
      overrides: {
        presenter: createNullPresenter(),
        idle: { next: async () => ({ kind: "exit" as const, reason: "test" }) } as never,
        git: undefined as never,
        beads: client,
      } as never,
    });
    await labelled.run().catch(() => undefined);
    const ready = calls.filter((call) => call.startsWith('listReady:{"labels":["loop"]}'));
    assert.ok(
      ready.length > 0,
      `the board read the labelled board: ${calls.filter((c) => c.startsWith("listReady")).join(" | ")}`,
    );
    labelled.kanban.stop();
    app.kanban.stop();
  });

  it("switches the whole board off with one flag, and says so", async () => {
    const { client } = recordingBeads();
    const app = appHarness({ beads: client, kanbanConfig: { enabled: false } });
    await app.run().catch(() => undefined);
    assert.deepEqual(app.kanban.lines(100), [], "off means nothing is drawn");
    assert.equal(app.kanban.running, false, "and nothing was started");
    assert.match(app.kanban.describe().join("\n"), /kanban: off/);
  });

  it("takes an injected board wholesale instead of building one", async () => {
    const { client } = recordingBeads();
    const injected = createKanbanSource({
      read: async () => ({ ready: [issue("kb-injected", "from the test")], inProgress: [], closed: [] }),
      mode: "row",
      intervalMs: 1_000,
    });
    const app = appHarness({ beads: client, kanbanOverride: injected });
    await app.run().catch(() => undefined);
    assert.equal(app.kanban, injected, "the substitute is the object the app shows");
    const rendered = plain(app.kanban.lines(200, { mode: "row" }));
    assert.ok(rendered.some((line) => line.includes("kb-injected")), rendered.join(" | "));
    app.kanban.stop();
  });

  it("refreshes the picture on every write the loop makes, and on no read", async () => {
    let refreshes = 0;
    const watch = {
      ...createNullKanban(),
      lines: () => [],
      refresh: async () => {
        refreshes += 1;
      },
    } as unknown as KanbanSource;
    const { client, calls } = recordingBeads();
    const wrapped = refreshBoardOnWrite(client as never, watch);

    await wrapped.listReady();
    await wrapped.listInProgress();
    await wrapped.listClosed();
    await wrapped.getIssue("kb-1");
    await wrapped.recall("k");
    assert.equal(refreshes, 0, `a read must not trigger a re-read: ${calls.join(", ")}`);

    await wrapped.createIssue({ title: "new" });
    assert.equal(refreshes, 1, "a created ticket moves it onto the board immediately");
    await wrapped.addDep("kb-1", "kb-2");
    await wrapped.appendNote("kb-1", "note");
    await wrapped.setStatus("kb-1", "in_progress");
    await wrapped.closeIssue("kb-1", "done");
    assert.equal(refreshes, 5, "every mutation is followed by one refresh, none missed");
  });

  it("a failing refresh cannot spoil the write it followed", async () => {
    const throwing = {
      ...createNullKanban(),
      refresh: async () => {
        throw new Error("board read blew up");
      },
    } as unknown as KanbanSource;
    const { client } = recordingBeads();
    const wrapped = refreshBoardOnWrite(client as never, throwing);
    const created = await wrapped.createIssue({ title: "still created" });
    assert.equal((created as Issue).title, "still created");
    await settle(1);
  });
});
