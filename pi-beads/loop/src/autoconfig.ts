/**
 * Auto-configuration: read the server's report, decide what the loop should be.
 *
 * This is the half of the probe that *thinks*. `src/health.ts` fetches; nothing
 * here does. `deriveFromHealth` is pure — same payload, same decision, no clock,
 * no network, no env — so the whole mapping is testable against a captured
 * payload, and a server contract that changes fails in the suite instead of at
 * 02:00 inside a ticket.
 *
 ## The one judgement call in this module
 *
 * When the report and `models.json` disagree, who wins? The answer is split by
 * *kind*, and the split is the thing to understand before changing anything:
 *
 * - **Facts about the server** — the context window, the output cap, whether
 *   `reasoning_effort` exists on the wire, whether the box is busy. The server
 *   wins. A declared `contextWindow` larger than the engine's is a 400 waiting
 *   at the wall, and "this endpoint accepts `reasoning_effort`" is not an
 *   opinion. Every such adoption goes into `notes` with the field it came from,
 *   so the terminal shows what the machine decided.
 * - **Choices the operator made** — `preserve_thinking`, which thinking level a
 *   pass runs at, the run budget. The operator wins, always. The derivation only
 *   *adds* what is missing, and reports what it could not honour.
 *
 * And one rule over both: **unknown is never zero.** A field the report does not
 * carry produces no value and no claim. Deriving `contextWindow: 0` from a
 * missing field would make every later guard pass while the request failed — a
 * guard that cannot fail is a guard that cannot protect you.
 *
 ## What a report cannot decide
 *
 * Ceilings, not strategy. The window says how big a ticket *may* be; it says
 * nothing about whether this one fits, and it never should — that question is
 * what the loop's own context measurement and the split pass are for. This
 * module removes the guesswork about the machine so the remaining question is
 * the honest one about the work.
 */
import type {
  ChatTemplateKwargValue,
  Model,
  ModelThinkingLevel,
  OpenAICompletionsCompat,
  ThinkingLevelMap,
  ThinkingTokenBudgetField,
} from "@earendil-works/pi-ai";

import { UNWORKABLE_OUTPUT_TOKENS } from "./context.ts";
import type { ServerHealth } from "./health.ts";

/**
 * pi's thinking levels, cheapest first. A `Record` over the union rather than a
 * list, so adding a level upstream makes this object stop compiling — the same
 * trick `src/agent.ts` uses for the same reason.
 */
const PI_LEVEL_RANK: Record<ModelThinkingLevel, number> = {
  off: 0,
  minimal: 1,
  low: 2,
  medium: 3,
  high: 4,
  xhigh: 5,
  max: 6,
};

const PI_LEVELS = Object.keys(PI_LEVEL_RANK) as ModelThinkingLevel[];
const PI_EFFORT_LEVELS = PI_LEVELS.filter((level) => level !== "off");

/**
 * What a server calls "do not think". It is not an effort level, so it is kept
 * out of the effort ranking: letting it be the nearest-below fallback for
 * `minimal` would turn thinking *off* when thinking lightly was asked for.
 */
const OFF_NAMES: readonly string[] = ["none", "off"];

/**
 * pi's names for "cap the reasoning tokens", in preference order. The union is
 * closed at three; a server that caps reasoning with some other name cannot be
 * addressed from here, and says so in the notes rather than being ignored.
 */
const BUDGET_FIELD_PREFERENCE: readonly ThinkingTokenBudgetField[] = [
  "thinking_token_budget",
  "thinking_budget",
  "thinking_budget_tokens",
];

// ── the shapes ──────────────────────────────────────────────────────────────

/**
 * The declared side of the comparison: what `models.json` (or pi's settings)
 * already claims. A subset of `Model`, so a resolved model passes straight in.
 */
export interface DeclaredLimits {
  readonly modelId?: string;
  readonly contextWindow?: number;
  readonly maxTokens?: number;
  readonly thinkingLevelMap?: ThinkingLevelMap;
}

