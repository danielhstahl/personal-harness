#!/usr/bin/env node
/**
 * pi-beads loop — CLI entry point (workspace-5yn.9).
 *
 * This file is deliberately almost empty. It reads the environment in exactly
 * one place, hands the result to `buildApp` in `src/app.js`, runs the loop and
 * maps the outcome to a process exit code. It constructs no adapter, opens no
 * session and issues no `bd` or `git` call — `test/loop.test.ts` greps this
 * file to keep it that way, because a second place that decides configuration
 * is a second place that can be wrong.
 */
import { pathToFileURL } from "node:url";

import { AgentError, parseThinkingLevel } from "./agent.ts";
import type { ThinkingLevel } from "./agent.ts";
import { runApp } from "./app.ts";
import type { AppConfig, KanbanSetting, NotifySetting } from "./app.ts";
import type { KanbanMode } from "./kanban.ts";
import type { PanelPlacement } from "./panel.ts";
import { LoopError } from "./loop.ts";
import type { LoopResult } from "./loop.ts";

// ── the knob vocabulary ────────────────────────────────────────────────────
//
// Every knob below is read with one of the helpers declared here, and every
// helper either produces a value of a declared type or throws. The throw is
// the shape `LOOP_NTFY_MAX_FAILURES` has used from the start — the key, the
// value exactly as it arrived, and the set that would have been accepted —
// because whoever typed the mistake is reading the terminal they typed it in,
// and should not have to find out later, by experiment, what the run actually
// decided.
//
// The rule the helpers encode: **unset means unset, set means valid.** A knob
// that is absent falls to the default its section documents. A knob that is
// present — including present-and-blank, what a bare `LOOP_KANBAN_MS=` line in
// an env file leaves behind — has to be a value, because afterwards "we
// guessed" and "that is what was asked for" look identical in the transcript.

/** What a switch answers to. Anything else is refused, never read as "off". */
const ON_WORDS: readonly string[] = ["1", "true", "yes", "on"];
const OFF_WORDS: readonly string[] = ["0", "false", "no", "off"];

/**
 * `LOOP_KANBAN` is one knob with three answers: hidden, one row, the grid.
 *
 * The off-words and the shape words share a map on purpose. Read as a boolean
 * the shape words are "not 1", so `LOOP_KANBAN=board` would switch the board
 * *off* — a trap that costs the first person who tries it an hour — and read
 * as a shape the off-words have nowhere to go. One map, one accepted set, and
 * what comes out is a {@link KanbanMode} rather than a re-derivation of one.
 */
const KANBAN_WORDS: Readonly<Record<string, KanbanMode>> = {
  "0": "off",
  off: "off",
  no: "off",
  false: "off",
  row: "row",
  board: "board",
};

/** Where a panel sits — see {@link PanelPlacement}. Nothing else sits anywhere. */
const PLACEMENT_WORDS: Readonly<Record<string, PanelPlacement>> = {
  band: "band",
  top: "top",
};

/** `report` is the default: name a stale lock, do not delete it (ADR-006). */
const STALE_LOCK_WORDS: Readonly<Record<string, "report" | "remove">> = {
  report: "report",
  remove: "remove",
};

/** What `LOOP_AUDIT_WRITE` may ask for. Unset writes nothing. */
const AUDIT_WRITE_WORDS: Readonly<Record<string, "none" | "proposed" | "inplace">> = {
  "0": "none",
  "false": "none",
  none: "none",
  "1": "proposed",
  "true": "proposed",
  yes: "proposed",
  proposed: "proposed",
  inplace: "inplace",
  "in-place": "inplace",
};

/**
 * Every `LOOP_*` key this loop reads, wherever it is read.
 *
 * A key with this prefix that is not on the list is a typo, and a typo is not
 * the same thing as an unset knob: it *is* set as far as the shell is
 * concerned and unset as far as the loop is concerned, so the run takes the
 * default and reads, afterwards, like a choice. `LOOP_KANBAN_MODE=board` is a
 * run with no board that believes it has one, and the only way to tell from
 * the outside is to go read the code — which is the shape of a harness nobody
 * can reason about.
 *
 * The list deliberately includes the two keys read outside `readEnv` —
 * `LOOP_DEBUG` (`src/beads.ts`, `src/repo.ts`, `src/vcs.ts`) and `LOOP_THEME`
 * (`src/idle.ts`) — because "the config parser has not heard of it" is not a
 * reason to call a live knob a typo.
 */
