/**
 * pi-beads loop — the completion notice.
 *
 * One job: turn the facts of a finished bead into an {@link EmailMessage}, and
 * hand it to a {@link Mailer}. It is a separate module from `src/mail.ts` for
 * the same reason `src/finalize.ts` is separate from `src/vcs.ts`: one knows the
 * shape of a bead, the other knows the shape of a wire. Neither needs to know
 * the other's details, and each can be tested without the other.
 *
 * WHAT THE NOTICE IS FOR
 * A completed bead is a fact about a run that finished an hour ago, sitting in a
 * database you are not looking at. The notice is the one place those facts are
 * assembled for a reader who was not watching: which bead, what it did, which
 * commit, what is left over, and where to go read more. Everything in the body
 * came from the run itself — the verdict the agent reported, the hash git read
 * back, the handoff key the loop wrote — so the mail is an index into the real
 * record rather than a paraphrase of it.
 *
 * WHAT IT IS NOT
 * A gate. `notifyCompletion` cannot fail a run: every path out of it is a
 * {@link MailDelivery}, and a `failed` delivery is information, not a stop. The
 * work is already committed and closed by the time this is called; mail is what
 * tells a human about it. That is also why the notifier gives up after a few
 * consecutive failures — a relay that is down should cost a handful of warnings
 * and then silence, not a warning per bead for the next six hours.
 */
import type { MailDelivery, Mailer } from "./mail.ts";
import { MAILER_ID, defaultFromAddress } from "./mail.ts";

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
  /** Prefix for the subject line. Empty string means no prefix at all. */
  readonly subjectPrefix?: string;
}

/** The subject-line cap. Long enough for an id and a sentence, short enough for a phone. */
export const MAX_SUBJECT_LENGTH = 120;

const DEFAULT_SUBJECT_PREFIX = "pi-beads";

// ── small formatters ────────────────────────────────────────────────────────

/** Collapse a paragraph into one line: what a subject line has room for. */
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

function formatTimestamp(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "unknown";
  const date = new Date(ms);
  const pad = (value: number): string => String(value).padStart(2, "0");
  return (
    `${date.getUTCFullYear()}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())} ` +
    `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())} UTC`
  );
}

/** `Label      value`, aligned so a glance reads down the column. */
function field(label: string, value: string): string {
  return `  ${label.padEnd(10, " ")}${value}`;
}

function bulletList(items: readonly string[]): string[] {
  return items.map((item, index) => `    ${index + 1}. ${item}`);
}

// ── the message ─────────────────────────────────────────────────────────────

/**
 * The subject line: the bead id first after the prefix, because that is the one
 * thing a reader searches mail by, and the summary after it so the inbox list
 * alone answers "what finished?".
 */
export function completionSubject(
  completion: BeadCompletion,
  subjectPrefix: string = DEFAULT_SUBJECT_PREFIX,
): string {
  const prefix = oneLine(subjectPrefix);
  const summary = oneLine(completion.summary) || oneLine(completion.title) || "no summary reported";
  const head = `[${prefix || MAILER_ID}] ${completion.issueId.trim()} completed`;
  return truncate(summary === "" ? head : `${head}: ${summary}`, MAX_SUBJECT_LENGTH);
}

/**
 * The body, as plain text.
 *
 * Sections are ordered by how soon a reader acts on them: what happened, what
 * changed, what is left, where to read more. Fields that the run could not know
 * say so rather than being dropped — a missing commit hash is a fact worth
 * seeing, and a silently absent line reads as "nothing to report".
 */
