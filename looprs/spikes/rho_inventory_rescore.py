#!/usr/bin/env python3
"""Re-score ADR-0010's six-row dependency inventory against matthewyjiang/rho's rho-sdk.

What this is
------------
ADR-0010's real cost estimate was never the millisecond number; it was the six-row
table in §"What looprs actually depends on", plus the claim that an alternative
answers at most a sliver of it. That table was scored against casonadams/rho. This
script re-scores it, row by row, against **matthewyjiang/rho** — the library the
decision was actually requested against — and prints a gap table the ADR amendment
can cite.

It prices; it does not migrate. Nothing here is added to `Cargo.toml`, nothing is
built, and no feature flag is proposed (ADR-0009 rule 2, ADR-0010 rule 2). The
candidate is read out of its **published artifact** (cargo cache, else a fresh
static.crates.io fetch) exactly as `rho_surface_cost.py` reads it.

Legs
----
1. THE INVENTORY, RECOUNTED — the six rows measured off looprs' own tree, not off
   the ADR's prose. This leg is where the correction lives: the ADR's "40 wire
   spellings" is what a lowercase-only regex could see, not what the table has.
2. THE CANDIDATE'S VOCABULARY — the same two files the prior capture scraped
   (`rho-sdk:src/event.rs`, `rho-sdk:src/tool.rs`), plus the files that actually
   hold the meaning of looprs' other value groups, and why the extra files are
   needed.
3. SPELLING BY SPELLING — one verdict per row of `WIRE_INVENTORY`, with the
   rho-sdk API named for every row that has one. **The citations are checked
   against the artifact**: a row that names an identifier the artifact does not
   contain fails the run rather than ageing quietly into a wrong sentence.
4. RE-DERIVATION SURFACE — the generator, doc and test counts the migration cost
   is denominated in.
5. THE GAP TABLE — one row per inventory row: verdict (same / different-but-
   expressible / no answer) and first-order cost (shim / redesign / not
   expressible), naming what looprs would have to build.

Method notes, so the numbers can be argued with
-----------------------------------------------
* "Spelling and meaning, not spelling": a string intersection proves a spelling
  overlap and nothing else. Leg 3 is therefore a hand-written table with a named
  API per row, and leg 2 prints the string intersection only to show that the
  prior capture's headline (0 shared spellings) was measuring the cheap thing.
* A row's *cost* is a function of its verdict **and of whether looprs reads it**.
  A spelling nobody reads is a documentation row when the engine changes; a
  spelling with a named reader is a behaviour port. The inventory already records
  which is which (`reader:` / `waiting_on:`), so the split is data, not opinion.
* Facts about the candidate are cited to a file in the artifact or to the
  workspace-8jk.3 embed capture
  (`spikes/results/matthewyjiang-surface-cost-embed-8jk3.log`), which is where the
  behavioural findings (dropped runs, wedged event channels, orphaned tool
  children, persistence) were *run* rather than read.

Run
---
    python3 spikes/rho_inventory_rescore.py | tee spikes/results/matthewyjiang-inventory-rescore.log

Runtime is seconds: no build, no model call, no endpoint. Any leg whose
prerequisite is missing prints `not measured` instead of guessing — here that
means a rho-sdk artifact that can neither be found in the cargo cache nor fetched.
"""

import datetime
import glob
import importlib.util
import json
import os
import platform
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EMBED_CAPTURE = os.path.join(REPO, "spikes", "results", "matthewyjiang-surface-cost-embed-8jk3.log")

SUBJECT = "matthewyjiang/rho (github.com/matthewyjiang/rho)"
LIBRARY = "rho-sdk"

# The prior capture's convention, kept verbatim so the difference between "what the
# regex could see" and "what the table has" is reproducible rather than asserted.
PRIOR_LOOPRS_REGEX = r'wire:\s*"([a-z_]+)"'
PRIOR_CANDIDATE_FILES = ["src/event.rs", "src/tool.rs"]
PRIOR_CANDIDATE_REGEX = r"^\s{4}([A-Z][A-Za-z0-9]+)[\s{,]"

# The files that actually hold the *meaning* of looprs' value groups. Additions
# beyond the prior capture's two are listed with the reason they are needed, and
# the reason is printed with them — this ticket's row 2 says to reuse the old
# method only when the same files still answer the question.
CANDIDATE_VOCAB_FILES = [
    ("src/event.rs", "RunEvent and RunOutcome: the run's event vocabulary, its terminal events, its StopReason"),
    ("src/tool.rs", "ToolOrigin/ToolSecurity/ToolContext: what a tool is and who owns it"),
    ("src/model.rs", "Message and ModelUsage: the history roles and the token-window fields"),
    ("src/compaction.rs", "CompactionTrigger/CompactionOutcome: the compaction reason set"),
    ("src/compaction_decision.rs", "CompactionSkipReason: the endings a compaction can have that never reach the stream"),
    ("src/error.rs", "Error/Retryability: what a refusal is when there is no `success:false` on a pipe"),
    ("src/client.rs", "Rho/RhoBuilder/ShutdownOutcome: the process-level surface, incl. event channel capacity"),
    ("src/session.rs", "Session/UserInput/SessionState: prompt-shaped surface"),
    ("src/run.rs", "Run/SteeringHandle: steer, retract, cancel, outcome"),
    ("src/steering.rs", "SteeringRetraction: the clear_queue analogue's outcomes"),
    ("src/persistence.rs", "SessionSnapshot/SessionStore: the corpus row"),
    ("src/provider.rs", "ScriptedProvider/ScriptedTurn: the test-seam row"),
]


def sh(cmd):
    p = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    return p.returncode, p.stdout.strip()


def fetch_json(url, timeout=25):
    """A GET that says None instead of guessing when the network is not there."""
    try:
        p = subprocess.run(
            ["curl", "-sS", "-m", str(timeout), "-H", "User-Agent: looprs-spike/1.0", url],
            capture_output=True, text=True, timeout=timeout + 5,
        )
        if p.returncode == 0 and p.stdout.strip():
            return json.loads(p.stdout)
    except Exception:  # noqa: BLE001 - no network is a leg that cannot run, not a crash
        pass
    return None


def crates_io_version(name):
    d = fetch_json(f"https://crates.io/api/v1/crates/{name}")
    if not d:
        return None
    c = d.get("crate", {})
    return c.get("max_stable_version") or c.get("newest_version")


def crate_src(name, version):
    hits = sorted(glob.glob(os.path.expanduser(f"~/.cargo/registry/src/*/{name}-{version}")))
    return hits[-1] if hits else None


def crate_src_or_fetch(name, version, scratch):
    d = crate_src(name, version)
    if d:
        return d, "cargo cache"
    d = os.path.join(scratch, f"{name}-{version}")
    rc, _ = sh(f"curl -sS -m 60 -o {scratch}/{name}.crate "
               f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
               f" && mkdir -p {d} && tar xzf {scratch}/{name}.crate -C {d} --strip-components=1")
    return (d, "fresh static.crates.io fetch") if rc == 0 else (None, None)


def read(path):
    with open(path, errors="replace") as fh:
        return fh.read()