const KNOWN_LOOP_KEYS: readonly string[] = [
  "LOOP_AUDIT",
  "LOOP_AUDIT_STRICT",
  "LOOP_AUDIT_TIMEOUT_MS",
  "LOOP_AUDIT_VERBOSE",
  "LOOP_AUDIT_WRITE",
  "LOOP_BD_BIN",
  "LOOP_CWD",
  "LOOP_DEBUG",
  "LOOP_DRY_RUN",
  "LOOP_EPIC_ID",
  "LOOP_EPIC_TITLE",
  "LOOP_GIT_BIN",
  "LOOP_GIT_KILL_GRACE_MS",
  "LOOP_GIT_LOCK_WAIT_MS",
  "LOOP_GIT_STALE_LOCK",
  "LOOP_GIT_STALE_LOCK_AFTER_MS",
  "LOOP_HEALTH_URL",
  "LOOP_KANBAN",
  "LOOP_KANBAN_AT",
  "LOOP_KANBAN_DONE",
  "LOOP_KANBAN_LINES",
  "LOOP_KANBAN_MS",
  "LOOP_KANBAN_VERBOSE",
  "LOOP_MAX_ITERATIONS",
  "LOOP_MONITOR",
  "LOOP_MONITOR_AT",
  "LOOP_MONITOR_LINES",
  "LOOP_MONITOR_MODELS_MS",
  "LOOP_MONITOR_MS",
  "LOOP_MONITOR_TIMEOUT_MS",
  "LOOP_MONITOR_URL",
  "LOOP_MONITOR_VERBOSE",
  "LOOP_NTFY_CLICK",
  "LOOP_NTFY_MAX_FAILURES",
  "LOOP_NTFY_MAX_MESSAGE_BYTES",
  "LOOP_NTFY_PRIORITY",
  "LOOP_NTFY_TAGS",
  "LOOP_NTFY_TIMEOUT_MS",
  "LOOP_NTFY_TITLE_PREFIX",
  "LOOP_NTFY_TOKEN",
  "LOOP_NTFY_TOPIC",
  "LOOP_NTFY_URL",
  "LOOP_RETRY_UNFIT_WORK",
  "LOOP_SPLIT_REPO_ACCESS",
  "LOOP_SPLIT_THINKING",
  "LOOP_THEME",
  "LOOP_VERBOSE",
  "LOOP_WORK_THINKING",
  "LOOP_WORK_TIMEOUT_MS",
  "LOOP_WRAP_UP_MS",
];

/**
 * The refusal every knob throws: the key, the value as it arrived, and what
 * would have been accepted. One helper, so "what was wrong with it?" has one
 * answer in one shape every time.
 */
function badConfig(name: string, raw: string | undefined, expected: string): LoopError {
  return new LoopError("bad-config", `${name}=${JSON.stringify(raw ?? "")} ${expected}`);
}

/** Plain Levenshtein, on names short enough not to care what it costs. */
function editDistance(a: string, b: string): number {
  const width = b.length + 1;
  const previous = new Uint32Array(width);
  const current = new Uint32Array(width);
  for (let j = 0; j < width; j += 1) previous[j] = j;
  for (let i = 1; i <= a.length; i += 1) {
    current[0] = i;
    for (let j = 1; j < width; j += 1) {
      const sameByte = a.charCodeAt(i - 1) === b.charCodeAt(j - 1) ? 0 : 1;
      current[j] = Math.min(
        (previous[j] ?? 0) + 1,
        (current[j - 1] ?? 0) + 1,
        (previous[j - 1] ?? 0) + sameByte,
      );
    }
    previous.set(current);
  }
  return previous[b.length] ?? 0;
}

/**
 * The nearest known key, when one is near enough to be what was meant.
 *
 * The tolerance grows with the length of the name, because a typo in
 * `LOOP_MONITOR_MODELS_MS` is two characters out of twenty-one while a typo in
 * `LOOP_AUDIT` is one out of ten. Past that the honest answer is that nothing
 * was close, and the accepted list is the thing to read.
 */