/** What to change on a resolved `Model`, as data. See {@link applyModelPatch}. */
export interface DerivedModelPatch {
  /** The window the server reports, when it reports one. */
  readonly contextWindow?: number;
  /**
   * Hard ceiling on one response's token budget. Carried as a *cap*, not a
   * value: the number applied is `min(declared, cap)`, computed against the
   * model in hand. Deriving a value here would silently rewrite the operator's
   * declared budget — replacing 16384 with the server's 8192 default, say —
   * which is exactly the kind of change this module must not make quietly.
   */
  readonly maxTokensCap?: number;
  /** What the server accepts as input. `["text"]` when vision is off. */
  readonly input?: readonly ("text" | "image")[];
  /** pi level → the effort name this server accepts. */
  readonly thinkingLevelMap?: ThinkingLevelMap;
  /** Wire-shape overrides. Only applied to an `openai-completions` model. */
  readonly compat?: Partial<OpenAICompletionsCompat>;
  /**
   * pi levels with a real path to this server. A level missing here has no
   * effect whatever the loop asks for — the sentence the loop could not produce
   * before this module existed.
   */
  readonly availableLevels?: readonly ModelThinkingLevel[];
  /** The model the report describes, for the mismatch check. */
  readonly reportModel?: string;
}

/** Whether the box has room for another request right now. */
export interface CapacityReading {
  readonly busy?: boolean;
  readonly slots?: number;
  readonly queued?: number;
  readonly inFlight?: number;
  readonly busyForS?: number;
  /** True when a pass may start immediately. Unknown fields read as open. */
  readonly open: boolean;
  /** One line saying why, either way. */
  readonly why: string;
  /** True when the report said nothing, so `open` is an assumption, not a fact. */
  readonly unknown: boolean;
}

export interface DerivedConfig {
  readonly modelPatch: DerivedModelPatch;
  /** The server's answer-room rule, parsed. Undefined when unreadable or absent. */
  readonly answerRoom?: AnswerRoomRule;
  /** The rule in the report's own words, for the startup line. */
  readonly answerRoomText?: string;
  readonly capacity: CapacityReading;
  /** Adopted values, each naming the report field it came from. */
  readonly notes: readonly string[];
  /** Clamps and things that cannot work — loud, but not fatal. */
  readonly warnings: readonly string[];
  /** Reasons not to start a run at all. */
  readonly blockers: readonly string[];
}

/** Mutable build-side of {@link DerivedModelPatch}; the exported shape stays frozen. */
type MutableModelPatch = { -readonly [K in keyof DerivedModelPatch]?: DerivedModelPatch[K] };

function positive(value: number | undefined): number | undefined {
  return value !== undefined && Number.isFinite(value) && value > 0 ? value : undefined;
}

// ── thinking ────────────────────────────────────────────────────────────────

/**
 * Rank the effort names this server lists on pi's scale.
 *
 * Unrecognised names are returned separately rather than being ranked: placing a
 * name never seen before above `xhigh` because it appeared late in the array
 * would be a guess with consequences.
 */
function rankAdvertisedEfforts(values: readonly string[]): {
  known: Map<string, number>;
  unknown: readonly string[];
  offName: string | undefined;
} {
  const known = new Map<string, number>();
  const unknown: string[] = [];
  let offName: string | undefined;
  for (const value of values) {
    const name = value.trim();
    if (name === "") continue;
    const lower = name.toLowerCase();
    if (OFF_NAMES.includes(lower)) {
      offName ??= name;
      continue;
    }
    const rank = PI_LEVEL_RANK[lower as ModelThinkingLevel];
    if (rank === undefined) unknown.push(name);
    else known.set(lower, rank);
  }
  return { known, unknown, offName };
}

/**
 * Map every pi level onto something this server accepts.
 *
 * `minimal` requested and not offered → the next one *up* the scale, which
 * over-thinks rather than under-thinks: over-thinking a ticket is a slower run,
 * under-thinking it is a worse one, and the note says which happened either way.
 * A level with nothing above it falls back to the nearest below.
 */
