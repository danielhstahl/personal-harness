/**
 * pi-beads loop — the completion notice.
 *
 * One job: turn the facts of a finished bead into the title and body of a
 * notice, and hand them to a publisher. It is a separate module from
 * `src/ntfy.ts` for the same reason `src/finalize.ts` is separate from
 * `src/vcs.ts`: one knows the shape of a bead, the other knows the shape of a
 * wire. Neither needs to know the other's details, and each is testable without
 * the other.
 *
 * WHAT THE NOTICE IS FOR
 * A completed bead is a fact about a run that finished an hour ago, sitting in
 * a database you are not looking at. The notice is the one place those facts
 * get assembled for a reader who was not watching: which bead, what it did,
 * which commit, what is left over. Every value came from the run itself — the
 * verdict the agent reported, the hash git read back, the handoff key the loop
 * wrote — so the notice is an index into the real record rather than a
 * paraphrase of it.
 *
 * WHAT CHANGED WHEN THIS STOPPED BEING EMAIL
 * The shape of the reader changed, so the shape of the message changed with it.
 * An email can be a page long because it is read at a desk; a notification is
 * read on a phone, in a glance, possibly while walking. The body is therefore
 * the four things worth acting on rather than nine sections, and the title is
 * still `[prefix] <bead id> completed: <summary>` because a notification is
 * *found by* its title long after it arrives.
 *
 * WHAT IT IS NOT
 * A gate. `notifyCompletion` cannot fail a run: every path out of it is a
 * {@link NtfyDelivery}, and a `failed` delivery is information, not a stop.
 * The work is already committed and closed by the time this is called; the
 * notice is what tells a human about it. That is also why the notifier gives up
 * after a few consecutive failures — a server that is down should cost a
 * handful of warnings and then silence, not a warning per bead for six hours.
 */
import type { NtfyDelivery, NtfyPublisher } from "./ntfy.ts";

/** The bead, as the run finished knowing it. Every field here is already fact. */
export interface BeadCompletion {
  readonly issueId: string;
  readonly title: string;
  /** What the work reported it did. */
  readonly summary: string;
  /** The commit that carries it, once there is one. */
  readonly commit?: string | null;
  readonly committedAgain?: boolean;
  readonly changedFiles?: readonly string[];
  readonly nextSteps?: readonly string[];
  readonly decisions?: readonly string[];
  /** The `bd close --reason` text, kept so the notice matches the board. */
  readonly closeReason?: string | null;
  readonly handoffKey?: string | null;
  readonly iteration?: number | null;
  /** The runner's verdict kind: `done`, `incomplete`, … */
  readonly workKind?: string | null;
  readonly elapsedMs?: number | null;
  readonly completedAt?: number | null;
}

/** What the run, rather than the bead, contributes to the notice. */
export interface NoticeContext {
  readonly cwd: string;
  readonly hostname?: string;
  readonly model?: string;
  /** The `[pi-beads]` in the title. Empty string means no prefix at all. */
  readonly titlePrefix?: string;
}

/** Long enough for an id and a sentence, short enough for a phone's lock screen. */
export const MAX_TITLE_LENGTH = 120;

export const NOTICE_ID = "pi-beads-loop";
const DEFAULT_TITLE_PREFIX = "pi-beads";

// ── small formatters ────────────────────────────────────────────────────────

/** Collapse a paragraph into one line: what a title has room for. */
export function oneLine(text: string): string {
  return text.replace(/\s+/gu, " ").trim();
}

