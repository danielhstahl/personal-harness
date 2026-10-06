# ADR-0004: The full screen we take, the clipboard we write, and what a selection copies

- **ID:** looprs-pdl.1
- **Status:** Accepted — 2026-10-06
- **Epic:** looprs-pdl (Full-screen TUI: own the screen, the scrollback, the selection, the clipboard)
- **Decides for:** looprs-pdl.4 (the frame migration), .6 (the scrollback store), .7 (bounded
  scrollback), .8 (mouse scroll), .9 (drag selection), .10 (select-to-copy), .11 (bracketed
  paste), .12 (full-screen children), .13 (keyboard parity) — none of which re-opens the four
  questions below, and each of which cites these rules by number
- **Rests on:** the measurements of looprs-pdl.2 — [`spikes/results/terminal-matrix.md`](../../spikes/results/terminal-matrix.md),
  `spikes/results/mouse-clipboard-{e2e,control,ssh}.log`, the two emulator legs — plus one new
  measurement this ticket added for the two costs the spike had not priced:
  [`examples/spike_clipboard_cost.rs`](../../examples/spike_clipboard_cost.rs) →
  [`spikes/results/clipboard-cost.log`](../../spikes/results/clipboard-cost.log)
- **Depends on without changing:** [ADR-0006](0006-terminal-mode-ledger.md) (the mode ledger, and
  "the exit path never asks the terminal a question") and
  [ADR-0005](0005-shell-output-content-model.md) (rule 6: the store is control-free; rule 7: copy is
  the content, not the picture). Every rule here is built out of those two; none contradicts them.
- **Numbering note:** this is the slot ADR-0005 explicitly left unissued for it.

---

## The four answers, up front

| | Question | Decision | The measurement it rests on |
| --- | --- | --- | --- |
| **Q1** | Which screen? | **The alternate screen (`?1049`), with the transcript journaled to a file as it finalises.** Recovery is a canonical path, not an exit-time act. | `?1049` saves and restores the main screen **and the cursor**, and the ledger hands it back exactly once on six exit paths including a panic, `SIGTERM` and `SIGHUP` (ADR-0006, `shutdown-e2e-pdl3.log` 149/149) |
| **Q2** | How do clipboard bytes reach the user? | **Native helper when this process shares a session with the clipboard; OSC 52 when it does not — and OSC 52 always behind the helper as the fallback.** Never both, never truncated. | pbcopy: **8.9–9.7 ms** per write, size-independent, read-back byte-exact. OSC 52: encode **0.002 ms** for 128 B, **6.1 ms** for 1 MiB; **lands byte-exact to 1 MiB in WezTerm (0.01 s, no prompt)** and **lands nothing at all in Apple Terminal**; crosses a real SSH hop into the local clipboard whole (**4,117 B, 0.13 s**) |
| **Q3** | What does a selection copy, and when? | **On release, once. A value addressed as *characters in the store's logical lines*, not as display rows — joined by provenance, never our wrap, never chrome, never styles.** Empty-after-trim copies nothing. | The wire can carry a drag (128 reports → 128 events, 0.282 ms, nothing merged), so the reason to copy once is not the wire, it is the **1.34× wire write + ~9 ms helper spawn per copy** and the ~200 of those a per-drag copy would fire |
| **Q4** | What is the confirmation? | **An in-app toast in a fixed overlay slot. Never the `Notifier` seam. N is characters — `chars().count()` of the final copied string.** | The same text in three units: `漢字` is **2 characters, 4 cells, 6 bytes**; `é` (decomposed) is **2 / 1 / 3**; the ZWJ family `👩‍👩‍👦` is **5 / 2 / 18** — and cells change with the window width while the copy does not |

Twenty-one rules follow. They are the contract; the prose underneath is the reason.

---

## Context

The epic's own argument is that almost every hard thing in `src/viewport.rs` exists because the
region above the pane is somebody else's. This ticket is the point where that argument gets paid
off or thrown away, and three more besides: how a copy reaches the system clipboard, what a
selection means when our wrap is not the text's, and what the app is allowed to claim when it
says "Copied".

Every other ticket in the epic needs those answered before it can be written, and each of them
would answer them differently under pressure. The four sections below take the decisions and pin
them to numbers so that the argument happens once, here, against measurements instead of against
each other.

Two things were deliberately done before this file was written. looprs-pdl.2 measured the mouse
and the clipboard instead of assuming them — including the things it could not measure, which are
listed as open questions at the end rather than quietly dropped. And looprs-pdl.5 decided what
a stored line *is* (`StyledLine`: content + styles + cells, control-free, `line.cells ==
line.text.width()`), which turns out to be what makes a copyable selection cheap: the cell →
character mapping the selection needs is the same mapping the wrap already uses.

---

## Q1 — Which screen do we take?

**R1. We take the alternate screen.** `Mode::AltScreen` (`?1049h` / `?1049l`) goes into the
ledger's default set in looprs-pdl.4, and from that frame every cell of the window is ours: our
own line store, our own scroll offset, our own selection band, our own wrap.

**R2. The transcript is journaled to a file as it finalises — not written at exit.** The same
event that appends a finalized entry to the store appends the same text to the journal, on its own
blocking task, flushed per entry. The journal is a *reading* copy: `bd` remains the system of
record, and the file says so in its own module comment so nobody starts parsing it as one.