export function deriveThinkingLevelMap(
  advertised: readonly string[] | undefined,
): {
  map: ThinkingLevelMap | undefined;
  available: readonly ModelThinkingLevel[];
  notes: string[];
} {
  const notes: string[] = [];
  if (advertised === undefined || advertised.length === 0) {
    return { map: undefined, available: [], notes: ["no thinking efforts are advertised"] };
  }
  const { known, unknown, offName } = rankAdvertisedEfforts(advertised);
  if (unknown.length > 0) {
    notes.push(
      `effort names this loop cannot rank and will not route to: ${unknown.join(", ")} ` +
        "(advertised still, but with no pi level to answer to)",
    );
  }
  if (known.size === 0) {
    return {
      map: undefined,
      available: [],
      notes: [`none of the advertised efforts (${advertised.join(", ")}) are known to pi`],
    };
  }

  const ascending = [...known.entries()].sort((a, b) => a[1] - b[1]);
  const map: ThinkingLevelMap = {};
  const available: ModelThinkingLevel[] = [];

  if (offName !== undefined) {
    map.off = offName;
    available.push("off");
  } else {
    notes.push(
      'this server offers no "none"/"off" effort, so thinking cannot be switched off from the client',
    );
  }

  for (const level of PI_EFFORT_LEVELS) {
    if (known.has(level)) {
      map[level] = level;
      available.push(level);
      continue;
    }
    const wanted = PI_LEVEL_RANK[level];
    const upward = ascending.find(([, rank]) => rank >= wanted);
    const fallback = upward ?? ascending[ascending.length - 1];
    if (fallback === undefined) continue;
    const [name, rank] = fallback;
    map[level] = name;
    available.push(level);
    notes.push(
      `this server has no "${level}" effort; ${level} routes to "${name}" ` +
        `(nearest available on the scale pi knows, rank ${rank} for ${wanted})`,
    );
  }

  return { map, available, notes };
}

/**
 * Pick which top-level field pi caps reasoning with, from what the server takes.
 *
 * The interesting case is the one that returns a note: a server that caps
 * reasoning with a field pi cannot name. Better to print that than to let a run
 * carry a budget field that is quietly dropped.
 */
export function pickThinkingBudgetField(health: ServerHealth): {
  field: ThinkingTokenBudgetField | undefined;
  notes: string[];
} {
  const accepted = new Set<string>([
    ...(health.supported ?? []),
    ...(health.thinking_control_aliases ?? []),
  ]);
  const notes: string[] = [];
  const field = BUDGET_FIELD_PREFERENCE.find((candidate) => accepted.has(candidate));
  if (field === undefined) {
    notes.push(
      "no reasoning-budget field this loop can send is advertised; thinking size " +
        "cannot be capped from the client",
    );
    return { field: undefined, notes };
  }
  const otherNames = [...accepted]
    .filter(
      (name) =>
        name.includes("thinking") &&
        name.includes("token") &&
        !BUDGET_FIELD_PREFERENCE.includes(name as ThinkingTokenBudgetField),
    )
    .sort();
  if (otherNames.length > 0) {
    notes.push(
      `capping reasoning through \`${field}\`; this server also names ${otherNames.join(", ")}, ` +
        "which pi's request builder cannot address",
    );
  }
  return { field, notes };
}

/** The parsed form of the server's answer-room rule. */
export interface AnswerRoomRule {
  readonly floorTokens: number;
  readonly percentOfMaxTokens: number;
}

const ANSWER_ROOM_RE = /^max\(\s*(\d+)\s*,\s*(\d+(?:\.\d+)?)\s*%\s*of\s*max_tokens\s*\)$/i;

/**
 * Parse the server's answer-room rule.
 *
 * `"max(1024, 15% of max_tokens)"` is prose doing a config field's job.
 * Parsing it is legitimate; defaulting it silently is not. A shape this cannot
 * read returns `null` and the caller keeps its own constant and says so — a
 * reworded rule that quietly moved the loop's stop point is exactly the drift
 * this module exists to catch.
 */
export function parseAnswerRoom(text: string | undefined): AnswerRoomRule | null {
  if (text === undefined) return null;
  const trimmed = text.trim();
  const matched = ANSWER_ROOM_RE.exec(trimmed);
  if (matched !== null) {
    return {
      floorTokens: Number(matched[1]),
      percentOfMaxTokens: Number(matched[2]),
    };
  }
  // A bare number is the same rule with no percentage term.
  if (/^\d+$/.test(trimmed)) {
    return { floorTokens: Number(trimmed), percentOfMaxTokens: 0 };
  }
  return null;
}

