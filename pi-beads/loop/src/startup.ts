/**
 * The startup comparison, wired to the world.
 *
 * `src/audit.ts` decides nothing about files or sockets; this module is the
 * interpreter for it: read `models.json`, work out which provider and model this
 * run is about to use, ask the server what it is, compare the two, render the
 * answer, and optionally write the suggested config somewhere a human can act
 * on.
 *
 * Three properties it is built to keep:
 *
 * 1. **It never takes the run down.** The audit is a courtesy at startup. A
 *    missing file, an unreachable server or a half-written health payload is a
 *    line on the screen, not a crash — unless the operator asked for strict, and
 *    then it is a deliberate stop with a reason rather than a mystery.
 * 2. **It never rewrites the live config by surprise.** Suggestions go to a
 *    sibling file by default. `inplace` exists for an operator who means it, and
 *    it writes a backup first because a config you cannot diff against what was
 *    there before is a config you cannot trust afterwards.
 * 3. **It is injectable end to end.** Reads, writes and the fetch are all
 *    parameters, so the whole startup path is testable without a disk, a server
 *    or a home directory.
 */
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { getAgentDir, SettingsManager } from "@earendil-works/pi-coding-agent";

import {
  applySuggestions,
  auditProvider,
  deriveModelConfigView,
  renderAudit,
  renderProposedConfig,
} from "./audit.ts";
import type { AuditLevels, AuditReport, ConfigTarget, ModelConfigView } from "./audit.ts";
import { healthUrlFor, probeHealth } from "./health.ts";
import type { JsonRecord } from "./health.ts";

/** Where the suggested config goes. `none` means print only. */
export type WriteMode = "none" | "proposed" | "inplace";

export interface StartupAuditOptions {
  readonly cwd: string;
  /** Explicit `--provider/--model`. Wins over anything on disk. */
  readonly modelRef?: { provider: string; id: string };
  /** The user's saved default. Defaults to reading pi's settings. */
  readonly defaultRef?: () => { provider?: string; id?: string } | undefined;
  readonly agentDir?: string;
  /** Defaults to `<agentDir>/models.json`. */
  readonly modelsPath?: string;
  /** Overrides the derived `<base>/health`. */
  readonly healthUrl?: string;
  readonly levels?: AuditLevels;
  readonly toolSchemas?: Readonly<Record<string, unknown>>;
  readonly timeoutMs?: number;
  /** Show the checks that passed as well as the ones that did not. */
  readonly verbose?: boolean;
  readonly writeMode?: WriteMode;
  readonly readFile?: (path: string) => string;
  readonly writeFile?: (path: string, contents: string) => void;
  readonly fileExists?: (path: string) => boolean;
  readonly fetchImpl?: typeof fetch;
  readonly now?: () => number;
}

export interface ResolvedTarget {
  readonly target?: ConfigTarget;
  readonly view?: ModelConfigView;
  /** Why there is no target, for the line the operator sees. */
  readonly note?: string;
  /** What was available, so "which one did you mean" is answerable. */
  readonly candidates: readonly string[];
}

export interface StartupAuditResult {
  readonly status: "audited" | "no-config" | "no-target" | "unreachable" | "failed";
  readonly report?: AuditReport;
  readonly blocking: boolean;
  readonly writtenTo?: string;
  readonly lines: readonly string[];
  /** The patched config, secret-redacted, for printing rather than writing. */
  readonly proposedJson?: string;
}

/** The default model/provider this pi would pick, read the way a session reads it. */
export function readDefaultModelRef(cwd: string, agentDir: string): { provider?: string; id: string } | undefined {
  try {
    const settings = SettingsManager.create(cwd, agentDir);
    const id = settings.getDefaultModel();
    if (id === undefined) return undefined;
    return { provider: settings.getDefaultProvider(), id };
  } catch {
    return undefined;
  }
}

/**
 * Which provider and model to compare, out of what `models.json` holds.
 *
 * Explicit first, then the user's saved default, then — only if there is exactly
 * one possibility — the single entry on disk. Ambiguity is reported with the
 * candidates rather than resolved by "first key wins", because an audit of the
 * wrong model is worse than no audit: its findings read as facts about the model
 * the run is actually using.
 */