export function completionBody(completion: BeadCompletion, context: NoticeContext): string {
  const lines: string[] = [
    `${completion.issueId.trim()} — ${oneLine(completion.title) || "(untitled bead)"}`,
    "",
    field("Bead", completion.issueId.trim()),
    field("Title", oneLine(completion.title) || "(untitled)"),
    field("Status", "closed"),
    field("When", formatTimestamp(completion.completedAt)),
    field(
      "Work",
      [
        completion.workKind ?? "unknown verdict",
        formatDuration(completion.elapsedMs) ?? "unknown duration",
        completion.iteration === null || completion.iteration === undefined
          ? "unknown iteration"
          : `iteration ${completion.iteration}`,
      ].join(", "),
    ),
    "",
    "What it did",
    ...(oneLine(completion.summary) === ""
      ? ["    (the run reported no summary)"]
      : [`    ${oneLine(completion.summary)}`]),
  ];

  const decisions = (completion.decisions ?? []).map(oneLine).filter((line) => line !== "");
  if (decisions.length > 0) {
    lines.push("", "Decisions taken", ...bulletList(decisions));
  }

  lines.push("", "Changes");
  const commit = (completion.commit ?? "").trim();
  lines.push(
    field(
      "Commit",
      commit === ""
        ? "(none recorded)"
        : commit + (completion.committedAgain === true ? " — reused from an earlier attempt, not committed twice" : ""),
      ),
  );
  const files = (completion.changedFiles ?? []).map((path) => path.trim()).filter((p) => p !== "");
  if (files.length === 0) {
    lines.push(field("Files", "(none reported)"));
  } else {
    lines.push(field("Files", `${files.length} changed`));
    lines.push(...files.map((path) => `    - ${path}`));
  }

  const next = (completion.nextSteps ?? []).map(oneLine).filter((line) => line !== "");
  lines.push("", "Still to do");
  lines.push(...(next.length === 0 ? ["    (nothing reported)"] : bulletList(next)));

  lines.push(
    "",
    "Where to read more",
    field("Bead", `bd show ${completion.issueId.trim()}`),
    field(
      "Handoff",
      completion.handoffKey === null || completion.handoffKey === undefined
        ? "(no handoff key recorded)"
        : `bd recall ${completion.handoffKey} --json`,
    ),
  );
  if (commit !== "") lines.push(field("Diff", `git show ${commit}`));
  if (completion.closeReason !== null && completion.closeReason !== undefined) {
    lines.push(field("Closed as", oneLine(completion.closeReason)));
  }
  lines.push(field("Repo", context.cwd));
  if (context.model !== undefined && context.model !== "") {
    lines.push(field("Model", context.model));
  }
  return lines.join("\n");
}

/**
 * The footer, with the delivery facts a reader checks when the mail itself looks
 * wrong: who it came from, who it went to, and which host wrote it.
 *
 * The copies are named separately because they are separate facts. "to dev,
 * lead" leaves a reader guessing whether the lead was asked to review or merely
 * told, and that is the exact distinction whoever set `LOOP_NOTIFY_CC` was
 * making when they set it.
 */
export function noticeFooter(
  context: NoticeContext,
  from: string,
  to: readonly string[],
  cc: readonly string[] = [],
): string {
  const origin = context.hostname === undefined ? MAILER_ID : `${MAILER_ID} on ${context.hostname}`;
  const copied = cc.length === 0 ? "" : `, cc ${cc.join(", ")}`;
  return [
    `— sent by ${origin} from ${context.cwd} to ${to.join(", ")}${copied} (from ${from}).`,
    "  A delivery failure never stops the loop: the work this notice describes is",
    "  committed and closed whatever the mail relay does next.",
  ].join("\n");
}