/**
 * The floor under "worth asking", for the budget the model will actually be
 * granted.
 *
 * `max(our floor, the server's)`: `UNWORKABLE_OUTPUT_TOKENS` is this loop's
 * judgment about what a verdict costs; the server's rule is about what it will
 * let an answer be. Taking the largest stops the run wherever the binding
 * constraint actually binds.
 *
 * The *granted* budget is a parameter rather than a field because the answer-room
 * rule is a percentage of it, and the honest number is the one on the model that
 * is about to be asked — after the cap has been applied, not before.
 */
export function answerRoomFloor(rule: AnswerRoomRule, grantedMaxTokens: number): number {
  const fromPercent = Math.ceil((rule.percentOfMaxTokens / 100) * grantedMaxTokens);
  return Math.max(UNWORKABLE_OUTPUT_TOKENS, rule.floorTokens, fromPercent);
}

// ── capacity ────────────────────────────────────────────────────────────────

/**
 * Is there room on the box for another pass?
 *
 * Unknown reads as open. The asymmetry is the whole reason: blocking on missing
 * data turns a reporting gap into downtime, while proceeding on missing data
 * costs nothing beyond what an unprobed loop already cost.
 */
export function readCapacity(health: ServerHealth): CapacityReading {
  const busy = health.busy;
  const slots = health.slots;
  const queued = health.queued;
  const inFlight = health.in_flight;
  const busyForS = health.busy_for_s;
  const unknown =
    busy === undefined && slots === undefined && queued === undefined && inFlight === undefined;
  if (unknown) {
    return {
      busy,
      slots,
      queued,
      inFlight,
      busyForS,
      open: true,
      unknown: true,
      why: "the report says nothing about load; assuming the box is reachable",
    };
  }
  const queuedClear = (queued ?? 0) === 0;
  const slotsFree = (inFlight ?? 0) < (slots ?? 1);
  const open = queuedClear && slotsFree;
  const reasons: string[] = [];
  if (!queuedClear) reasons.push(`${queued} request(s) already queued ahead of us`);
  if (!slotsFree) reasons.push(`${inFlight} in flight against ${slots ?? 1} slot(s)`);
  if (open && busy === true && busyForS !== undefined) {
    return {
      busy,
      slots,
      queued,
      inFlight,
      busyForS,
      open,
      unknown: false,
      why:
        `room to join (${inFlight ?? 0}/${slots ?? 1} slots, nothing queued) ` +
        `though busy for ${busyForS}s`,
    };
  }
  return {
    busy,
    slots,
    queued,
    inFlight,
    busyForS,
    open,
    unknown: false,
    why: open
      ? `room to start (${inFlight ?? 0}/${slots ?? 1} slots, ${queued ?? 0} queued)`
      : reasons.join("; "),
  };
}

// ── the derivation ──────────────────────────────────────────────────────────

/**
 * Derive everything the loop can configure from the report. Pure; never throws.
 *
 * `declared` is the model the loop was going to use anyway. Passing it is what
 * makes the derivation a *comparison* instead of a list of facts: the cap only
 * means something against the budget that was declared, and a report describing
 * a different model than the configured one is a config mistake that looks
 * exactly like a model mistake.
 */