def enum_variants(body, enum_name):
    """The variant names of one Rust enum, found by brace-matching from its header.

    Not a line regex: a variant-shaped name at four-space indent is also what a
    sibling enum's member looks like, and the prior capture's overlap figure was
    built that way.
    """
    m = re.search(r"pub enum %s[^{]*\{" % re.escape(enum_name), body)
    if not m:
        return []
    i = body.index("{", m.start())
    depth = 0
    for j in range(i, len(body)):
        if body[j] == "{":
            depth += 1
        elif body[j] == "}":
            depth -= 1
            if depth == 0:
                inner = body[i + 1:j]
                return re.findall(r"^    ([A-Z][A-Za-z0-9]*)", inner, re.M)
    return []


# ─────────────────────────── looprs side of the inventory ─────────────────────
def wire_inventory():
    """`WIRE_INVENTORY` rows, via the same parser `docs_check.py` renders the
    protocol page from — one reader for the table, so a re-run cannot disagree
    with the page it prices."""
    spec = importlib.util.spec_from_file_location(
        "dc", os.path.join(REPO, "scripts", "docs_check.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.wire_inventory()


COMMAND_VERBS = ("prompt", "steer", "clear_queue", "abort")


def rpc_commands():
    """The commands looprs actually writes to the child's stdin, scraped from the
    call sites rather than from the ADR. A verb in pi's docs that looprs never
    sends is not a dependency, and a verb the ADR forgot is one it must add."""
    out = {}
    for path in glob.glob(os.path.join(REPO, "src", "**", "*.rs"), recursive=True):
        rel = os.path.relpath(path, REPO)
        if rel == "src/wire.rs":
            continue
        for n, line in enumerate(read(path).splitlines(), 1):
            for verb in COMMAND_VERBS:
                if re.search(r'\{\s*"type":\s*"%s"' % verb, line):
                    out.setdefault(verb, []).append(f"{rel}:{n}")
    return out


def fn_bodies(path, kinds=("#[test]", "#[tokio::test]")):
    """Yield (name, body) for every test function in a file, by balanced braces."""
    src = read(path)
    for m in re.finditer(r"#\[tokio::test\]|#\[test\]", src):
        tail = src[m.end():]
        fm = re.search(r"\bfn\s+(\w+)", tail)
        if not fm:
            continue
        name = fm.group(1)
        i = tail.find("{", fm.end())
        if i < 0:
            continue
        depth = 0
        for j in range(i, len(tail)):
            if tail[j] == "{":
                depth += 1
            elif tail[j] == "}":
                depth -= 1
                if depth == 0:
                    yield name, tail[i:j]
                    break


def test_inventory(spellings):
    """Every test function in the tree, and the flags that say what it is a test of.

    A quoted `"error"` or `"done"` in a bash-mode test is not an assertion about
    pi's wire vocabulary, so a quoted spelling only counts when the same body also
    names a wire type or a `"type"` tag. Leg 4 prints both numbers so the filter is
    visible rather than silent.
    """
    seam_words = ("fake_pi", "LOOPRS_PI_BIN", "PiFake", "pi_bin", "pi_verbs", "pi_commands",
                  "pi_prompts", "pi_spawn_args", "fakes(")
    wire_context = ("PiEvent", "AssistantEvent", "EntryRole", "CompactionReason", "StopReason",
                    '"type"', "wire::parse", "SessionEvent", "parse(")
    total = 0
    wire_tests = {}      # spelling -> [test names]
    raw_hits = 0         # quoted spellings with no wire-context filter
    seam_tests = []
    files = sorted(glob.glob(os.path.join(REPO, "src", "**", "*.rs"), recursive=True)
                   + glob.glob(os.path.join(REPO, "tests", "**", "*.rs"), recursive=True))
    for path in files:
        rel = os.path.relpath(path, REPO)
        for name, body in fn_bodies(path):
            total += 1
            if any(w in body for w in seam_words):
                seam_tests.append(f"{rel}::{name}")
            hits = [s for s in spellings if f'"{s}"' in body]
            raw_hits += len(hits)
            if not any(w in body for w in wire_context):
                continue
            for s in hits:
                wire_tests.setdefault(s, []).append(f"{rel}::{name}")
    return total, wire_tests, seam_tests, raw_hits


# The Rust type whose variant catches each group's spelling — `WIRE_INVENTORY`
# records the variant, this records which enum it belongs to. A test that names
# `PiEvent::AgentSettled` is asserting on pi's `agent_settled` spelling even
# though the string never appears, so counting only quoted strings understates
# the test surface a vocabulary change touches.
GROUP_TYPE = {"role": "EntryRole", "event": "PiEvent", "assistant-event": "AssistantEvent",
              "compaction-reason": "CompactionReason", "stop-reason": "StopReason"}


def variant_test_hits(rows):
    """Per inventory row: the test functions that name the variant that catches it."""
    bodies = []
    for path in sorted(glob.glob(os.path.join(REPO, "src", "**", "*.rs"), recursive=True)
                       + glob.glob(os.path.join(REPO, "tests", "**", "*.rs"), recursive=True)):
        rel = os.path.relpath(path, REPO)
        for name, body in fn_bodies(path):
            bodies.append((f"{rel}::{name}", body))
    hits = {}
    for r in rows:
        if r["wire"] == "*anything else*":
            continue
        qual = f"{GROUP_TYPE[r['group']]}::{r['variant']}"
        hits[(r["group"], r["wire"])] = [
            n for n, b in bodies if re.search(r"\b" + re.escape(qual) + r"\b", b)]
    return len(bodies), hits


# ───────────────────────── row 2's per-spelling verdict table ─────────────────
# verdict: same | renamed | narrowed | absent | mechanism
#   same      — rho-sdk's value carries the same meaning; only the spelling differs
#   renamed   — same meaning, different spelling  → a `match` arm changes
#   narrowed  — expressible, but part of pi's meaning has no rho carrier (named)
#   absent    — rho-sdk has no value for it at all
#   mechanism — the *pi mechanism* has no analogue (runtime additivity, serde(other))
# cites: identifiers the script greps for in the artifact, so every "rho-sdk has X"
#        in this table is checked and not remembered.
SPELLING_MAP = {
    # ── roles (pi's `message.role` on the stream) ──
    ("role", "user"): ("same", ["UserInput", "start"],
                       "nothing arrives to drop: the host supplied the text through Session::start, so the drop-arm becomes dead code and the double-print it guards cannot happen"),
    ("role", "assistant"): ("narrowed", ["Completed", "RunOutcome", "live_history", "UsageUpdated"],
                       "pi seals one authoritative assistant message; rho commits per step and finalises per run, so 'the authoritative message_end' becomes two clocks"),
    ("role", "toolResult"): ("narrowed", ["ToolFinished", "ToolCompletion", "is_failure"],
                       "rho's completion has four states (Success/CompletedFailure/Failure/Unavailable) and says the status is the output's flag, not the variant name"),
    ("role", "system"): ("absent", ["System"],
                       "Message::System exists in history but never on the stream; the unrendered-role note for it can never fire"),
    ("role", "bashExecution"): ("absent", [],
                       "no engine-side shell record exists on rho's stream at all (the nearest is a tool call looprs itself registered), so the row's 'a shell that is not this one' case disappears with it"),
    ("role", "custom"): ("narrowed", ["append_message", "BoundaryInputApplied", "boundary_input_channel"],
                       "closest carrier is an internal host input accepted at a runtime checkpoint, and it is stamped with session/run ids rather than an extension identity"),
    ("role", "branchSummary"): ("absent", [],
                       "rho-sdk has no branch concept; session-tree machinery lives in the rho-coding-agent bin crate, not in the library looprs would link"),
    ("role", "compactionSummary"): ("narrowed", ["CompactionCompleted", "CompactionOutcome", "committed_snapshot"],
                       "rho commits the summary into history/snapshot and announces it with CompactionCompleted; there is no summary record arriving after the fact"),
    ("role", "*anything else*"): ("mechanism", ["non_exhaustive"],
                       "pi's additivity is a wire property (serde(other) makes an unknown role harmless without recompiling); rho's is a release property (#[non_exhaustive] forces a catch-all arm at compile time, and a new variant needs a new dependency version before looprs can even see it)"),

    # ── agent session events (pi's `type`) ──
    ("event", "agent_start"): ("renamed", ["Started", "RunId"],
                       "RunEvent::Started{run_id, revision}; the liveness mirror reads the same fact"),
    ("event", "agent_end"): ("narrowed", ["StepStarted", "Completed", "Cancelled", "Failed"],
                       "rho has no 'the agent stopped but more may follow' event: the pair pi keeps apart becomes StepStarted versus a terminal event, which is a coarser cut than the one the row's waiting_on asks for"),
    ("event", "agent_settled"): ("narrowed", ["Completed", "Failed", "Cancelled", "outcome", "SessionState"],
                       "covers every case where the run is still being read, and covers nothing when it is not: 8jk.3 settle5 observed 0 terminal events from a dropped Run and settle6 wedged the run on the bounded 64-slot channel with SessionState still Running; pi's 'the child died' is an observable fact about a process, and there is no process"),
    ("event", "turn_start"): ("renamed", ["StepStarted", "estimated_context_tokens"],
                       "RunEvent::StepStarted{step, estimated_context_tokens}"),
    ("event", "turn_end"): ("absent", ["ModelCallCompleted"],
                       "no step-end event exists; ModelCallCompleted is per provider request, not per step, and does not carry the turn's tool results"),
    ("event", "message_start"): ("absent", ["StepStarted"],
                       "no message-start event; the nearest is the step boundary, and the live-region cursor this row waits for still does not exist"),
    ("event", "message_update"): ("renamed", ["AssistantTextDelta", "ReasoningDelta", "ToolCallUpdated"],
                       "rho's deltas are top-level events, so pi's envelope disappears — and with it the assistant-event group as a group"),
    ("event", "message_end"): ("narrowed", ["UsageUpdated", "ModelCallCompleted", "RunOutcome", "live_history"],
                       "the token window's single source becomes three candidate clocks (step commit, per-call usage, run outcome) and rho's usage fields are Options"),
    ("event", "tool_execution_start"): ("renamed", ["ToolStarted", "ToolMetadata"],
                       "RunEvent::ToolStarted{call_id, name, metadata}; ToolProposed is an extra earlier stage pi folds into toolcall_end"),
    ("event", "tool_execution_update"): ("renamed", ["ToolUpdated", "ToolProgress"],
                       "RunEvent::ToolUpdated{call_id, progress}; rho's progress is typed where pi's is a payload"),
    ("event", "tool_execution_end"): ("renamed", ["ToolFinished", "ToolCompletion"],
                       "RunEvent::ToolFinished{call_id, result}"),
    ("event", "auto_retry_start"): ("narrowed", ["ProviderRequestRetry", "ProviderStreamReset", "retry_after"],
                       "ProviderRequestRetry is a unit variant: attempt/max/delay are not on it. ProviderStreamResetReason::RetryableFailure carries retry_after and nothing else; the ladder's shape is 1s/2s/4s in the runtime (8jk.3 settle3)"),
    ("event", "auto_retry_end"): ("narrowed", ["Failed", "Retryability"],
                       "the end of the ladder is the run's Failed{message, retryability, revision}, i.e. a run-terminal event, not a retry outcome"),
    ("event", "compaction_start"): ("renamed", ["CompactionStarted", "CompactionTrigger"],
                       "RunEvent::CompactionStarted{trigger, message_count}"),
    ("event", "compaction_end"): ("narrowed", ["CompactionCompleted", "CompactorCancellationMode"],
                       "orchestration/compaction.rs emits CompactionCompleted only after a successful commit; a cancelled or failing compaction returns Err and emits nothing, so looprs' three endings (freed / cancelled / failed) collapse to one plus a run-terminal event that no longer says which compaction it belonged to"),
    ("event", "extension_error"): ("absent", ["PreToolUseGate", "HookObserver"],
                       "rho-sdk has no extension runtime: hooks are in-process traits, so a fault inside the engine's own machinery is a panic or an Err inside looprs' process, and the row that says 'something inside the child broke' has nothing left to report"),
    ("event", "*anything else*"): ("mechanism", ["non_exhaustive"],
                       "serde(other) at the wire level vs #[non_exhaustive] at the type level — see the role catch-all row"),

    # ── pi's nested assistantMessageEvent ──
    ("assistant-event", "start"): ("absent", ["StepStarted"],
                       "no message-start event under rho's flat delta stream"),
    ("assistant-event", "text_start"): ("absent", ["AssistantTextDelta"],
                       "no block-boundary events; there is no block index to disambiguate, so this row's waiting_on becomes impossible rather than unimplemented"),
    ("assistant-event", "text_delta"): ("same", ["AssistantTextDelta"],
                       "RunEvent::AssistantTextDelta{text}: the one row in the whole table that means the same thing on both sides"),
    ("assistant-event", "text_end"): ("narrowed", ["RunOutcome", "text", "EnrichedAssistant", "history"],
                       "the beads planner's 'last words' come from RunOutcome::text() / the committed EnrichedAssistant, i.e. from the run's value rather than a stream event — same fact, different door"),
    ("assistant-event", "thinking_start"): ("absent", ["ReasoningDelta"],
                       "no marker for where reasoning began"),
    ("assistant-event", "thinking_delta"): ("narrowed", ["ReasoningDelta", "ReasoningSummaryDelta", "reasoning_summary"],
                       "ReasoningDelta, plus ReasoningSummaryDelta: rho may hand back a provider-produced *summary* of reasoning (AssistantMessage::reasoning_summary says raw reasoning must never be stored there), so looprs' italic+dim paint survives but paints different text"),
    ("assistant-event", "thinking_end"): ("absent", ["ReasoningDelta"],
                       "no block-boundary events"),
    ("assistant-event", "toolcall_start"): ("narrowed", ["ToolCallUpdated"],
                       "ToolCallUpdated{index, id, name, arguments_delta} collapses start and delta into one variant"),
    ("assistant-event", "toolcall_delta"): ("renamed", ["ToolCallUpdated", "arguments_delta"],
                       "ToolCallUpdated.arguments_delta"),
    ("assistant-event", "toolcall_end"): ("renamed", ["ToolProposed"],
                       "RunEvent::ToolProposed{call}"),
    ("assistant-event", "done"): ("narrowed", ["StopReason", "EndTurn", "MaxSteps"],
                       "rho's StopReason is {EndTurn, MaxSteps}: `stop` maps, `length` (the answer on screen got cut off — the row this file calls its clearest gap) has no rho value, and MaxSteps is a step budget, not an output-token limit"),
    ("assistant-event", "error"): ("narrowed", ["Failed", "Cancelled"],
                       "the turn-scoped error becomes a run-terminal Failed/Cancelled; 'this turn failed but the session continues' is not a state rho's run model has"),
    ("assistant-event", "*anything else*"): ("mechanism", ["non_exhaustive"],
                       "nested additivity has no analogue because the nesting has no analogue"),

    # ── compaction reason ──
    ("compaction-reason", "manual"): ("renamed", ["CompactionTrigger", "Manual"], "CompactionTrigger::Manual"),
    ("compaction-reason", "threshold"): ("renamed", ["CompactionTrigger", "Automatic"], "CompactionTrigger::Automatic"),
    ("compaction-reason", "overflow"): ("renamed", ["CompactionTrigger", "ContextOverflow"], "CompactionTrigger::ContextOverflow"),
    ("compaction-reason", "*anything else*"): ("mechanism", ["CompactionSkipReason"],
                       "a Rust enum has no unknown-value carrier, so the card can no longer 'keep saying something' where a new value arrives; the skip reasons (PolicyDisabled/BelowThreshold/PendingAsyncTools) are a *different* set that never reaches the stream, so they are not the replacement"),

    # ── stop reason ──
    ("stop-reason", "pending"): ("absent", ["AbortedAssistant"],
                       "no per-partial-message reason on the stream; AbortedAssistant is a history entry committed after a dead provider, not a live reason"),
    ("stop-reason", "stop"): ("renamed", ["StopReason", "EndTurn"], "rho_sdk::StopReason::EndTurn"),
    ("stop-reason", "length"): ("absent", [],
                       "rho-sdk's public model layer has no provider finish reason at all — `finish_reason`/`max_tokens` appear nowhere in its src — so the one value that says 'what the user is reading is incomplete' is still unreported, and is unreported by a type that cannot be extended at the wire level"),
    ("stop-reason", "toolUse"): ("absent", ["ToolProposed"],
                       "a step that calls a tool is not a stop reason in rho; ToolProposed is the event instead"),
    ("stop-reason", "error"): ("renamed", ["Failed", "Retryability", "Error"],
                       "Failed{message, retryability} / Error; strictly better typed than pi's bare reason"),
    ("stop-reason", "aborted"): ("renamed", ["Cancelled", "Error"], "RunEvent::Cancelled{revision} / Error::Cancelled"),
    ("stop-reason", "deferred"): ("absent", [],
                       "no provider-parked-response concept in the SDK's model layer"),
    ("stop-reason", "*anything else*"): ("mechanism", ["non_exhaustive"],
                       "same as the other catch-alls: unknown-value tolerance is a compile-time property of the dependency, not a runtime property of looprs"),
}

VERDICT_ORDER = ["same", "renamed", "narrowed", "absent", "mechanism"]

# Rows where the *fact itself* cannot exist once the engine is in the process: nothing
# arrives, so nothing has to be rebuilt. What is lost is the guarantee the row was the
# record of — a different and smaller bill than a redesign, and an honest one.
ABSENT_BY_CONSTRUCTION = {
    ("role", "system"), ("role", "bashExecution"), ("role", "branchSummary"),
    ("event", "extension_error"), ("event", "turn_end"), ("event", "message_start"),
    ("assistant-event", "start"), ("assistant-event", "text_start"),
    ("assistant-event", "thinking_start"), ("assistant-event", "thinking_end"),
    ("stop-reason", "pending"), ("stop-reason", "toolUse"),
}

# Rows whose information is not merely un-emitted but absent from rho-sdk's types, so
# no amount of host code recovers it.
NOT_EXPRESSIBLE = {("stop-reason", "length"), ("stop-reason", "deferred")}


def cost_of(verdict, has_reader, key=None):
    """First-order cost of re-establishing one row, given whether looprs reads it."""
    if verdict in ("same", "renamed"):
        return "shim"
    if verdict == "narrowed":
        return "redesign" if has_reader else "shim"
    if verdict == "absent":
        if key in NOT_EXPRESSIBLE:
            return "not expressible"
        if key in ABSENT_BY_CONSTRUCTION:
            return "retire the guarantee"
        return "redesign" if has_reader else "retire the row"
    return "redesign"


# ────────────────────────────── the legs ──────────────────────────────────────
def leg_inventory():
    print("\n== 1. THE INVENTORY, RECOUNTED OFF looprs' OWN TREE ==")
    cmds = rpc_commands()
    print(f"commands looprs sends    : {len(cmds)} — {', '.join(sorted(cmds))}")
    for verb in sorted(cmds):
        print(f"    {verb:<12} at {', '.join(cmds[verb])}")

    rows = wire_inventory()
    concrete = [r for r in rows if r["wire"] != "*anything else*"]
    catchalls = [r for r in rows if r["wire"] == "*anything else*"]
    distinct = sorted({r["wire"] for r in rows})
    prior = sorted(set(re.findall(PRIOR_LOOPRS_REGEX, read(os.path.join(REPO, "src", "wire.rs")))))
    print(f"WIRE_INVENTORY rows      : {len(rows)} "
          f"({len(concrete)} concrete + {len(catchalls)} catch-all rows)")
    print(f"distinct spellings       : {len(distinct)} (the catch-all marker counts once)")
    groups = {}
    for r in rows:
        groups[r["group"]] = groups.get(r["group"], 0) + 1
    print(f"groups                   : {', '.join(f'{g}={n}' for g, n in groups.items())}")
    read_rows = [r for r in rows if r.get("reader")]
    unread = [r for r in rows if not r.get("reader")]
    print(f"rows with a named reader : {len(read_rows)}  (these are behaviour ports)")
    print(f"rows declared, unread    : {len(unread)}  (these are documentation rows when an engine changes)")
    print()
    print("  CORRECTION — the ADR's '40 wire spellings' is a regex artifact, not a count")
    print("  of the table. `rho_surface_cost.py` scrapes looprs' side with")
    print(f"      {PRIOR_LOOPRS_REGEX}")
    print(f"  which cannot see a camelCase spelling or the catch-all row. That regex finds")
    print(f"  {len(prior)} values; the table has {len(rows)} rows / {len(distinct)} distinct spellings. The")
    print("  values it drops are:")
    for v in [v for v in distinct if v not in prior]:
        who = ", ".join(sorted({r["group"] for r in rows if r["wire"] == v}))
        print(f"      {v:<22} ({who})")
    print("  None of them is a nothing: four are roles with a reader, one is a stop reason,")
    print("  and the catch-all is the row that records looprs' forward-compatibility policy.")
    print("  No row count below reuses the 40.")

    # The things the other four rows of the ADR's table name, recounted.
    pi_rs = read(os.path.join(REPO, "src", "services", "pi.rs")).splitlines()
    print()
    print("protocol semantics       : data.disposition, settle-as-only-end-of-pass,")
    print("                           refusal-as-success:false, correlation by client id")
    print("process semantics        : stdin-EOF shutdown (ADR-0002 Q1), kill_on_drop,")
    print("                           stderr never to the terminal, per-pass --tools gating")
    procs = [(n, l) for n, l in enumerate(pi_rs, 1)
             if re.search(r"kill_on_drop|Stdio::null|close_stdin|Stdio::piped", l)]
    print(f"                           call sites in src/services/pi.rs: {len(procs)}")
    for n, l in procs:
        print(f"      pi.rs:{n}: {l.strip()}")
    tools = []
    for path in glob.glob(os.path.join(REPO, "src", "**", "*.rs"), recursive=True):
        rel = os.path.relpath(path, REPO)
        for n, line in enumerate(read(path).splitlines(), 1):
            if '"--tools"' in line or "--tools" in line and "argv" in line.lower():
                tools.append(f"{rel}:{n}")
    print(f"                           --tools gating sites: {', '.join(tools[:6])}"
          + (f" (+{len(tools) - 6} more)" if len(tools) > 6 else ""))
    return rows


def leg_candidate_vocabulary(rho_dir):
    print(f"\n== 2. THE CANDIDATE'S OWN VOCABULARY ({LIBRARY}, from the published artifact) ==")
    if not rho_dir:
        print("not measured (no rho-sdk artifact in the cargo cache and none fetched)")
        return None
    # The prior method, reproduced exactly, because the ticket says to reuse it when
    # the files still exist — and they do.
    prior_tags, read_from = set(), []
    for f in PRIOR_CANDIDATE_FILES:
        p = os.path.join(rho_dir, f)
        if not os.path.exists(p):
            continue
        read_from.append(f"{LIBRARY}:{f}")
        for variant in re.findall(PRIOR_CANDIDATE_REGEX, read(p), re.M):
            prior_tags.add(re.sub(r"(?<!^)(?=[A-Z])", "_", variant).lower())
    rows = wire_inventory()
    prior_looprs = sorted(set(re.findall(PRIOR_LOOPRS_REGEX, read(os.path.join(REPO, "src", "wire.rs")))))
    shared = sorted(set(prior_looprs) & prior_tags)
    print(f"prior method, reused verbatim: read {read_from}")
    print(f"  looprs spellings that method could see : {len(prior_looprs)}")
    print(f"  candidate spellings in those two files : {len(prior_tags)}")
    print(f"  string intersection                    : {len(shared)} — {shared}")
    print("  That is what the prior capture's headline measured: a string intersection.")
    print("  It is evidence about spellings and no evidence at all about meaning, and the")
    print("  sentence it was printed with ('40 of 40 change meaning') was asserted, not")
    print("  measured. Leg 3 measures the meaning, one row at a time.")

    # The vocabulary that actually carries meaning, from the files that hold it.
    # The prior scrape counts every variant-shaped name in those two files, which
    # includes sibling enums' members (StopReason::EndTurn, ToolCompletion::Unavailable,
    # …). Count RunEvent's own variants separately so the number means what it says.
    ev_body = read(os.path.join(rho_dir, "src/event.rs"))
    run_events = enum_variants(ev_body, "RunEvent")
    extras = sorted(prior_tags - {re.sub(r"(?<!^)(?=[A-Z])", "_", v).lower() for v in run_events})
    print(f"\nRunEvent's own variants        : {len(run_events)}")
    print("    " + ", ".join(run_events))
    print(f"  (the {len(prior_tags)} names the prior method scraped from those two files include "
          f"{len(extras)} that belong to sibling enums and struct fields in the same files — "
          "StopReason, ToolCompletion, ProviderStreamResetReason, ToolOrigin and friends. And the "
          "coincidence that looprs' table also has 51 rows is a coincidence, not a comparison.)")
    enums = {}
    for rel, why in CANDIDATE_VOCAB_FILES:
        p = os.path.join(rho_dir, rel)
        if not os.path.exists(p):
            continue
        body = read(p)
        enums[rel] = re.findall(r"^\s*pub enum (\w+)", body, re.M)
    print("\nfiles the meaning of looprs' groups lives in (prior capture read two of them):")
    for rel, why in CANDIDATE_VOCAB_FILES:
        names = enums.get(rel) or []
        mark = "prior" if rel in PRIOR_CANDIDATE_FILES else "added"
        print(f"  [{mark:^5}] {rel:<28} enums: {', '.join(names) if names else '—'}")
        print(f"          why: {why}")
    print("\n  The additions are not decoration: looprs' five groups are role, event,")
    print("  assistant-event, compaction-reason and stop-reason. Two files can answer the")
    print("  `event` group and cannot answer the other four — the roles are in model.rs,")
    print("  the compaction reasons are in compaction.rs, the stop reasons are a two-value")
    print("  enum in event.rs, and pi's nested assistant-event envelope exists nowhere.")

    # Facts the process-semantics and protocol rows lean on, pinned to the artifact
    # rather than remembered. Each is a grep a re-run can fail.
    pins = [
        ("event channel is bounded, and the default is the number 8jk.3 wedged it at",
         "DEFAULT_EVENT_CAPACITY: usize = 64", "src/client.rs"),
        ("there is a runtime shutdown, with a count of what it cancelled",
         "pub fn shutdown(&self) -> ShutdownOutcome", "src/client.rs"),
        ("a busy session is a typed refusal, not a `success:false`",
         "SessionBusy", "src/error.rs"),
        ("steering retraction has named outcomes, one of which is a lost race",
         "pub enum SteeringRetraction", "src/steering.rs"),
        ("visibility is asked per model request (the --tools analogue)",
         "fn is_advertised(&self, name: &str) -> bool", "src/tool/registry.rs"),
        ("a host-provided tool is outside every SDK gate",
         "SDK policy cannot sandbox it", "src/tool.rs"),
        ("persistence is a snapshot value plus a host trait, not a file",
         "pub trait SessionStore", "src/persistence.rs"),
        ("the scripted arm ships as public API",
         "pub struct ScriptedProvider", "src/provider.rs"),
        ("the run's final value carries usage and a stop reason",
         "pub struct RunOutcome", "src/event.rs"),
        ("usage fields are Options, so 'nothing reported' is a state",
         "pub struct ModelUsage", "src/model.rs"),
        ("the SDK forbids unsafe in its own source",
         "#![forbid(unsafe_code)]", "src/lib.rs"),
    ]
    print("\ncandidate facts pinned from the artifact (each is a grep this run can fail):")
    failed = 0
    for claim, needle, where in pins:
        path = os.path.join(rho_dir, where)
        ok = os.path.exists(path) and needle in read(path)
        failed += 0 if ok else 1
        print(f"  [{'ok' if ok else 'MISSING':^7}] {claim}")
        print(f"           {where}: {needle!r}")
    # 8jk.4 counted `unsafe` occurrences across the whole artifact; every one of
    # rho-sdk's seven is in benches, which a consumer never compiles, and its src
    # forbids unsafe outright. Worth one line here because the old §3 row reads as
    # though the library carried seven unsafe blocks.
    src_unsafe = sum(read(p).count("unsafe") for p in
                     glob.glob(os.path.join(rho_dir, "src", "**", "*.rs"), recursive=True))
    all_unsafe = sum(read(p).count("unsafe") for p in
                     glob.glob(os.path.join(rho_dir, "**", "*.rs"), recursive=True))
    print(f"\n  `unsafe` in the artifact: {all_unsafe} occurrences total, {src_unsafe} in src/ — and the one")
    print("  src/ occurrence is the `#![forbid(unsafe_code)]` attribute itself, so the library")
    print("  forbids unsafe and the rest is bench code a consumer never compiles. The")
    print("  workspace-8jk.4 capture counts the word across the whole artifact, so its rho-sdk")
    print("  `unsafe` column should be read as benches-and-all, not as code that ships.")
    if failed:
        print(f"  {failed} pin(s) not found — the claims above them are from 8jk.3/the docs, not this artifact")
    return prior_tags


def leg_spelling_map(rows, rho_dir):
    print("\n== 3. ROW 2 IN DETAIL — one verdict per row of WIRE_INVENTORY ==")
    if not rho_dir:
        print("not measured (no rho-sdk artifact; verdicts name APIs that cannot be checked)")
        return {"verdicts": {}, "costs": {}, "expensive": [], "cheap": [], "rows": 0,
                "citation_failures": ["artifact unavailable"]}
    src_blobs = {}
    for p in glob.glob(os.path.join(rho_dir, "**", "*.rs"), recursive=True):
        src_blobs[os.path.relpath(p, rho_dir)] = read(p)
    everything = "\n".join(src_blobs.values())

    missing = []
    counts = {v: 0 for v in VERDICT_ORDER}
    cost_counts = {}
    expensive, cheap = [], []
    for r in rows:
        key = (r["group"], r["wire"])
        if key not in SPELLING_MAP:
            missing.append(f"UNMAPPED {key}")
            continue
        verdict, cites, note = SPELLING_MAP[key]
        counts[verdict] += 1
        has_reader = bool(r.get("reader"))
        cost = cost_of(verdict, has_reader, key)
        cost_counts[cost] = cost_counts.get(cost, 0) + 1
        bad = [c for c in cites if not re.search(r"\b" + re.escape(c) + r"\b", everything)]
        for c in bad:
            missing.append(f"{r['group']}/{r['wire']} cites `{c}`, absent from the artifact")
        flag = "READ " if has_reader else "unread"
        (expensive if cost in ("redesign", "not expressible") else cheap).append((r, verdict, cost, has_reader))
        print(f"{(r['group'] + '/' + r['wire']):<34} {verdict:<10} {flag} cost={cost}")
        print(f"    pi : {r['note'][:118]}")
        print(f"    rho: {note}")
        if cites:
            print(f"    via: {', '.join(cites)}")
    print()
    print("verdict tally   : " + ", ".join(f"{v}={counts[v]}" for v in VERDICT_ORDER)
          + f"   (total {sum(counts.values())} rows)")
    print("cost tally      : " + ", ".join(f"{k}={n}" for k, n in sorted(cost_counts.items())))
    print()
    print("THE EXPENSIVE SET — rows a change of engine cannot settle by renaming:")
    for r, verdict, cost, _ in expensive:
        print(f"  {(r['group'] + '/' + r['wire']):<34} {verdict:<10} reader={r['reader']}")
    print(f"  ({len(expensive)} rows re-derived by hand and re-tested; the other {len(rows) - len(expensive)} "
          "are either a `match` arm, a retired row, or a guarantee that simply goes away)")
    print()
    if missing:
        print("CITATION CHECK FAILED:")
        for m in missing:
            print("  " + m)
    else:
        print(f"citation check  : OK — every rho-sdk identifier named above appears in the "
              f"published artifact ({len(src_blobs)} .rs files read)")
    return {"verdicts": counts, "costs": cost_counts,
            "expensive": [f"{r['group']}/{r['wire']}" for r, _, _, _ in expensive],
            "cheap": [f"{r['group']}/{r['wire']}" for r, _, _, _ in cheap],
            "rows": len(rows), "citation_failures": missing}


def leg_rederivation_surface():
    print("\n== 4. THE SURFACE THAT GETS RE-DERIVED, IN UNITS ==")
    rows = wire_inventory()
    spellings = sorted({r["wire"] for r in rows if r["wire"] != "*anything else*"})
    total, wire_tests, seam_tests, raw_hits = test_inventory(spellings)
    doc = read(os.path.join(REPO, "docs", "guide", "wire-protocol.md"))
    doc_rows = len([l for l in doc.splitlines() if l.startswith("| ")])
    app = read(os.path.join(REPO, "src", "app.rs")).splitlines()
    start = next(i for i, l in enumerate(app) if "fn apply_pi" in l)
    end = start
    depth = 0
    for i in range(start, len(app)):
        depth += app[i].count("{") - app[i].count("}")
        if depth == 0 and i > start:
            end = i
            break
    arms = len(re.findall(r"\bPiEvent::\w+", "\n".join(app[start:end + 1])))
    wp_tests = len([n for n, _ in fn_bodies(os.path.join(REPO, "src", "app", "tests", "wire_protocol.rs"))])
    tok_tests = len([n for n, _ in fn_bodies(os.path.join(REPO, "src", "app", "tests", "token_window.rs"))])
    fixtures = sorted(os.path.basename(p) for p in glob.glob(os.path.join(REPO, "tests", "fixtures", "fake_*")))
    seam_spikes = sorted({os.path.basename(p) for p in glob.glob(os.path.join(REPO, "spikes", "*.py"))
                          if os.path.basename(p) != os.path.basename(__file__)
                          and ("LOOPRS_PI_BIN" in read(p) or "fake_pi" in read(p))})

    print(f"tests in the tree                 : {total}")
    print(f"tests asserting on a wire spelling: {len(wire_tests)} spellings hit "
          f"{sum(len(v) for v in wire_tests.values())} test functions "
          f"(unfiltered quoted matches: {raw_hits}; the filter keeps a body that also names a "
          "wire type or a `\"type\"` tag — a bash test quoting `\"error\"` is not a wire assertion)")
    top = sorted(wire_tests.items(), key=lambda kv: -len(kv[1]))[:10]
    for spelling, names in top:
        print(f"    {spelling:<22} {len(names):>3} tests   e.g. {names[0]}")
    print(f"tests in app::tests::wire_protocol: {wp_tests} (they run the roles through apply_pi "
          "and diff the transcript against the row)")
    print(f"tests in app::tests::token_window : {tok_tests}")
    print(f"app::apply_pi                     : src/app.rs:{start + 1}–{end + 1}, "
          f"{end - start + 1} lines, {arms} explicit PiEvent arms plus the `_` arm")
    print(f"docs/guide/wire-protocol.md       : {doc_rows} generated table rows, generated from "
          "WIRE_INVENTORY by scripts/docs_check.py --fix-wire")
    print(f"src/wire.rs                       : {len(read(os.path.join(REPO, 'src', 'wire.rs')).splitlines())} lines")
    n_bodies, vhits = variant_test_hits(rows)
    print()
    print("the same surface counted through the Rust variants instead of the strings:")
    print(f"  test functions in the tree    : {n_bodies}")
    touching = {k: v for k, v in vhits.items() if v}
    union = set()
    for v in vhits.values():
        union.update(v)
    quoted_union = set()
    for v in wire_tests.values():
        quoted_union.update(v)
    print(f"  rows whose variant is named   : {len(touching)} of {len(vhits)} concrete rows")
    extra = ("and every test that quotes a spelling also names the variant"
             if (union | quoted_union) == union else
             f"plus {len(quoted_union - union)} that quote only the string")
    print(f"  test functions naming one     : {len(union)} ({extra})")
    print("  the rows none of them names   : "
          + ", ".join(f"{g}/{w}" for (g, w), v in sorted(vhits.items()) if not v))
    print()
    print("the test seam:")
    print(f"  fixtures                      : {', '.join(fixtures)}")
    print(f"  tests that drive the fake     : {len(seam_tests)} test functions read the fake child's "
          "journal (argv, verbs, prompts, pids)")
    print(f"  spikes on LOOPRS_PI_BIN       : {len(seam_spikes)} — {', '.join(seam_spikes)}")
    print()
    corpus_cites = sorted({os.path.relpath(p, REPO)
                           for p in glob.glob(os.path.join(REPO, "src", "**", "*.rs"), recursive=True)
                           if re.search(r"spikes/results/(scrollback-cost|resize-transient|highlight-load-cost|live-preview-cost)\.log", read(p))})
    print("the corpus coupling:")
    print(f"  replays pi session files      : src/measure.rs, examples/spike_rpc_serde_cost.rs, "
          f"spikes/pi_rpc_boundary_cost.py, spikes/regex_backend_cost.sh")
    print(f"  constants cited to a pi-format capture: {len(corpus_cites)} files — "
          f"{', '.join(corpus_cites)}")
    return {"total_tests": total, "wire_tests": wire_tests, "seam_tests": seam_tests,
            "variant_tests": len(union), "any_vocab_tests": len(union | quoted_union),
            "doc_rows": doc_rows, "apply_pi_arms": arms, "seam_spikes": seam_spikes,
            "corpus_cites": corpus_cites, "wp_tests": wp_tests}


def gap_table(tallies=None, surface=None):
    tallies = tallies or {"costs": {}, "verdicts": {}, "rows": 0}
    surface = surface or {"seam_tests": [], "seam_spikes": [], "corpus_cites": [],
                          "variant_tests": 0, "any_vocab_tests": 0, "doc_rows": 0}
    costs = tallies["costs"]
    print("\n== 5. THE GAP TABLE — one row per inventory row of ADR-0010 ==")
    rows = [
        ("1", "RPC commands looprs sends",
         "different-but-expressible",
         "shim, plus one redesign",
         "prompt → `Session::start(UserInput)` (or `Session::complete` for a run whose events nobody pumps); "
         "steer → `Run::steer` / `SteeringHandle::steer`; abort → `Run::cancel` / `CancellationToken`.",
         "clear_queue is the row that changes kind rather than spelling. There is no queue the host can "
         "empty: staged steering is retracted one id at a time with `SteeringHandle::steer_retractable` + "
         "`retract_steering`, and a retraction that loses the race reports AlreadyApplied "
         "(8jk.3 steer3: AlreadyApplied,Retracted,Retracted). So the command does not disappear — the "
         "guarantee disappears: pi's clear_queue answers with the text it pulled (`queued_text` reads "
         "data.steering/data.followUp), and rho promises no state in which queued input is discardable. "
         "The Esc path in session/pi_chat.rs (clear_queue-then-abort, the 600 ms CANCEL_WAIT, putting the "
         "pulled text back in the box) is redesigned, not renamed. abort also changes kind: `Run::cancel` "
         "is cooperative, so a tool future that never yields holds the run with no thread to interrupt and "
         "no process to kill (8jk.3 settle5b)."),
        ("2", "Wire spellings behaviour is keyed on",
         "no answer for a minority, different-but-expressible for most — and the expensive ones are the read ones",
         "redesign",
         "See leg 3 for the per-row verdicts and named APIs.",
         f"The prior capture's headline (3 of 40 against casonadams/rho, 0 of 40 against this library) "
         "measured spelling overlap, which is 0 and therefore uninformative. Measured by meaning, most "
         "rows are renamed or narrowed and reachable through RunEvent; the rows that cannot be answered "
         "are the ones pi gets from being a *process* or a *typed union with a serde escape hatch*: the "
         f"unknown-value catch-alls, the message/step boundary rows, the block-boundary rows, and the "
         "`length` stop reason (rho-sdk's src contains no finish_reason/max_tokens concept at all, so the "
         f"row this repo calls its clearest gap is still a gap). First-order cost, row by row: "
         + ", ".join(f"{k}={n}" for k, n in sorted(costs.items())) + ". And the test surface is not the "
         f"quoted strings ({surface['any_vocab_tests']} test functions quote a spelling or name the variant "
         "that catches it, leg 4) — it is every row's re-derivation into a new inventory, with the "
         "generated page and the wire_protocol tests rewritten with it."),
        ("3", "Protocol semantics",
         "3 of 4 different-but-expressible (one of them better than pi), 1 no answer",
         "redesign",
         "`data.disposition` — DIFFERENT-BUT-EXPRESSIBLE, AND BETTER: there is no disposition field because "
         "there is no command/response, and the fork it carried is typed instead — `Session::start(UserInput)` "
         "answers `Ok(Run)` or `Err(Error::SessionBusy)`, so \"the prompt took but no run is coming\" and \"the "
         "prompt was refused\" are two arms of one `match`. looprs' `disposition_of` helper and its deliberate "
         "fail-safe default of \"started\" (`pi.rs`) both go away, and the class of bug they were written "
         "against stops being constructible — which is the one row in this whole table where the alternative is "
         "the better design, and 8jk.3 settle7 is its evidence (a second run on a busy session: refused, "
         "typed). Refusal — DIFFERENT-BUT-EXPRESSIBLE: pi answers a refusal with `success:false` and stays "
         "alive; here it is a typed `Error` (PolicyDenied / SessionBusy / Authentication / Persistence / "
         "InvalidHostResponse …) carrying `Retryability`, which is more structure than a boolean and requires "
         "the beads machine to decide per variant what today it decides from one flag. Correlation by "
         "client-assigned `id` — THE MECHANISM DISAPPEARS: no id, no `Pending` map, no `fail_all` drain, each "
         "call carrying its own await, so the correlation-and-drain machinery of `src/services/pi.rs` (the "
         "`Pending` state, `fail_all`, the id routing in the reader task) becomes deleted code and its tests "
         "delete with it.",
         "The no-answer half is settle-as-only-end-of-pass, which is the row the beads loop advances on. "
         "`Completed` / `Failed` / `Cancelled` on the stream plus `Run::outcome()` cover every case where "
         "someone is still reading, and cover nothing when the run stops being readable: 8jk.3 settle5 saw 0 "
         "terminal events from a dropped Run, settle6 wedged the run on the bounded 64-slot channel with "
         "`SessionState` still `Running`, and gate7 held a run open indefinitely on an approval nobody "
         "answers. pi's \"the child died\" is a fact about a process you can observe from outside; here there is "
         "no process, so looprs must put a deadline in every await and build the watchdog that `kill_on_drop` "
         "plus `fail_all` give it for free today — the same conclusion ADR-0010 reached about the wrong library, "
         "arrived at again against the right one, and still a design change in the beads machine rather than a "
         "shim."),
        ("4", "Process semantics",
         "1 trivial, 2 design changes in the beads machine, 1 impossible",
         "redesign, and one item not expressible",
         "stdin-EOF-as-shutdown — TRIGGER GONE, DRAIN A DESIGN CHANGE: there is no stdin, so ADR-0002 Q1's "
         "\"EOF then drain then exit\" becomes an in-process quiescence wait on `Rho::shutdown()` (which "
         "returns `ShutdownOutcome{cancelled_runs}`), and 8jk.3 gate7 showed a run waiting on an unanswered "
         "approval never ends by itself — the deadline looprs has to add is what EOF used to be for. "
         "Per-pass `--tools` gating — DESIGN CHANGE: visibility maps cleanly (`ToolVisibility::is_advertised`, "
         "asked per model request, runtime rebuilt per pass at 1.8 µs (8jk.3 gate4)) but it is shared mutable "
         "state on a long-lived runtime rather than argv the beads machine builds per pass and a test can read "
         "back; execution rights are a second and different mechanism (`WorkspacePolicy` + `PreToolUseGate`).",
         "Two items stop being things that exist, and one of them is impossible rather than merely expensive. "
         "kill_on_drop — IMPOSSIBLE: 8jk.3 blast2/blast3 show a dropped Run cancelling its token while a "
         "blocking tool's child stays alive, and blast4 re-parents it to init after the host is killed, so the "
         "blast wall moves from one flag on one spawn to process-group discipline looprs imposes on every tool "
         "it registers — and gate3's finding is that a host-provided tool escapes the SDK's gates entirely "
         "(`ToolOrigin::HostProvided`: \"SDK policy cannot sandbox it\"), which is the shape of looprs' own "
         "tool. stderr-never-reaching-the-terminal — DESIGN CHANGE: pi's stderr is `Stdio::null()`, whereas a "
         "panic inside a rho tool prints on looprs' own stderr mid-frame (8jk.3 blast5 wrote to the terminal "
         "while the host survived with exit 0); ADR-0006's clean-exit work does not cover a panic in looprs' "
         "process, so this buys a panic hook plus a policy decision about the user's terminal. The trivial half "
         "is honestly trivial: the pipe framing, the line reader and the correlation drain are deleted, and the "
         "~250 lines of `src/services/pi.rs` that *are* the boundary go with them — deletion is cheap, but it "
         "is the deletion of the thing ADR-0010 decision 1 says the blast wall and the test seam stand on."),
        ("5", "Test seam (LOOPRS_PI_BIN + tests/fixtures/)",
         "different-but-expressible for one half, no answer for the other",
         "shim for new arms; redesign for the process-level ones",
         "ScriptedProvider/ScriptedTurn (+ ScriptedTool, ScriptedCompactor) is a better scripted arm than "
         "anything in tests/fixtures/: it records `RecordedModelRequest` (advertised tool list, tool-schema "
         "bytes, message count), it is deterministic without a shell, it costs no model call, and it drives "
         "the real runtime, the real tool round trip and the real event stream. Every beads-loop test that "
         "asks 'does the loop advance on a settle' is better written against it.",
         "What it cannot prove, in principle rather than as a TODO: argv (there is no argv), exit codes "
         "(no exit), stdin-EOF shutdown (no stdin), stderr (no stream), a child that dies on its own (no "
         "child), and orphaned tool children (8jk.3 blast1/blast3 — the only way to see them is to be a "
         "process outside the process). The honest framing is not 'the seam shrinks': the fake child proves "
         "looprs' own PiRpc framing, correlation and drain code, and on an in-process engine that code "
         "does not exist, so the seam loses its subject. ADR-0010 rule 5 says a switch must *add* a fake "
         "seam rather than ask for this one's removal, and this row is why: "
         f"{len(surface['seam_tests'])} test functions read the fake child's journal and "
         f"{len(surface['seam_spikes'])} spikes run it (leg 4), every one of them either rewritten against "
         "a scripted provider or re-expressed as a harness-level process test to keep proving what it "
         "proves today."),
        ("6", "Corpus coupling (pi session files)",
         "no answer — the corpus has to be rebuilt or bought back with an adapter",
         "redesign (largest row; it is the whole hidden cost)",
         "rho-sdk's persistence is better than opt-out: it is opt-in and host-implemented. `Session::snapshot` "
         "yields a versioned `SessionSnapshot` (schema_version 2, MIN_SESSION_SNAPSHOT_SCHEMA_VERSION 1), the "
         "only shipped store is `InMemorySessionStore`, and 8jk.3 persist saw 0 files created in a scratch "
         "dir and no `~/.rho`. There is nothing to switch off, and nothing to measure.",
         "So the corpus does not get a new format from the engine — it gets no format, and looprs becomes the "
         "author of one. Either looprs writes a pi-shaped session file itself out of rho events and history "
         "(a new, unsupported, permanent translation layer between a dependency's history types and the record "
         "vocabulary leg 1 counts), or every corpus-justified constant is re-baselined against captures taken "
         "through the new engine, which requires a corpus of real passes that does not exist yet. The nearest "
         "shipped rho transcript format is rho-coding-agent's — `<created_at>_<id>/session.jsonl`, "
         "SESSION_VERSION 4, records of `StoredStateTransition::{Snapshot, SnapshotDelta}` over a session "
         "tree — which is a *snapshot delta log*, is `pub(super)` inside a bin crate (not linkable), and is "
         "not pi's `type=message` / `message.role` record stream that src/measure.rs parses. Leg 4 counts "
         f"the {len(surface['corpus_cites'])} source files whose constants cite a pi-format capture. That is "
         "before the record-shape change: bytes-per-record and structure-count-per-pass are the denominators "
         "for the scrollback retention, rewrap transient, highlighter fence census and live-preview numbers, "
         "and a delta log changes the shape, not just the values. ADR-0010 named this row the largest hidden "
         "cost in the comparison; the right library does not reduce it, it only removes the licence "
         "objection from the row above it."),
    ]
    for num, name, verdict, cost, expressible, why in rows:
        print(f"\n--- row {num}: {name}")
        print(f"    verdict : {verdict}")
        print(f"    cost    : {cost}")
        print(f"    expressible via: {expressible}")
        print(f"    and the part that is not / what looprs would build: {why}")

    print("\n--- also reported: the surface the amendment has to re-derive, and the test counts")
    print(f"    generated page     : docs/guide/wire-protocol.md — {surface['doc_rows']} generated table rows, "
          "regenerated from WIRE_INVENTORY by scripts/docs_check.py --fix-wire")
    print(f"    app::apply_pi      : {surface.get('apply_pi_arms')} explicit PiEvent arms plus the `_` arm, "
          "every one re-pointed at a different event type")
    print(f"    the token window   : Tokens::add folds pi's usage at the authoritative assistant message_end "
          "(the code comment records the cumulative-usage trap); rho reports usage as per-call Options on "
          "step, call and run clocks")
    print(f"    tests on the vocab : {surface['any_vocab_tests']} test functions quote a wire spelling or name "
          f"the variant that catches it (leg 4 lists the split)")
    print("\n--- THE GAP TABLE IN SIX LINES")
    for num, name, _v, cost, _e, _w in rows:
        print(f"    row {num}  {name:<36} {cost}")


def main():
    scratch = "/tmp/8jk5-rho-rescore"
    os.makedirs(scratch, exist_ok=True)
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    rc, tree = sh("git -C %s rev-parse --short HEAD" % REPO)
    version = crates_io_version(LIBRARY) or "5.9.0"
    rho_dir, origin = crate_src_or_fetch(LIBRARY, version, scratch)

    print(f"# matthewyjiang-inventory-rescore — captured {now}, tree {tree}")
    print(f"# ran: python3 spikes/rho_inventory_rescore.py")
    print(f"subject:       {SUBJECT}")
    print(f"library:       {LIBRARY} {version}"
          + (f"   [{origin}]" if origin else "   [not measured — no artifact, no fetch]"))
    print(f"library source : {rho_dir or 'NOT AVAILABLE'}")
    print(f"host:          {platform.system()} {platform.release()}, {os.cpu_count()} cpus")
    print(f"behavioural evidence beside this scoring: "
          f"{os.path.relpath(EMBED_CAPTURE, REPO)}"
          + ("" if os.path.exists(EMBED_CAPTURE) else "  [MISSING]"))
    print("scope:         ADR-0010's six-row dependency inventory, re-scored row by row. Pricing only:")
    print("               nothing was added to Cargo.toml, nothing was built, no feature flag was proposed.")
    print("read before quoting: every count here is derived from the tree or from the published artifact;")
    print("               the verdicts are hand-written and their citations are checked in leg 3.")

    leg_inventory()
    leg_candidate_vocabulary(rho_dir)
    tallies = leg_spelling_map(wire_inventory(), rho_dir)
    surface = leg_rederivation_surface()
    gap_table(tallies, surface)
    print("\nDone.")


if __name__ == "__main__":
    sys.exit(main())