function closestKey(name: string, known: readonly string[]): string | undefined {
  const upper = name.toUpperCase();
  // A known key the unknown one *starts with* is the likelier mistake however
  // the edit distance falls out: `LOOP_KANBAN_MODE` is three substitutions from
  // `LOOP_KANBAN_DONE` and five from `LOOP_KANBAN`, but it is the base knob it
  // was reaching for. So prefix first, distance only among those.
  const prefixed = known.filter((candidate) => upper.startsWith(candidate));
  if (prefixed.length > 0) {
    return prefixed.reduce((shortest, candidate) =>
      candidate.length < shortest.length ? candidate : shortest,
    );
  }
  let best: string | undefined;
  let bestDistance = Number.POSITIVE_INFINITY;
  for (const candidate of known) {
    const distance = editDistance(upper, candidate.toUpperCase());
    if (distance < bestDistance) {
      bestDistance = distance;
      best = candidate;
    }
  }
  return best !== undefined && bestDistance <= Math.max(2, Math.floor(name.length / 5))
    ? best
    : undefined;
}

/**
 * Refuse any `LOOP_*` key this loop does not read.
 *
 * Runs before a single knob is looked at, so the cheapest and least visible
 * mistake — a name with a character out of place, which no reader downstream
 * will ever notice because it is simply not one of the keys — is the first
 * thing said and the last thing that needs saying.
 */
function rejectUnknownKnobs(source: Readonly<Record<string, string | undefined>>): void {
  const unknown = Object.keys(source).filter(
    (key) => key.startsWith("LOOP_") && !KNOWN_LOOP_KEYS.includes(key),
  );
  if (unknown.length === 0) return;
  const named = unknown.map((key) => {
    const near = closestKey(key, KNOWN_LOOP_KEYS);
    return near === undefined ? key : `${key} (did you mean ${near}?)`;
  });
  const one = unknown.length === 1;
  throw new LoopError(
    "bad-config",
    `${named.join(", ")} ${one ? "is" : "are"} not ${one ? "a knob" : "knobs"} this loop reads — ` +
      "an unrecognised key keeps every default and looks like a choice. " +
      `Accepted LOOP_* knobs: ${[...KNOWN_LOOP_KEYS].sort().join(", ")}`,
  );
}

/**
 * The one environment-reading site in this module.
 *
 * `.11` replaces this with the real config layer; everything below stays
 * untouched because nothing else reads the process environment here.
 *
 * `LOOP_WORK_THINKING` / `LOOP_SPLIT_THINKING` set the level of each pass and
 * are validated on the way in: unset means *unset* (the user's configured
 * default, then pi's own), never a silently-guessed level.
 *
 * Every other knob is held to that same standard. Each one is read through a
 * helper that yields the declared type or throws a `bad-config` naming the key,
 * the raw value and the accepted set — see the vocabulary above — and all of
 * them are read *here*, before `cli` and therefore before `runApp`. Nothing is
 * spawned, no bead is claimed and no terminal is taken until the whole
 * environment has been read and understood, so a typo costs one line in the
 * shell instead of a half-started run.
 */