export function deriveFromHealth(
  health: ServerHealth,
  declared: DeclaredLimits = {},
): DerivedConfig {
  const notes: string[] = [];
  const warnings: string[] = [];
  const blockers: string[] = [];
  const patch: MutableModelPatch = {};

  if (health.model !== undefined) patch.reportModel = health.model;

  // ── room ────────────────────────────────────────────────────────────────
  const contextWindow = positive(health.context);
  const slotCtx = positive(health.slot_ctx);
  const pool = positive(health.kv_pool_positions);
  const slots = positive(health.slots);
  if (contextWindow !== undefined) {
    patch.contextWindow = contextWindow;
    const declaredWindow = positive(declared.contextWindow);
    if (declaredWindow !== undefined && declaredWindow !== contextWindow) {
      notes.push(
        `context window ${contextWindow} tokens; models.json claimed ${declaredWindow} ` +
          "(the server's number is the one that binds)",
      );
    } else {
      notes.push(`context window ${contextWindow} tokens (report: context)`);
    }
  } else {
    notes.push("no context window in the report; the declared one stands");
  }
  // A slot reporting the whole pool means the pool is *shared*, not divided.
  // One ticket at a time, that is free. Two, and they contend for it.
  if (contextWindow !== undefined && slots !== undefined && slots > 1 && pool !== undefined) {
    const needed = contextWindow * slots;
    if (pool < needed) {
      warnings.push(
        `${slots} slots of ${contextWindow} tokens need ${needed} KV positions but the pool holds ` +
          `${pool}: one pass at a time is fine, but ${slots} at once make the honest window about ` +
          `${Math.floor(pool / slots)} tokens each — do not parallelise ticket work on the declared window`,
      );
    }
  }
  if (slotCtx !== undefined && contextWindow !== undefined && slotCtx < contextWindow) {
    notes.push(`per-slot context is ${slotCtx}, smaller than the ${contextWindow} the box reports`);
  }

  const cap = positive(health.max_tokens_cap);
  const declaredMaxTokens = positive(declared.maxTokens);
  if (cap !== undefined) {
    // Carried as a cap, never as a value. Setting `patch.maxTokens` from the
    // report alone would silently rewrite the operator's budget — turning a
    // declared 16384 into the server's 8192 default, say — which is the exact
    // class of change this module must never make quietly.
    patch.maxTokensCap = cap;
    if (declaredMaxTokens === undefined) {
      notes.push(`no declared maxTokens in scope; the ${cap} cap will bound whatever budget is granted`);
    } else if (declaredMaxTokens > cap) {
      warnings.push(
        `the declared maxTokens (${declaredMaxTokens}) is over the server's cap (${cap}); clamping to ` +
          `${cap}. A larger request is a 400, not a longer answer`,
      );
    } else {
      notes.push(`the declared maxTokens (${declaredMaxTokens}) is inside the server's cap (${cap})`);
    }
  }

  // ── vision ──────────────────────────────────────────────────────────────
  if (health.vision?.enabled === true) {
    patch.input = ["text", "image"];
    notes.push("vision is on: image input is accepted");
  } else if (health.vision !== undefined) {
    patch.input = ["text"];
    const because = health.vision.disabled_because?.[0];
    notes.push(`vision is off, input is text only${because === undefined ? "" : ` (${because})`}`);
  }

  // ── thinking ────────────────────────────────────────────────────────────
  const acceptsEffort =
    (health.supported?.includes("reasoning_effort") ?? false) ||
    (health.chat_template_kwargs?.includes("reasoning_effort") ?? false);
  const usesChatTemplate = (health.chat_template_kwargs?.length ?? 0) > 0;

  if (!acceptsEffort) {
    warnings.push(
      "this server does not advertise reasoning_effort, so the loop's thinking level cannot " +
        "reach the wire: every pass runs at the server's own default" +
        `${health.reasoning_effort_default === undefined ? "" : ` (${health.reasoning_effort_default})`}. ` +
        "LOOP_WORK_THINKING and LOOP_SPLIT_THINKING will change nothing but the log",
    );
  } else {
    const { map, available, notes: levelNotes } = deriveThinkingLevelMap(health.reasoning_effort_values);
    notes.push(...levelNotes);
    if (map !== undefined && available.length > 0) {
      patch.thinkingLevelMap = map;
      patch.availableLevels = available;
    }
  }

  const compat: Partial<OpenAICompletionsCompat> = {};
  if (acceptsEffort) {
    // A fact about the wire, not a choice: a declared `false` is overridden, and
    // said aloud. Without this the level is validated, logged, and dropped.
    compat.supportsReasoningEffort = true;
  }
  if (acceptsEffort && usesChatTemplate) {
    // The only shape that carries effort *and* the thinking toggle through the
    // chat template. Which keys are added is checked against what is advertised;
    // keys the operator already declared survive — see `applyModelPatch`.
    const kwargs: Record<string, ChatTemplateKwargValue> = {
      enable_thinking: { $var: "thinking.enabled" },
      reasoning_effort: { $var: "thinking.effort" },
    };
    const advertisedKwargs = new Set(health.chat_template_kwargs ?? []);
    const sent: string[] = [];
    for (const key of Object.keys(kwargs)) {
      if (advertisedKwargs.has(key)) sent.push(key);
      else {
        delete kwargs[key];
        warnings.push(`chat_template_kwargs.${key} is not advertised as honoured, so it is not sent`);
      }
    }
    if (sent.length > 0) {
      compat.thinkingFormat = "chat-template";
      compat.chatTemplateKwargs = kwargs;
      notes.push(`thinking travels as chat_template_kwargs: ${sent.sort().join(", ")}`);
    }
  } else if (acceptsEffort) {
    notes.push("thinking travels as top-level reasoning_effort (no chat-template kwargs reported)");
  }

  const budget = pickThinkingBudgetField(health);
  notes.push(...budget.notes);
  if (budget.field !== undefined) {
    compat.thinkingTokenBudgetField = budget.field;
    if (health.token_budget_covers_reasoning === true) {
      notes.push(
        `reasoning shares the response budget with the answer, so the cap rides on \`${budget.field}\` ` +
          "— and only rides when one is configured: without `thinkingBudgets` the thinking phase is " +
          "uncapped and the answer-room rule is the only guard on it",
      );
    }
  }

  if (Object.keys(compat).length > 0) patch.compat = compat;

  if (health.tool_calls?.forced_call_disables_thinking === true) {
    notes.push(
      'a forced tool call skips thinking here, so the loop must never send tool_choice:"required" ' +
        "for a verdict call — the report would come back unreasoned",
    );
  }

  const ignored = health.accepted_but_ignored;
  if (ignored !== undefined && ignored.length > 0) {
    notes.push(`fields this server accepts and ignores: ${[...ignored].sort().join(", ")}`);
  }

  // ── two machines answering for one run ──────────────────────────────────
  if (
    declared.modelId !== undefined &&
    health.model !== undefined &&
    declared.modelId !== health.model
  ) {
    warnings.push(
      `the configured model is "${declared.modelId}" but the box reports "${health.model}"; these ` +
        "are not the same model, and every limit derived here belongs to the second one",
    );
  }

  // ── blockers, capacity, floor ───────────────────────────────────────────
  if (health.engine?.responds === false) {
    blockers.push(
      "the API answers but the engine behind it does not: a pass would fail on its first request",
    );
  }
  if (health.version?.match === false) {
    warnings.push(
      `the API (${health.version.api ?? "?"}) and the engine (${health.version.engine ?? "?"}) report ` +
        "different versions; this report may not describe the binary actually serving requests",
    );
  }
  if (health.status !== undefined && health.status !== "ok") {
    warnings.push(`the health report says status: ${health.status}`);
  }

  const capacity = readCapacity(health);
  const parsedRoom = parseAnswerRoom(health.thinking_answer_room);
  const answerRoom: AnswerRoomRule | undefined = parsedRoom ?? undefined;
  if (answerRoom === undefined) {
    notes.push(
      health.thinking_answer_room === undefined
        ? "no answer-room rule in the report; this loop's own floor " +
            `(${UNWORKABLE_OUTPUT_TOKENS} tokens) stands`
        : `the answer-room rule "${health.thinking_answer_room}" is not a shape this loop can read; ` +
            `keeping ${UNWORKABLE_OUTPUT_TOKENS} tokens`,
    );
  } else {
    notes.push(
      `answer room max(${answerRoom.floorTokens}, ${answerRoom.percentOfMaxTokens}% of the granted ` +
        "budget): a pass stops as context-exhausted below that",
    );
  }

  return {
    modelPatch: patch,
    answerRoom,
    answerRoomText:
      answerRoom === undefined
        ? undefined
        : `max(${answerRoom.floorTokens}, ${answerRoom.percentOfMaxTokens}% of granted)`,
    capacity,
    notes,
    warnings,
    blockers,
  };
}