/** The whole message: subject, body, and the headers a machine-sent notice wants. */
export function completionMessage(
  completion: BeadCompletion,
  context: NoticeContext,
  mailer: { from: string; to: readonly string[]; cc?: readonly string[] },
): {
  subject: string;
  body: string;
  cc: readonly string[];
  headers: Record<string, string>;
} {
  const subject = completionSubject(completion, context.subjectPrefix ?? DEFAULT_SUBJECT_PREFIX);
  const body = completionBody(completion, context);
  const headers: Record<string, string> = {
    // RFC 3834: this is generated traffic, and saying so is what keeps a
    // vacation autoresponder from starting a conversation with a build server.
    "Auto-Submitted": "auto-generated",
    "X-Loop-Issue": completion.issueId.trim(),
  };
  const commit = (completion.commit ?? "").trim();
  if (commit !== "") headers["X-Loop-Commit"] = commit;
  if (completion.handoffKey !== undefined && completion.handoffKey !== null) {
    headers["X-Loop-Handoff"] = completion.handoffKey;
  }
  if (completion.workKind !== undefined && completion.workKind !== null) {
    headers["X-Loop-Verdict"] = completion.workKind;
  }
  const cc = mailer.cc ?? [];
  const footer = noticeFooter(context, mailer.from, mailer.to, cc);
  return { subject, body: `${body}\n\n${footer}`, cc, headers };
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
  notifyCompletion(completion: BeadCompletion): Promise<MailDelivery>;
}

export interface NotifierOptions {
  readonly mailer: Mailer;
  readonly context: NoticeContext;
  /**
   * Give up after this many failed sends in a row. Default 3.
   *
   * A relay that is down is down; discovering that once per bead for six hours is
   * a worse report than discovering it three times and then saying nothing. The
   * giving-up itself is logged, so the silence has a start time.
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
    async notifyCompletion(): Promise<MailDelivery> {
      return { kind: "skipped", reason };
    },
  };
}

/**
 * Bind a mailer to the run's context, and add the runaway guard.
 *
 * The guard is a counter rather than a timer because the thing being guarded is
 * the loop's patience: `notifyCompletion` is called once per finished bead, so
 * "three in a row" is three beads' worth of futility, which is enough for
 * anybody.
 */
export function createNotifier(options: NotifierOptions): Notifier {
  const maxFailures = options.maxConsecutiveFailures ?? 3;
  const log = options.logger ?? (() => undefined);
  const mailer = options.mailer;
  let consecutiveFailures = 0;
  let disabled: string | null = mailer.enabled ? null : "the mailer has no recipients";

  return {
    // A getter, not a snapshot: after the guard trips, "is mail on?" has one
    // answer and it is "no". A `enabled: mailer.enabled && disabled === null`
    // computed once at construction would keep reporting `true` for the rest of
    // a run in which nothing will ever be sent again, which is the exact
    // opposite of what the property is for.
    get enabled(): boolean {
      return mailer.enabled && disabled === null;
    },
    destination: mailer.recipients,
    transport: mailer.transport,
    async notifyCompletion(completion: BeadCompletion): Promise<MailDelivery> {
      if (disabled !== null) {
        return { kind: "skipped", reason: disabled };
      }
      const from = mailer.from ?? defaultFromAddress();
      const message = completionMessage(completion, options.context, {
        from,
        // The primaries and the copies are passed as what they are. Handing the
        // combined list over as `to` was the bug this prevents: the copies were
        // in the envelope, so they received the mail, but the `To:` header
        // listed them as addressees and no `Cc:` header was ever written — a
        // blind copy nobody configured, with a lying header on top of it.
        to: mailer.to,
        cc: mailer.cc,
      });
      const delivery = await mailer.send({
        from,
        to: mailer.to,
        cc: mailer.cc,
        subject: message.subject,
        body: message.body,
        headers: message.headers,
      });
      if (delivery.kind === "failed") {
        consecutiveFailures += 1;
        if (consecutiveFailures >= maxFailures) {
          disabled =
            `${consecutiveFailures} completion notices in a row failed to send ` +
            `(last: ${delivery.reason}); mail is switched off for the rest of this run`;
          log(disabled);
          // The giving-up travels on the delivery the caller is already
          // reporting. Nobody wires the logger at composition time on purpose,
          // so a reason that only reached `log` would be heard on the *next*
          // bead's turn — and a run that closes exactly `maxFailures` beads
          // would never hear it at all.
          return { ...delivery, reason: `${delivery.reason}. ${disabled}` };
        }
      } else {
        consecutiveFailures = 0;
      }
      return delivery;
    },
  };
}