export function resolveTarget(
  raw: unknown,
  options: {
    readonly explicit?: { provider: string; id: string };
    readonly fallback?: { provider?: string; id?: string };
  } = {},
): ResolvedTarget {
  const providers = rawRaw(raw);
  const names = Object.keys(providers);
  const candidates = names.flatMap((name) => {
    const models = modelsOf(providers[name]);
    return models.length === 0
      ? [name]
      : models.map((model) => `${name}/${stringAt(model, ["id"]) ?? "?"}`);
  });

  if (names.length === 0) {
    return { note: "`models.json` declares no providers", candidates };
  }

  let provider: string | undefined;
  let modelId: string | undefined;

  if (options.explicit !== undefined) {
    provider = options.explicit.provider;
    modelId = options.explicit.id;
  } else if (options.fallback?.id !== undefined) {
    provider = options.fallback.provider ?? (names.length === 1 ? names[0] : undefined);
    modelId = options.fallback.id;
    if (provider === undefined) {
      return {
        note: `the default model id "${modelId}" names no provider and there is more than one here`,
        candidates,
      };
    }
  } else if (names.length === 1) {
    const [only] = names;
    if (only === undefined) {
      return { note: "could not resolve a provider and model id", candidates };
    }
    provider = only;
    const models = modelsOf(providers[only]);
    if (models.length === 1) modelId = stringAt(models[0], ["id"]);
    else if (models.length === 0) {
      return { note: `provider "${provider}" declares no models`, candidates };
    } else {
      return {
        note: `provider "${provider}" has ${models.length} models and no default was configured`,
        candidates,
      };
    }
  } else {
    return {
      note: `${names.length} providers are configured and no default model was set`,
      candidates,
    };
  }

  if (provider === undefined || modelId === undefined || modelId === "") {
    return { note: "could not resolve a provider and model id", candidates };
  }
  const providerConfig = providers[provider];
  if (!isRecord(providerConfig)) {
    return { note: `provider "${provider}" is not declared in models.json`, candidates };
  }
  const models = modelsOf(providerConfig);
  const index = models.findIndex((model) => stringAt(model, ["id"]) === modelId);
  const modelEntry = index >= 0 ? models[index] : undefined;
  const view = deriveModelConfigView({
    provider,
    providerConfig,
    ...(modelEntry === undefined || !isRecord(modelEntry) ? {} : { modelConfig: modelEntry }),
  });
  return {
    target: { provider, modelId, ...(index >= 0 ? { modelIndex: index } : {}) },
    view,
    candidates,
  };
}

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringAt(source: unknown, path: readonly string[]): string | undefined {
  let current: unknown = source;
  for (const key of path) {
    if (!isRecord(current)) return undefined;
    current = current[key];
  }
  return typeof current === "string" && current !== "" ? current : undefined;
}

function rawRaw(raw: unknown): JsonRecord {
  return isRecord(raw) && isRecord(raw.providers) ? raw.providers : {};
}

function modelsOf(provider: unknown): unknown[] {
  return isRecord(provider) && Array.isArray(provider.models) ? provider.models : [];
}

/** The proposed-config destination for a write mode. */
export function proposedPathFor(modelsPath: string, mode: WriteMode): string {
  if (mode === "inplace") return modelsPath;
  return `${modelsPath}.proposed`;
}

/**
 * Run the startup comparison.
 *
 * Returns rendered lines as well as the structured report: the loop paints them
 * through its presenter and a CLI prints them raw, and neither of them should
 * have to know how the comparison works.
 */