// ── applying it ─────────────────────────────────────────────────────────────

/**
 * Merge the derived patch onto a resolved model. Pure: a new object, the input
 * model untouched.
 *
 * Deliberately a shallow-but-careful merge rather than a replacement, because
 * the model came out of the operator's `models.json` and carries choices in it.
 * `compat` keys are added, not overwritten; `chatTemplateKwargs` merges
 * key-by-key with the declared keys winning, so a declared `preserve_thinking`
 * survives the derived set.
 */
export function applyModelPatch(model: Model<any>, derived: DerivedConfig): Model<any> {
  const patch = derived.modelPatch;
  const next: Model<any> = { ...model };

  if (patch.contextWindow !== undefined) next.contextWindow = patch.contextWindow;
  if (patch.input !== undefined) next.input = [...patch.input];
  if (patch.maxTokensCap !== undefined) {
    // The clamp happens here, against the model actually in hand, rather than in
    // the derivation: `min(declared, cap)` needs the declared value, and taking
    // the server's number wholesale would overwrite the operator's budget.
    const declared = model.maxTokens > 0 ? model.maxTokens : patch.maxTokensCap;
    const clamped = Math.min(declared, patch.maxTokensCap);
    if (clamped !== model.maxTokens) next.maxTokens = clamped;
  }

  const derivedCompat = patch.compat;
  if (derivedCompat !== undefined) {
    const existingCompat = (model.compat ?? {}) as OpenAICompletionsCompat;
    const merged: OpenAICompletionsCompat = { ...existingCompat, ...derivedCompat };
    const declaredKwargs = existingCompat.chatTemplateKwargs ?? {};
    const incomingKwargs = derivedCompat.chatTemplateKwargs ?? {};
    if (Object.keys(incomingKwargs).length > 0 || Object.keys(declaredKwargs).length > 0) {
      // Declared keys win: they are the operator's choice about the replay, not
      // a fact about the endpoint.
      merged.chatTemplateKwargs = { ...incomingKwargs, ...declaredKwargs };
    }
    next.compat = merged;
  }

  if (patch.thinkingLevelMap !== undefined) {
    // Same rule one level up: a level the operator mapped by hand beats the derived one.
    next.thinkingLevelMap = {
      ...patch.thinkingLevelMap,
      ...(model.thinkingLevelMap ?? {}),
    };
  }

  return next;
}

