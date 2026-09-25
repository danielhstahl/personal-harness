# ADR-003: A read-only monitor for the inference backend, drawn above the work

- **Status:** Accepted
- **Modules:** `src/monitor.ts` (poller, field reader, panel), `src/render.ts` (band on the work surface), `src/idle.ts` (top line when idle), `src/app.ts` (composition), `src/main.ts` (env knobs)

## Context

ADR-002 established that the server knows things worth having *before* a ticket is
claimed. It turns out the server also knows things worth having **while a ticket is
being worked**: how full the KV cache is right now, whether this turn is running
with a drafter at all, what tokens/sec the decode is actually doing, whether the
prompt cache is being hit, whether four other sessions are queueing in front of
this one.

None of that reaches the loop. The runner sees a stream of assistant text. When a
pass goes slow the operator's question — "slow because the model is slow, or
because we are one of nine requests and the KV pool is full?" — is
unanswerable from anything the TUI shows, and the answer is on the box, in
`/metrics`, one `GET` away.

Four endpoints are available and unauthenticated:

| Endpoint | Answer | Shape |
| --- | --- | --- |
| `/health` | window, slots, in-flight, queued, KV headroom, `max_tokens_cap`, drafter availability, **and where its own other pages are** | JSON |
| `/metrics` | cumulative counters: tokens generated, requests served, cache hits/misses, KV used, per-model series | Prometheus text |
| `/cache` | prompt cache state and hit rate | JSON |
| `/v1/models` | what is loaded, and what else is available | JSON |

Two constraints in this codebase shaped everything below.

**1. There is no "top of the screen" while a session runs.** ADR-001 §6 put the
session in pi-tui's main screen so that everything the session emits lands in
scrollback, where the user can scroll up and read it. That choice is what makes
the loop readable all day, and it means the transcript *is* the scroll region.
A component appended at the top of that transcript is a line of content like any
other: it scrolls up and away within a few dozen tokens of output. Verified
directly against `TuiMainScreen` with a head component under a growing transcript:

```
top A · top B · top C          <- 0 ticks of output
top C · line 1 · line 2 · line 3   <- after 3 lines, "A" and "B" are gone from the viewport
```

With 14 lines of history and a 6-row viewport, the head component's last render
is no longer on screen at all. A "top monitor" placed in the transcript is
therefore not a monitor: it is a line that was once at the top.

**2. The monitor must not be able to cost the run anything.** The loop's scarce
resource is the context budget and the operator's trust. A component that can
fail should not be able to fail *into* the work.

## Decision

**Add `src/monitor.ts`: a read-only poller with a tolerant field reader, and draw
what it knows in the fixed chrome — the band directly above the footer while a
session runs, and the top line proper when the loop is idle.**

### 1. It is read-only, and that is enforced by shape

`src/monitor.ts` has no `bd` handle, no issue id, no session handle, and no
`POST` anywhere. Every failure path returns a value rather than throwing; the
only way it touches the run is by handing both surfaces a `string[]` of lines to
draw. It cannot write to the board because it has nothing to write with.

It is also honest about which of its readings are evidence:

- a source contributes a fact only if it *answered with a parseable body*;
- a key read from an endpoint that never answered is treated as absent, so a
  missing server shows `—` rather than `0`;
- nothing is ever guessed at. If no source exposes KV capacity, the panel says
  `kv —` and does not divide by an assumed pool size.

### 2. It draws in the fixed chrome, because that is the only place that stays put

The work presenter's fixed region (footer, hints, token counter) is the only
part of the work surface that is redrawn at the same position on every frame.
The monitor joins it as one or two lines immediately above the footer:

```
  … session transcript, scrolling into scrollback as usual …
● 1s · t/s 132 · kv 64% 168k/262k · slots 1/4 · queued 0
ctx 262k · out 66k · draft mtp · cache 89% hit · req 2.0/s (415 served)
issue 9f2c · main · 2 files changed · 12/256k ctx · ⌃Q quit
```

`monitorPlacement: "top"` puts the same band first on the surface for anyone who
prefers it there; it will scroll, and that is the trade-off they chose rather
than one the code hid from them. The idle surface has no transcript, so there the
monitor **is** the top line — above the board status, above the prompt.

When the presenter is released (session over, waiting for the finalizer; or
finalize → idle) the monitor stops being drawn. Those lines describe what the
inference server is doing *now*, and after a handoff "now" is no longer
describing the work the user is looking at. Scrollback ends with content. A
piped log never sees it at all: the plain path writes transcript lines only.

### 3. The field set

Line 1 is the live line — what a pass is doing at this moment:

| Field | Source | Why it is here |
| --- | --- | --- |
| `● 2s` | freshest endpoint age | whether the numbers below are current, and the outage marker (`✕ unreachable`) |
| `t/s` | slope of `tokens_generated` | the number that answers "is it slow" |
| `kv 63% 165k/262k` | `kv_used` / KV capacity (`slots` × `kv_max_context`, `kv_capacity_tokens`, `total_kv_bytes`÷`bytes_per_slot`, …) | the number that answers "why is it slow" |
| `slots 1/4` | `in_flight` / `slots` | whether we are the only one here |
| `queued` | `queued` | requests not even started |
| `⚠ working` | `is_busy` | shown only when the panel is otherwise silent, as an explanation of the silence |

Line 2 is the settled line — what the server is:

| Field | Source |
| --- | --- |
| `ctx` | context window |
| `out` | `max_tokens_cap` |
| `draft` | drafter in use (`none` in warning colour when there is none) |
| `cache 89% hit (+21/-2)` | prompt cache, **as an interval**, not a lifetime average |
| `req 2.0/s (415 served)` | request rate and total |
| `model <id> +N more` | `/v1/models` |
| `api`/`engine` | added when there is room |

Colour is semantic, never decorative: green for a full cache or a running
drafter, amber for pressure, red for a drafter that was expected and is absent.
The panel fits the terminal width by dropping whole fields from the right, low
value first, and ends the last kept line with `+N more` rather than cutting a
field in half. `LOOP_MONITOR_LINES` buys a third line for the KV pools.

### 4. Rates are computed as slopes, and refuse to lie

Everything interesting is cumulative. The panel shows a rate by differencing the
same key against the same source at two different times. The function that does
it returns `undefined` — no number, rather than a wrong one — when:

- the earlier reading is missing or in a different units class (`ms` vs `tokens`);
- the two readings came from different keys, so a renamed counter cannot be
  differenced into a rate;
- the counter went **down**, which means a restart, and the honest reading is
  "the server restarted", not a huge negative throughput.

Cache hit rate is the same problem one level up: a lifetime `hits/(hits+misses)`
is inert during a run and would report 95% long after the cache stopped helping.
The panel reports the interval and shows the delta (`+21/-2`) so the number is
falsifiable: the first read shows `—`, never a fabricated baseline.

### 5. The endpoints tell us where they are

`/health` advertises its own `metrics` / `cache_counters` paths (or an
`endpoints` list). `/v1` and `/metrics` are guesses. On the **first** cycle
`/health` is polled first, alone, and its advertisement is applied before the
other three are asked — so a server that keeps its counters at `/counters` is
never once asked at `/metrics`, and a non-standard layout needs no
configuration at all.

An endpoint that answers 404/405/501 is marked `absent` and dropped from the
rotation, with a line in the panel and a line in `--monitor-verbose` output.
`/cache` being absent is not a failure; it is a server without a prompt cache,
and the panel says so once instead of retrying it forever.

### 6. Staleness is shown, never hidden

Every figure carries its age and the panel is prefixed with the age of the
freshest source: `● 2s`. A server that stops answering keeps the last good
numbers on screen with a growing age — which is the right behaviour during a
transient outage and wrong only if it were presented as live, hence the dot.
If **nothing** ever answered, the panel says `✕ unreachable: <reason>` and the
line stops looking like a reading altogether.

### 7. It costs what it says it costs

Default one poll per second (`LOOP_MONITOR_MS`), a 1.5s timeout per request
(`LOOP_MONITOR_TIMEOUT_MS`), and `/v1/models` every 30s (`LOOP_MONITOR_MODELS_MS`).
Each poll is 3–4 small `GET`s: tens of kilobytes a minute, and one frame per
poll through the existing 33ms coalescing window — the monitor repaints the
fixed region, it never re-renders the transcript, and it cannot make a frame
arrive faster than the presenter already allows.

While pi's own request is in flight the event loop is busy with the response
stream and the poll will be late rather than competing; polls are never allowed
to overlap themselves, so a slow server produces one long wait and one set of
numbers, not a queue of pending requests.

### 8. Off is a no-op, and unknown is not silent

- `LOOP_MONITOR=0` builds a `createNullMonitor()` that renders nothing, polls
  nothing, and satisfies the same interface — the surfaces do not carry
  `if (monitor)` logic and cannot get it wrong.
- If no base URL can be resolved (no provider, no config), the same null
  monitor is used and `--monitor-verbose` says why.