**R3. One canonical name, announced at startup and never at exit.** `$LOOPRS_TRANSCRIPT_DIR`, else
`$XDG_DATA_HOME/looprs/transcripts/`, else `~/.local/share/looprs/transcripts/`, holding
`session-<UTC>-<pid>.txt` plus a `last` symlink re-pointed atomically at session start. Files
`0600`, directory `0700`, `LOOPRS_TRANSCRIPT=off` disables. The startup banner prints the path
once. **Nothing transcript-shaped is written on the exit path.**

**R4. No reprint of the transcript onto the main screen at exit.** The screen the user comes back
to is the screen they left, byte for byte, and nothing of ours is appended to their scrollback.

### What R1 costs, and where each part is paid back

The ticket named the cost honestly, and it deserves an answer per part:

- **While we run, the user cannot wheel-scroll looprs' output into the terminal's own scrollback,
  because there is no scrollback above us.** Paid back by the store: the transcript *is* the
  scrollable content, with a wheel (pdl.8) and a keyboard (pdl.13) driving our own offset, a
  pin-to-tail rule and an "N new" affordance. It is also paid back better: a transcript we own can
  be bounded, searched and re-wrapped; one parked in the terminal's buffer cannot.
- **On exit, the transcript is not in the user's scrollback and never will be.** Paid back by R2
  and R3: it is a file, at one fixed name, which is more recoverable than a scrollback is (a
  scrollback is the terminal's memory, and it gets truncated, closed with the window, and lost on
  a crash).
- **The user is inside a screen they did not ask for.** Paid back by a startup banner that says so
  once, in the two facts that matter — how to scroll, and how to quit — and by the `?1049` promise
  itself: ADR-0006 records that `?1049l` restores the main screen *and the cursor* exactly, so
  the terminal they come back to is the terminal they left.

### Why the alternate screen and not a full-height inline pane

Because the inline option pays the mouse-capture cost and still does not own the buffer, and
because the thing it buys — the transcript sitting in the terminal's own scrollback — is bought
back better by R2.

The inline viewport's cost is not hypothetical, it is documented at the module level in
`src/viewport.rs`: building an inline `Terminal` asks the terminal where the cursor is
(`ESC[6n`), and *that one fact governs the frame loop*, because the query and the async key
stream are fighting over the same stdin — "the difference between a repaint 60 ms after a resize
and the app dying with *The cursor position could not be read within a normal duration*". Three
rules hang off it: the key stream is stopped and restarted across anything that can query; the
window size is polled every frame because `SIGWINCH` is not a byte on stdin; and neither a failed
`fit` nor a failed frame may end the session. Add `follow_the_pane_down` (to follow a movement
ratatui performs and will not report) and `LiveAnchor` (because the exit path must know the pane's
top row and cannot ask). A full-height *inline* viewport is the same machinery with a taller pane:
it still grows above itself, so it still needs the anchor, the query, the stop/restart and the
poll.

What it also cannot do, with the transcript living in memory we do not own:

- **bound it** (pdl.7's whole reason for existing — "the history is ours and bounded by us");
- **re-wrap it on resize against the content the user was looking at** (pdl.6), because the
  terminal re-wraps it against *its* buffer and never tells us where our content went;
- **draw a selection band across it** (pdl.9), or honour a wheel against its offset (pdl.8), or
  show a truthful "N new" while unpinned;
- and it still has to take mouse capture to do any of those, which takes the native drag-to-select
  away anyway (the epic's own price list).

So the inline option costs the anchor machinery *and* the capture, and buys a scrollback it cannot
scroll. The alternate screen costs the journal and buys the screen.

### The two-year question: where did the transcript go, and can it be back in ten seconds?

Yes, from one fixed path, with the process already dead:

```sh
less ~/.local/share/looprs/transcripts/last            # or: pbcopy < ~/.local/share/looprs/transcripts/last
```

The properties that make that true, each one a consequence of R2/R3 rather than of luck:

- **It is not on the exit path.** A journal written at exit is a journal that does not exist after
  `kill -9`, after an OOM kill, after a power loss — and it does not exist after a trim either,
  which is the failure pdl.7 is specifically about. ADR-0006's ledger covers `SIGTERM`, `SIGHUP`
  and a panic *inside the draw*, and its own text says the one thing it cannot cover is
  `SIGKILL` ("the ledger's bytes went out with the last flush before it, which is all there is").
  R2 puts the transcript's durability on the same footing as a shell's own history file: whatever
  happened before the death is present.
- **One name, learned at startup.** The recovery does not depend on having read an exit line, on a
  PID, or on a filename pattern. `last` is the answer for every session, including the one that
  died.
- **Plain UTF-8 text with no presentation in it** — the copy value of every entry in order, so
  `less`/`grep`/`pbcopy` all work and nothing has to be decoded. It is the same rule as R14
  applied to the whole session: journal == "select everything and copy".

**R3 explicitly does not print the path at exit**, and that is a decision with a reason in it:
`holding_the_alternate_screen_means_no_erase_and_no_closing_newline` in ADR-0006 pins *the leave
sequence as the last byte the app writes on the alt-screen path*, and the `?1049l` restores the
cursor to where the user's prompt will be. Writing the journal's contents — or even a sentence
about it — after the leave lands text on the user's own prompt line and breaks the byte-tail shape
that spike checks. If a later ticket wants an exit-time line anyway, it is a new decision that
widens that test, and it should say so rather than discover it.

### Alternatives rejected

1. **Full-height inline viewport** — keeps the anchor/query/stop-restart machinery this epic exists
   to delete, and cannot bound, re-wrap, scroll or select its own transcript. Its one real win
   (native scrollback of looprs' output) is bought back by R2 with fewer dependencies.
2. **Alternate screen + reprint the transcript on the main screen at exit** — costs a body of text
   proportional to the session shoved into the scrollback the user just got back (a long beads pass
   is thousands of lines to shovel past to reach a prompt), is impossible on exactly the deaths
   that matter, contradicts ADR-0006's byte-tail contract, and duplicates a file R2 keeps anyway.
3. **Alternate screen with the transcript in memory only** — the silent loss this whole section
   exists to prevent.

---

## Q2 — How do clipboard bytes reach the user?

**R5. The transport is chosen by *where this process is running*, not by a guess about the
terminal.** Decided once at startup, logged once, overridable with
`LOOPRS_CLIPBOARD=auto|native|osc52|off`. There is deliberately **no capability query** in this
decision: everything it needs is free to read (environment and `PATH`), and a terminal that
answers a query tells you about its parser, not about its policy.

**R6. Same session as the clipboard ⇒ native helper first, and verified.** No `SSH_CONNECTION`,
`SSH_CLIENT` or `SSH_TTY` in the environment and a helper present (`pbcopy`, `wl-copy`, `xclip`):
use the helper, then read the clipboard back with the matching reader (`pbpaste` / `wl-paste` /
`xclip -o`) and compare. `OSC 52` sits behind it as the fallback for a helper that turns out to be
missing or that errors at call time.

**R7. Remote session ⇒ OSC 52 only.** If the SSH variables are set, a native helper on the far
side writes the *remote* clipboard, which is not where the user's paste is. OSC 52 is what puts
bytes into the window the user is actually looking at.

**R8. Exactly one transport per copy.** Writing both is not belt-and-braces: if they disagree,
nothing says which one the user's `Cmd-V` will read, and the toast cannot be honest about the
result. A fallback happens *instead*, never *as well*.

**R9. Never truncate, and impose no cap of our own.** No silent clipping at any size. What the
user selected is what goes, or nothing goes and R20 says so.

**R10. Never use the OSC 52 read-back *query* (`ESC]52;c?`) as anything.** It is answered by
nobody: `bytes_back=0` in the bare pty, in WezTerm in both directions, and in Apple Terminal.
Verification comes from a local reader only — which is precisely what R7 means when it says the
remote copy is not verifiable.

**R11. One injected sink, one task, one slot.** The copy goes through a `Clipboard` trait injected
on `SessionConfig` exactly like `Notifier` (default `Noop`, `RecordingClipboard` in `src/testing.rs`),
onto a queue of depth **1, latest-wins**, read by a task that performs the write. The UI never
blocks on the clipboard. Depth 1 is not an optimisation: four queued copies of superseded
selections is four lies about what the user last selected.

### What the two transports cost

From `spikes/results/clipboard-cost.log` (median of 7, this machine):

| | 128 B | 4 KiB | 64 KiB | 1 MiB |
| --- | --- | --- | --- | --- |
| OSC 52 — encode + frame, in-process (`crossterm`'s own writer) | **0.002 ms** | 0.025 ms | 0.38 ms | **6.1 ms** |
| …wire bytes | 181 | 5,473 | 87,393 | 1,398,113 (**~1.34×**) |
| native — `pbcopy` write (spawn, pipe, wait) | **9.7 ms** | 8.9 ms | 8.9 ms | **9.7 ms** |
| native — `pbpaste` read-back (verification) | +10.7 ms | +9.7 ms | +9.8 ms | +10.6 ms |

Three readings that decide things:

- **A verified native copy costs ~19 ms and does not care how big it is.** 19 ms at one copy per
  drag release is invisible, and it buys the only honest "Copied" in the design.
- **OSC 52 is 10²–10⁴× cheaper in-process**, which is why it is the right answer when we cannot
  verify anything else: an unverified copy that costs nothing beats an unverified copy that costs a
  process spawn.
- **The 1.34× base64 expansion is the only size cost we can see.** The size costs we *cannot* see
  are the terminal's, and they are per-terminal and undiscoverable — which is R9's whole point.

### The failure ladder, and what the user sees

| Situation | What happens | What the toast says (R20) |
| --- | --- | --- |
| native write ok, read-back matches | verified copy | `Copied 1,284 characters · clipboard` |
| native helper absent (no `pbcopy`/`wl-copy`/`xclip`) | fall back to OSC 52 (R6) | `Copied 1,284 characters · OSC 52 (not confirmed)` |
| remote session (R7) | OSC 52, no reader available | `Copied 1,284 characters · OSC 52 (not confirmed)` |
| native helper spawns but exits non-zero, OSC 52 then succeeds | fallback taken | as above, with `helper failed: <reason>` appended once |
| native write ok, read-back **differs** | someone else copied between the two spawns, or the helper mangled it | `Not copied: the clipboard changed under us — select again` (never `Copied`) |
| both transports error | nothing went anywhere | `Copy failed: nothing was copied (native: <err>; OSC 52: <err>)` |
| copy task hits its 2 s deadline (wedged compositor, stalled SSH) | the *late* failure is made visible, and the queued slot is freed | `Copy failed: clipboard did not answer in 2s — nothing confirmed` |

The asymmetry that matters, and that the ladder is arranged to respect: **a native failure is
detectable and an OSC 52 failure is not.** Measured — Apple Terminal took a 64 B OSC 52 copy and
delivered **nothing in 6.01 s**, with no error of any kind on either end. So the design does not
try to detect it. It does something better: it never lets the user find out somewhere else. The
toast's verb and suffix say which class of "we sent it" this is, and the escape hatch in R17
(`LOOPRS_MOUSE=off`, plus a keyboard copy that never depends on the terminal) is named in the
same place the user is looking.

### Alternatives rejected

1. **OSC 52 as the only transport.** Measured to be nonexistent in Apple Terminal (0 B landed at
   any size), and unverifiable everywhere because the read-back query answers nothing. Fine as the
   remote answer, unacceptable as the only answer.
2. **OSC 52 first locally with a native fallback on failure.** The failure it would be falling back
   from is undetectable (Apple Terminal), so the fallback would never fire and the toast would lie.
   A fallback that cannot fire is not a fallback.
3. **Native helper always, including over SSH.** Writes the wrong clipboard, silently.
4. **A clipboard crate (`arboard`, the "arbox" option in the ticket).** Rejected for the same
   reason ADR-0005 rejected a VT-emulator crate: it is a bigger dependency to do the thing we
   already do with a `PATH` lookup, it adds a platform abstraction layer to a decision that is
   about *where we are*, not about *which OS we are on*, and it cannot help over SSH either —
   which is the only case where the transport choice is genuinely hard.
5. **A startup capability query (`DECRQM`/`DA1`) to pick the transport.** Tempting (WezTerm
   answers `?65;4;6;18;22c`, Apple Terminal answers `?1;2c` and nothing else, so the two are
   distinguishable), but it prices the decision at a round trip and still leaves the policy
   unknowable: a terminal can answer every query and then gate the copy behind a permission we
   cannot see. R5 keeps the decision free. A startup `DECRQM` sweep remains the right shape for
   *ADR-0006's* tee'd-mode problem, where the thing being learned is a mode we must restore, not a
   transport we must pick.

---

## Q3 — What does a selection copy, and when?

**R12. Copy fires on drag *release*, once per release.** Drag events update the drawn selection
and nothing else. Consecutive releases are consecutive copies; the last one wins (pdl.10).

**R13. A selection whose value is empty after trimming copies nothing and says nothing.** A
zero-cell drag is a click (pdl.9), and a selection of blanks is not content. No clipboard write,
no toast, and the user's previous clipboard is untouched.

**R14. The copied value is the store's content, addressed as characters, not as display rows.** A
selection is resolved to a range of **characters within transcript entries**; the display rows are
the projection the mouse hit-tests, and the wrap map the store carries converts rows → characters
by walking clusters with the same `unicode_width` the wrap used (pdl.6). Within one logical line
the copy is a **contiguous slice** — never a concatenation of row slices, because a soft wrap may
have consumed the space at the break and a concatenation silently deletes it. Between logical
lines: a hard newline contributes **exactly one `\n`**, a soft-wrap continuation contributes
**nothing**, trailing blanks are trimmed per ADR-0005 rule 7, styles never reach the paste, and
control bytes cannot reach the paste because the store cannot contain them (ADR-0005 rule 6).

**R15. We copy logical lines. Our wrap never reaches the paste.**

**R16. Chrome is never selected and never copied** — status row, tool-card chrome, the input box,
the toast. A drag across those bands selects the transcript rows in its span and skips the chrome
(pdl.9).

**R17. Copy-on-select can be turned off, and the other ways to copy do not depend on it.**
`LOOPRS_COPY_ON_SELECT=0` makes a selection a selection; the keyboard copy (pdl.13) copies the
live selection *always*, whatever that setting says; and `LOOPRS_MOUSE=off` declines mouse capture
entirely, which hands native drag-to-select back to the terminal. Three hatches because the one
that looks obvious — shift-drag falling through to the terminal's own selection — is measured as
*impossible to confirm from a pty* (open question 2), so it is not allowed to be load-bearing.

### Why release and not drag, priced

The wire is not the constraint. Measured: 128 SGR reports in a single 1,536-byte write came out
the other side as **128 events in 0.282 ms**, worst inter-event gap 0.036 ms, ~4.5×10⁵ events/s,
nothing merged and nothing dropped. A drag's motion events are therefore *not* worth throttling,
and pdl.9 should redraw the selection band on every one of them.

What is not free is the copy. Per copy: a 1.34× wire write, or a ~9 ms process spawn, plus the
toast, plus whatever the user's clipboard manager does. A two-second drag at a modest 100 motion
events/s is ~200 copies: ~1.8 s of process spawning, 200 clipboard-manager history entries, and
200 toast updates for one gesture the user has not finished yet. That is not a faster feature; it
is a different, worse feature. R12 keeps the interaction at event rate — the redraw runs on every
drag event — and the side effect at gesture rate.

### What a selection copies — the join, spelled out

One paragraph, stored as **one logical line of 43 characters**, displayed at a 20-column wrap that
broke it at spaces into rows of 19 / 19 / 3:

```text
displayed (3 rows)                the logical line those rows are a projection of
The quick brown fox     <- 19     "The quick brown fox jumps over the lazy dog\n"
jumps over the lazy     <- 19      └ one entry, 44 characters counting its newline
dog                   <- 3

R14 copies:    The quick brown fox jumps over the lazy dog\n   (44: one contiguous slice)
not this:      The quick brown fox\njumps over the lazy\ndog\n  (44 too, but three hard breaks the source never had)
nor this:      The quick brown foxjumps over the lazydog        (41: the wrap ate two spaces)
```

The third line is the trap R14 exists to close, and it is why the rule is stated as *addressing*
rather than as *joining*: 41 characters with two words welded together is not a slightly worse
copy, it is a corrupted one, and it is exactly what "glue the selected rows together" produces
when the wrap dropped the spaces it broke on. The safe shape is the one the store already supports:
rows → character offsets → one slice of the entry's own text.

Note too that the second line and the correct line **have the same character count and different
text** — which is a small illustration of why R19 pins the count to the single string that is
computed once and handed to the sink, and why "Copied 44 characters" is not by itself a definition
of anything.

And a code block, where every line the user saw *is* a line the text has: the copy is identical to
what was stored, because nothing was ever wrapped into it.

And a code block, where every line the user saw *is* a line the text has: the copy is identical to
what was stored, because nothing was ever wrapped into it.

The ticket allowed that "markdown prose and code blocks give different right answers". They do, and
R15 turns out to be right for both — but for opposite reasons, and it is worth having them written
down because they are the reason this is a decision and not a default:

- **Prose:** our wrap column is a property of the window, not of the text. A copy that carried it
  would make the *same selection* copy differently in two different window widths, and would
  hard-break sentences that the target application (Slack, a doc, an email) would have reflowed
  itself.
- **Code:** a copy hard-wrapped at our column is **broken code** — a `\n` in the middle of an
  expression — while the logical line pastes as what was written. The user can always re-wrap code
  themselves; they cannot un-break it.

**The loss, named:** what the user pasted is not what they saw at our wrap column. A selection
copied into a fixed-width target re-wraps differently than it looked. Accepted, because the
alternative is worse in both halves: hard line breaks are wrong for prose and destructive for
code, and the epic explicitly rules out reflow-to-logical-line reconstruction as a feature.

**Which brings us to the one sentence in the epic that looks like it contradicts R15, so it is worth
settling here rather than in a code review.** The epic's out-of-scope list says *"no
reflow-to-logical-line copying"*. R15 copies logical lines. The two are not the same act, and the
difference is which direction the text moves:

- **Joining the display rows the user selected back into the logical lines they came from is in
  scope, and unavoidable.** The store already carries the hard-newline-vs-soft-wrap flag for every
  row precisely because pdl.9 and pdl.10 cannot reconstruct pasteable text without it — pdl.6's
  own text: "joining two soft-wrapped rows must insert nothing, and separating two logical lines
  must insert exactly one `\n`". Not doing it is not "copying what was displayed"; it is
  inserting newlines the source never had.
- **Re-*flow* — re-wrapping the joined text to some width, reconstructing paragraph structure the
  store does not have, or re-indenting it — is out.** We join what was stored; we never lay it out
  again on the way out. That is the thing the epic ruled out, and R15 does not do it.

**Can the user lose their previous clipboard by selecting something to *read*?** Yes, and R17
exists because of it. The clipboard has one slot and copy-on-select takes it; nothing here keeps
the previous value, because keeping it *is* clipboard history, which is out of scope. So the
escape hatches are: turn copy-on-select off (`LOOPRS_COPY_ON_SELECT=0`) and use the keyboard copy
that always works, or take the mouse back (`LOOPRS_MOUSE=off`). The startup banner names them all,
because the first time a user loses a clipboard by *reading* is not the time to be reading a
manual.

---

## Q4 — What the confirmation is

**R18. The confirmation is an in-app toast drawn by the TUI, in a fixed overlay slot, and it never
enters the `Notifier` seam.** A pill of exactly the text's width, reverse-video, anchored to the
bottom-right of the transcript band, drawn last over the frame. Not a new row, and not a segment
of the status row.

**R19. N is characters: `chars().count()` of the final copied string, and it is named in the code
next to the count.** Never `str::len()` (bytes), never `UnicodeWidthStr::width` (cells). The
thousands separator is the toast's; the unit is the type's.

**R20. Three states, carried by the verb, and no optimistic toast.**

| State | Text |
| --- | --- |
| verified (native, read-back matched) | `Copied 1,284 characters · clipboard` |
| sent, unverifiable (OSC 52 — remote, or no local reader) | `Copied 1,284 characters · OSC 52 (not confirmed)` |
| failed or unconfirmed-mismatched | `Copy failed: <reason> — nothing was copied` |

**R21. Latency budget and lifetime, named.** `Copied` appears within **25 ms** of release on the
verified native path (9 ms write + 10 ms read-back), within **50 ms** locally over OSC 52, and
within **500 ms** over SSH (measured 0.13 s for a 4 KiB copy plus scheduling slack). The toast
auto-dismisses at **2 s**, is replaced — not queued — by the next one, and is dismissed by the
next key or click. A copy whose task has not reported at its 2 s deadline is reported as a failure,
not left waiting.

### Why a toast and not a desktop notification

The `Notifier` seam is not a general-purpose "tell a human" pipe, and its own module docs say so
sharply: **exactly one `notify()` call exists in the whole binary**, in the arm that handles
`PassOutcome::Closed`, because "`agent_settled` is not completion: it is the worker having stopped
talking" and the only completion fact this harness has is the board's verdict. Putting a copy
confirmation on that seam would:

- **carry a keystroke on a network POST with a 10 s timeout and a retry.** The copy itself costs
  0.002–19 ms. A confirmation whose transport is five orders of magnitude slower than the thing
  it confirms is not a confirmation of that thing;
- **fire from the very place the notifier's design forbids**: the notifier's producer must never
  await the network, and a copy is inherently interactive — its whole value is that it answers
  immediately;
- **be missing exactly where it is needed.** The desktop bus / network path is unavailable in the
  SSH and container cases where the clipboard is least certain, so the notification-shaped
  confirmation would be *least* reliable precisely where the copy is most likely to have silently
  failed;
- **be a spam machine.** A bead done is one fact per ticket, minutes or hours apart. A copy is one
  fact per selection.

The person who just dragged a selection is, by construction, at this terminal. Its confirmation
belongs at this terminal. And the toast's impermanence costs nothing: the durable artifact is the
clipboard contents, not the message about them.

**Same event as the bead-done notification? No** — different fact, different sink, different
trigger. The copy path never touches `Notifier`, and the toast slot is owned by the copy path alone.
A future "ticket closed" toast would be a new decision with its own trigger and its own text, not
this rule widened — the same discipline `notification.rs` applies to `LeftForHuman`.

### Why the toast is an overlay, not a row

Measured: a reshape of the live region leaves a hole between the erase and the bytes that replace
it — `spikes/results/flash-e2e-at-pdl5.log` puts the worst hole at **8.8–9.3 ms** with 25
reshapes over a 1.5 ms budget. A toast that adds a row is a reshape: it re-lays out the transcript
and moves the text the user just selected out from under the pointer, on every copy. An overlay in
a fixed slot changes no row's geometry, so it cannot move anything. It covers whatever transcript
cells sit in the corner for 2 s, which is a cost we can live with precisely because the toast says
how many characters went.

The status row was the other candidate and is the wrong one for a second reason: the row is a
priority ladder whose top slot belongs to `ERROR` ("the error is never dropped, only shortened"),
and a copy toast must not displace an error, nor be dropped because the row is full.

### Characters, cells, bytes — all three, measured

From `spikes/results/clipboard-cost.log`, cells measured with the same `unicode-width 0.2` the
renderer uses:

| text | characters | cells | bytes |
| --- | --- | --- | --- |
| `漢字` | 2 | 4 | 6 |
| `日本語のコード` | 7 | 14 | 21 |
| `👩‍👩‍👦` (ZWJ family) | 5 | 2 | 18 |
| `é` (decomposed: `e` + U+0301) | 2 | 1 | 3 |
| `🇯🇵` (regional indicator pair) | 2 | 2 | 8 |
| `const s: &str = "日本語";` | 22 | 25 | 28 |

So the three numbers differ by up to **9×** on ordinary text (the ZWJ family is 2 cells and 18
bytes), and **cells is not a property of the copy at all**: ADR-0005 makes re-wrap a pure function
of `cells` against the current width, so the
cell count of a paragraph changes when the window is resized while the copy does not change. A
toast in cells would report a different number for the same clipboard contents depending on the
window. `bytes` is what the wire carries but is not what anybody means by "how much text". The
ticket said characters; the measurement says the ticket was right, and for a better reason than
the obvious one.

What R19 buys beyond correctness of the label: `CopiedCount(characters: usize)` as the type means
`pdl.10` cannot quietly drift to `len()` under pressure, which is the exact drift the terminal
matrix warns about ("the `Copied N` message should be printed from the bytes the app put on the
wire **and** verified against the marker, or not printed at all"). R19 + R20 are that rule with
the two halves attached: the count comes from the same string that went to the sink, and the verb
comes from whether we can prove it landed.

---

## Out of scope

Decided *out*, not merely unbuilt. Each of these is a thing a later contributor could plausibly add
and this file is where "no" is recorded:

- **No VT100/xterm emulation.** ADR-0001's screen-buffer tee (`src/screen.rs`) still carries
  full-screen children straight to the terminal. Nothing here interprets cursor addressing,
  scroll regions or a screen model.
- **No rectangular / block / column selection.** Selection is a reading-order range only.
- **No clipboard history, paste ring, or multiple buffers** — including "keep the previous
  clipboard so copy-on-select cannot destroy it". That is history. R17 is the answer instead.
- **No reflow of the copied text to any width.** We copy logical lines; we do not re-wrap to the
  terminal's width, to the source's width, or to 80 columns. No "copy as markdown", no
  de-indent, no comment-stripping.
- **No OSC 52 read-back, no clipboard-clear, no OSC 52 chunking protocol.** The read-back query
  is measured dead (R10); a clear is not a copy.
- **No X11 primary/secondary selection synchronisation.** `ClipboardType::Primary` is not touched;
  one clipboard, the one `Cmd-V`/`Ctrl-V` reads.
- **No copy over the notification path** — no "send the selection to my phone" (see R18).
- **No mouse-drag of the input caret**, no drag-to-move anything, no middle-click paste (that is
  pdl.11's territory, and pdl.8's rule that unbound buttons are not silently swallowed).
- **No transcript-file parsing contract.** The journal is for reading; `bd` is the record.
- **No retention or rotation policy for the journal** beyond file modes and an off switch. Named
  below as an open question rather than left implicit.

---

## Open questions the spike could not settle

Each with the thing that would settle it, so none of these stays theoretical by accident.

1. **The shape of a physical trackpad flick** — events per flick, their spacing. `pdl.2` measured
   the *ceiling* (128 reports, 0.282 ms, nothing merged), not the input device. Settle with:
   `python3 spikes/mouse_clipboard_e2e.py --in-terminal --record-flick`. Affects pdl.8's row
   delta; affects nothing in this ADR.
2. **Whether shift-drag still reaches the terminal's native selection while we hold
   `?1000+?1002+?1006`** — nothing comes back up a pty to say whether a selection band got
   painted. The failure mode is a mitigation that does not exist. Settle with: the `--in-terminal`
   leg, two shift-drags and a `y`/`n`. If it does not work, R17's `LOOPRS_MOUSE=off` is the only
   escape hatch that remains, and pdl.10's failure-toast text must not promise shift-drag.
3. **What the user sees while a full-screen child holds our alternate screen** — the pty has no
   window. Settle with: run vim inside the real-terminal leg.
4. **Per-terminal OSC 52 caps, for every terminal except WezTerm and Apple Terminal.** We have one
   terminal with no cap found up to 1 MiB and one whose cap is zero, and nothing in between — and
   the plausible failure mode is a terminal that *clips silently* above some size. iTerm2, Ghostty,
   kitty and Zed's panel are all unmeasured; `PDL2_LAUNCH='open -a iTerm {file}'` runs the leg in
   any of them without editing the spike.
5. **tmux.** Its own OSC 52 handling is a known trap (tmux intercepts OSC 52 and only forwards it
   under a configured `set-clipboard`), and its alternate-screen handling wraps ours. Untested.
   Settle with: the `--in-terminal` and `--ssh` legs run *inside* tmux.
6. **Terminals that gate a clipboard write behind a human answer.** Measured only as "0.01 s, no
   prompt observed" in WezTerm, with a coarse proxy for the other case (>1.5 s). The app cannot
   see a prompt at all — it just waits. This is one of the two reasons R6 prefers a verified native
   copy locally; the other is Apple Terminal.
7. **Whether Apple Terminal should be declared supported at all.** Measured facts: it answers
   nothing about `?1000/?1002/?1003/?1006/?1049/?2004`, implements no xterm mouse reporting, and
   delivers no OSC 52. Which means in that terminal the answer to this entire epic is
   `LOOPRS_MOUSE=off` plus a native copy — the old behaviour. Whether that is "supported" or
   "explicitly unsupported with a legible message at startup" is a product call this ADR
   deliberately does not make.
8. **Whether the journal should carry timestamps.** They make a long session searchable and they
   break the property that the journal *is* "select all and copy". Taken aside for pdl.7 with the
   tie-breaker written here: the copy-value property is worth more than a timestamp, and a
   timestamped companion (`.log`) is available if someone truly wants both.
9. **Journal growth.** One file per run, unbounded, in the user's data directory. Unaddressed
   here; pdl.7 owns the byte budget for memory and should say something about disk while it is in
   there.

---

## What each ticket inherits (and may not re-litigate)

| Ticket | Gets | Rules |
| --- | --- | --- |
| pdl.4 (frame migration) | `Mode::AltScreen` in the default set; no reprint; the anchor machinery's reason for existing ends here | R1, R4 |
| pdl.6 (store) | provenance is mandatory, not a nice-to-have: entry id, hard-vs-soft continuation, cell→char map | R14 |
| pdl.7 (bounded scrollback) | the file route: append-as-you-finalise, one canonical `last`, plain UTF-8, 0600 | R2, R3 |
| pdl.8 (mouse scroll) | no throttle on the redraw; row delta comes from the flick measurement, not the wire ceiling | R12 |
| pdl.9 (selection) | the join rule, chrome exclusion, the wrap-is-not-content rule | R14, R15, R16 |
| pdl.10 (copy) | the sink shape, depth-1 latest-wins, one transport, never truncate, the three toast states, the unit | R6–R11, R13, R18–R21 |
| pdl.11 (paste) | no extra transport, no extra mode: reuse the ledger's `BracketedPaste` and this ADR's sink for its copy path | R5, R11 |
| pdl.12 (children) | the transport decision is not affected by who holds the screen; the copy path must not fire while a child holds it | R8, R12 |
| pdl.13 (keyboard) | the keyboard copy works regardless of `LOOPRS_COPY_ON_SELECT`, uses the same sink and the same toast, and is the escape hatch the failure text names | R17, R18, R20 |

---

## Consequences

* **The clipboard becomes an injected sink** with the same shape and the same rule as the notifier:
  `SessionConfig::default()` carries `Noop`, so the test suite is clipboard-free by construction,
  and `RecordingClipboard` makes the exact string and the exact count assertable.
* **A new persistent file with the user's secrets in it.** The journal holds everything they pasted
  into a money-spending agent. Hence R3's modes (`0600`/`0700`), the location in the user's own
  data dir rather than `/tmp`, the `LOOPRS_TRANSCRIPT=off` switch, and the explicit statement that
  it is not the record. That is a real new obligation, and it is cheaper to state now than to
  discover later.
* **The transport answer is "it depends, and here is the rule".** Locally: verified, unlimited,
  ~19 ms. Over SSH: unverified, crosses the hop, ~0.13 s. The toast tells the user which one they
  are in, which is what makes "it depends" an acceptable answer rather than a hedge.
* **Two different verbs in the UI**, and one of them (`not confirmed`) is a state most apps never
  show. That is a consequence of Apple Terminal's measured zero and the dead read-back query, not
  of a taste for hedging.
* **`pdl.4` gets the deletion it came for.** With nothing left to ask the terminal, the key stream
  never has to stop and restart, the per-frame window-size poll loses its reason, and the anchor
  that had to be published to teardown for the erase loses its job. ADR-0006 already said
  `LiveAnchor` could not be deleted while both paths existed; this ADR picks the path.
* **The status row stays a ladder with `ERROR` on top.** R18 keeps the toast out of it, which keeps
  the row's priority order honest and its purity contract intact.
* **This ticket ships no behaviour.** One example and its log. Every rule above lands in a later
  ticket; the value here is that they land consistently, and that the ones which are *not* going
  to be built are written down before someone builds them by accident.

## Proof

A decision ticket's proof is the measurements it leans on and the fact that they can be re-run:

```sh
# the transport costs and the three-unit table this file quotes (new, looprs-pdl.1)
cargo run -q --example spike_clipboard_cost | tee spikes/results/clipboard-cost.log

# the capability facts behind Q1/Q2/Q3 (looprs-pdl.2)
cargo build --examples
python3 spikes/mouse_clipboard_e2e.py           | tee spikes/results/mouse-clipboard-e2e.log
python3 spikes/mouse_clipboard_e2e.py --control | tee spikes/results/mouse-clipboard-e2e-control.log
python3 spikes/mouse_clipboard_e2e.py ssh       | tee spikes/results/mouse-clipboard-ssh.log
# …and the per-terminal legs, collected in spikes/results/terminal-matrix.md
```

Rule → measurement map, so no rule rests on an adjective:

| Rule | Measurement |
| --- | --- |
| R1, R3, R4 | `?1049` parks and restores screen + cursor; the ledger hands every mode back **exactly once** across quit / `LOOPRS_MODES=all` / `SIGTERM` / `SIGHUP` / panic-in-draw / killed-child-holds-the-screen — ADR-0006: **149/149**, against **85/112** for the same spike on the pre-ticket binary (`shutdown-e2e-pdl3.log`, `…-control.log`) |
| R6, R8, R20, R21 | `clipboard-cost.log`: pbcopy write 8.9–9.7 ms, read-back 9.7–10.7 ms, byte-exact at 128 B → 1 MiB |
| R6, R7, R9, R10 | `terminal-matrix.md`: WezTerm OSC 52 byte-exact at every size to **1 MiB**, **0.01 s**, no prompt; Apple Terminal **0 B in 6.01 s**; SSH remote→local **4,117 B whole in 0.13 s**; read-back query answered by nobody (`bytes_back=0`) |
| R7 | `mouse-clipboard-ssh.log`: the hop carries the bytes; without a far-side pty **21/21 bytes arrive and 0 events decode** — the transport is not the problem, the tty is |
| R9 | the only two caps measured are **0 B** and **≥1 MiB**; nothing between, and nothing discoverable from inside the app |
| R12, R13 | burst: **128/128 reports in 0.282 ms**, worst gap 0.036 ms, nothing merged (so the drag redraw is free and the copy is not) |
| R14 | ADR-0005: `line.cells == line.text.width()` asserted per line; the store is control-free; `copy_text()` is the trim rule |
| R19 | `clipboard-cost.log` unit table: 2/4/6, 7/14/21, 5/2/18, 2/1/3 — and per-character widths summed over the ZWJ family (6) disagree with the family's own width (2) |
| R18 | `flash-e2e-at-pdl5.log`: a reshape's erase→content hole is 8.8–9.3 ms, so the toast is an overlay |
| R17, open-2 | the shift-drag fall-through is **measured as unmeasurable from a pty** (`5b`), which is why R17 has three hatches and not one |

`./scripts/check.sh` is clean (fmt, `clippy --all-targets -D warnings`, 440 tests, 3 ignored) with
the new example added and nothing else touched.