/**
 * The startup print: one line per decision, each naming where it came from.
 *
 * This is what makes an auto-configured run debuggable. Without it a value that
 * arrived off the network is indistinguishable from one that was typed, and the
 * only way to tell them apart is to read the code that made the choice.
 */
export function formatResolvedProfile(derived: DerivedConfig): readonly string[] {
  const patch = derived.modelPatch;
  const facts: string[] = [];
  if (patch.reportModel !== undefined) facts.push(`model ${patch.reportModel}`);
  if (patch.contextWindow !== undefined) facts.push(`window ${patch.contextWindow}`);
  if (patch.maxTokensCap !== undefined) facts.push(`out cap ${patch.maxTokensCap}`);
  if (patch.input !== undefined) facts.push(`input ${patch.input.join("+")}`);
  if (patch.availableLevels !== undefined) facts.push(`levels ${patch.availableLevels.join("/")}`);
  if (patch.compat?.thinkingFormat !== undefined) facts.push(`thinking as ${patch.compat.thinkingFormat}`);
  if (patch.compat?.thinkingTokenBudgetField !== undefined) {
    facts.push(`budget via ${patch.compat.thinkingTokenBudgetField}`);
  }

  const lines: string[] = [];
  if (facts.length > 0) lines.push(`server profile: ${facts.join(" · ")}`);
  if (derived.answerRoomText !== undefined) lines.push(`answer room: ${derived.answerRoomText}`);
  lines.push(`server capacity: ${derived.capacity.why}`);
  return lines;
}

/** Levels the loop may ask for that this server cannot be given. For config validation. */
export function unreachableLevels(
  derived: DerivedConfig,
  requested: readonly ModelThinkingLevel[],
): readonly ModelThinkingLevel[] {
  const available = derived.modelPatch.availableLevels;
  if (available === undefined) return [];
  return requested.filter((level) => !available.includes(level));
}