function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, Math.max(0, max - 1)).trimEnd()}…`;
}

/** `3m 12s`, `812ms`, `1h 04m` — whatever fits a line without a calculator. */
export function formatDuration(ms: number | null | undefined): string | null {
  if (ms === null || ms === undefined || !Number.isFinite(ms) || ms < 0) return null;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = Math.floor(ms / 1000) % 60;
  const minutes = Math.floor(ms / 60_000) % 60;
  const hours = Math.floor(ms / 3_600_000);
  if (hours > 0) return `${hours}h ${String(minutes).padStart(2, "0")}m`;
  if (minutes > 0) return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
  return `${seconds}s`;
}

/** `2025-09-26 11:03 UTC` — short enough for a notice line, still unambiguous. */
export function formatTimestamp(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "unknown";
  const date = new Date(ms);
  const pad = (value: number): string => String(value).padStart(2, "0");
  return (
    `${date.getUTCFullYear()}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())} ` +
    `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())} UTC`
  );
}

/** Join the pieces of a status line with ` · `, dropping whatever is missing. */
function dotJoin(parts: readonly (string | null | undefined)[]): string {
  return parts
    .map((part) => (typeof part === "string" ? part.trim() : ""))
    .filter((part) => part !== "")
    .join(" · ");
}

// ── the message ─────────────────────────────────────────────────────────────

/**
 * The title: the bead id first after the prefix, because that is the one thing a
 * reader searches by, and the summary after it so the notification list alone
 * answers "what finished?".
 */
export function completionTitle(
  completion: BeadCompletion,
  titlePrefix: string = DEFAULT_TITLE_PREFIX,
): string {
  const prefix = oneLine(titlePrefix) || DEFAULT_TITLE_PREFIX;
  const summary = oneLine(completion.summary) || oneLine(completion.title) || "no summary reported";
  const head = `[${prefix}] ${completion.issueId.trim()} completed`;
  return truncate(summary === "" ? head : `${head}: ${summary}`, MAX_TITLE_LENGTH);
}

/**
 * The body: the four things a reader can act on, in that order, plus where to
 * read more.
 *
 * Sections that the run could not know say so rather than vanishing — a missing
 * commit hash is a fact worth seeing, and a silently absent line reads as
 * "nothing to report".
 */
export function completionBody(completion: BeadCompletion, context: NoticeContext): string {
  const id = completion.issueId.trim();
  const summary = oneLine(completion.summary) || "(the run reported no summary)";

  const files = (completion.changedFiles ?? []).map((path) => path.trim()).filter((p) => p !== "");
  const commit = (completion.commit ?? "").trim();
  const commitPart =
    commit === ""
      ? "no commit recorded"
      : commit.slice(0, 12) + (completion.committedAgain === true ? " (reused, not committed twice)" : "");

  const status = dotJoin([
    completion.workKind ?? "unknown verdict",
    formatDuration(completion.elapsedMs) ?? "unknown duration",
    completion.iteration === null || completion.iteration === undefined
      ? null
      : `iteration ${completion.iteration}`,
    formatTimestamp(completion.completedAt),
  ]);
  const scope = dotJoin([
    commitPart,
    files.length === 0 ? "no files reported" : `${files.length} file${files.length === 1 ? "" : "s"}`,
  ]);

  const next = (completion.nextSteps ?? []).map(oneLine).filter((line) => line !== "");
  const lines: string[] = [
    `${id} — ${oneLine(completion.title) || "(untitled bead)"}`,
    summary,
    "",
    status,
    scope,
  ];

  if (files.length > 0 && files.length <= 6) {
    lines.push(files.map((path) => `- ${path}`).join("\n"));
  } else if (files.length > 6) {
    lines.push(`- ${files.slice(0, 5).join(", ")} …`);
  }

  lines.push(
    "",
    next.length === 0
      ? "nothing left to do"
      : `next: ${next.slice(0, 3).join("; ")}${next.length > 3 ? ` (+${next.length - 3} more)` : ""}`,
  );

  const reading = dotJoin([
    `bd show ${id}`,
    completion.handoffKey === null || completion.handoffKey === undefined
      ? null
      : `bd recall ${completion.handoffKey}`,
  ]);
  if (reading !== "") lines.push(`read: ${reading}`);
  // The machine the work happened on, always: one phone is often wired to more
  // than one repository, and "which project?" is the first question a notice
  // that names only a bead id leaves open.
  lines.push(`from ${context.cwd}${context.hostname === undefined ? "" : ` on ${context.hostname}`}`);

  return lines.join("\n");
}

/** The whole notice: a title you can find later, and a body you can read now. */
export function completionNotice(
  completion: BeadCompletion,
  context: NoticeContext,
): { title: string; body: string } {
  return {
    title: completionTitle(completion, context.titlePrefix ?? DEFAULT_TITLE_PREFIX),
    body: completionBody(completion, context),
  };
}

// ── the notifier ────────────────────────────────────────────────────────────

/**
 * What the loop calls. `enabled` says whether anything will ever go out, so a
 * caller can be honest in its own log without sending a probe.
 */
export interface Notifier {
  readonly enabled: boolean;
  readonly destination: readonly string[];
  readonly transport: string;
  notifyCompletion(completion: BeadCompletion): Promise<NtfyDelivery>;
}

export interface NotifierOptions {
  readonly publisher: NtfyPublisher;
  readonly context: NoticeContext;
  /** ntfy priority for these notices: 1–5 or a name. */
  readonly priority?: string;
  /** Emoji names for the notification. */
  readonly tags?: readonly string[];
  /** URL the notification opens. */
  readonly click?: string;
  /**
   * Give up after this many failed publishes in a row. Default 3.
   *
   * A server that is down is down; discovering that once per bead for six hours
   * is a worse report than discovering it three times and then saying nothing.
   * The giving-up itself is logged, so the silence has a start time.
   */
  readonly maxConsecutiveFailures?: number;
  readonly logger?: (line: string) => void;
}

/** A notifier that will never send, with the reason it will not. */
export function createNullNotifier(reason: string): Notifier {
  return {
    enabled: false,
    destination: [],
    transport: "none",
    async notifyCompletion(): Promise<NtfyDelivery> {
      return { kind: "skipped", reason };
    },
  };
}

/**
 * Bind a publisher to the run's context, and add the runaway guard.
 *
 * The guard is a counter rather than a timer because the thing being guarded is
 * the loop's patience: `notifyCompletion` is called once per finished bead, so
 * "three in a row" is three beads' worth of futility, which is enough for
 * anybody.
 */
export function createNotifier(options: NotifierOptions): Notifier {
  const maxFailures = options.maxConsecutiveFailures ?? 3;
  const log = options.logger ?? (() => undefined);
  const publisher = options.publisher;
  let consecutiveFailures = 0;
  let disabled: string | null = publisher.enabled ? null : "the publisher has no topic";

  return {
    // A getter, not a snapshot: after the guard trips, "is the notice on?" has
    // one answer and it is "no". A value computed once at construction would
    // keep reporting `true` for the rest of a run in which nothing will ever be
    // sent again, which is the precise opposite of what the property is for.
    get enabled(): boolean {
      return publisher.enabled && disabled === null;
    },
    destination: [publisher.destination],
    transport: publisher.transport,
    async notifyCompletion(completion: BeadCompletion): Promise<NtfyDelivery> {
      if (disabled !== null) {
        return { kind: "skipped", reason: disabled };
      }
      const notice = completionNotice(completion, options.context);
      const delivery = await publisher.publish({
        title: notice.title,
        body: notice.body,
        ...(options.priority === undefined ? {} : { priority: options.priority }),
        ...(options.tags === undefined ? {} : { tags: options.tags }),
        ...(options.click === undefined ? {} : { click: options.click }),
      });
      if (delivery.kind === "failed") {
        consecutiveFailures += 1;
        if (consecutiveFailures >= maxFailures) {
          disabled =
            `${consecutiveFailures} completion notices in a row failed to publish ` +
            `(last: ${delivery.reason}); notices are switched off for the rest of this run`;
          log(disabled);
          // The giving-up travels on the delivery the caller is already
          // reporting. Nobody wires the logger at composition time on purpose,
          // so a reason that only reached `log` would be heard on the *next*
          // bead's turn — and a run that closes exactly `maxFailures` beads and
          // then goes idle would never hear it at all.
          return { ...delivery, reason: `${delivery.reason}. ${disabled}` };
        }
      } else {
        consecutiveFailures = 0;
      }
      return delivery;
    },
  };
}