export async function runStartupAudit(options: StartupAuditOptions): Promise<StartupAuditResult> {
  const now = options.now;
  const read = options.readFile ?? ((path: string) => readFileSync(path, "utf8"));

  const write = options.writeFile ?? ((path: string, contents: string) => void writeFileSync(path, contents));
  const exists = options.fileExists ?? ((path: string) => existsSync(path));
  const agentDir = options.agentDir ?? getAgentDir();
  const modelsPath = options.modelsPath ?? join(agentDir, "models.json");
  const mode: WriteMode = options.writeMode ?? "none";
  const timeoutMs = options.timeoutMs;

  const emit = (lines: readonly string[], extra: Partial<StartupAuditResult>): StartupAuditResult => ({
    status: extra.status ?? "audited",
    blocking: extra.blocking ?? false,
    lines,
    ...(extra.report === undefined ? {} : { report: extra.report }),
    ...(extra.writtenTo === undefined ? {} : { writtenTo: extra.writtenTo }),
    ...(extra.proposedJson === undefined ? {} : { proposedJson: extra.proposedJson }),
  });

  let raw: unknown;
  try {
    if (!exists(modelsPath)) {
      return emit(
        [`provider audit — no models.json at ${modelsPath}; nothing to compare`],
        { status: "no-config" },
      );
    }
    raw = JSON.parse(read(modelsPath));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return emit([`provider audit — could not read ${modelsPath}: ${message}`], { status: "no-config" });
  }

  const fallback =
    options.defaultRef !== undefined
      ? options.defaultRef()
      : readDefaultModelRef(options.cwd, agentDir);
  const resolved = resolveTarget(raw, {
    ...(options.modelRef === undefined ? {} : { explicit: options.modelRef }),
    ...(fallback === undefined ? {} : { fallback }),
  });
  if (resolved.view === undefined || resolved.target === undefined) {
    return emit(
      [
        `provider audit — no target: ${resolved.note ?? "unresolved"}`,
        `  available: ${resolved.candidates.length > 0 ? resolved.candidates.join(", ") : "(none)"}`,
        "  pass PI_PROVIDER/PI_MODEL (or set a default model) to pick one",
      ],
      { status: "no-target" },
    );
  }

  const healthUrl = options.healthUrl ?? healthUrlFor(resolved.view.baseUrl);
  if (healthUrl === undefined) {
    return emit(
      [
        `provider audit — cannot derive a health URL from baseUrl "${resolved.view.baseUrl}"`,
        `  the report is expected at the base, not under /v1 — set LOOP_HEALTH_URL if it lives elsewhere`,
      ],
      { status: "unreachable" },
    );
  }

  const probe = await probeHealth({
    url: healthUrl,
    ...(timeoutMs === undefined ? {} : { timeoutMs }),
    ...(options.fetchImpl === undefined ? {} : { fetchImpl: options.fetchImpl }),
    ...(now === undefined ? {} : { now }),
  });

  const report = auditProvider({
    view: resolved.view,
    target: resolved.target,
    healthUrl,
    ...(probe.report === undefined ? {} : { health: probe.report }),
    ...(options.levels === undefined ? {} : { levels: options.levels }),
    ...(options.toolSchemas === undefined ? {} : { toolSchemas: options.toolSchemas }),
  });

  const target = resolved.target;
  const lines = renderAudit(report, {
    showOk: options.verbose,
    probeMs: Math.round(probe.elapsedMs),
  });
  if (probe.error !== undefined) {
    lines[0] = `${lines[0] ?? ""} — probe failed: ${probe.error}`;
  }

  // Printable, secret-free. The file below is not redacted: it is written beside
  // the file that already holds the credential, under the same trust boundary,
  // and a redacted key there would just be a broken config.
  const proposedJson = renderProposedConfig(raw, report, target);

  let writtenTo: string | undefined;
  if (mode !== "none" && report.suggestions.length > 0) {
    const destination = proposedPathFor(modelsPath, mode);
    try {
      if (mode === "inplace") write(`${modelsPath}.bak`, `${JSON.stringify(raw, null, 2)}\n`);
      write(destination, `${JSON.stringify(applySuggestions(raw, report.suggestions, target), null, 2)}\n`);
      writtenTo = destination;
      lines.push(
        mode === "inplace"
          ? `  patched config written to ${destination}; the original is at ${modelsPath}.bak`
          : `  patched config written to ${destination} — review it, then copy it over ${modelsPath}`,
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      lines.push(`  could not write the patched config: ${message}`);
    }
  }

  return emit(
    lines,
    {
      status: probe.ok ? "audited" : "unreachable",
      report,
      blocking: report.blocking,
      proposedJson,
      ...(writtenTo === undefined ? {} : { writtenTo }),
    },
  );
}