export function readEnv(source: Readonly<Record<string, string | undefined>> = process.env): AppConfig {
  // First thing read, before a single knob: an unknown key is the likeliest
  // mistake and the one nothing downstream would ever notice.
  rejectUnknownKnobs(source);

  /**
   * A whole number, or a refusal that names the key.
   *
   * The old reader dropped whatever it could not parse — harmless for a key
   * nobody typed, a trap for one somebody did. `LOOP_KANBAN_MS=5s` ran at the
   * default 5000, and the only evidence was a board that felt slow. Set means
   * parseable, the way `LOOP_NTFY_MAX_FAILURES` already insists, and safely
   * representable: `1e21` is a whole number that is not the whole number
   * anybody meant.
   */
  const wholeNumber = (name: string, unit: string, min = 0): number | undefined => {
    const raw = source[name];
    if (raw === undefined) return undefined;
    const shown = raw.trim();
    const parsed = Number(shown);
    // Digits, the whole way. `Number()` is generous in ways a knob should not
    // be: `0x10` is 16, `1e3` is 1000, `Infinity` is a number — each of them
    // a value that means something other than what the operator wrote, which
    // is the whole class of thing this pass is removing.
    if (!/^\d+$/u.test(shown) || !Number.isSafeInteger(parsed) || parsed < min) {
      throw badConfig(
        name,
        raw,
        `is not a whole number of ${unit} — accepted: a whole number, at least ${min}`,
      );
    }
    return parsed;
  };
  /**
   * A switch with a strict vocabulary.
   *
   * `undefined` only when the key is absent, which is what makes `?? default`
   * mean "the default, because nothing was said" rather than "the default,
   * because what was said made no sense". `LOOP_VERBOSE=ture` used to be a run
   * that was not verbose, and from outside that is indistinguishable from a
   * run somebody configured not to be verbose.
   */
  const flag = (name: string): boolean | undefined => {
    const raw = source[name];
    if (raw === undefined) return undefined;
    const value = raw.trim().toLowerCase();
    if (ON_WORDS.includes(value)) return true;
    if (OFF_WORDS.includes(value)) return false;
    throw badConfig(
      name,
      raw,
      `is not a yes/no value — accepted: ${ON_WORDS.join(", ")} (on) or ${OFF_WORDS.join(", ")} (off)`,
    );
  };
  /** A switch that is **on unless told otherwise**: `flag()`, plus the default. */
  const defaultOnFlag = (name: string): boolean => flag(name) ?? true;
  /**
   * A knob whose value is a word out of a declared map.
   *
   * The map's keys are the whole accepted vocabulary and its values are the
   * only thing a caller can receive, so `LOOP_KANBAN="bord"` is not a third
   * shape nobody thought about — it is an error that says what the shapes are.
   */
  const word = <T extends string>(
    name: string,
    map: Readonly<Record<string, T>>,
    what: string,
  ): T | undefined => {
    const raw = source[name];
    if (raw === undefined) return undefined;
    const value = raw.trim().toLowerCase();
    if (value !== "" && Object.hasOwn(map, value)) return map[value] as T;
    throw badConfig(
      name,
      raw,
      `is not ${what} — accepted: ${Object.keys(map).join(", ")}`,
    );
  };
  /**
   * Where a panel sits. `"band"` — the fixed chrome above the footer — is the
   * default, but `LOOP_KANBAN_AT=uppert` is not a band. It is a knob that
   * failed, and a knob that failed quietly is the thing this whole pass exists
   * to remove: `main.ts` used to answer `"upper"`, `"topp"` and `"band "` with
   * `"band"` and no complaint of any kind.
   */
  const placementAt = (name: string): PanelPlacement =>
    word(name, PLACEMENT_WORDS, "a panel placement") ?? "band";
  /**
   * Spread a knob into its section only when it was actually set.
   *
   * An unset knob never reaches the config as `undefined`: each section reads a
   * missing key as "use my own default" and a present one as "use this", and
   * that difference is the whole content of "unset means unset". It also keeps
   * a knob's name written down once, which the `...(read(X) === undefined ? {} :
   * { y: read(X) })` shape it replaces managed to write twice — twice the
   * reading, twice the chance of the two disagreeing.
   */
  const set = <K extends string, V>(key: K, value: V | undefined): { [P in K]?: V } =>
    value === undefined ? ({} as { [P in K]?: V }) : (({ [key]: value }) as { [P in K]?: V });
  const cwd = source.LOOP_CWD ?? process.cwd();
  const provider = source.PI_PROVIDER;
  const model = source.PI_MODEL;
  /**
   * A thinking level, read where the environment is read and validated on the
   * way out. A level pi would not accept is *refused*: a run at a level nobody
   * asked for looks exactly like one that was configured, from the outside.
   *
   * Every other knob in this function is refused on the same terms. The ones
   * that used to round a bad value down to a default are the reason this file
   * has a vocabulary now rather than a pile of ternaries.
   */
  const thinking = (name: string): ThinkingLevel | undefined =>
    parseThinkingLevel(source[name]);
  /**
   * Where the audited config goes: nothing, a `models.json.proposed` beside
   * the real file, or the live file itself with a `.bak` behind it. Unset
   * writes nothing.
   *
   * It used to be that anything truthy reached `"proposed"`, so
   * `LOOP_AUDIT_WRITE=proopsed` wrote a file nobody meant to write and
   * `LOOP_AUDIT_WRITE=in-placey` silently wrote the proposal instead of the
   * live config. The accepted words are the ones the README already named.
   */
  const auditWriteMode = (): "none" | "proposed" | "inplace" =>
    word("LOOP_AUDIT_WRITE", AUDIT_WRITE_WORDS, "a config write mode") ?? "none";
  const auditTimeout = wholeNumber("LOOP_AUDIT_TIMEOUT_MS", "milliseconds", 1);
  /**
   * Split a tag list the way a person types one.
   *
   * Comma or whitespace both separate, because the natural thing to write into
   * an environment variable is `LOOP_NTFY_TAGS="+1 tada"` and the natural thing
   * *not* to notice is that one of them silently never arrived.
   */
  const tagList = (raw: string | undefined): string[] =>
    (raw ?? "")
      .split(/[,;\s]+/u)
      .map((entry) => entry.trim())
      .filter((entry) => entry !== "");

  /**
   * How many failed publishes in a row before the notice gives up, refused
   * rather than dropped.
   *
   * `LOOP_NTFY_MAX_FAILURES` is refused rather than dropped, on the same terms
   * the rest of this file now holds every knob to: a value that is set has to
   * be a whole number of tries — and a *safely representable* one, since
   * `1e21` parses to an integer that is not the integer anybody meant — and
   * saying so is cheaper than a run that quietly gave up after three when it was
   * told to try nine. The code is `notify-config` rather than `bad-config`
   * because the failure belongs to the notice, not to the switchboard.
   */
  const maxFailuresSetting = (): number | undefined => {
    const raw = source.LOOP_NTFY_MAX_FAILURES;
    if (raw === undefined || raw.trim() === "") return undefined;
    const parsed = Number(raw);
    if (!Number.isSafeInteger(parsed) || parsed < 1) {
      throw new LoopError(
        "notify-config",
        `LOOP_NTFY_MAX_FAILURES must be a whole number of tries, at least 1, not "${raw.trim()}"`,
      );
    }
    return parsed;
  };

  /**
   * Our declared understanding of the server's `limit-message-bytes`.
   *
   * This layer checks only that a byte count was *written* as one: a whole,
   * safely representable number of bytes, at least one. Whether that number is
   * one the transport can carry is not knowable from the environment — the CLI
   * does not choose the transport, `buildNotifier` in `src/app.ts` does — so
   * the range check lives there and reads the limits off the transport itself
   * (`NTFY_MESSAGE_LIMITS` / `isNtfyMessageBytes`) instead of being two ntfy
   * constants welded into the config parser.
   *
   * Unset means "assume the transport's own default", and if that default is
   * too high the publisher shrinks on its own the first time the server
   * complains. Setting it is how you skip that discovery: set the number your
   * server actually runs.
   */
  const messageBytesSetting = (): number | undefined => {
    const raw = source.LOOP_NTFY_MAX_MESSAGE_BYTES;
    if (raw === undefined || raw.trim() === "") return undefined;
    const parsed = Number(raw);
    if (!Number.isSafeInteger(parsed) || parsed < 1) {
      throw new LoopError(
        "notify-config",
        `LOOP_NTFY_MAX_MESSAGE_BYTES must be a whole number of bytes, not "${raw.trim()}"`,
      );
    }
    return parsed;
  };

  /**
   * The completion notice: one optional feature with the topic as its switch.
   *
   * `undefined` when nothing ntfy-related is set at all, which keeps "this run
   * was never asked to notify anybody" distinct in the transcript from "this run
   * was asked and cannot". Those two read very differently to whoever is looking
   * for a notice that did not arrive.
   *
   * `LOOP_NTFY_TOPIC` is the switch, and it takes either a bare topic name —
   * joined onto `LOOP_NTFY_URL`, which defaults to the hosted server — or a
   * complete URL, which is the form people copy out of the ntfy web UI. A
   * self-hosted server is a different value in the same variable, not a
   * different configuration: that is what makes local hosting a one-line change.
   */
  const notifySetting = (): NotifySetting | undefined => {
    const knobs: readonly (string | undefined)[] = [
      source.LOOP_NTFY_TOPIC,
      source.LOOP_NTFY_URL,
      source.LOOP_NTFY_TOKEN,
      source.LOOP_NTFY_PRIORITY,
      source.LOOP_NTFY_TAGS,
      source.LOOP_NTFY_CLICK,
      source.LOOP_NTFY_TITLE_PREFIX,
      source.LOOP_NTFY_TIMEOUT_MS,
      source.LOOP_NTFY_MAX_FAILURES,
      source.LOOP_NTFY_MAX_MESSAGE_BYTES,
    ];
    if (knobs.every((value) => value === undefined || value.trim() === "")) return undefined;
    const topic = (source.LOOP_NTFY_TOPIC ?? "").trim();
    const tags = tagList(source.LOOP_NTFY_TAGS);
    const timeoutMs = wholeNumber("LOOP_NTFY_TIMEOUT_MS", "milliseconds", 1);
    const maxFailures = maxFailuresSetting();
    const maxMessageBytes = messageBytesSetting();
    return {
      // The topic is the switch. A server configured with nowhere to publish is
      // a server that will never be exercised, and saying so beats pretending.
      enabled: topic !== "",
      ...(topic === "" ? {} : { topic }),
      ...(source.LOOP_NTFY_URL === undefined ? {} : { url: source.LOOP_NTFY_URL.trim() }),
      ...(source.LOOP_NTFY_TOKEN === undefined ? {} : { token: source.LOOP_NTFY_TOKEN.trim() }),
      ...(source.LOOP_NTFY_PRIORITY === undefined
        ? {}
        : { priority: source.LOOP_NTFY_PRIORITY.trim() }),
      ...(tags.length === 0 ? {} : { tags }),
      ...(source.LOOP_NTFY_CLICK === undefined ? {} : { click: source.LOOP_NTFY_CLICK.trim() }),
      ...(source.LOOP_NTFY_TITLE_PREFIX === undefined
        ? {}
        : { titlePrefix: source.LOOP_NTFY_TITLE_PREFIX }),
      ...set("timeoutMs", timeoutMs),
      ...(maxFailures === undefined ? {} : { maxConsecutiveFailures: maxFailures }),
      ...(maxMessageBytes === undefined ? {} : { maxMessageBytes }),
    };
  };

  /**
   * The mini kanban, read as one knob with three answers.
   *
   * {@link KANBAN_WORDS} is the whole vocabulary: the off-words hide the
   * board, `row` and `board` pick the shape, and unset leaves each surface on
   * the shape it prefers. `"bord"` is none of those. It used to become
   * `enabled: true` with the shape left `undefined` — a typo wearing the face
   * of a decision, which is the exact thing this pass exists to remove.
   */
  const kanbanSetting = (): KanbanSetting => {
    const shape = word("LOOP_KANBAN", KANBAN_WORDS, "a board setting");
    const intervalMs = wholeNumber("LOOP_KANBAN_MS", "milliseconds", 1);
    const lines = wholeNumber("LOOP_KANBAN_LINES", "rows", 1);
    const doneLimit = wholeNumber("LOOP_KANBAN_DONE", "closed tickets", 0);
    return {
      enabled: shape !== "off",
      // `off` is carried by `enabled`, not by a second way of saying the same
      // thing: a consumer that read `mode: "off"` while `enabled: true` would
      // have to guess which of the two it was being told about. So `off` maps to
      // no shape at all, and `set` keeps the key out of the config entirely.
      ...set("mode", shape === "off" ? undefined : shape),
      ...set("intervalMs", intervalMs),
      ...set("lines", lines),
      ...set("doneLimit", doneLimit),
      placement: placementAt("LOOP_KANBAN_AT"),
      verbose: flag("LOOP_KANBAN_VERBOSE") ?? false,
    };
  };

  // The remaining numeric and keyword knobs, each read once, here, so a
  // section below spreads a value rather than re-reading an environment key
  // and hoping it parses the same way twice. A zero is allowed where waiting
  // zero is a thing somebody can mean (do not wait, keep every closed ticket)
  // and refused where it is not (a poll that never polls).
  const gitLockWaitMs = wholeNumber("LOOP_GIT_LOCK_WAIT_MS", "milliseconds");
  const gitKillGraceMs = wholeNumber("LOOP_GIT_KILL_GRACE_MS", "milliseconds");
  const gitStaleAfterMs = wholeNumber("LOOP_GIT_STALE_LOCK_AFTER_MS", "milliseconds");
  const staleLockPolicy = word("LOOP_GIT_STALE_LOCK", STALE_LOCK_WORDS, "a stale-lock policy");
  const workTimeoutMs = wholeNumber("LOOP_WORK_TIMEOUT_MS", "milliseconds", 1);
  const wrapUpMs = wholeNumber("LOOP_WRAP_UP_MS", "milliseconds", 1);
  const maxIterations = wholeNumber("LOOP_MAX_ITERATIONS", "iterations", 1);
  const monitorIntervalMs = wholeNumber("LOOP_MONITOR_MS", "milliseconds", 1);
  const monitorTimeoutMs = wholeNumber("LOOP_MONITOR_TIMEOUT_MS", "milliseconds", 1);
  const monitorModelsMs = wholeNumber("LOOP_MONITOR_MODELS_MS", "milliseconds", 1);
  const monitorLines = wholeNumber("LOOP_MONITOR_LINES", "rows", 1);
  return {
    cwd,
    bdBin: source.LOOP_BD_BIN,
    gitBin: source.LOOP_GIT_BIN,
    /**
     * The index-lock policy. See
     * [ADR-006](../docs/ADR-006-index-lock.md): the wait covers contention
     * with something alive, the stale threshold decides when a lock looks like a
     * crashed process instead, and removal stays off until it is asked for —
     * asked for by one of the two words that mean it, not by anything that
     * happens not to be `"remove"`.
     */
    gitLock: {
      ...set("waitMs", gitLockWaitMs),
      ...set("killGraceMs", gitKillGraceMs),
      ...set("staleAfterMs", gitStaleAfterMs),
      ...set("stalePolicy", staleLockPolicy),
    },
    modelRef: provider !== undefined && model !== undefined ? { provider, id: model } : undefined,
    workTimeoutMs,
    /** When to tell a run to land and report. Unset = budget minus the lead. */
    wrapUpMs,
    /** Opt back up: retry a run that ran out of time or context in place. */
    retryUnfitWork: flag("LOOP_RETRY_UNFIT_WORK"),
    workThinkingLevel: thinking("LOOP_WORK_THINKING"),
    splitThinkingLevel: thinking("LOOP_SPLIT_THINKING"),
    /**
     * The planning pass gets `read` + `bash` so its tickets name files that
     * actually exist. On unless turned off; `off` restores the sealed planner
     * that works from the words of the request alone.
     */
    splitRepoAccess: defaultOnFlag("LOOP_SPLIT_REPO_ACCESS"),
    /**
     * The startup provider comparison. Default on: the failures it catches cost
     * a whole work pass each and are free to see before one starts. It is a
     * warning, not a gate, unless `LOOP_AUDIT_STRICT` says otherwise.
     */
    providerAudit: {
      enabled: flag("LOOP_AUDIT") ?? true,
      strict: flag("LOOP_AUDIT_STRICT") ?? false,
      verbose: flag("LOOP_AUDIT_VERBOSE") ?? false,
      writeMode: auditWriteMode(),
      ...(source.LOOP_HEALTH_URL === undefined ? {} : { healthUrl: source.LOOP_HEALTH_URL }),
      ...set("timeoutMs", auditTimeout),
    },
    maxIterations,
    /** The read-only backend monitor. See `MonitorSetting` in `src/app.ts`. */
    monitor: {
      enabled: flag("LOOP_MONITOR") ?? true,
      ...set("intervalMs", monitorIntervalMs),
      ...set("timeoutMs", monitorTimeoutMs),
      ...set("modelsEveryMs", monitorModelsMs),
      ...set("url", source.LOOP_MONITOR_URL),
      ...set("lines", monitorLines),
      placement: placementAt("LOOP_MONITOR_AT"),
      verbose: flag("LOOP_MONITOR_VERBOSE") ?? false,
    },
    /** The mini kanban. See `KanbanSetting` in `src/app.ts`. */
    kanban: kanbanSetting(),
    /** The completion notice. See `NotifySetting` in `src/app.ts`. */
    notify: notifySetting(),
    themeName: source.PI_THEME,
    dryRun: flag("LOOP_DRY_RUN"),
    verbose: flag("LOOP_VERBOSE"),
    epicId: source.LOOP_EPIC_ID,
    epicTitle: source.LOOP_EPIC_TITLE,
  };
}

