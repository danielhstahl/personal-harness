#!/usr/bin/env node
/**
 * The mini kanban without a board.
 *
 *   node tools/kanban-demo.mjs            # a synthetic board, every shape
 *   node tools/kanban-demo.mjs --live     # the real `bd`, read-only
 *
 * Renders the same three columns the loop's surfaces render — ready, in
 * progress, done — in both modes, at three widths, plus the two failure shapes
 * (a column that answered empty and a column that never answered) because those
 * are the ones that matter and the hardest to see on a working board.
 *
 * `--live` points the same renderer at a real `bd`: it runs the three list
 * queries through the same client the loop uses and prints what came back. It
 * writes nothing — there is nothing in this path that could.
 *
 * Not a test. It is what the "live check" paragraph of
 * docs/ADR-004-mini-kanban.md was run from.
 */
import { initTheme } from "@earendil-works/pi-coding-agent";
import { stripTerminalSequences } from "@earendil-works/pi-tui";
import { createBdClient } from "../src/beads.ts";
import { kanbanView, renderKanban } from "../src/kanban.ts";
import { createPresenterTheme } from "../src/render.ts";

initTheme("dark", false);
const theme = createPresenterTheme();
const now = Date.parse("2026-01-01T12:00:00Z");
const ago = (mins) => new Date(now - mins * 60_000).toISOString();

const issue = (id, title, status, extra = {}) => ({
  id,
  title,
  status,
  priority: 2,
  issue_type: "task",
  created_at: ago(600),
  updated_at: ago(30),
  ...extra,
});

const synthetic = {
  ready: [
    issue("loop-3k", "ship the kanban", "open", { priority: 1, updated_at: ago(240) }),
    issue("loop-7m", "back off when bd is unhappy", "open", { priority: 1, updated_at: ago(180) }),
    issue("loop-9q", "teach the board about new bd fields", "open", { updated_at: ago(90) }),
    issue("loop-2t", "blocked behind the audit", "open", {
      updated_at: ago(400),
      dependencies: [{ issue_id: "loop-2t", depends_on_id: "loop-1m", type: "blocks" }],
    }),
  ],
  inProgress: [
    issue("loop-4k", "wire the board into both surfaces", "in_progress", {
      owner: "daniel",
      started_at: ago(11),
      updated_at: ago(1),
    }),
  ],
  closed: [
    issue("loop-1m", "add the backend monitor", "closed", { closed_at: ago(95) }),
    issue("loop-2b", "audit the provider config", "closed", { closed_at: ago(310) }),
    issue("loop-0a", "bootstrap the loop", "closed", { closed_at: ago(4_320) }),
    issue("loop-0b", "pick a transport", "closed", { closed_at: ago(4_400) }),
    issue("loop-0c", "the first commit", "closed", { closed_at: ago(5_000) }),
  ],
  closedWindow: 3,
};

const show = (label, lines, options = {}) => {
  console.log(`\n### ${label}`);
  if (lines.length === 0) return console.log("  (nothing rendered)");
  const paint = (line) => `  ${options.raw ? line : stripTerminalSequences(line)}`;
  console.log(lines.map(paint).join("\n"));
};

const live = process.argv.includes("--live");
let board = synthetic;
if (live) {
  const bd = createBdClient({ bin: process.env.LOOP_BD_BIN ?? "bd" });
  const [ready, inProgress, closed] = await Promise.all([
    bd.listReady(),
    bd.listInProgress(),
    bd.listClosed({ limit: 30 }),
  ]);
  board = { ready, inProgress, closed, closedWindow: 30 };
  console.log(`live board: ${ready.length} ready, ${inProgress.length} in progress, ${closed.length} closed (window 30)`);
}

const view = kanbanView({ now, read: board, currentId: "loop-4k", doneLimit: 4 });

show("board, 100 columns", renderKanban(view, theme, { width: 100, maxLines: 5, mode: "board" }), { raw: true });
show("board, 100 columns, no colour (piped)", renderKanban(view, null, { width: 100, maxLines: 5, mode: "board" }));
show("board, 64 columns", renderKanban(view, theme, { width: 64, maxLines: 5, mode: "board" }));
show("row (the work surface's shape)", renderKanban(view, theme, { width: 100, mode: "row" }), { raw: true });
show("board at 40 columns degrades to the row", renderKanban(view, theme, { width: 40, maxLines: 5, mode: "board" }));

show(
  "an empty column reads 0, an unread one reads ?",
  renderKanban(
    kanbanView({
      now,
      read: { ready: [], inProgress: board.inProgress, failed: ["done"], error: "bd: exit 1" },
      doneLimit: 4,
    }),
    theme,
    { width: 100, maxLines: 4, mode: "board" },
  ),
);

show(
  "nothing answered at all",
  renderKanban(kanbanView({ now, read: { failed: ["ready", "progress", "done"], error: "bd not on PATH" } }), theme, {
    width: 100,
    maxLines: 1,
    mode: "row",
  }),
);

show(
  "a board with nothing on it, which must not look like a failure",
  renderKanban(kanbanView({ now, read: { ready: [], inProgress: [], closed: [] } }), theme, {
    width: 100,
    maxLines: 4,
    mode: "board",
  }),
);