- `LOOP_MONITOR_VERBOSE=1` prints, per endpoint: the URL, the state, the age,
  the latency, and **the keys that endpoint exposed but that no field reads**.
  That last line is the whole reason a field-set for a server this codebase has
  never seen can be extended in one edit instead of by guesswork.

## Rejected alternatives

**A status line appended to the transcript.** Measured above: it scrolls off
within a screenful. It also has no stable position, so a "top" that is only
sometimes at the top is worse than no top.

**An overlay drawn over the transcript.** pi-tui has no floating layer; an
overlay would mean writing over the diff-rendered region and keeping the
terminal's idea of its own screen in sync by hand — the exact class of bug
ADR-001 §3 rejected for the spinner. It would also occlude content the user is
reading, which is worse than content that scrolls away.

**Reading the numbers off pi's own usage events.** Pi reports tokens it was
charged, not what the server is doing. No drafter visibility, no queue, no KV
occupancy, no cache hit rate — i.e. every field that motivates this ADR.

**A separate TUI pane.** A second render target means a second terminal to
manage inside a single-terminal application, and a second thing to keep themed
and sized. The band costs two lines where the user already has them.

**Trusting one endpoint.** `/metrics` alone cannot say whether we are queued;
`/health` alone cannot give a rate. The panel keeps per-endpoint state and
labels each field with where it came from, so a partial server renders a partial
panel instead of a wrong one.

## Consequences

**Problems this solves.** The slow-pass question is answerable without leaving
the TUI. Drafter absence, a full KV pool, a cold prompt cache, and foreign load
on the same box become visible during the run that is paying for them rather
than in a post-mortem. Because the field reader reports unrecognised keys, a
server upgrade that renames things is visible on the first verbose run.

**Trade-offs accepted.**

- **Two more lines of chrome on the work surface.** Configurable to one line
  (`LOOP_MONITOR_LINES=1`, which drops the settled line) or off.
- **The panel is a second opinion, not a source of truth.** It can legitimately
  disagree with what the session reports (a pass that finished while the last
  poll was old). That is why every line is timestamped and why the monitor is
  removed from the surface the moment the work is handed off.
- **Field names are guesses for servers this repo has not seen.** Mitigated, not
  solved, by candidate paths plus `describe()` naming what a server actually
  exposed. A wrong guess shows `—`, never a wrong number.
- **Polling instead of pushing.** Up to one interval of lag. A SSE/streaming
  metrics channel would be nicer and is not offered by the server.
- **The band shares the footer's constraint budget.** On a very narrow terminal
  the footer's hints and the monitor compete for width; the monitor drops fields
  first, so the footer keeps its keys.

## Verification

`test/monitor.test.ts` (70 tests): URL derivation including gateway-served
reports and idempotent re-derivation; JSON flattening and Prometheus parsing;
candidate-path reads with first-match and no-match-wins semantics; the four
endpoints' field coverage; freshness and staleness; rate computation with
restart, unit-class and renamed-source refusals; interval cache hit rate; KV
pool aggregation with unknown-pool attribution; panel packing, width limits and
`+N more`; the poller's ordering (health first), absent-endpoint retirement,
listener fan-out with a throwing neighbour, and timer cleanup on `stop()`;
the work surface's band placement, top placement, removal on release,
one-frame-per-poll coalescing, and absence from piped output; the idle
surface's top line, repaint-on-notify and unsubscribe-on-dispose; the poller's
joining semantics (`poll()` joins the cycle in flight rather than starting a
second, which is what `app.ts` awaits before describing the run); the
`describe()` unread-path report (a model id the panel is showing is not
reported as unread, `data.0.object` is); and the composition root resolving
the server from `models.json`, honouring `LOOP_MONITOR_URL`, and rendering
nothing — with a stated reason — when nothing could be resolved.

Run `node tools/monitor-stub.mjs` to see all of it without a GPU box: it serves
`/health` from this repo's own fixture, Prometheus text and cache JSON that
move, `/v1/models`, and drives a real monitor against them. The panel it
produced:

```
● 0s · t/s 2724 · kv 63% 165k/262k · slots 1/4 · queued 0
ctx 262k · out 66k · draft mtp · cache 89% hit · req 2.0/s (415 served) · model halogen-qwen3.8-flash-next
```

with `describe()` reporting all four endpoints answered and their latencies, and
at 60 columns the second line degraded to `… cache 89% hit +3 more`.

The verbose `describe()` line for the fixture's `/health` — 87 paths exposed,
and the ~65 it does not read named individually — is the feature described in
§8 working: the unread set is mostly the configuration surface that
ADR-002's audit owns, and the moment a server adds something the panel ought to
show, it appears there.