function summary(result: LoopResult): string {
  const closed = result.transcript.effects.filter((entry) => entry.kind === "beads.close_issue").length;
  const created = result.transcript.createdIssueIds.length;
  const counts = `created ${created} issue(s), closed ${closed} write(s), ` +
    `${result.transcript.transitions.length} transition(s)`;
  switch (result.kind) {
    case "done":
      return `Stopped cleanly after ${result.iterations} iteration(s); ${counts}.`;
    case "planned":
      return `Dry run: ${result.reason ?? "nothing executed"}.`;
    case "aborted":
      return `Aborted after ${result.iterations} iteration(s); ${counts}. ` +
        "An issue may still be in_progress and will be resumed by the next run.";
    case "blocked":
      return `Blocked: ${result.reason ?? "no pending effect"}`;
    case "iteration-limit":
      return `Stopped at the iteration limit: ${result.reason ?? ""}`;
    case "fatal":
      return `Fatal: ${result.reason ?? "unknown failure"}`;
  }
}

/** Map a loop outcome onto a process exit code. Exported so it is testable. */
export function exitCodeFor(result: LoopResult): number {
  return result.exitCode;
}

/**
 * The one mapping from a thrown error to the line the operator is told.
 *
 * A `LoopError` carries its own code. An `AgentError` is a refused value — a
 * configured model that is not in the catalog, a thinking level nothing
 * recognises — which is the operator's business rather than a crash, and wants
 * one plain line instead of a stack pointing at the place the string was
 * checked. `undefined` means "not one of ours", and leaves the caller to decide
 * whether that is a line to print or an error to rethrow.
 */
