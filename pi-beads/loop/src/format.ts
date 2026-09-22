/**
 * One-line summaries for plain (non-TUI) log output.
 *
 * Originally vendored from
 * https://github.com/yogibear54/my-own-pi-stuff/blob/main/extensions/stream-output/index.ts
 *
 * Per [ADR-001](../docs/ADR-001-transport-and-rendering.md) this module is **not**
 * the renderer: highlighted output comes from pi's own `Markdown` /
 * `AssistantMessageComponent` on pi-tui. These helpers exist only for the
 * compact "tool call → one-line summary" lines the loop writes to a plain log.
 *
 * The unused hand-rolled ANSI palette that used to live at the top of this file
 * was deleted — that palette is exactly what the ADR rejected.
 */
// ── Helpers for parsing tool args/results ──────────────────────────────

// Safely parse args that may be a stringified JSON object or already an object
export function parseArgs(args: unknown): Record<string, unknown> | null {
  if (!args) return null;
  if (typeof args === "string") {
    try {
      const parsed = JSON.parse(args);
      if (typeof parsed === "object" && parsed !== null)
        return parsed as Record<string, unknown>;
    } catch {
      /* not JSON */
    }
    return null;
  }
  if (typeof args === "object") return args as Record<string, unknown>;
  return null;
}

// Extract the actual text from a content array like [{type: "text", text: "..."}]
export function extractText(result: unknown): string | null {
  if (!result) return null;

  let obj: unknown = result;
  if (typeof result === "string") {
    try {
      obj = JSON.parse(result);
    } catch {
      return result;
    }
  }

  if (typeof obj === "object" && obj !== null) {
    const rec = obj as Record<string, unknown>;
    if (Array.isArray(rec.content)) {
      const texts = rec.content
        .filter(
          (item: any) =>
            typeof item === "object" &&
            item.type === "text" &&
            typeof item.text === "string",
        )
        .map((item: any) => item.text as string);
      if (texts.length > 0) return texts.join("\n");
    }
  }
  return null;
}

// Format tool-specific concise args display
export function formatToolArgs(toolName: string, args: unknown): string {
  const obj = parseArgs(args);
  if (!obj) return String(args ?? "");

  switch (toolName) {
    case "bash":
      return obj.command ? `$ ${obj.command}` : formatGeneric(obj);
    case "read": {
      let s = `${obj.path ?? "?"}`;
      if (obj.offset)
        s += `:${obj.offset}${obj.limit ? `-${Number(obj.offset) + Number(obj.limit)}` : "+"}`;
      return s;
    }
    case "edit": {
      const count = Array.isArray(obj.edits) ? obj.edits.length : 0;
      return `${obj.path ?? "?"} (${count} edit${count !== 1 ? "s" : ""})`;
    }
    case "write":
      return `${obj.path ?? "?"} (${formatBytes(String(obj.content ?? "").length)})`;
    default:
      return formatGeneric(obj);
  }
}

/**
 * Format a tool result as a short summary line. `toolName` is accepted for
 * symmetry with {@link formatToolArgs} (callers pass the same value to both) but
 * the current summary strategy is content-driven, so it is unused. Prefix kept to
 * satisfy `noUnusedParameters`.
 */
export function formatToolResult(
  _toolName: string,
  result: unknown,
  maxLen = 1500,
): string {
  // Try extracting text from content array first
  const text = extractText(result);
  if (text !== null) {
    if (text.length === 0) return "(empty)";
    return truncateLines(text, maxLen);
  }

  // Fallback: generic formatting
  return formatGenericObj(result, maxLen);
}

// ── Formatting utilities ───────────────────────────────────────────────

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  return `${(bytes / 1024).toFixed(1)}KB`;
}

export function formatGeneric(obj: Record<string, unknown>): string {
  return Object.entries(obj)
    .filter(([k]) => k !== "content")
    .map(([k, v]) => {
      const s = typeof v === "string" ? v : JSON.stringify(v);
      return `${k}: ${s.length > 80 ? s.slice(0, 80) + "…" : s}`;
    })
    .join(", ");
}

export function formatGenericObj(val: unknown, maxLen = 500): string {
  let s: string;
  if (typeof val === "string") {
    try {
      const parsed = JSON.parse(val);
      if (typeof parsed === "object" && parsed !== null) {
        s = JSON.stringify(parsed, null, 2);
      } else {
        s = val;
      }
    } catch {
      s = val;
    }
  } else if (typeof val === "object" && val !== null) {
    s = JSON.stringify(val, null, 2);
  } else {
    s = String(val);
  }
  if (s.length > maxLen) s = s.slice(0, maxLen) + "…";
  return s;
}

export function truncateLines(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  const lines = text.split("\n");
  let result = "";
  let i = 0;
  for (; i < lines.length; i++) {
    const next = result + (i > 0 ? "\n" : "") + lines[i];
    if (next.length > maxLen) break;
    result = next;
  }
  if (i < lines.length) {
    const remaining = lines.length - i;
    result += `\n… (${remaining} more line${remaining !== 1 ? "s" : ""})`;
  }
  return result;
}

// Indent multiline content with hanging padding
export function indentContent(text: string, indent: number): string {
  const padding = " ".repeat(indent);
  return text
    .split("\n")
    .map((line, i) => (i === 0 ? line : padding + line))
    .join("\n");
}