function fatalLine(error: unknown): string | undefined {
  if (LoopError.is(error)) return `Fatal: ${error.code}: ${error.message}`;
  if (AgentError.is(error)) return `Fatal: ${error.message}`;
  return undefined;
}

export async function cli(
  config: AppConfig,
  write: (line: string) => void = (line) => void process.stdout.write(`${line}\n`),
): Promise<number> {
  try {
    const result = await runApp(config);
    write(summary(result));
    return exitCodeFor(result);
  } catch (error) {
    // `runLoop` turns its own failures into a result, so this is the seam for
    // everything beneath it: a bad config, an adapter that could not be built.
    const fatal = fatalLine(error);
    if (fatal !== undefined) {
      write(fatal);
      return 2;
    }
    const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
    write(`Fatal: unexpected failure: ${message}`);
    return 2;
  }
}

/**
 * Read the config, then run it — with the refusal case handled before `cli` is
 * ever entered.
 *
 * `readEnv` *refuses* a thinking level pi has never heard of instead of rounding
 * it down, and that happens on the way in, outside `cli`'s try. Same line and the
 * same exit code as a fatal from the run, because to whoever is watching it is
 * the same event: the loop did not start.
 *
 * `read` is a thunk rather than a config because the refusal happens *while
 * reading*: a config already built cannot be the thing that failed.
 */
export async function runFromEnv(
  read: () => AppConfig = () => readEnv(),
  write: (line: string) => void = (line) => void process.stdout.write(`${line}\n`),
): Promise<number> {
  try {
    return await cli(await read(), write);
  } catch (error) {
    const fatal = fatalLine(error);
    if (fatal !== undefined) {
      write(fatal);
      return 2;
    }
    throw error;
  }
}

/**
 * Only run when this file is the entry point. Without this guard, a test that
 * imports `readEnv` to check the mapping would also start the loop.
 */
const isEntry =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(process.argv[1]).href;

if (isEntry) {
  process.exitCode = await runFromEnv();
}
